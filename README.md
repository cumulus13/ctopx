# ctopx

A `ctop`-style container monitor that also **tells you when something is wrong**. One static binary gives you:

- a terminal UI (like ctop) with per-container CPU, memory, network, disk I/O and live sparklines
- an alert engine with thresholds, sustained-duration checks, cooldowns, reminders, recovery notices and silences
- notifications to **ntfy**, **Telegram**, **GNTP** (Growl/Snarl), and **webhooks** (generic JSON with HMAC signature, Discord, Slack)
- a REST API, a live Server-Sent-Events stream and a Prometheus `/metrics` endpoint
- a modern web UI (dark and light theme) embedded in the binary: no Node, no CDN, works offline

It monitors Docker containers and the host machine itself (CPU, memory, swap, disk, load).

## Quick start

```sh
cargo build --release          # produces target/release/ctopx (ctopx.exe on Windows)
ctopx init                     # writes ./ctopx.toml with every option documented
ctopx check                    # validates the config and lists the rules in effect
ctopx test-notify              # sends a test message to every channel
ctopx                          # terminal UI; alerts keep working while it is open
ctopx serve                    # headless: collector + alerts + API + web UI on 127.0.0.1:8686
ctopx tui --serve              # terminal UI and web UI together
```

Without a config file ctopx runs with built-in rules and no channels: alerts show up in the UIs only.
Config lookup order: `--config`, `$CTOPX_CONFIG`, `./ctopx.toml`, then `%APPDATA%\ctopx\ctopx.toml` (Windows) or `~/.config/ctopx/ctopx.toml`.

ctopx connects to Docker the way the Docker CLI does (`DOCKER_HOST`, otherwise the local socket or named pipe), or use `docker.host` in the config.
If the daemon is not running when ctopx starts, it keeps retrying and reports it as an alert instead of exiting.

## What gets alerted by default

Run `ctopx check` for the exact list. Out of the box, with no configuration:

| Rule | Condition | Severity |
|---|---|---|
| container-cpu-high | CPU above 90% for 60 s | warning |
| container-memory-high / -critical | memory above 90% for 60 s / 97% for 15 s | warning / critical |
| container-crashed | die with non-zero exit code, or OOM kill | critical |
| container-unhealthy / -recovered | health check turns unhealthy / healthy | critical / info |
| container-restart-loop | 3 deaths within 5 minutes | critical |
| container-lifecycle | start, stop, restart, destroy | info |
| docker-daemon-down | Docker unreachable for 10 s | critical |
| host-cpu-high, host-memory-high | above 90% for 2 minutes | warning |
| host-disk-high | any disk above 90%; reminder every 6 h | warning |

Channels only deliver notifications at or above their `min_severity` (default `warning`), so lifecycle info events appear in the UI and TUI but do not buzz your phone unless you lower a channel to `info`. Define a rule with the same name as a built-in one to replace it, or set `alerts.use_default_rules = false`.

## Rules

```toml
[[rules]]
name = "db-cpu"
kind = "metric"              # metric | event | restart_loop | docker_daemon
metric = "cpu_percent"
op = ">"                     # > >= < <=
threshold = 80
for_secs = 120               # must stay past the threshold this long
severity = "critical"        # info | warning | critical
scope = ["postgres*"]        # container name globs; exclude = [...] also available
channels = ["phone"]         # empty = every channel
cooldown_secs = 600          # minimum gap between notifications for the same rule and subject
repeat_secs = 1800           # remind while still firing
notify_resolved = true
```

Metrics: `cpu_percent mem_percent mem_bytes net_rx_bps net_tx_bps blk_read_bps blk_write_bps pids` and for the host `host_cpu_percent host_mem_percent host_swap_percent host_disk_percent host_load1`.
Events: `start stop die crash oom restart kill pause unpause create destroy health_unhealthy health_healthy` (`crash` is a `die` with a non-zero exit code).

How alerts behave:

- A metric alert opens only after the condition holds for `for_secs`, and closes when it clears. A recovery notice is sent only if the opening notification was sent.
- If a container stops or disappears, its metric alerts close quietly.
- Cooldowns apply per rule and subject, so a flapping container does not flood you.
- Silences (UI or `POST /api/silences`) mute notifications by name glob and optionally by rule. The alert stays visible.
- Failed deliveries are retried twice (after 1 s and 4 s) and logged under Channels > Recent deliveries. One broken channel never blocks the others.

## Channels

```toml
[[channels]]
name = "phone"
type = "ntfy"                # JSON publish API, so any ntfy server works
url = "https://ntfy.sh"
topic = "my-ctopx-alerts"
token = "${NTFY_TOKEN}"      # or username / password

[[channels]]
name = "telegram"
type = "telegram"
bot_token = "${TELEGRAM_BOT_TOKEN}"
chat_id = "123456789"

[[channels]]
name = "desktop"
type = "gntp"                # Growl for Windows, Snarl, any GNTP listener
host = "127.0.0.1"
port = 23053
password = ""                # optional

[[channels]]
name = "ops"
type = "webhook"
url = "https://example.com/hook"
format = "generic"           # generic | discord | slack
secret = "${WEBHOOK_SECRET}" # adds X-Ctopx-Signature: sha256=<hex hmac of the raw body>
```

`${NAME}` in the config is replaced by the environment variable (startup fails if it is unset), so secrets stay out of the file.

Generic webhook payload:

```json
{"version":1,"instance":"host","title":"...","message":"...","severity":"critical","kind":"firing",
 "rule":"container-cpu-high","subject":"web-1","value":96.0,"timestamp":"2026-11-21T04:00:00+00:00","url":null}
```

`kind` is one of `firing`, `resolved`, `event`, `reminder`, `test`.

## Terminal UI

`↑↓`/`j k` select, `Enter` details with sparklines, `/` filter, `s` cycle sort, `a` show or hide stopped, `Tab` or `1 2 3` switch between Containers, Alerts and Events, `S` start/stop and `R` restart (only when `server.allow_actions = true`), `q` quit.
In TUI mode logs go to `ctopx.log` in the system temp directory.

## HTTP API

Everything under `/api` and `/metrics` requires `Authorization: Bearer <token>` (or `X-API-Key`, or `?token=` for the SSE stream) when `server.token` is set. `/api/health` is always open.

| Method and path | Purpose |
|---|---|
| `GET /api/state` | full snapshot: host, containers, docker status, active alerts |
| `GET /api/stream` | SSE: `snapshot` every tick, plus `alert`, `docker_event`, `delivery` |
| `GET /api/containers`, `/api/containers/{name}` | metrics (detail includes history) |
| `POST /api/containers/{name}/{start\|stop\|restart}` | needs `server.allow_actions = true` |
| `GET /api/history` | recent samples for sparklines |
| `GET /api/alerts`, `/api/alerts/history` | active and resolved alerts |
| `GET /api/events`, `/api/deliveries` | Docker events and notification delivery log |
| `GET /api/rules`, `/api/channels` | effective rules; channels with secrets removed |
| `POST /api/channels/{name}/test`, `POST /api/notify/test` | send a test notification |
| `GET/POST /api/silences`, `DELETE /api/silences/{id}` | manage silences |
| `POST /api/reload` | re-read the config (rules and channels; bind, token and docker settings need a restart) |
| `GET /metrics` | Prometheus text format |

## Security

- The default bind is `127.0.0.1:8686`. Binding elsewhere without a token logs a warning.
- ctopx refuses to start if it is reachable from the network, `allow_actions` is on and there is no token.
- Container actions are off by default. Access to the Docker socket is effectively root on the host, so treat the token accordingly and put TLS in front (reverse proxy) if you expose the port.

## Design notes

- **actix-web, not loco**: loco brings an ORM, migrations and a project skeleton that this tool does not need. actix-web serves the API, SSE and embedded assets from one small binary.
- **Rust, single binary**: the web UI (plain HTML, CSS and JS, no build step) is embedded with `rust-embed`. TLS uses rustls with the `ring` backend, so there is no OpenSSL dependency.
- **CPU percentage** is computed from successive samples (container CPU delta over system CPU delta, as `docker stats` does) rather than Docker's `precpu` fields, which are empty in one-shot mode.
- **Memory** is usage minus page cache, matching `docker stats`.

## Limitations

- Alert history, events and silences live in memory and are lost on restart. Rules and channels live in the config file and cannot be edited from the UI.
- GNTP is sent unencrypted (`NONE`) with optional SHA256 password authentication; multi-line text is flattened to one line.
- Developed and tested on Linux. The Windows code paths (Docker named pipe, Windows containers' CPU accounting) use the same libraries but have not been run on Windows yet.
- Only Docker is supported (not Podman-specific, Kubernetes or Swarm views).
- A container's notification for an OOM kill and for the crash that follows share one cooldown, so you get one message, not two.

## Development

```sh
cargo test      # unit tests: rules engine, config parsing, globbing
cargo clippy --all-targets
```

## 👤 Author
        
[Hadi Cahyadi](mailto:cumulus13@gmail.com)
    

[![Buy Me a Coffee](https://www.buymeacoffee.com/assets/img/custom_images/orange_img.png)](https://www.buymeacoffee.com/cumulus13)

[![Donate via Ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/cumulus13)
 
[Support me on Patreon](https://www.patreon.com/cumulus13)