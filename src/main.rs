mod config;
mod docker;
mod engine;
mod host;
mod model;
mod notify;
mod state;
mod tui;
mod web;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use clap_color_help::default_styles;
use clap_version_flag::colorful_version;
use std::path::PathBuf;
use std::sync::Arc;

const EXAMPLE_CONFIG: &str = include_str!("../ctopx.example.toml");

#[derive(Parser)]
#[command(
    name = "ctopx",
    version,
    about = "ctop-style container monitor with alert notifications, REST API and web UI",
    styles = default_styles()
)]
struct Cli {
    /// Path to ctopx.toml (default: $CTOPX_CONFIG, ./ctopx.toml, then the user config dir)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive terminal UI (default). Alerts keep working while it is open.
    Tui {
        /// Also serve the web UI and API
        #[arg(long)]
        serve: bool,
        #[arg(long)]
        bind: Option<String>,
        /// Allow start/stop/restart/pause/kill/remove from the web UI and API too
        #[arg(long)]
        allow_actions: bool,
        /// Disable container actions in the terminal UI (they are on by default there)
        #[arg(long)]
        read_only: bool,
    },
    /// Headless mode: collector, alerts, REST API and web UI
    Serve {
        #[arg(long)]
        bind: Option<String>,
        /// Allow start/stop/restart/pause/kill/remove from the web UI and API
        /// (same as server.allow_actions = true)
        #[arg(long)]
        allow_actions: bool,
    },
    /// Write an example config file
    Init {
        #[arg(default_value = "ctopx.toml")]
        path: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Validate the config and print a summary
    Check,
    /// Send a test notification to one channel (or all)
    TestNotify { channel: Option<String> },
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && (args[1] == "-V" || args[1] == "--version") {
        let version = colorful_version!();
        version.print_and_exit();
    }
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::Init { path, force }) = &cli.cmd {
        if path.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite)",
                path.display()
            );
        }
        std::fs::write(path, EXAMPLE_CONFIG)?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    let cfg_path = config::find_config(cli.config.as_deref());
    let (cfg, cfg_path) = match &cfg_path {
        Some(p) => (config::load(p)?, Some(p.clone())),
        None => (config::Config::default(), None),
    };
    let errs = cfg.validate();
    if !errs.is_empty() {
        bail!("invalid configuration:\n  {}", errs.join("\n  "));
    }

    if matches!(cli.cmd, Some(Cmd::Check)) {
        match &cfg_path {
            Some(p) => println!("config: {}", p.display()),
            None => println!(
                "config: none found, using built-in defaults (run `ctopx init` to create one)"
            ),
        }
        println!(
            "channels: {}",
            if cfg.channels.is_empty() {
                "none (alerts will only show in the UI)".to_string()
            } else {
                cfg.channels
                    .iter()
                    .map(|c| format!("{} [{}]", c.name, c.kind.type_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        let rules = cfg.effective_rules();
        println!(
            "rules: {} ({} enabled)",
            rules.len(),
            rules.iter().filter(|r| r.enabled).count()
        );
        for r in rules {
            println!(
                "  {:<28} {:<9} {}",
                r.name,
                r.severity.as_str(),
                r.describe()
            );
        }
        println!("ok");
        return Ok(());
    }

    docker::apply_host(cfg.docker.host.as_deref());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let state = state::AppState::new(cfg, cfg_path, None);

    match cli.cmd.unwrap_or(Cmd::Tui {
        serve: false,
        bind: None,
        allow_actions: false,
        read_only: false,
    }) {
        Cmd::Serve {
            bind,
            allow_actions,
        } => {
            init_logging(false);
            state
                .actions_forced
                .store(allow_actions, std::sync::atomic::Ordering::Relaxed);
            rt.block_on(async move {
                if state.cfg().channels.is_empty() {
                    tracing::warn!("no notification channels configured: alerts will only be visible in the UI and API");
                }
                tokio::spawn(docker::run_collector(state.clone()));
                tokio::spawn(docker::run_events(state.clone()));
                web::serve(state, bind).await
            })
        }
        Cmd::Tui {
            serve,
            bind,
            allow_actions,
            read_only,
        } => {
            init_logging(true);
            state
                .actions_forced
                .store(allow_actions, std::sync::atomic::Ordering::Relaxed);
            state
                .local_actions
                .store(!read_only, std::sync::atomic::Ordering::Relaxed);
            rt.block_on(async move {
                tokio::spawn(docker::run_collector(state.clone()));
                tokio::spawn(docker::run_events(state.clone()));
                if serve {
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = web::serve(st.clone(), bind).await {
                            tracing::error!("web server stopped: {e:#}");
                            *st.notice.lock().unwrap() = Some(format!("web server: {e:#}"));
                        }
                    });
                }
                tui::run(state).await
            })
        }
        Cmd::TestNotify { channel } => rt.block_on(test_notify(state, channel)),
        Cmd::Init { .. } | Cmd::Check => unreachable!(),
    }
}

async fn test_notify(state: Arc<state::AppState>, channel: Option<String>) -> Result<()> {
    let names: Vec<String> = match channel {
        Some(c) => vec![c],
        None => state
            .cfg()
            .channels
            .iter()
            .map(|c| c.name.clone())
            .collect(),
    };
    if names.is_empty() {
        bail!("no channels configured");
    }
    let mut failed = 0;
    for n in names {
        match notify::test_channel(&state, &n).await {
            Ok(()) => println!("{n}: ok"),
            Err(e) => {
                failed += 1;
                println!("{n}: FAILED: {e:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} channel(s) failed");
    }
    Ok(())
}

fn init_logging(to_file: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ctopx=info,warn".into());
    if to_file {
        let path = std::env::temp_dir().join("ctopx.log");
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::sync::Mutex::new(f))
                .with_ansi(false)
                .init();
        }
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
