//! Shared data types: metric views, alerts, events and notifications.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

pub const HISTORY_LEN: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct History {
    pub cpu: VecDeque<f32>,
    pub mem: VecDeque<f32>,
    pub rx: VecDeque<f32>,
    pub tx: VecDeque<f32>,
}

fn push_capped(q: &mut VecDeque<f32>, v: f32) {
    if q.len() >= HISTORY_LEN {
        q.pop_front();
    }
    q.push_back(v);
}

impl History {
    pub fn push(&mut self, cpu: f32, mem: f32, rx: f32, tx: f32) {
        push_capped(&mut self.cpu, cpu);
        push_capped(&mut self.mem, mem);
        push_capped(&mut self.rx, rx);
        push_capped(&mut self.tx, tx);
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ContainerView {
    pub id: String,
    pub short_id: String,
    pub name: String,
    pub image: String,
    pub project: Option<String>,
    pub state: String,
    pub status: String,
    pub health: Option<String>,
    pub created: i64,
    pub ports: Vec<String>,
    pub cpu_percent: f64,
    pub mem_bytes: u64,
    pub mem_limit: u64,
    pub mem_percent: f64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub net_rx_total: u64,
    pub net_tx_total: u64,
    pub blk_read_bps: f64,
    pub blk_write_bps: f64,
    pub pids: u64,
    pub updated_at: i64,
    #[serde(skip)]
    pub history: History,
}

impl ContainerView {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskView {
    pub mount: String,
    pub total: u64,
    pub used: u64,
    pub percent: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HostView {
    pub hostname: String,
    pub os: String,
    pub cpus: usize,
    pub uptime_secs: u64,
    pub cpu_percent: f64,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_percent: f64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub swap_percent: f64,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub disks: Vec<DiskView>,
    pub disk_percent_max: f64,
    #[serde(skip)]
    pub cpu_hist: VecDeque<f32>,
    #[serde(skip)]
    pub mem_hist: VecDeque<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Firing,
    Resolved,
}

#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub id: u64,
    pub rule: String,
    pub subject: String,
    pub severity: Severity,
    pub state: AlertState,
    pub message: String,
    pub value: Option<f64>,
    pub started_at: i64,
    pub resolved_at: Option<i64>,
    pub notified: bool,
    pub silenced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    pub ts: i64,
    pub action: String,
    pub subject: String,
    pub message: String,
    pub notified_rules: Vec<String>,
}

/// How hard ctopx is currently working the Docker API.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PollInfo {
    /// Effective time between polls, including any automatic slow-down.
    pub interval_ms: u64,
    /// How long the last poll took.
    pub cycle_ms: u64,
    /// 1 normally; doubles (up to 16) while Docker is slow to answer.
    pub throttle: u32,
    /// Docker API requests per second averaged over the last interval.
    pub requests_per_sec: f64,
    pub requests_total: u64,
    /// Containers whose stats were fetched in the last poll.
    pub stats_polled: usize,
    /// "live" while a UI is open, "idle" otherwise.
    pub mode: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub ts: i64,
    pub channel: String,
    pub kind: String,
    pub title: String,
    pub ok: bool,
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Silence {
    pub id: u64,
    pub pattern: String,
    pub rule: Option<String>,
    pub until: i64,
    pub reason: String,
}

/// What a channel actually sends.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub severity: Severity,
    /// "firing" | "resolved" | "event" | "reminder" | "test"
    pub kind: String,
    pub rule: String,
    pub subject: String,
    pub value: Option<f64>,
    pub ts: i64,
    #[serde(skip)]
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub n: Notification,
    pub channels: Vec<String>,
}

/// Messages pushed to SSE clients.
#[derive(Debug, Clone)]
pub enum BusMsg {
    Tick,
    Alert(Alert),
    Event(EventRecord),
    Delivery(Delivery),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DockerEvent {
    pub id: String,
    pub name: String,
    pub action: String,
    pub attrs: HashMap<String, String>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn human_bytes(b: f64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{:.0} {}", v, U[i])
    } else {
        format!("{:.1} {}", v, U[i])
    }
}
