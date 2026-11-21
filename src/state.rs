//! Process-wide shared state.

use crate::config::Config;
use crate::engine::Engine;
use crate::model::*;
use crate::notify::Notifier;
use bollard::Docker;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tokio::sync::broadcast;

pub struct DockerStatus {
    pub up: bool,
    pub error: Option<String>,
    pub since: Instant,
}

pub struct AppState {
    pub cfg: RwLock<Arc<Config>>,
    pub cfg_path: Option<PathBuf>,
    /// Created lazily: the Docker socket may not exist yet when ctopx starts.
    pub docker: RwLock<Option<Docker>>,
    pub docker_status: RwLock<DockerStatus>,
    pub containers: RwLock<BTreeMap<String, ContainerView>>,
    pub host: RwLock<Option<HostView>>,
    pub engine: Mutex<Engine>,
    pub bus: broadcast::Sender<BusMsg>,
    pub notifier: RwLock<Arc<Notifier>>,
    pub started: Instant,
    /// One-line message for the TUI footer (e.g. web server bind errors).
    pub notice: Mutex<Option<String>>,
}

impl AppState {
    pub fn new(cfg: Config, cfg_path: Option<PathBuf>, docker: Option<Docker>) -> Arc<Self> {
        let (bus, _) = broadcast::channel(256);
        let notifier = Notifier::new(&cfg);
        let engine = Engine::new(cfg.alerts.history_size);
        Arc::new(Self {
            cfg: RwLock::new(Arc::new(cfg)),
            cfg_path,
            docker: RwLock::new(docker),
            docker_status: RwLock::new(DockerStatus {
                up: false,
                error: Some("not connected yet".into()),
                since: Instant::now(),
            }),
            containers: RwLock::new(BTreeMap::new()),
            host: RwLock::new(None),
            engine: Mutex::new(engine),
            bus,
            notifier: RwLock::new(Arc::new(notifier)),
            started: Instant::now(),
            notice: Mutex::new(None),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    pub fn docker_up(&self) -> bool {
        self.docker_status.read().unwrap().up
    }

    /// Re-reads the config file; server/docker connection settings need a restart.
    pub fn reload(&self) -> anyhow::Result<Vec<String>> {
        let Some(path) = &self.cfg_path else {
            anyhow::bail!("no config file in use")
        };
        let new = crate::config::load(path)?;
        let errs = new.validate();
        if !errs.is_empty() {
            anyhow::bail!("config invalid:\n  {}", errs.join("\n  "));
        }
        let notifier = Notifier::new(&new);
        let warnings =
            vec!["server.bind, server.token and docker.* changes need a restart".to_string()];
        *self.notifier.write().unwrap() = Arc::new(notifier);
        *self.cfg.write().unwrap() = Arc::new(new);
        Ok(warnings)
    }
}
