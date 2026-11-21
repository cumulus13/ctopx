//! Notification channels: ntfy, Telegram, webhook (generic/Discord/Slack) and GNTP.

use crate::config::{ChannelCfg, ChannelKind, Config, WebhookFormat};
use crate::model::*;
use crate::state::AppState;
use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Notifier {
    pub channels: Vec<ChannelCfg>,
    client: reqwest::Client,
    instance: String,
    public_url: Option<String>,
}

impl Notifier {
    pub fn new(cfg: &Config) -> Self {
        // reqwest is built without a bundled TLS provider; install ring once (a second call is a no-op).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("ctopx/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        Self {
            channels: cfg.channels.clone(),
            client,
            instance: cfg.instance_name(),
            public_url: cfg.server.public_url.clone(),
        }
    }

    fn wants(&self, ch: &ChannelCfg, out: &Outgoing) -> bool {
        ch.enabled
            && (out.n.force || out.n.severity >= ch.min_severity)
            && (out.channels.is_empty() || out.channels.contains(&ch.name))
    }

    async fn send_once(&self, ch: &ChannelCfg, n: &Notification) -> Result<()> {
        match &ch.kind {
            ChannelKind::Ntfy {
                url,
                topic,
                token,
                username,
                password,
            } => {
                self.ntfy(
                    url.as_deref(),
                    topic,
                    token.as_deref(),
                    username.as_deref(),
                    password.as_deref(),
                    n,
                )
                .await
            }
            ChannelKind::Telegram {
                bot_token,
                chat_id,
                api_base,
                thread_id,
            } => {
                self.telegram(bot_token, chat_id, api_base.as_deref(), *thread_id, n)
                    .await
            }
            ChannelKind::Webhook {
                url,
                format,
                secret,
                headers,
            } => {
                self.webhook(url, *format, secret.as_deref(), headers, n)
                    .await
            }
            ChannelKind::Gntp {
                host,
                port,
                password,
            } => gntp(host, *port, password.as_deref(), n).await,
        }
    }

    async fn ntfy(
        &self,
        url: Option<&str>,
        topic: &str,
        token: Option<&str>,
        user: Option<&str>,
        pass: Option<&str>,
        n: &Notification,
    ) -> Result<()> {
        let base = url.unwrap_or("https://ntfy.sh").trim_end_matches('/');
        let (priority, tags): (u8, Vec<&str>) = match (n.kind.as_str(), n.severity) {
            ("resolved", _) => (3, vec!["white_check_mark"]),
            (_, Severity::Critical) => (5, vec!["rotating_light"]),
            (_, Severity::Warning) => (4, vec!["warning"]),
            _ => (3, vec!["information_source"]),
        };
        let mut body = json!({ "topic": topic, "title": n.title, "message": n.body, "priority": priority, "tags": tags });
        if let Some(u) = &self.public_url {
            body["click"] = json!(u);
        }
        let mut req = self.client.post(base).json(&body);
        if let Some(t) = token.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(t);
        } else if let Some(u) = user.filter(|u| !u.is_empty()) {
            req = req.basic_auth(u, pass);
        }
        let resp = req.send().await.context("ntfy request failed")?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "ntfy returned {status}: {}",
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }

    async fn telegram(
        &self,
        token: &str,
        chat: &str,
        api_base: Option<&str>,
        thread: Option<i64>,
        n: &Notification,
    ) -> Result<()> {
        let base = api_base
            .unwrap_or("https://api.telegram.org")
            .trim_end_matches('/');
        let icon = match (n.kind.as_str(), n.severity) {
            ("resolved", _) => "✅",
            (_, Severity::Critical) => "🚨",
            (_, Severity::Warning) => "⚠️",
            _ => "ℹ️",
        };
        let mut text = format!(
            "{icon} <b>{}</b>\n{}",
            html_escape(&n.title),
            html_escape(&n.body)
        );
        if let Some(u) = &self.public_url {
            text.push_str(&format!(
                "\n<a href=\"{}\">Open dashboard</a>",
                html_escape(u)
            ));
        }
        let mut body = json!({ "chat_id": chat, "text": text, "parse_mode": "HTML", "disable_web_page_preview": true, "disable_notification": n.severity == Severity::Info });
        if let Some(t) = thread {
            body["message_thread_id"] = json!(t);
        }
        let resp = self
            .client
            .post(format!("{base}/bot{token}/sendMessage"))
            .json(&body)
            .send()
            .await
            .context("telegram request failed")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        if !status.is_success() || v.get("ok") != Some(&json!(true)) {
            bail!(
                "telegram returned {status}: {}",
                v.get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    async fn webhook(
        &self,
        url: &str,
        format: WebhookFormat,
        secret: Option<&str>,
        headers: &std::collections::BTreeMap<String, String>,
        n: &Notification,
    ) -> Result<()> {
        let color = match (n.kind.as_str(), n.severity) {
            ("resolved", _) => 0x2ecc71,
            (_, Severity::Critical) => 0xe74c3c,
            (_, Severity::Warning) => 0xf39c12,
            _ => 0x3498db,
        };
        let ts = chrono::DateTime::from_timestamp_millis(n.ts)
            .unwrap_or_default()
            .to_rfc3339();
        let payload = match format {
            WebhookFormat::Generic => json!({
                "version": 1, "instance": self.instance, "title": n.title, "message": n.body,
                "severity": n.severity, "kind": n.kind, "rule": n.rule, "subject": n.subject,
                "value": n.value, "timestamp": ts, "url": self.public_url,
            }),
            WebhookFormat::Discord => json!({
                "username": "ctopx",
                "embeds": [{ "title": n.title, "description": n.body, "color": color, "timestamp": ts, "footer": { "text": self.instance } }]
            }),
            WebhookFormat::Slack => json!({
                "text": format!("*{}*\n{}", n.title, n.body),
                "attachments": [{ "color": format!("#{color:06x}"), "footer": self.instance, "ts": n.ts / 1000 }]
            }),
        };
        let bytes = serde_json::to_vec(&payload)?;
        let mut req = self
            .client
            .post(url)
            .header("content-type", "application/json");
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(s) = secret.filter(|s| !s.is_empty()) {
            let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(s.as_bytes())
                .map_err(|e| anyhow!("bad hmac key: {e}"))?;
            mac.update(&bytes);
            req = req.header(
                "X-Ctopx-Signature",
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            );
        }
        let resp = req
            .body(bytes)
            .send()
            .await
            .context("webhook request failed")?;
        let status = resp.status();
        if !status.is_success() {
            bail!(
                "webhook returned {status}: {}",
                resp.text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>()
            );
        }
        Ok(())
    }
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------- GNTP ----------

fn random_salt() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now_ms() as u64 ^ (i as u64) << 32);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    out
}

/// "GNTP/1.0 <TYPE> NONE[ SHA256:<keyhash>.<salt>]" with an unencrypted body.
fn gntp_header(kind: &str, password: Option<&str>) -> String {
    match password.filter(|p| !p.is_empty()) {
        None => format!("GNTP/1.0 {kind} NONE\r\n"),
        Some(pw) => {
            let salt = random_salt();
            let mut basis = pw.as_bytes().to_vec();
            basis.extend_from_slice(&salt);
            let key = Sha256::digest(&basis);
            let key_hash = Sha256::digest(key);
            format!(
                "GNTP/1.0 {kind} NONE SHA256:{}.{}\r\n",
                hex::encode_upper(key_hash),
                hex::encode_upper(salt)
            )
        }
    }
}

fn one_line(s: &str) -> String {
    s.replace("\r\n", " | ").replace(['\n', '\r'], " | ")
}

async fn gntp_exchange(addr: &str, msg: String) -> Result<()> {
    let mut sock = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .context("GNTP connect timed out")?
        .with_context(|| format!("cannot connect to GNTP server {addr}"))?;
    sock.write_all(msg.as_bytes()).await?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk))
            .await
            .context("GNTP response timed out")??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let resp = String::from_utf8_lossy(&buf).to_string();
    let first = resp.lines().next().unwrap_or("");
    if first.contains("-OK") || first.contains("-CALLBACK") {
        Ok(())
    } else {
        let desc = resp
            .lines()
            .find_map(|l| l.strip_prefix("Error-Description:"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| resp.trim().to_string());
        bail!("GNTP error: {desc}")
    }
}

async fn gntp(host: &str, port: u16, password: Option<&str>, n: &Notification) -> Result<()> {
    let addr = format!("{host}:{port}");
    let app = "ctopx";
    // Register the application and its notification types (idempotent).
    let mut reg = gntp_header("REGISTER", password);
    reg.push_str(&format!(
        "Application-Name: {app}\r\nNotifications-Count: 3\r\n\r\n"
    ));
    for (name, display) in [
        ("info", "Info"),
        ("warning", "Warning"),
        ("critical", "Critical"),
    ] {
        reg.push_str(&format!("Notification-Name: {name}\r\nNotification-Display-Name: {display}\r\nNotification-Enabled: True\r\n\r\n"));
    }
    reg.push_str("\r\n");
    gntp_exchange(&addr, reg).await.context("GNTP register")?;

    let prio = match n.severity {
        Severity::Info => 0,
        Severity::Warning => 1,
        Severity::Critical => 2,
    };
    let mut msg = gntp_header("NOTIFY", password);
    msg.push_str(&format!(
        "Application-Name: {app}\r\nNotification-Name: {}\r\nNotification-ID: {}\r\nNotification-Title: {}\r\nNotification-Text: {}\r\nNotification-Sticky: {}\r\nNotification-Priority: {prio}\r\n\r\n\r\n",
        n.severity.as_str(),
        n.ts,
        one_line(&n.title),
        one_line(&n.body),
        if n.severity == Severity::Critical { "True" } else { "False" },
    ));
    gntp_exchange(&addr, msg).await.context("GNTP notify")
}

// ---------- dispatch ----------

/// Sends to every matching channel in the background and records the outcome.
pub fn dispatch(state: &Arc<AppState>, out: Outgoing) {
    let notifier = state.notifier.read().unwrap().clone();
    for ch in notifier.channels.iter().filter(|c| notifier.wants(c, &out)) {
        let (state, notifier, ch, n) = (state.clone(), notifier.clone(), ch.clone(), out.n.clone());
        tokio::spawn(async move {
            let res = send_with_retry(&notifier, &ch, &n).await;
            let d = Delivery {
                ts: now_ms(),
                channel: ch.name.clone(),
                kind: ch.kind.type_name().into(),
                title: n.title.clone(),
                ok: res.is_ok(),
                attempts: res.as_ref().map(|a| *a).unwrap_or_else(|(a, _)| *a),
                error: res.as_ref().err().map(|(_, e)| e.clone()),
            };
            if let Some(e) = &d.error {
                tracing::warn!(channel = %d.channel, "notification failed: {e}");
            }
            state.engine.lock().unwrap().record_delivery(d.clone());
            let _ = state.bus.send(BusMsg::Delivery(d));
        });
    }
}

async fn send_with_retry(
    nf: &Notifier,
    ch: &ChannelCfg,
    n: &Notification,
) -> std::result::Result<u32, (u32, String)> {
    let mut last = String::new();
    for attempt in 1..=3u32 {
        match nf.send_once(ch, n).await {
            Ok(()) => return Ok(attempt),
            Err(e) => {
                last = format!("{e:#}");
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(if attempt == 1 { 1 } else { 4 })).await;
                }
            }
        }
    }
    Err((3, last))
}

/// Sends once without retries and returns the error text, for `test-notify` and the API.
pub async fn test_channel(state: &Arc<AppState>, name: &str) -> Result<()> {
    let nf = state.notifier.read().unwrap().clone();
    let ch = nf
        .channels
        .iter()
        .find(|c| c.name == name)
        .cloned()
        .ok_or_else(|| anyhow!("unknown channel '{name}'"))?;
    nf.send_once(&ch, &test_notification()).await
}

pub fn test_notification() -> Notification {
    Notification {
        title: "ctopx test notification".into(),
        body: "If you can read this, the channel works.".into(),
        severity: Severity::Warning,
        kind: "test".into(),
        rule: "test".into(),
        subject: "ctopx".into(),
        value: None,
        ts: now_ms(),
        force: true,
    }
}
