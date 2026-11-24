//! Configuration (TOML) and built-in default rules.

use crate::model::{Severity, human_bytes};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn yes() -> bool {
    true
}
fn star() -> Vec<String> {
    vec!["*".into()]
}
fn warning() -> Severity {
    Severity::Warning
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub docker: DockerCfg,
    pub network: NetworkCfg,
    pub host: HostCfg,
    pub alerts: AlertsCfg,
    pub channels: Vec<ChannelCfg>,
    pub rules: Vec<RuleCfg>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub bind: String,
    pub token: Option<String>,
    pub allow_actions: bool,
    /// Shown in notification titles. Defaults to the host name.
    pub name: Option<String>,
    /// Link target put in notifications (e.g. https://ctopx.example.com).
    pub public_url: Option<String>,
}
impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8686".into(),
            token: None,
            allow_actions: false,
            name: None,
            public_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DockerCfg {
    /// Set to false to monitor only the host (CPU, memory, disk) without a Docker daemon,
    /// for example on Termux. Docker rules and the daemon-down alert are then skipped.
    pub enabled: bool,
    /// Overrides DOCKER_HOST (tcp://..., unix://..., npipe://...).
    pub host: Option<String>,
    /// Poll interval while a UI (web, TUI) is open.
    pub interval_secs: u64,
    /// Poll interval when nobody is watching. Alerts are still evaluated at this pace.
    pub idle_interval_secs: u64,
    /// Maximum simultaneous stats requests sent to Docker.
    pub max_concurrency: usize,
    /// Slow polling down automatically when Docker answers slowly.
    pub adaptive: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}
impl Default for DockerCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            host: None,
            interval_secs: 2,
            idle_interval_secs: 10,
            max_concurrency: 4,
            adaptive: true,
            include: star(),
            exclude: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct NetworkCfg {
    /// Extra trusted CA certificates (PEM) for notification channels, for example a corporate
    /// or self-hosted CA. `SSL_CERT_FILE` is honoured too when this is not set.
    pub ca_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HostCfg {
    pub enabled: bool,
}
impl Default for HostCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AlertsCfg {
    pub use_default_rules: bool,
    pub history_size: usize,
}
impl Default for AlertsCfg {
    fn default() -> Self {
        Self {
            use_default_rules: true,
            history_size: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookFormat {
    #[default]
    Generic,
    Discord,
    Slack,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelCfg {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Channels only deliver notifications at or above this severity.
    #[serde(default = "warning")]
    pub min_severity: Severity,
    #[serde(flatten)]
    pub kind: ChannelKind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ChannelKind {
    Ntfy {
        #[serde(default)]
        url: Option<String>,
        topic: String,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    Telegram {
        bot_token: String,
        chat_id: String,
        #[serde(default)]
        api_base: Option<String>,
        #[serde(default)]
        thread_id: Option<i64>,
    },
    Webhook {
        url: String,
        #[serde(default)]
        format: WebhookFormat,
        #[serde(default)]
        secret: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    Gntp {
        #[serde(default = "gntp_host")]
        host: String,
        #[serde(default = "gntp_port")]
        port: u16,
        #[serde(default)]
        password: Option<String>,
    },
}
fn gntp_host() -> String {
    "127.0.0.1".into()
}
fn gntp_port() -> u16 {
    23053
}

impl ChannelKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            ChannelKind::Ntfy { .. } => "ntfy",
            ChannelKind::Telegram { .. } => "telegram",
            ChannelKind::Webhook { .. } => "webhook",
            ChannelKind::Gntp { .. } => "gntp",
        }
    }
    /// Human-readable target with secrets removed.
    pub fn target(&self) -> String {
        match self {
            ChannelKind::Ntfy { url, topic, .. } => {
                format!(
                    "{}/{}",
                    url.as_deref()
                        .unwrap_or("https://ntfy.sh")
                        .trim_end_matches('/'),
                    topic
                )
            }
            ChannelKind::Telegram { chat_id, .. } => format!("chat {chat_id}"),
            ChannelKind::Webhook { url, format, .. } => {
                let host = url.split('/').nth(2).unwrap_or("?");
                format!("{host} ({format:?})").to_lowercase()
            }
            ChannelKind::Gntp { host, port, .. } => format!("{host}:{port}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    CpuPercent,
    MemPercent,
    MemBytes,
    NetRxBps,
    NetTxBps,
    BlkReadBps,
    BlkWriteBps,
    Pids,
    HostCpuPercent,
    HostMemPercent,
    HostSwapPercent,
    HostDiskPercent,
    HostLoad1,
}

impl MetricKind {
    pub fn name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }
    pub fn is_host(&self) -> bool {
        matches!(
            self,
            Self::HostCpuPercent
                | Self::HostMemPercent
                | Self::HostSwapPercent
                | Self::HostDiskPercent
                | Self::HostLoad1
        )
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::CpuPercent | Self::HostCpuPercent => "CPU usage",
            Self::MemPercent | Self::HostMemPercent => "memory usage",
            Self::MemBytes => "memory",
            Self::NetRxBps => "network receive rate",
            Self::NetTxBps => "network transmit rate",
            Self::BlkReadBps => "disk read rate",
            Self::BlkWriteBps => "disk write rate",
            Self::Pids => "process count",
            Self::HostSwapPercent => "swap usage",
            Self::HostDiskPercent => "disk usage",
            Self::HostLoad1 => "load average",
        }
    }
    pub fn fmt(&self, v: f64) -> String {
        match self {
            Self::CpuPercent
            | Self::MemPercent
            | Self::HostCpuPercent
            | Self::HostMemPercent
            | Self::HostSwapPercent
            | Self::HostDiskPercent => format!("{v:.1}%"),
            Self::MemBytes => human_bytes(v),
            Self::NetRxBps | Self::NetTxBps | Self::BlkReadBps | Self::BlkWriteBps => {
                format!("{}/s", human_bytes(v))
            }
            Self::Pids => format!("{v:.0}"),
            Self::HostLoad1 => format!("{v:.2}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub enum Op {
    #[default]
    #[serde(rename = ">", alias = "gt")]
    Gt,
    #[serde(rename = ">=", alias = "ge")]
    Ge,
    #[serde(rename = "<", alias = "lt")]
    Lt,
    #[serde(rename = "<=", alias = "le")]
    Le,
}
impl Op {
    pub fn test(&self, v: f64, t: f64) -> bool {
        match self {
            Op::Gt => v > t,
            Op::Ge => v >= t,
            Op::Lt => v < t,
            Op::Le => v <= t,
        }
    }
    pub fn symbol(&self) -> &'static str {
        match self {
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Lt => "<",
            Op::Le => "<=",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleKind {
    /// Fires when a metric stays past a threshold for `for_secs`.
    Metric {
        metric: MetricKind,
        #[serde(default)]
        op: Op,
        threshold: f64,
        #[serde(default)]
        for_secs: u64,
    },
    /// Fires immediately on Docker events (start, stop, die, crash, oom, restart, kill,
    /// pause, unpause, create, destroy, health_unhealthy, health_healthy).
    Event { events: Vec<String> },
    /// Fires when a container dies `restarts` times within `window_secs`.
    RestartLoop {
        #[serde(default = "three")]
        restarts: u32,
        #[serde(default = "five_min")]
        window_secs: u64,
    },
    /// Fires when the Docker daemon is unreachable for `for_secs`.
    DockerDaemon {
        #[serde(default)]
        for_secs: u64,
    },
}
fn three() -> u32 {
    3
}
fn five_min() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuleCfg {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: RuleKind,
    #[serde(default = "warning")]
    pub severity: Severity,
    /// Container name globs (`*` and `?`). Ignored for host and daemon rules.
    #[serde(default = "star")]
    pub scope: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Channel names. Empty means every channel.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Minimum seconds between notifications for the same rule and subject.
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
    /// Re-send a reminder while the alert is still firing.
    #[serde(default)]
    pub repeat_secs: Option<u64>,
    #[serde(default = "yes")]
    pub notify_resolved: bool,
}

impl RuleCfg {
    pub fn cooldown(&self) -> u64 {
        self.cooldown_secs.unwrap_or(match self.kind {
            RuleKind::Event { .. } => 10,
            _ => 300,
        })
    }
    pub fn describe(&self) -> String {
        match &self.kind {
            RuleKind::Metric {
                metric,
                op,
                threshold,
                for_secs,
            } => {
                format!(
                    "{} {} {} for {}s",
                    metric.name(),
                    op.symbol(),
                    metric.fmt(*threshold),
                    for_secs
                )
            }
            RuleKind::Event { events } => format!("events: {}", events.join(", ")),
            RuleKind::RestartLoop {
                restarts,
                window_secs,
            } => format!("{restarts} deaths within {window_secs}s"),
            RuleKind::DockerDaemon { for_secs } => format!("daemon unreachable for {for_secs}s"),
        }
    }
    pub fn matches_subject(&self, name: &str) -> bool {
        self.scope.iter().any(|p| glob(p, name)) && !self.exclude.iter().any(|p| glob(p, name))
    }
}

/// Case-insensitive glob supporting `*` and `?`.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star_pi, mut star_ti) = (0usize, 0usize, usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn rule(name: &str, kind: RuleKind, severity: Severity) -> RuleCfg {
    RuleCfg {
        name: name.into(),
        enabled: true,
        kind,
        severity,
        scope: star(),
        exclude: vec![],
        channels: vec![],
        cooldown_secs: None,
        repeat_secs: None,
        notify_resolved: true,
    }
}

pub fn builtin_rules() -> Vec<RuleCfg> {
    use MetricKind::*;
    let m = |metric, threshold, for_secs| RuleKind::Metric {
        metric,
        op: Op::Gt,
        threshold,
        for_secs,
    };
    let ev = |e: &[&str]| RuleKind::Event {
        events: e.iter().map(|s| s.to_string()).collect(),
    };
    let mut disk = rule(
        "host-disk-high",
        m(HostDiskPercent, 90.0, 0),
        Severity::Warning,
    );
    disk.cooldown_secs = Some(3600);
    disk.repeat_secs = Some(6 * 3600);
    vec![
        rule(
            "container-cpu-high",
            m(CpuPercent, 90.0, 60),
            Severity::Warning,
        ),
        rule(
            "container-memory-high",
            m(MemPercent, 90.0, 60),
            Severity::Warning,
        ),
        rule(
            "container-memory-critical",
            m(MemPercent, 97.0, 15),
            Severity::Critical,
        ),
        rule(
            "container-crashed",
            ev(&["crash", "oom"]),
            Severity::Critical,
        ),
        rule(
            "container-unhealthy",
            ev(&["health_unhealthy"]),
            Severity::Critical,
        ),
        rule(
            "container-recovered",
            ev(&["health_healthy"]),
            Severity::Info,
        ),
        rule(
            "container-lifecycle",
            ev(&["start", "stop", "restart", "destroy"]),
            Severity::Info,
        ),
        rule(
            "container-restart-loop",
            RuleKind::RestartLoop {
                restarts: 3,
                window_secs: 300,
            },
            Severity::Critical,
        ),
        rule(
            "docker-daemon-down",
            RuleKind::DockerDaemon { for_secs: 10 },
            Severity::Critical,
        ),
        rule(
            "host-cpu-high",
            m(HostCpuPercent, 90.0, 120),
            Severity::Warning,
        ),
        rule(
            "host-memory-high",
            m(HostMemPercent, 90.0, 120),
            Severity::Warning,
        ),
        disk,
    ]
}

impl Config {
    pub fn effective_rules(&self) -> Vec<RuleCfg> {
        let mut out = self.rules.clone();
        if self.alerts.use_default_rules {
            for r in builtin_rules() {
                if !out.iter().any(|o| o.name == r.name) {
                    out.push(r);
                }
            }
        }
        out
    }

    pub fn instance_name(&self) -> String {
        self.server
            .name
            .clone()
            .or_else(|| sysinfo::System::host_name().ok())
            .unwrap_or_else(|| "ctopx".into())
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errs = vec![];
        let mut seen = std::collections::HashSet::new();
        for c in &self.channels {
            if !seen.insert(c.name.clone()) {
                errs.push(format!("duplicate channel name '{}'", c.name));
            }
            match &c.kind {
                ChannelKind::Ntfy { topic, .. } if topic.trim().is_empty() => {
                    errs.push(format!("channel '{}': ntfy topic is empty", c.name))
                }
                ChannelKind::Telegram {
                    bot_token, chat_id, ..
                } if bot_token.is_empty() || chat_id.is_empty() => errs.push(format!(
                    "channel '{}': telegram needs bot_token and chat_id",
                    c.name
                )),
                ChannelKind::Webhook { url, .. } if !url.starts_with("http") => errs.push(format!(
                    "channel '{}': webhook url must start with http(s)://",
                    c.name
                )),
                _ => {}
            }
        }
        let mut rn = std::collections::HashSet::new();
        for r in &self.rules {
            if !rn.insert(r.name.clone()) {
                errs.push(format!("duplicate rule name '{}'", r.name));
            }
        }
        for r in self.effective_rules() {
            for ch in &r.channels {
                if !self.channels.iter().any(|c| &c.name == ch) {
                    errs.push(format!(
                        "rule '{}' references unknown channel '{}'",
                        r.name, ch
                    ));
                }
            }
            if let RuleKind::Event { events } = &r.kind {
                const KNOWN: &[&str] = &[
                    "start",
                    "stop",
                    "die",
                    "crash",
                    "oom",
                    "restart",
                    "kill",
                    "pause",
                    "unpause",
                    "create",
                    "destroy",
                    "health_unhealthy",
                    "health_healthy",
                    "health_starting",
                ];
                for e in events {
                    if !KNOWN.contains(&e.as_str()) {
                        errs.push(format!(
                            "rule '{}': unknown event '{}' (known: {})",
                            r.name,
                            e,
                            KNOWN.join(", ")
                        ));
                    }
                }
            }
        }
        if self.docker.interval_secs == 0 || self.docker.idle_interval_secs == 0 {
            errs.push("docker.interval_secs and docker.idle_interval_secs must be >= 1".into());
        }
        if self.docker.max_concurrency == 0 {
            errs.push("docker.max_concurrency must be >= 1".into());
        }
        errs
    }
}

/// Replaces `${NAME}` with the environment variable value.
fn expand_env(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            bail!("unterminated ${{ in config")
        };
        let name = &after[..j];
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => bail!("environment variable '{name}' referenced in config is not set"),
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn load(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    // Env vars are expanded on non-comment lines only.
    let mut cleaned = String::new();
    for l in raw.lines() {
        if l.trim_start().starts_with('#') {
            cleaned.push('\n');
        } else {
            cleaned.push_str(&expand_env(l)?);
            cleaned.push('\n');
        }
    }
    let cfg: Config =
        toml::from_str(&cleaned).with_context(|| format!("invalid config {}", path.display()))?;
    Ok(cfg)
}

pub fn find_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("CTOPX_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let local = PathBuf::from("ctopx.toml");
    if local.exists() {
        return Some(local);
    }
    let dir = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    dir.map(|d| d.join("ctopx").join("ctopx.toml"))
        .filter(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globbing() {
        assert!(glob("*", "anything"));
        assert!(glob("web-*", "Web-1"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "abbc"));
        assert!(glob("*db*", "my-db-1"));
        assert!(!glob("web-*", "api-1"));
    }
    #[test]
    fn example_config_parses_and_validates() {
        let cfg: Config =
            toml::from_str(include_str!("../ctopx.example.toml")).expect("example parses");
        assert!(cfg.validate().is_empty());
        assert!(!cfg.effective_rules().is_empty());
    }
    #[test]
    fn channel_and_rule_parsing() {
        let cfg: Config = toml::from_str(
            r#"
            [[channels]]
            name = "a"
            type = "gntp"
            port = 23053
            [[channels]]
            name = "b"
            type = "ntfy"
            topic = "t"
            min_severity = "critical"
            [[rules]]
            name = "r1"
            kind = "metric"
            metric = "host_disk_percent"
            op = ">="
            threshold = 80
            channels = ["a"]
            [[rules]]
            name = "r2"
            kind = "event"
            events = ["start", "bogus"]
        "#,
        )
        .unwrap();
        let errs = cfg.validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("bogus"));
    }
    #[test]
    fn unknown_channel_reference_is_reported() {
        let cfg: Config =
            toml::from_str("[[rules]]\nname=\"x\"\nkind=\"docker_daemon\"\nchannels=[\"missing\"]")
                .unwrap();
        assert!(cfg.validate().iter().any(|e| e.contains("unknown channel")));
    }
    #[test]
    fn env_expansion() {
        unsafe { std::env::set_var("CTOPX_TEST_VAR", "abc") };
        assert_eq!(expand_env("x=\"${CTOPX_TEST_VAR}\"").unwrap(), "x=\"abc\"");
        assert!(expand_env("${CTOPX_SURELY_UNSET_VAR}").is_err());
    }
    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_empty());
    }
}
