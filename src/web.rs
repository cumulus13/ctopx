//! HTTP API, SSE stream, Prometheus metrics and the embedded web UI.

use crate::docker::{Action, Source, container_action};
use crate::model::*;
use crate::notify;
use crate::state::AppState;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::middleware::{Next, from_fn};
use actix_web::web::{self, Bytes};
use actix_web::{App, Error, HttpRequest, HttpResponse, HttpServer};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

type St = web::Data<Arc<AppState>>;

fn fail(code: StatusCode, msg: impl Into<String>) -> HttpResponse {
    HttpResponse::build(code).json(json!({ "error": msg.into() }))
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn auth(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let token = req
        .app_data::<St>()
        .and_then(|s| s.cfg().server.token.clone())
        .filter(|t| !t.is_empty());
    if let Some(expected) = token {
        let h = req.headers();
        let given = h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| {
                h.get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
            .or_else(|| {
                req.query_string()
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("token="))
                    .map(percent_decode)
            });
        if !given.is_some_and(|g| ct_eq(&g, &expected)) {
            return Err(actix_web::error::ErrorUnauthorized(
                "{\"error\":\"missing or invalid token\"}",
            ));
        }
    }
    next.call(req).await
}

fn snapshot(state: &Arc<AppState>) -> Value {
    let cfg = state.cfg();
    let containers: Vec<ContainerView> =
        state.containers.read().unwrap().values().cloned().collect();
    let host = state.host.read().unwrap().clone();
    let (up, error) = {
        let s = state.docker_status.read().unwrap();
        (s.up, s.error.clone())
    };
    let alerts = state.engine.lock().unwrap().active_alerts();
    json!({
        "ts": now_ms(),
        "instance": cfg.instance_name(),
        "version": env!("CARGO_PKG_VERSION"),
        "allow_actions": state.remote_actions_allowed(),
        "poll": state.poll.read().unwrap().clone(),
        "docker": { "up": up, "error": error },
        "host": host,
        "containers": containers,
        "alerts": alerts,
    })
}

async fn health(state: St) -> HttpResponse {
    HttpResponse::Ok().json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION"), "docker_up": state.docker_up(), "uptime_secs": state.started.elapsed().as_secs() }))
}

async fn get_state(state: St) -> HttpResponse {
    HttpResponse::Ok().json(snapshot(&state))
}

async fn list_containers(state: St) -> HttpResponse {
    let v: Vec<ContainerView> = state.containers.read().unwrap().values().cloned().collect();
    HttpResponse::Ok().json(v)
}

async fn get_container(state: St, name: web::Path<String>) -> HttpResponse {
    match state.containers.read().unwrap().get(name.as_str()) {
        Some(c) => HttpResponse::Ok().json(json!({ "container": c, "history": c.history })),
        None => fail(StatusCode::NOT_FOUND, "unknown container"),
    }
}

async fn history(state: St) -> HttpResponse {
    let mut m = serde_json::Map::new();
    for (k, c) in state.containers.read().unwrap().iter() {
        m.insert(
            k.clone(),
            serde_json::to_value(&c.history).unwrap_or_default(),
        );
    }
    let host = state.host.read().unwrap().clone();
    HttpResponse::Ok().json(json!({ "containers": m, "host": host.map(|h| json!({"cpu": h.cpu_hist, "mem": h.mem_hist})) }))
}

async fn do_action(state: St, path: web::Path<(String, String)>) -> HttpResponse {
    let (name, action) = path.into_inner();
    let action: Action = match action.parse() {
        Ok(a) => a,
        Err(e) => return fail(StatusCode::BAD_REQUEST, e.to_string()),
    };
    match container_action(&state, &name, action, Source::Remote).await {
        Ok(()) => HttpResponse::Ok().json(json!({ "ok": true })),
        Err(e) if e.to_string().contains("disabled") => fail(StatusCode::FORBIDDEN, e.to_string()),
        Err(e) => fail(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn alerts(state: St) -> HttpResponse {
    HttpResponse::Ok().json(state.engine.lock().unwrap().active_alerts())
}

async fn alert_history(state: St) -> HttpResponse {
    let e = state.engine.lock().unwrap();
    HttpResponse::Ok().json(e.history.iter().rev().cloned().collect::<Vec<_>>())
}

async fn events(state: St) -> HttpResponse {
    let e = state.engine.lock().unwrap();
    HttpResponse::Ok().json(e.events.iter().rev().cloned().collect::<Vec<_>>())
}

async fn deliveries(state: St) -> HttpResponse {
    let e = state.engine.lock().unwrap();
    HttpResponse::Ok().json(e.deliveries.iter().rev().cloned().collect::<Vec<_>>())
}

async fn rules(state: St) -> HttpResponse {
    let v: Vec<Value> = state
        .cfg()
        .effective_rules()
        .into_iter()
        .map(|r| {
            let mut j = serde_json::to_value(&r).unwrap_or_default();
            j["description"] = json!(r.describe());
            j["cooldown_secs"] = json!(r.cooldown());
            j
        })
        .collect();
    HttpResponse::Ok().json(v)
}

async fn channels(state: St) -> HttpResponse {
    let v: Vec<Value> = state
        .cfg()
        .channels
        .iter()
        .map(|c| json!({ "name": c.name, "type": c.kind.type_name(), "target": c.kind.target(), "enabled": c.enabled, "min_severity": c.min_severity }))
        .collect();
    HttpResponse::Ok().json(v)
}

async fn test_one(state: St, name: web::Path<String>) -> HttpResponse {
    match notify::test_channel(&state, &name).await {
        Ok(()) => HttpResponse::Ok().json(json!({ "ok": true })),
        Err(e) => fail(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn test_all(state: St) -> HttpResponse {
    notify::dispatch(
        &state,
        Outgoing {
            n: notify::test_notification(),
            channels: vec![],
        },
    );
    HttpResponse::Accepted().json(json!({ "ok": true }))
}

async fn list_silences(state: St) -> HttpResponse {
    let mut e = state.engine.lock().unwrap();
    let now = now_ms();
    e.silences.retain(|s| s.until > now);
    HttpResponse::Ok().json(&e.silences)
}

#[derive(Deserialize)]
struct NewSilence {
    pattern: String,
    rule: Option<String>,
    duration_secs: u64,
    reason: Option<String>,
}

async fn add_silence(state: St, body: web::Json<NewSilence>) -> HttpResponse {
    if body.pattern.trim().is_empty() || body.duration_secs == 0 {
        return fail(
            StatusCode::BAD_REQUEST,
            "pattern and a positive duration_secs are required",
        );
    }
    let b = body.into_inner();
    let s = state.engine.lock().unwrap().add_silence(
        b.pattern,
        b.rule,
        b.duration_secs,
        b.reason.unwrap_or_default(),
    );
    HttpResponse::Created().json(s)
}

async fn del_silence(state: St, id: web::Path<u64>) -> HttpResponse {
    if state.engine.lock().unwrap().remove_silence(*id) {
        HttpResponse::Ok().json(json!({ "ok": true }))
    } else {
        fail(StatusCode::NOT_FOUND, "no such silence")
    }
}

async fn reload(state: St) -> HttpResponse {
    match state.reload() {
        Ok(w) => HttpResponse::Ok().json(json!({ "ok": true, "notes": w })),
        Err(e) => fail(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

fn sse(name: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

async fn stream(state: St) -> HttpResponse {
    let rx = state.bus.subscribe();
    let st = state.get_ref().clone();
    // Counts this client as a viewer so the collector polls at the fast pace while it is connected.
    let guard = st.add_viewer();
    let first = sse("snapshot", &snapshot(&st));
    let s = futures_util::stream::unfold(
        (rx, st, Some(first), guard),
        |(mut rx, st, pending, guard)| async move {
            if let Some(b) = pending {
                return Some((Ok::<Bytes, Error>(b), (rx, st, None, guard)));
            }
            loop {
                match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
                    Err(_) => {
                        return Some((
                            Ok(Bytes::from_static(b": ping\n\n")),
                            (rx, st, None, guard),
                        ));
                    }
                    Ok(Ok(BusMsg::Tick)) => {
                        let b = sse("snapshot", &snapshot(&st));
                        return Some((Ok(b), (rx, st, None, guard)));
                    }
                    Ok(Ok(BusMsg::Alert(a))) => {
                        return Some((Ok(sse("alert", &json!(a))), (rx, st, None, guard)));
                    }
                    Ok(Ok(BusMsg::Event(e))) => {
                        return Some((Ok(sse("docker_event", &json!(e))), (rx, st, None, guard)));
                    }
                    Ok(Ok(BusMsg::Delivery(d))) => {
                        return Some((Ok(sse("delivery", &json!(d))), (rx, st, None, guard)));
                    }
                    Ok(Err(RecvError::Lagged(_))) => continue,
                    Ok(Err(RecvError::Closed)) => return None,
                }
            }
        },
    );
    HttpResponse::Ok()
        .insert_header(("content-type", "text/event-stream"))
        .insert_header(("cache-control", "no-cache"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(s)
}

fn prom_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

async fn metrics(state: St) -> HttpResponse {
    let mut o = String::new();
    let up = state.docker_up() as u8;
    o.push_str(&format!(
        "# TYPE ctopx_docker_up gauge\nctopx_docker_up {up}\n"
    ));
    let poll = state.poll.read().unwrap().clone();
    o.push_str(&format!(
        "# TYPE ctopx_docker_requests_total counter\nctopx_docker_requests_total {}\n\
         # TYPE ctopx_poll_interval_seconds gauge\nctopx_poll_interval_seconds {}\n\
         # TYPE ctopx_poll_cycle_seconds gauge\nctopx_poll_cycle_seconds {}\n\
         # TYPE ctopx_poll_throttle gauge\nctopx_poll_throttle {}\n",
        poll.requests_total,
        poll.interval_ms as f64 / 1000.0,
        poll.cycle_ms as f64 / 1000.0,
        poll.throttle
    ));
    let cs: Vec<ContainerView> = state.containers.read().unwrap().values().cloned().collect();
    for (metric, f) in [
        (
            "ctopx_container_running",
            (|c: &ContainerView| c.is_running() as u8 as f64) as fn(&ContainerView) -> f64,
        ),
        ("ctopx_container_cpu_percent", |c| c.cpu_percent),
        ("ctopx_container_memory_bytes", |c| c.mem_bytes as f64),
        ("ctopx_container_memory_percent", |c| c.mem_percent),
        ("ctopx_container_network_receive_bytes_per_second", |c| {
            c.net_rx_bps
        }),
        ("ctopx_container_network_transmit_bytes_per_second", |c| {
            c.net_tx_bps
        }),
        ("ctopx_container_pids", |c| c.pids as f64),
    ] {
        o.push_str(&format!("# TYPE {metric} gauge\n"));
        for c in &cs {
            o.push_str(&format!(
                "{metric}{{name=\"{}\",image=\"{}\"}} {}\n",
                prom_label(&c.name),
                prom_label(&c.image),
                f(c)
            ));
        }
    }
    if let Some(h) = state.host.read().unwrap().as_ref() {
        for (m, v) in [
            ("ctopx_host_cpu_percent", h.cpu_percent),
            ("ctopx_host_memory_percent", h.mem_percent),
            ("ctopx_host_swap_percent", h.swap_percent),
            ("ctopx_host_disk_percent_max", h.disk_percent_max),
            ("ctopx_host_load1", h.load1),
        ] {
            o.push_str(&format!("# TYPE {m} gauge\n{m} {v}\n"));
        }
    }
    let active = state.engine.lock().unwrap().active_alerts();
    o.push_str("# TYPE ctopx_alerts_active gauge\n");
    for sev in [Severity::Info, Severity::Warning, Severity::Critical] {
        o.push_str(&format!(
            "ctopx_alerts_active{{severity=\"{}\"}} {}\n",
            sev.as_str(),
            active.iter().filter(|a| a.severity == sev).count()
        ));
    }
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(o)
}

async fn static_files(req: HttpRequest) -> HttpResponse {
    let path = req.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(f) => HttpResponse::Ok()
            .content_type(f.metadata.mimetype().to_string())
            .insert_header(("cache-control", "no-cache"))
            .body(f.data.into_owned()),
        None => fail(StatusCode::NOT_FOUND, "not found"),
    }
}

fn is_loopback(bind: &str) -> bool {
    let host = bind
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(bind)
        .trim_matches(['[', ']']);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

pub async fn serve(state: Arc<AppState>, bind_override: Option<String>) -> anyhow::Result<()> {
    let cfg = state.cfg();
    let bind = bind_override.unwrap_or_else(|| cfg.server.bind.clone());
    let has_token = cfg.server.token.as_deref().is_some_and(|t| !t.is_empty());
    if !is_loopback(&bind) && !has_token {
        if state.remote_actions_allowed() {
            anyhow::bail!(
                "refusing to start: {bind} is not a loopback address, container actions are enabled and server.token is empty. Set a token or bind to 127.0.0.1."
            );
        }
        tracing::warn!(
            "{bind} is reachable from the network and server.token is empty: anyone who can connect can read container data"
        );
    }
    let data: St = web::Data::new(state.clone());
    let server = HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            .route("/api/health", web::get().to(health))
            .service(
                web::scope("/api")
                    .wrap(from_fn(auth))
                    .route("/state", web::get().to(get_state))
                    .route("/stream", web::get().to(stream))
                    .route("/containers", web::get().to(list_containers))
                    .route("/containers/{name}", web::get().to(get_container))
                    .route("/containers/{name}/{action}", web::post().to(do_action))
                    .route("/history", web::get().to(history))
                    .route("/alerts", web::get().to(alerts))
                    .route("/alerts/history", web::get().to(alert_history))
                    .route("/events", web::get().to(events))
                    .route("/deliveries", web::get().to(deliveries))
                    .route("/rules", web::get().to(rules))
                    .route("/channels", web::get().to(channels))
                    .route("/channels/{name}/test", web::post().to(test_one))
                    .route("/notify/test", web::post().to(test_all))
                    .route("/silences", web::get().to(list_silences))
                    .route("/silences", web::post().to(add_silence))
                    .route("/silences/{id}", web::delete().to(del_silence))
                    .route("/reload", web::post().to(reload)),
            )
            .service(
                web::scope("/metrics")
                    .wrap(from_fn(auth))
                    .route("", web::get().to(metrics)),
            )
            .default_service(web::get().to(static_files))
    })
    .bind(&bind)
    .map_err(|e| anyhow::anyhow!("cannot bind {bind}: {e}"))?;
    tracing::info!("web UI and API on http://{bind}");
    server.run().await?;
    Ok(())
}
