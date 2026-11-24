//! Alert engine: evaluates rules against collected metrics and Docker events.

use crate::config::{Config, MetricKind, RuleCfg, RuleKind};
use crate::model::*;
use crate::notify;
use crate::state::AppState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Tracker {
    pending_since: Option<Instant>,
    firing: bool,
}

pub struct Engine {
    next_id: u64,
    next_silence: u64,
    history_cap: usize,
    active: HashMap<(String, String), Alert>,
    alert_notified_at: HashMap<(String, String), Instant>,
    trackers: HashMap<(String, String), Tracker>,
    last_sent: HashMap<(String, String), Instant>,
    die_times: HashMap<String, VecDeque<Instant>>,
    pub history: VecDeque<Alert>,
    pub events: VecDeque<EventRecord>,
    pub deliveries: VecDeque<Delivery>,
    pub silences: Vec<Silence>,
}

impl Engine {
    pub fn new(history_cap: usize) -> Self {
        Self {
            next_id: 1,
            next_silence: 1,
            history_cap: history_cap.max(50),
            active: HashMap::new(),
            alert_notified_at: HashMap::new(),
            trackers: HashMap::new(),
            last_sent: HashMap::new(),
            die_times: HashMap::new(),
            history: VecDeque::new(),
            events: VecDeque::new(),
            deliveries: VecDeque::new(),
            silences: vec![],
        }
    }

    pub fn active_alerts(&self) -> Vec<Alert> {
        let mut v: Vec<Alert> = self.active.values().cloned().collect();
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.started_at.cmp(&a.started_at))
        });
        v
    }

    pub fn record_delivery(&mut self, d: Delivery) {
        if self.deliveries.len() >= 200 {
            self.deliveries.pop_front();
        }
        self.deliveries.push_back(d);
    }

    pub fn add_silence(
        &mut self,
        pattern: String,
        rule: Option<String>,
        duration_secs: u64,
        reason: String,
    ) -> Silence {
        let s = Silence {
            id: self.next_silence,
            pattern,
            rule,
            until: now_ms() + (duration_secs as i64) * 1000,
            reason,
        };
        self.next_silence += 1;
        self.silences.push(s.clone());
        s
    }

    pub fn remove_silence(&mut self, id: u64) -> bool {
        let before = self.silences.len();
        self.silences.retain(|s| s.id != id);
        before != self.silences.len()
    }

    fn silenced(&mut self, rule: &str, subject: &str) -> bool {
        let now = now_ms();
        self.silences.retain(|s| s.until > now);
        self.silences.iter().any(|s| {
            s.rule.as_deref().is_none_or(|r| r == rule) && crate::config::glob(&s.pattern, subject)
        })
    }

    fn cooled_down(&self, key: &(String, String), cooldown: u64, now: Instant) -> bool {
        self.last_sent
            .get(key)
            .is_none_or(|t| now.duration_since(*t) >= Duration::from_secs(cooldown))
    }

    fn push_history(&mut self, a: Alert) {
        if self.history.len() >= self.history_cap {
            self.history.pop_front();
        }
        self.history.push_back(a);
    }

    #[allow(clippy::too_many_arguments)]
    fn fire(
        &mut self,
        bus: &tokio::sync::broadcast::Sender<BusMsg>,
        inst: &str,
        rule: &RuleCfg,
        subject: &str,
        value: Option<f64>,
        title: String,
        message: String,
        now: Instant,
        out: &mut Vec<Outgoing>,
    ) {
        let key = (rule.name.clone(), subject.to_string());
        let silenced = self.silenced(&rule.name, subject);
        let send = !silenced && self.cooled_down(&key, rule.cooldown(), now);
        let alert = Alert {
            id: self.next_id,
            rule: rule.name.clone(),
            subject: subject.to_string(),
            severity: rule.severity,
            state: AlertState::Firing,
            message: message.clone(),
            value,
            started_at: now_ms(),
            resolved_at: None,
            notified: send,
            silenced,
        };
        self.next_id += 1;
        if send {
            self.last_sent.insert(key.clone(), now);
            self.alert_notified_at.insert(key.clone(), now);
            out.push(Outgoing {
                n: Notification {
                    title: format!("[{inst}] {title}"),
                    body: message,
                    severity: rule.severity,
                    kind: "firing".into(),
                    rule: rule.name.clone(),
                    subject: subject.into(),
                    value,
                    ts: now_ms(),
                    force: false,
                },
                channels: rule.channels.clone(),
            });
        }
        let _ = bus.send(BusMsg::Alert(alert.clone()));
        self.active.insert(key, alert);
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &mut self,
        bus: &tokio::sync::broadcast::Sender<BusMsg>,
        inst: &str,
        rule: &RuleCfg,
        subject: &str,
        title: String,
        message: String,
        quiet: bool,
        out: &mut Vec<Outgoing>,
    ) {
        let key = (rule.name.clone(), subject.to_string());
        self.alert_notified_at.remove(&key);
        if let Some(mut a) = self.active.remove(&key) {
            a.state = AlertState::Resolved;
            a.resolved_at = Some(now_ms());
            if a.notified && rule.notify_resolved && !quiet && !self.silenced(&rule.name, subject) {
                out.push(Outgoing {
                    n: Notification {
                        title: format!("[{inst}] {title}"),
                        body: message,
                        severity: rule.severity.min(Severity::Warning),
                        kind: "resolved".into(),
                        rule: rule.name.clone(),
                        subject: subject.into(),
                        value: None,
                        ts: now_ms(),
                        force: false,
                    },
                    channels: rule.channels.clone(),
                });
            }
            let _ = bus.send(BusMsg::Alert(a.clone()));
            self.push_history(a);
        }
    }
}

fn metric_value(m: MetricKind, c: &ContainerView) -> f64 {
    match m {
        MetricKind::CpuPercent => c.cpu_percent,
        MetricKind::MemPercent => c.mem_percent,
        MetricKind::MemBytes => c.mem_bytes as f64,
        MetricKind::NetRxBps => c.net_rx_bps,
        MetricKind::NetTxBps => c.net_tx_bps,
        MetricKind::BlkReadBps => c.blk_read_bps,
        MetricKind::BlkWriteBps => c.blk_write_bps,
        MetricKind::Pids => c.pids as f64,
        _ => 0.0,
    }
}

fn host_value(m: MetricKind, h: &HostView) -> f64 {
    match m {
        MetricKind::HostCpuPercent => h.cpu_percent,
        MetricKind::HostMemPercent => h.mem_percent,
        MetricKind::HostSwapPercent => h.swap_percent,
        MetricKind::HostDiskPercent => h.disk_percent_max,
        MetricKind::HostLoad1 => h.load1,
        _ => 0.0,
    }
}

/// Runs every non-event rule against the latest state. Call once per collection tick.
pub fn evaluate(state: &Arc<AppState>) {
    let cfg: Arc<Config> = state.cfg();
    let rules = cfg.effective_rules();
    let inst = cfg.instance_name();
    let containers: Vec<ContainerView> =
        state.containers.read().unwrap().values().cloned().collect();
    let host = state.host.read().unwrap().clone();
    let (docker_up, docker_err, docker_off) = {
        let s = state.docker_status.read().unwrap();
        (s.up, s.error.clone(), s.disabled)
    };
    let now = Instant::now();
    let mut out: Vec<Outgoing> = vec![];
    {
        let mut e = state.engine.lock().unwrap();
        for rule in rules.iter() {
            let rname = &rule.name;
            // Subjects currently observed: (subject, condition, value, message-if-firing)
            struct Obs {
                subject: String,
                cond: bool,
                value: Option<f64>,
                title: String,
                msg: String,
                ok_title: String,
                ok_msg: String,
            }
            let mut obs: Vec<Obs> = vec![];
            let for_secs;
            if !rule.enabled {
                // Disabled rules clear anything they had open.
                let keys: Vec<_> = e.active.keys().filter(|k| &k.0 == rname).cloned().collect();
                for k in keys {
                    e.resolve(
                        &state.bus,
                        &inst,
                        rule,
                        &k.1,
                        String::new(),
                        String::new(),
                        true,
                        &mut out,
                    );
                    e.trackers.remove(&k);
                }
                continue;
            }
            match &rule.kind {
                RuleKind::Metric {
                    metric,
                    op,
                    threshold,
                    for_secs: f,
                } => {
                    for_secs = *f;
                    if metric.is_host() {
                        if let Some(h) = &host {
                            let v = host_value(*metric, h);
                            obs.push(Obs {
                                subject: "host".into(),
                                cond: op.test(v, *threshold),
                                value: Some(v),
                                title: format!(
                                    "host: {} {} {}",
                                    metric.label(),
                                    op.symbol(),
                                    metric.fmt(*threshold)
                                ),
                                msg: format!(
                                    "{} is {} ({} {})",
                                    metric.label(),
                                    metric.fmt(v),
                                    op.symbol(),
                                    metric.fmt(*threshold)
                                ),
                                ok_title: format!("host: {} back to normal", metric.label()),
                                ok_msg: format!("{} is {}", metric.label(), metric.fmt(v)),
                            });
                        }
                    } else {
                        for c in containers
                            .iter()
                            .filter(|c| c.is_running() && rule.matches_subject(&c.name))
                        {
                            let v = metric_value(*metric, c);
                            obs.push(Obs {
                                subject: c.name.clone(),
                                cond: op.test(v, *threshold),
                                value: Some(v),
                                title: format!(
                                    "{}: {} {} {}",
                                    c.name,
                                    metric.label(),
                                    op.symbol(),
                                    metric.fmt(*threshold)
                                ),
                                msg: format!(
                                    "{} is {} for container {} ({})",
                                    metric.label(),
                                    metric.fmt(v),
                                    c.name,
                                    c.image
                                ),
                                ok_title: format!("{}: {} back to normal", c.name, metric.label()),
                                ok_msg: format!("{} is {}", metric.label(), metric.fmt(v)),
                            });
                        }
                    }
                }
                RuleKind::DockerDaemon { for_secs: f } => {
                    for_secs = *f;
                    // With docker.enabled = false there is no daemon to watch.
                    if !docker_off {
                        obs.push(Obs {
                            subject: "docker".into(),
                            cond: !docker_up,
                            value: None,
                            title: "Docker daemon unreachable".into(),
                            msg: format!(
                                "Cannot reach the Docker daemon: {}",
                                docker_err.clone().unwrap_or_default()
                            ),
                            ok_title: "Docker daemon reachable again".into(),
                            ok_msg: "The Docker daemon is responding again".into(),
                        });
                    }
                }
                RuleKind::RestartLoop {
                    restarts,
                    window_secs,
                } => {
                    for_secs = 0;
                    let window = Duration::from_secs(*window_secs);
                    for c in containers.iter().filter(|c| rule.matches_subject(&c.name)) {
                        let n = e
                            .die_times
                            .get(&c.name)
                            .map(|q| {
                                q.iter()
                                    .filter(|t| now.duration_since(**t) <= window)
                                    .count()
                            })
                            .unwrap_or(0);
                        obs.push(Obs {
                            subject: c.name.clone(),
                            cond: n as u32 >= *restarts,
                            value: Some(n as f64),
                            title: format!("{}: restart loop", c.name),
                            msg: format!(
                                "{} died {} times in the last {}s",
                                c.name, n, window_secs
                            ),
                            ok_title: format!("{}: restart loop ended", c.name),
                            ok_msg: format!("{} has been stable for {}s", c.name, window_secs),
                        });
                    }
                }
                RuleKind::Event { .. } => continue,
            }

            let mut seen: HashSet<(String, String)> = HashSet::new();
            for o in obs {
                let key = (rname.clone(), o.subject.clone());
                seen.insert(key.clone());
                let t = e.trackers.entry(key.clone()).or_insert(Tracker {
                    pending_since: None,
                    firing: false,
                });
                if o.cond {
                    let since = *t.pending_since.get_or_insert(now);
                    if !t.firing && now.duration_since(since) >= Duration::from_secs(for_secs) {
                        t.firing = true;
                        e.fire(
                            &state.bus, &inst, rule, &o.subject, o.value, o.title, o.msg, now,
                            &mut out,
                        );
                    } else if t.firing {
                        // Keep the live value on the open alert and send reminders.
                        let remind = rule.repeat_secs.is_some_and(|r| {
                            e.alert_notified_at
                                .get(&key)
                                .is_some_and(|t0| now.duration_since(*t0) >= Duration::from_secs(r))
                        });
                        if let Some(a) = e.active.get_mut(&key) {
                            a.value = o.value;
                            a.message = o.msg.clone();
                        }
                        if remind && !e.silenced(rname, &o.subject) {
                            e.alert_notified_at.insert(key.clone(), now);
                            out.push(Outgoing {
                                n: Notification {
                                    title: format!("[{inst}] {} (still firing)", o.title),
                                    body: o.msg,
                                    severity: rule.severity,
                                    kind: "reminder".into(),
                                    rule: rname.clone(),
                                    subject: o.subject.clone(),
                                    value: o.value,
                                    ts: now_ms(),
                                    force: false,
                                },
                                channels: rule.channels.clone(),
                            });
                        }
                    }
                } else {
                    t.pending_since = None;
                    if t.firing {
                        t.firing = false;
                        e.resolve(
                            &state.bus, &inst, rule, &o.subject, o.ok_title, o.ok_msg, false,
                            &mut out,
                        );
                    }
                }
            }
            // Subjects that vanished (container stopped/removed): close quietly.
            let gone: Vec<_> = e
                .trackers
                .keys()
                .filter(|k| &k.0 == rname && !seen.contains(*k))
                .cloned()
                .collect();
            for k in gone {
                if e.active.contains_key(&k) {
                    e.resolve(
                        &state.bus,
                        &inst,
                        rule,
                        &k.1,
                        String::new(),
                        String::new(),
                        true,
                        &mut out,
                    );
                }
                e.trackers.remove(&k);
            }
        }
        // Forget die timestamps older than an hour.
        for q in e.die_times.values_mut() {
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(3600))
            {
                q.pop_front();
            }
        }
    }
    for o in out {
        notify::dispatch(state, o);
    }
    let _ = state.bus.send(BusMsg::Tick);
}

fn event_message(ev: &DockerEvent) -> (String, String) {
    let image = ev.attrs.get("image").cloned().unwrap_or_default();
    let code = ev.attrs.get("exitCode").cloned().unwrap_or_default();
    let name = &ev.name;
    let (headline, body) = match ev.action.as_str() {
        "start" => (
            format!("{name} started"),
            format!("Container {name} started"),
        ),
        "stop" => (
            format!("{name} stopped"),
            format!("Container {name} was stopped"),
        ),
        "die" => (
            format!("{name} exited ({code})"),
            format!("Container {name} exited with code {code}"),
        ),
        "crash" => (
            format!("{name} crashed (exit {code})"),
            format!("Container {name} exited with non-zero code {code}"),
        ),
        "oom" => (
            format!("{name} out of memory"),
            format!("Container {name} was killed by the out-of-memory killer"),
        ),
        "kill" => (
            format!("{name} killed"),
            format!("Container {name} received a kill signal"),
        ),
        "restart" => (
            format!("{name} restarted"),
            format!("Container {name} restarted"),
        ),
        "health_unhealthy" => (
            format!("{name} is unhealthy"),
            format!("Health check of {name} is failing"),
        ),
        "health_healthy" => (
            format!("{name} is healthy"),
            format!("Health check of {name} passes again"),
        ),
        "destroy" => (
            format!("{name} removed"),
            format!("Container {name} was removed"),
        ),
        "create" => (
            format!("{name} created"),
            format!("Container {name} was created"),
        ),
        other => (
            format!("{name}: {other}"),
            format!("Container {name}: {other}"),
        ),
    };
    let body = if image.is_empty() {
        body
    } else {
        format!("{body} (image {image})")
    };
    (headline, body)
}

/// Handles one Docker container event: records it and fires matching event rules.
pub fn on_event(state: &Arc<AppState>, ev: DockerEvent) {
    let cfg = state.cfg();
    let inst = cfg.instance_name();
    let rules = cfg.effective_rules();
    let (headline, body) = event_message(&ev);
    let now = Instant::now();
    let mut out = vec![];
    let mut fired = vec![];
    {
        let mut e = state.engine.lock().unwrap();
        if ev.action == "die" {
            let q = e.die_times.entry(ev.name.clone()).or_default();
            q.push_back(now);
            while q.len() > 100 {
                q.pop_front();
            }
        }
        for rule in rules.iter().filter(|r| r.enabled) {
            let RuleKind::Event { events } = &rule.kind else {
                continue;
            };
            if !events.iter().any(|x| x == &ev.action) || !rule.matches_subject(&ev.name) {
                continue;
            }
            let key = (rule.name.clone(), ev.name.clone());
            if e.silenced(&rule.name, &ev.name) || !e.cooled_down(&key, rule.cooldown(), now) {
                continue;
            }
            e.last_sent.insert(key, now);
            fired.push(rule.name.clone());
            out.push(Outgoing {
                n: Notification {
                    title: format!("[{inst}] {headline}"),
                    body: body.clone(),
                    severity: rule.severity,
                    kind: "event".into(),
                    rule: rule.name.clone(),
                    subject: ev.name.clone(),
                    value: None,
                    ts: now_ms(),
                    force: false,
                },
                channels: rule.channels.clone(),
            });
        }
        let rec = EventRecord {
            ts: now_ms(),
            action: ev.action.clone(),
            subject: ev.name.clone(),
            message: body,
            notified_rules: fired,
        };
        if e.events.len() >= 500 {
            e.events.pop_front();
        }
        e.events.push_back(rec.clone());
        let _ = state.bus.send(BusMsg::Event(rec));
    }
    for o in out {
        notify::dispatch(state, o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Op, RuleCfg, RuleKind};

    fn state_with(rule: RuleCfg) -> Arc<AppState> {
        let cfg = Config {
            rules: vec![rule],
            alerts: crate::config::AlertsCfg {
                use_default_rules: false,
                history_size: 50,
            },
            ..Config::default()
        };
        AppState::new(cfg, None, None)
    }

    fn container(name: &str, cpu: f64) -> ContainerView {
        ContainerView {
            name: name.into(),
            id: name.into(),
            state: "running".into(),
            cpu_percent: cpu,
            ..Default::default()
        }
    }

    fn cpu_rule(for_secs: u64) -> RuleCfg {
        RuleCfg {
            name: "cpu".into(),
            enabled: true,
            kind: RuleKind::Metric {
                metric: MetricKind::CpuPercent,
                op: Op::Gt,
                threshold: 80.0,
                for_secs,
            },
            severity: Severity::Warning,
            scope: vec!["web*".into()],
            exclude: vec![],
            channels: vec![],
            cooldown_secs: Some(0),
            repeat_secs: None,
            notify_resolved: true,
        }
    }

    #[tokio::test]
    async fn fires_and_resolves() {
        let st = state_with(cpu_rule(0));
        st.containers
            .write()
            .unwrap()
            .insert("web-1".into(), container("web-1", 95.0));
        st.containers
            .write()
            .unwrap()
            .insert("db-1".into(), container("db-1", 99.0));
        evaluate(&st);
        let active = st.engine.lock().unwrap().active_alerts();
        assert_eq!(active.len(), 1, "only the web-* container is in scope");
        assert_eq!(active[0].subject, "web-1");
        st.containers
            .write()
            .unwrap()
            .get_mut("web-1")
            .unwrap()
            .cpu_percent = 10.0;
        evaluate(&st);
        let e = st.engine.lock().unwrap();
        assert!(e.active_alerts().is_empty());
        assert_eq!(e.history.len(), 1);
    }

    #[tokio::test]
    async fn for_secs_delays_firing() {
        let st = state_with(cpu_rule(3600));
        st.containers
            .write()
            .unwrap()
            .insert("web-1".into(), container("web-1", 95.0));
        evaluate(&st);
        assert!(st.engine.lock().unwrap().active_alerts().is_empty());
    }

    #[tokio::test]
    async fn stopped_container_closes_alert_quietly() {
        let st = state_with(cpu_rule(0));
        st.containers
            .write()
            .unwrap()
            .insert("web-1".into(), container("web-1", 95.0));
        evaluate(&st);
        st.containers
            .write()
            .unwrap()
            .get_mut("web-1")
            .unwrap()
            .state = "exited".into();
        evaluate(&st);
        assert!(st.engine.lock().unwrap().active_alerts().is_empty());
    }

    #[tokio::test]
    async fn silence_suppresses_notification_but_keeps_alert() {
        let st = state_with(cpu_rule(0));
        st.engine
            .lock()
            .unwrap()
            .add_silence("web-*".into(), None, 60, "test".into());
        st.containers
            .write()
            .unwrap()
            .insert("web-1".into(), container("web-1", 95.0));
        evaluate(&st);
        let a = st.engine.lock().unwrap().active_alerts();
        assert_eq!(a.len(), 1);
        assert!(a[0].silenced && !a[0].notified);
    }

    #[tokio::test]
    async fn restart_loop_counts_deaths() {
        let rule = RuleCfg {
            name: "loop".into(),
            kind: RuleKind::RestartLoop {
                restarts: 3,
                window_secs: 300,
            },
            ..cpu_rule(0)
        };
        let st = state_with(rule);
        st.containers
            .write()
            .unwrap()
            .insert("web-1".into(), container("web-1", 1.0));
        let ev = || DockerEvent {
            id: "x".into(),
            name: "web-1".into(),
            action: "die".into(),
            attrs: Default::default(),
        };
        on_event(&st, ev());
        on_event(&st, ev());
        evaluate(&st);
        assert!(st.engine.lock().unwrap().active_alerts().is_empty());
        on_event(&st, ev());
        evaluate(&st);
        assert_eq!(st.engine.lock().unwrap().active_alerts().len(), 1);
    }

    #[tokio::test]
    async fn event_rule_respects_cooldown() {
        let rule = RuleCfg {
            name: "ev".into(),
            kind: RuleKind::Event {
                events: vec!["start".into()],
            },
            cooldown_secs: Some(60),
            ..cpu_rule(0)
        };
        let st = state_with(rule);
        let ev = || DockerEvent {
            id: "x".into(),
            name: "web-1".into(),
            action: "start".into(),
            attrs: Default::default(),
        };
        on_event(&st, ev());
        on_event(&st, ev());
        let e = st.engine.lock().unwrap();
        assert_eq!(e.events.len(), 2);
        assert_eq!(
            e.events
                .iter()
                .filter(|r| !r.notified_rules.is_empty())
                .count(),
            1
        );
    }
}
