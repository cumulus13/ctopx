//! Docker collection: container list + stats polling, event stream, and actions.

use crate::config::glob;
use crate::engine;
use crate::model::*;
use crate::state::AppState;
use anyhow::{Result, anyhow, bail};
use bollard::Docker;
use bollard::models::{ContainerStatsResponse, ContainerSummaryHealthStatusEnum};
use bollard::query_parameters::{
    EventsOptionsBuilder, KillContainerOptionsBuilder, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, RestartContainerOptionsBuilder, StatsOptionsBuilder,
    StopContainerOptionsBuilder,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Applies `docker.host` from the config as DOCKER_HOST.
pub fn apply_host(host: Option<&str>) {
    if let Some(h) = host.filter(|h| !h.is_empty()) {
        // SAFETY: called from `main` before the async runtime and any other thread starts.
        unsafe { std::env::set_var("DOCKER_HOST", h) };
    }
}

/// Returns the shared Docker client, creating it on first success. Creation fails while the
/// daemon's socket or pipe does not exist, so callers retry on every collection tick.
pub fn client(state: &Arc<AppState>) -> Result<Docker> {
    if let Some(d) = state.docker.read().unwrap().clone() {
        return Ok(d);
    }
    let d = Docker::connect_with_defaults().map_err(|e| anyhow!("{e}"))?;
    *state.docker.write().unwrap() = Some(d.clone());
    Ok(d)
}

#[derive(Default, Clone)]
struct Prev {
    at: Option<Instant>,
    cpu_total: u64,
    sys_total: u64,
    rx: u64,
    tx: u64,
    rd: u64,
    wr: u64,
}

struct Sample {
    cpu_percent: f64,
    mem_bytes: u64,
    mem_limit: u64,
    rx: u64,
    tx: u64,
    rd: u64,
    wr: u64,
    pids: u64,
    cpu_total: u64,
    sys_total: u64,
}

fn parse_stats(s: &ContainerStatsResponse, prev: &Prev, now: Instant) -> Sample {
    let cpu_total = s
        .cpu_stats
        .as_ref()
        .and_then(|c| c.cpu_usage.as_ref())
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let sys_total = s
        .cpu_stats
        .as_ref()
        .and_then(|c| c.system_cpu_usage)
        .unwrap_or(0);
    let online = s
        .cpu_stats
        .as_ref()
        .and_then(|c| c.online_cpus)
        .map(|n| n as f64)
        .or_else(|| {
            s.cpu_stats
                .as_ref()
                .and_then(|c| c.cpu_usage.as_ref())
                .and_then(|u| u.percpu_usage.as_ref())
                .map(|v| v.len() as f64)
        })
        .filter(|n| *n > 0.0)
        .unwrap_or(1.0);

    let cpu_percent = match prev.at {
        None => 0.0,
        Some(t0) => {
            let d_cpu = cpu_total.saturating_sub(prev.cpu_total) as f64;
            if sys_total > 0 && prev.sys_total > 0 {
                let d_sys = sys_total.saturating_sub(prev.sys_total) as f64;
                if d_sys > 0.0 {
                    d_cpu / d_sys * online * 100.0
                } else {
                    0.0
                }
            } else {
                // Windows containers report no system counter: CPU time is in 100ns units.
                let procs = s
                    .num_procs
                    .filter(|n| *n > 0)
                    .map(|n| n as f64)
                    .unwrap_or(online);
                let dt_100ns = now.duration_since(t0).as_nanos() as f64 / 100.0;
                if dt_100ns > 0.0 {
                    d_cpu / (dt_100ns * procs) * 100.0
                } else {
                    0.0
                }
            }
        }
    };

    let (mem_bytes, mem_limit) = match &s.memory_stats {
        Some(m) => {
            let usage = m.usage.unwrap_or(0);
            let used = if let Some(pws) = m.privateworkingset.filter(|v| *v > 0) {
                pws
            } else {
                let cache = m
                    .stats
                    .as_ref()
                    .and_then(|st| {
                        st.get("total_inactive_file")
                            .or_else(|| st.get("inactive_file"))
                            .or_else(|| st.get("cache"))
                    })
                    .copied()
                    .unwrap_or(0);
                usage.saturating_sub(cache)
            };
            (used, m.limit.unwrap_or(0))
        }
        None => (0, 0),
    };

    let (mut rx, mut tx) = (0u64, 0u64);
    if let Some(nets) = &s.networks {
        for n in nets.values() {
            rx += n.rx_bytes.unwrap_or(0);
            tx += n.tx_bytes.unwrap_or(0);
        }
    }
    let (mut rd, mut wr) = (0u64, 0u64);
    if let Some(entries) = s
        .blkio_stats
        .as_ref()
        .and_then(|b| b.io_service_bytes_recursive.as_ref())
    {
        for e in entries {
            match e.op.as_deref().map(|o| o.to_ascii_lowercase()).as_deref() {
                Some("read") => rd += e.value.unwrap_or(0),
                Some("write") => wr += e.value.unwrap_or(0),
                _ => {}
            }
        }
    }
    let pids = s.pids_stats.as_ref().and_then(|p| p.current).unwrap_or(0);
    Sample {
        cpu_percent: cpu_percent.clamp(0.0, 100.0 * online),
        mem_bytes,
        mem_limit,
        rx,
        tx,
        rd,
        wr,
        pids,
        cpu_total,
        sys_total,
    }
}

fn rate(cur: u64, prev: u64, dt: f64) -> f64 {
    if dt > 0.0 {
        cur.saturating_sub(prev) as f64 / dt
    } else {
        0.0
    }
}

fn health_str(h: &Option<bollard::models::ContainerSummaryHealth>) -> Option<String> {
    match h.as_ref().and_then(|h| h.status.as_ref()) {
        Some(ContainerSummaryHealthStatusEnum::HEALTHY) => Some("healthy".into()),
        Some(ContainerSummaryHealthStatusEnum::UNHEALTHY) => Some("unhealthy".into()),
        Some(ContainerSummaryHealthStatusEnum::STARTING) => Some("starting".into()),
        _ => None,
    }
}

/// Containers whose stats failed recently are left alone for a while so a stuck container
/// cannot make ctopx pile requests onto an already struggling daemon.
type Backoff = HashMap<String, (u32, Instant)>;

/// Returns how many containers had their stats fetched.
async fn poll_containers(
    state: &Arc<AppState>,
    prev: &mut HashMap<String, Prev>,
    backoff: &mut Backoff,
    live: bool,
    every: Duration,
) -> Result<usize> {
    let cfg = state.cfg();
    let docker = client(state)?;
    state.count_request();
    let list = docker
        .list_containers(Some(
            ListContainersOptionsBuilder::default().all(true).build(),
        ))
        .await?;

    struct Row {
        view: ContainerView,
    }
    let mut rows: Vec<Row> = vec![];
    for c in list {
        let Some(id) = c.id.clone() else { continue };
        let name = c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_else(|| id.chars().take(12).collect());
        if !cfg.docker.include.iter().any(|p| glob(p, &name))
            || cfg.docker.exclude.iter().any(|p| glob(p, &name))
        {
            continue;
        }
        let ports = c
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| match p.public_port {
                        Some(pp) => format!(
                            "{}:{}->{}/{}",
                            p.ip.clone().unwrap_or_default(),
                            pp,
                            p.private_port,
                            p.typ.as_ref().map(|t| t.to_string()).unwrap_or_default()
                        ),
                        None => format!(
                            "{}/{}",
                            p.private_port,
                            p.typ.as_ref().map(|t| t.to_string()).unwrap_or_default()
                        ),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let view = ContainerView {
            short_id: id.chars().take(12).collect(),
            id,
            name,
            image: c.image.clone().unwrap_or_default(),
            project: c
                .labels
                .as_ref()
                .and_then(|l| l.get("com.docker.compose.project").cloned()),
            state: c.state.as_ref().map(|s| s.to_string()).unwrap_or_default(),
            status: c.status.clone().unwrap_or_default(),
            health: health_str(&c.health),
            created: c.created.unwrap_or(0),
            ports,
            ..Default::default()
        };
        rows.push(Row { view });
    }

    // Decide which running containers need stats this round. With nobody watching, only those
    // that an enabled container-metric rule can match.
    let rules = cfg.effective_rules();
    let metric_rules: Vec<_> = rules
        .iter()
        .filter(|r| r.enabled)
        .filter(|r| matches!(&r.kind, crate::config::RuleKind::Metric { metric, .. } if !metric.is_host()))
        .collect();
    let now = Instant::now();
    let wanted: Vec<String> = rows
        .iter()
        .filter(|r| r.view.is_running())
        .filter(|r| live || metric_rules.iter().any(|m| m.matches_subject(&r.view.name)))
        .filter(|r| {
            backoff
                .get(&r.view.id)
                .is_none_or(|(_, until)| *until <= now)
        })
        .map(|r| r.view.id.clone())
        .collect();
    let polled = wanted.len();
    state
        .requests
        .fetch_add(polled as u64, std::sync::atomic::Ordering::Relaxed);

    let docker2 = docker.clone();
    let fetched: Vec<(String, Option<ContainerStatsResponse>)> = futures_util::stream::iter(wanted)
        .map(|id| {
            let docker = docker2.clone();
            async move {
                let opts = StatsOptionsBuilder::default()
                    .stream(false)
                    .one_shot(true)
                    .build();
                let mut st = docker.stats(&id, Some(opts));
                match tokio::time::timeout(Duration::from_secs(5), st.next()).await {
                    Ok(Some(Ok(s))) => (id, Some(s)),
                    _ => (id, None),
                }
            }
        })
        .buffer_unordered(cfg.docker.max_concurrency.max(1))
        .collect()
        .await;
    let now = Instant::now();
    let mut stats: HashMap<String, ContainerStatsResponse> = HashMap::new();
    for (id, s) in fetched {
        match s {
            Some(s) => {
                backoff.remove(&id);
                stats.insert(id, s);
            }
            None => {
                let n = backoff.get(&id).map(|(n, _)| *n).unwrap_or(0) + 1;
                let wait = every
                    .saturating_mul(1 << n.min(5))
                    .min(Duration::from_secs(60));
                tracing::debug!("stats for {id} failed {n} time(s); skipping it for {wait:?}");
                backoff.insert(id, (n, now + wait));
            }
        }
    }
    // Do not average CPU over a long gap (for example right after idle polling).
    let max_age = Duration::from_secs(60).max(every * 3);
    for p in prev.values_mut() {
        if p.at.is_some_and(|t| now.duration_since(t) > max_age) {
            *p = Prev::default();
        }
    }

    let mut map = state.containers.write().unwrap();
    let old = std::mem::take(&mut *map);
    let mut live_ids = std::collections::HashSet::new();
    for Row { mut view } in rows {
        live_ids.insert(view.id.clone());
        let prior = old.get(&view.name);
        view.history = prior.map(|p| p.history.clone()).unwrap_or_default();
        view.updated_at = now_ms();
        if let Some(s) = stats.get(&view.id) {
            let p = prev.entry(view.id.clone()).or_default();
            let smp = parse_stats(s, p, now);
            let dt =
                p.at.map(|t| now.duration_since(t).as_secs_f64())
                    .unwrap_or(0.0);
            view.cpu_percent = smp.cpu_percent;
            view.mem_bytes = smp.mem_bytes;
            view.mem_limit = smp.mem_limit;
            view.mem_percent = if smp.mem_limit > 0 {
                smp.mem_bytes as f64 / smp.mem_limit as f64 * 100.0
            } else {
                0.0
            };
            view.net_rx_bps = if p.at.is_some() {
                rate(smp.rx, p.rx, dt)
            } else {
                0.0
            };
            view.net_tx_bps = if p.at.is_some() {
                rate(smp.tx, p.tx, dt)
            } else {
                0.0
            };
            view.blk_read_bps = if p.at.is_some() {
                rate(smp.rd, p.rd, dt)
            } else {
                0.0
            };
            view.blk_write_bps = if p.at.is_some() {
                rate(smp.wr, p.wr, dt)
            } else {
                0.0
            };
            view.net_rx_total = smp.rx;
            view.net_tx_total = smp.tx;
            view.pids = smp.pids;
            *p = Prev {
                at: Some(now),
                cpu_total: smp.cpu_total,
                sys_total: smp.sys_total,
                rx: smp.rx,
                tx: smp.tx,
                rd: smp.rd,
                wr: smp.wr,
            };
            view.history.push(
                view.cpu_percent as f32,
                view.mem_percent as f32,
                view.net_rx_bps as f32,
                view.net_tx_bps as f32,
            );
        } else if view.is_running() {
            // Stats unavailable this round: keep last known numbers rather than flapping to zero.
            if let Some(p) = prior {
                view.cpu_percent = p.cpu_percent;
                view.mem_bytes = p.mem_bytes;
                view.mem_limit = p.mem_limit;
                view.mem_percent = p.mem_percent;
                view.net_rx_bps = p.net_rx_bps;
                view.net_tx_bps = p.net_tx_bps;
                view.net_rx_total = p.net_rx_total;
                view.net_tx_total = p.net_tx_total;
                view.blk_read_bps = p.blk_read_bps;
                view.blk_write_bps = p.blk_write_bps;
                view.pids = p.pids;
            }
        } else {
            prev.remove(&view.id);
        }
        map.insert(view.name.clone(), view);
    }
    prev.retain(|id, _| live_ids.contains(id));
    backoff.retain(|id, _| live_ids.contains(id));
    Ok(polled)
}

fn set_status(state: &Arc<AppState>, up: bool, err: Option<String>) {
    let mut s = state.docker_status.write().unwrap();
    if s.up != up {
        s.since = Instant::now();
        if up {
            tracing::info!("Docker daemon reachable");
        } else {
            tracing::warn!(
                "Docker daemon unreachable: {}",
                err.clone().unwrap_or_default()
            );
        }
    }
    s.up = up;
    s.error = err;
}

/// Main collection loop: Docker + host metrics, then rule evaluation.
///
/// Polling pace adapts so ctopx does not load the Docker daemon needlessly: `interval_secs`
/// while a UI is open, `idle_interval_secs` otherwise, and it backs off further (up to 16x)
/// while Docker takes a large share of the interval just to answer.
pub async fn run_collector(state: Arc<AppState>) {
    let mut prev: HashMap<String, Prev> = HashMap::new();
    let mut backoff: Backoff = HashMap::new();
    let mut host = crate::host::HostCollector::new();
    let mut factor: u32 = 1;
    loop {
        let cfg = state.cfg();
        let live = state.viewers.load(std::sync::atomic::Ordering::Relaxed) > 0;
        let base = Duration::from_secs(
            if live {
                cfg.docker.interval_secs
            } else {
                cfg.docker.idle_interval_secs.max(cfg.docker.interval_secs)
            }
            .max(1),
        );
        let every = base * factor;
        let req_before = state.requests.load(std::sync::atomic::Ordering::Relaxed);
        let started = Instant::now();
        let mut polled = 0;
        match poll_containers(&state, &mut prev, &mut backoff, live, every).await {
            Ok(n) => {
                polled = n;
                set_status(&state, true, None)
            }
            Err(e) => set_status(&state, false, Some(format!("{e}"))),
        }
        if cfg.host.enabled {
            let view = host.collect();
            *state.host.write().unwrap() = Some(view);
        }
        engine::evaluate(&state);
        let spent = started.elapsed();

        if cfg.docker.adaptive {
            if spent > base.mul_f32(0.4) {
                factor = (factor * 2).min(16);
            } else if spent < base.mul_f32(0.15) && factor > 1 {
                factor /= 2;
            }
        }
        let reqs = state.requests.load(std::sync::atomic::Ordering::Relaxed) - req_before;
        *state.poll.write().unwrap() = PollInfo {
            interval_ms: (base * factor).as_millis() as u64,
            cycle_ms: spent.as_millis() as u64,
            throttle: factor,
            requests_per_sec: reqs as f64 / (base * factor).as_secs_f64().max(0.001),
            requests_total: state.requests.load(std::sync::atomic::Ordering::Relaxed),
            stats_polled: polled,
            mode: if live { "live" } else { "idle" }.into(),
        };
        let wait = (base * factor)
            .saturating_sub(spent)
            .max(Duration::from_millis(200));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = state.wake.notified() => {}
        }
    }
}

/// Streams Docker events, reconnecting forever.
pub async fn run_events(state: Arc<AppState>) {
    loop {
        let Ok(docker) = client(&state) else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        let mut filters: HashMap<&str, Vec<&str>> = HashMap::new();
        filters.insert("type", vec!["container"]);
        let opts = EventsOptionsBuilder::default().filters(&filters).build();
        let mut stream = Box::pin(docker.events(Some(opts)));
        while let Some(item) = stream.next().await {
            let msg = match item {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!("event stream error: {e}");
                    break;
                }
            };
            let Some(actor) = msg.actor else { continue };
            let raw = msg.action.unwrap_or_default();
            if raw.starts_with("exec_") {
                continue;
            }
            let action = match raw.split_once(':') {
                Some(("health_status", s)) => format!("health_{}", s.trim()),
                _ => raw,
            };
            let attrs = actor.attributes.unwrap_or_default();
            let name = attrs.get("name").cloned().unwrap_or_else(|| {
                actor
                    .id
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(12)
                    .collect()
            });
            let ev = DockerEvent {
                id: actor.id.unwrap_or_default(),
                name,
                action: action.clone(),
                attrs,
            };
            if action == "die" {
                let code = ev.attrs.get("exitCode").map(String::as_str).unwrap_or("0");
                if code != "0" {
                    let mut crash = ev.clone();
                    crash.action = "crash".into();
                    engine::on_event(&state, crash);
                }
            }
            engine::on_event(&state, ev);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
    Kill,
    Remove,
}

impl std::str::FromStr for Action {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "start" => Ok(Action::Start),
            "stop" => Ok(Action::Stop),
            "restart" => Ok(Action::Restart),
            "pause" => Ok(Action::Pause),
            "unpause" => Ok(Action::Unpause),
            "kill" => Ok(Action::Kill),
            "remove" => Ok(Action::Remove),
            _ => bail!(
                "unknown action '{s}' (use start, stop, restart, pause, unpause, kill or remove)"
            ),
        }
    }
}

/// Where an action request came from. Remote (HTTP) needs `server.allow_actions` or
/// `--allow-actions`; Local (the terminal UI) is allowed unless started with `--read-only`.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    Local,
    Remote,
}

pub async fn container_action(
    state: &Arc<AppState>,
    name: &str,
    action: Action,
    source: Source,
) -> Result<()> {
    match source {
        Source::Remote if !state.remote_actions_allowed() => {
            bail!(
                "container actions are disabled: set server.allow_actions = true or start with --allow-actions"
            )
        }
        Source::Local
            if !state
                .local_actions
                .load(std::sync::atomic::Ordering::Relaxed) =>
        {
            bail!("container actions are disabled: ctopx was started with --read-only")
        }
        _ => {}
    }
    let id = state
        .containers
        .read()
        .unwrap()
        .get(name)
        .map(|c| c.id.clone())
        .ok_or_else(|| anyhow!("unknown container '{name}'"))?;
    let docker = client(state)?;
    state.count_request();
    match action {
        Action::Start => docker.start_container(&id, None).await?,
        Action::Stop => {
            docker
                .stop_container(
                    &id,
                    Some(StopContainerOptionsBuilder::default().t(10).build()),
                )
                .await?
        }
        Action::Restart => {
            docker
                .restart_container(
                    &id,
                    Some(RestartContainerOptionsBuilder::default().t(10).build()),
                )
                .await?
        }
        Action::Pause => docker.pause_container(&id).await?,
        Action::Unpause => docker.unpause_container(&id).await?,
        Action::Kill => {
            docker
                .kill_container(
                    &id,
                    Some(
                        KillContainerOptionsBuilder::default()
                            .signal("SIGKILL")
                            .build(),
                    ),
                )
                .await?
        }
        // No force: Docker refuses to remove a running container, which is the safe behaviour.
        Action::Remove => {
            docker
                .remove_container(&id, Some(RemoveContainerOptionsBuilder::default().force(false).build()))
                .await
                .map_err(|e| {
                    if e.to_string().contains("409") {
                        anyhow!("Docker refused to remove {name} (409 conflict). A running container must be stopped first.")
                    } else {
                        anyhow::Error::from(e)
                    }
                })?;
        }
    }
    state.wake.notify_one();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::Ordering;

    fn state() -> Arc<AppState> {
        AppState::new(Config::default(), None, None)
    }

    #[test]
    fn parses_every_action() {
        for (s, a) in [
            ("start", Action::Start),
            ("stop", Action::Stop),
            ("restart", Action::Restart),
            ("pause", Action::Pause),
            ("unpause", Action::Unpause),
            ("kill", Action::Kill),
            ("remove", Action::Remove),
        ] {
            assert_eq!(s.parse::<Action>().unwrap(), a);
        }
        assert!("explode".parse::<Action>().is_err());
    }

    #[tokio::test]
    async fn remote_actions_are_off_by_default() {
        let st = state();
        let e = container_action(&st, "x", Action::Stop, Source::Remote)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("--allow-actions"), "{e}");
    }

    #[tokio::test]
    async fn allow_actions_flag_opens_the_remote_gate() {
        let st = state();
        st.actions_forced.store(true, Ordering::Relaxed);
        // Past the gate the next failure is the unknown container, not "disabled".
        let e = container_action(&st, "x", Action::Stop, Source::Remote)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("unknown container"), "{e}");
    }

    #[tokio::test]
    async fn local_actions_default_on_and_read_only_blocks_them() {
        let st = state();
        let e = container_action(&st, "x", Action::Stop, Source::Local)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("unknown container"), "{e}");
        st.local_actions.store(false, Ordering::Relaxed);
        let e = container_action(&st, "x", Action::Stop, Source::Local)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("--read-only"), "{e}");
    }

    #[test]
    fn viewer_guard_counts_open_uis() {
        let st = state();
        assert_eq!(st.viewers.load(Ordering::Relaxed), 0);
        let g1 = st.add_viewer();
        let g2 = st.add_viewer();
        assert_eq!(st.viewers.load(Ordering::Relaxed), 2);
        drop(g1);
        drop(g2);
        assert_eq!(st.viewers.load(Ordering::Relaxed), 0);
    }
}
