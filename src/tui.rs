//! Terminal UI (ratatui). Alerts and notifications keep running while it is open.

use crate::docker::{Action, Source, container_action};
use crate::model::*;
use crate::state::AppState;
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Sparkline, Table, TableState, Tabs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Containers,
    Alerts,
    Events,
}

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    State,
    Cpu,
    Mem,
    Net,
    Pids,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Name => Sort::State,
            Sort::State => Sort::Cpu,
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Net,
            Sort::Net => Sort::Pids,
            Sort::Pids => Sort::Name,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::State => "status",
            Sort::Cpu => "cpu",
            Sort::Mem => "memory",
            Sort::Net => "network",
            Sort::Pids => "pids",
        }
    }
    /// Numeric columns start with the biggest value on top, text columns A to Z.
    fn default_asc(self) -> bool {
        matches!(self, Sort::Name | Sort::State)
    }
}

struct Pending {
    action: Action,
    name: String,
}

struct App {
    tab: Tab,
    sort: Sort,
    asc: bool,
    selected: Option<String>,
    scroll: usize,
    filter: String,
    filtering: bool,
    show_all: bool,
    detail: bool,
    pending: Option<Pending>,
    message: Option<(String, Instant)>,
    flash: Arc<Mutex<Option<String>>>,
    quit: bool,
}

fn level_color(p: f64) -> Color {
    if p >= 85.0 {
        Color::Red
    } else if p >= 60.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn bar(p: f64, width: usize) -> String {
    let filled = ((p.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn rate(b: f64) -> String {
    if b < 1.0 {
        "-".into()
    } else {
        format!("{}/s", human_bytes(b))
    }
}

fn mem_pair(c: &ContainerView) -> String {
    let used = human_bytes(c.mem_bytes as f64);
    if c.mem_limit > 0 {
        format!("{used} / {}", human_bytes(c.mem_limit as f64))
    } else {
        format!("{used} / no limit")
    }
}

fn state_style(c: &ContainerView) -> (&'static str, Color) {
    if c.health.as_deref() == Some("unhealthy") {
        return ("●", Color::Red);
    }
    match c.state.as_str() {
        "running" => ("●", Color::Green),
        "paused" => ("◐", Color::Yellow),
        "restarting" => ("↻", Color::Yellow),
        "created" => ("○", Color::Gray),
        _ => ("■", Color::DarkGray),
    }
}

fn visible(state: &Arc<AppState>, app: &App) -> Vec<ContainerView> {
    let q = app.filter.to_lowercase();
    let mut v: Vec<ContainerView> = state
        .containers
        .read()
        .unwrap()
        .values()
        .filter(|c| app.show_all || c.is_running())
        .filter(|c| {
            q.is_empty()
                || c.name.to_lowercase().contains(&q)
                || c.image.to_lowercase().contains(&q)
        })
        .cloned()
        .collect();
    // Sort ascending by the chosen key (ties by name), then flip for descending.
    v.sort_by(|a, b| {
        let ord = match app.sort {
            Sort::Name => a.name.cmp(&b.name),
            Sort::State => a.state.cmp(&b.state),
            Sort::Cpu => a.cpu_percent.total_cmp(&b.cpu_percent),
            Sort::Mem => a.mem_bytes.cmp(&b.mem_bytes),
            Sort::Net => (a.net_rx_bps + a.net_tx_bps).total_cmp(&(b.net_rx_bps + b.net_tx_bps)),
            Sort::Pids => a.pids.cmp(&b.pids),
        };
        ord.then_with(|| a.name.cmp(&b.name))
    });
    if !app.asc {
        v.reverse();
    }
    v
}

pub async fn run(state: Arc<AppState>) -> Result<()> {
    // The TUI counts as an open UI: the collector polls at the fast pace while it runs.
    let _viewer = state.add_viewer();
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, state).await;
    ratatui::restore();
    res
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, state: Arc<AppState>) -> Result<()> {
    let mut app = App {
        tab: Tab::Containers,
        sort: Sort::Name,
        asc: true,
        selected: None,
        scroll: 0,
        filter: String::new(),
        filtering: false,
        show_all: true,
        detail: false,
        pending: None,
        message: None,
        flash: Arc::new(Mutex::new(None)),
        quit: false,
    };
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    while !app.quit {
        terminal.draw(|f| draw(f, &state, &mut app))?;
        tokio::select! {
            _ = tick.tick() => {}
            ev = events.next() => {
                if let Some(Ok(Event::Key(k))) = ev
                    && k.kind == KeyEventKind::Press
                {
                    on_key(k, &state, &mut app);
                }
            }
        }
        if let Some(m) = app.flash.lock().unwrap().take() {
            app.message = Some((m, Instant::now()));
        }
    }
    Ok(())
}

fn spawn_action(state: &Arc<AppState>, app: &App, name: String, action: Action) {
    let (st, flash) = (state.clone(), app.flash.clone());
    tokio::spawn(async move {
        let msg = match container_action(&st, &name, action, Source::Local).await {
            Ok(()) => format!("{action:?} sent to {name}"),
            Err(e) => format!("{name}: {e:#}"),
        };
        *flash.lock().unwrap() = Some(msg);
    });
}

fn on_key(k: KeyEvent, state: &Arc<AppState>, app: &mut App) {
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }
    // Waiting for y/n on a destructive action.
    if let Some(p) = app.pending.take() {
        if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            spawn_action(state, app, p.name, p.action);
        }
        return;
    }
    if app.filtering {
        match k.code {
            KeyCode::Enter => app.filtering = false,
            KeyCode::Esc => {
                app.filtering = false;
                app.filter.clear();
            }
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Char(c) => app.filter.push(c),
            _ => {}
        }
        return;
    }
    match k.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('1') => app.tab = Tab::Containers,
        KeyCode::Char('2') => app.tab = Tab::Alerts,
        KeyCode::Char('3') => app.tab = Tab::Events,
        KeyCode::Tab => {
            app.tab = match app.tab {
                Tab::Containers => Tab::Alerts,
                Tab::Alerts => Tab::Events,
                Tab::Events => Tab::Containers,
            }
        }
        KeyCode::Esc => {
            app.detail = false;
            app.filter.clear();
        }
        _ => {}
    }
    if app.tab != Tab::Containers {
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => app.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => app.scroll = app.scroll.saturating_sub(1),
            _ => {}
        }
        return;
    }
    let list = visible(state, app);
    let idx = app
        .selected
        .as_ref()
        .and_then(|n| list.iter().position(|c| &c.name == n))
        .unwrap_or(0);
    let mut go = |i: usize| {
        app.selected = list
            .get(i.min(list.len().saturating_sub(1)))
            .map(|c| c.name.clone())
    };
    match k.code {
        KeyCode::Down | KeyCode::Char('j') => go(idx + 1),
        KeyCode::Up | KeyCode::Char('k') => go(idx.saturating_sub(1)),
        KeyCode::PageDown => go(idx + 10),
        KeyCode::PageUp => go(idx.saturating_sub(10)),
        KeyCode::Home | KeyCode::Char('g') => go(0),
        KeyCode::End | KeyCode::Char('G') => go(usize::MAX),
        KeyCode::Enter => app.detail = !app.detail,
        KeyCode::Char('/') => app.filtering = true,
        KeyCode::Char('s') => {
            app.sort = app.sort.next();
            app.asc = app.sort.default_asc();
        }
        KeyCode::Char('r') => app.asc = !app.asc,
        KeyCode::Char('a') => app.show_all = !app.show_all,
        KeyCode::Char(ch @ ('S' | 'R' | 'P' | 'K' | 'D')) => {
            let Some(c) = list.get(idx) else { return };
            let action = match (ch, c.state.as_str()) {
                ('S', "running" | "paused" | "restarting") => Action::Stop,
                ('S', _) => Action::Start,
                ('R', _) => Action::Restart,
                ('P', "paused") => Action::Unpause,
                ('P', _) => Action::Pause,
                ('K', _) => Action::Kill,
                _ => Action::Remove,
            };
            if matches!(action, Action::Kill | Action::Remove) {
                app.pending = Some(Pending {
                    action,
                    name: c.name.clone(),
                });
            } else {
                spawn_action(state, app, c.name.clone(), action);
            }
        }
        _ => {}
    }
}

fn draw(f: &mut Frame, state: &Arc<AppState>, app: &mut App) {
    let [head, tabs, body, foot] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, head, state);
    let alerts = state.engine.lock().unwrap().active_alerts();
    let titles = vec![
        "1 Containers".to_string(),
        format!("2 Alerts ({})", alerts.len()),
        "3 Events".to_string(),
    ];
    let sel = match app.tab {
        Tab::Containers => 0,
        Tab::Alerts => 1,
        Tab::Events => 2,
    };
    f.render_widget(
        Tabs::new(titles)
            .select(sel)
            .highlight_style(Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED))
            .divider(" "),
        tabs,
    );
    match app.tab {
        Tab::Containers => draw_containers(f, body, state, app),
        Tab::Alerts => draw_alerts(f, body, state, app, &alerts),
        Tab::Events => draw_events(f, body, state, app),
    }
    draw_footer(f, foot, app);
}

fn draw_header(f: &mut Frame, area: Rect, state: &Arc<AppState>) {
    let (up, err, off) = {
        let s = state.docker_status.read().unwrap();
        (s.up, s.error.clone(), s.disabled)
    };
    let host = state.host.read().unwrap().clone();
    let cs = state.containers.read().unwrap();
    let running = cs.values().filter(|c| c.is_running()).count();
    let total = cs.len();
    drop(cs);
    let mut spans = vec![];
    if let Some(h) = host {
        for (label, p) in [
            ("CPU", h.cpu_percent),
            ("MEM", h.mem_percent),
            ("DSK", h.disk_percent_max),
        ] {
            spans.push(Span::raw(format!("{label} ")));
            spans.push(Span::styled(bar(p, 10), Style::new().fg(level_color(p))));
            spans.push(Span::raw(format!(" {p:>4.0}%   ")));
        }
        spans.push(Span::raw(format!("load {:.2}   ", h.load1)));
    }
    let dock = if off {
        Span::styled(
            "● docker monitoring off (host only)",
            Style::new().fg(Color::DarkGray),
        )
    } else if up {
        Span::styled("● docker up", Style::new().fg(Color::Green))
    } else {
        Span::styled(
            format!("● docker down ({})", err.unwrap_or_default()),
            Style::new().fg(Color::Red),
        )
    };
    let line2 = Line::from(vec![
        dock,
        Span::raw(format!("   containers {running}/{total} running")),
    ]);
    // How hard ctopx is working Docker, so a slow daemon is never a mystery.
    let p = state.poll.read().unwrap().clone();
    let mut line3 = vec![Span::styled(
        format!(
            "docker api: poll every {:.1}s, {:.1} req/s, last poll {} ms",
            p.interval_ms as f64 / 1000.0,
            p.requests_per_sec,
            p.cycle_ms
        ),
        Style::new().fg(Color::DarkGray),
    )];
    if p.throttle > 1 {
        line3.push(Span::styled(
            format!("   slowed x{} to protect Docker", p.throttle),
            Style::new().fg(Color::Yellow),
        ));
    }
    let name = state.cfg().instance_name();
    f.render_widget(
        Paragraph::new(vec![Line::from(spans), line2, Line::from(line3)])
            .block(Block::bordered().title(format!(" ctopx @ {name} "))),
        area,
    );
}

fn draw_containers(f: &mut Frame, area: Rect, state: &Arc<AppState>, app: &mut App) {
    let list = visible(state, app);
    if app
        .selected
        .as_ref()
        .is_none_or(|n| !list.iter().any(|c| &c.name == n))
    {
        app.selected = list.first().map(|c| c.name.clone());
    }
    let idx = app
        .selected
        .as_ref()
        .and_then(|n| list.iter().position(|c| &c.name == n));
    let (table_area, detail_area) = if app.detail {
        let [a, b] =
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };

    // The sorted column carries an arrow: ▲ ascending, ▼ descending.
    let arrow = if app.asc { "▲" } else { "▼" };
    let h = |label: &str, s: Sort| -> String {
        if app.sort == s {
            format!("{label} {arrow}")
        } else {
            label.to_string()
        }
    };
    let header = Row::new([
        String::new(),
        h("NAME", Sort::Name),
        "IMAGE".into(),
        h("CPU", Sort::Cpu),
        String::new(),
        h("MEM USED / TOTAL", Sort::Mem),
        "MEM%".into(),
        h("NET RX", Sort::Net),
        "NET TX".into(),
        h("PIDS", Sort::Pids),
        h("STATUS", Sort::State),
    ])
    .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan));
    let rows: Vec<Row> = list
        .iter()
        .map(|c| {
            let (icon, col) = state_style(c);
            let running = c.is_running();
            let dim = if running {
                Style::new()
            } else {
                Style::new().fg(Color::DarkGray)
            };
            let dash = || "-".to_string();
            Row::new(vec![
                Cell::from(Span::styled(icon, Style::new().fg(col))),
                Cell::from(c.name.clone()),
                Cell::from(c.image.clone()),
                Cell::from(Span::styled(
                    if running {
                        format!("{:>5.1}%", c.cpu_percent)
                    } else {
                        dash()
                    },
                    Style::new().fg(level_color(c.cpu_percent)),
                )),
                Cell::from(Span::styled(
                    if running {
                        bar(c.cpu_percent.min(100.0), 8)
                    } else {
                        String::new()
                    },
                    Style::new().fg(level_color(c.cpu_percent)),
                )),
                Cell::from(if running { mem_pair(c) } else { dash() }),
                Cell::from(Span::styled(
                    if running {
                        format!("{:>5.1}%", c.mem_percent)
                    } else {
                        dash()
                    },
                    Style::new().fg(level_color(c.mem_percent)),
                )),
                Cell::from(if running { rate(c.net_rx_bps) } else { dash() }),
                Cell::from(if running { rate(c.net_tx_bps) } else { dash() }),
                Cell::from(if running { c.pids.to_string() } else { dash() }),
                Cell::from(c.status.clone()),
            ])
            .style(dim)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(22),
        Constraint::Length(20),
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(26),
        Constraint::Length(7),
        Constraint::Length(12),
        Constraint::Length(11),
        Constraint::Length(7),
        Constraint::Min(10),
    ];
    let title = format!(
        " containers ({}) sorted by {} {} {}",
        list.len(),
        app.sort.label(),
        arrow,
        if app.show_all { "" } else { "(running only) " }
    );
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut ts = TableState::default().with_selected(idx);
    f.render_stateful_widget(table, table_area, &mut ts);

    if let (Some(a), Some(i)) = (detail_area, idx) {
        draw_detail(f, a, &list[i]);
    }
}

fn spark(
    f: &mut Frame,
    area: Rect,
    title: &str,
    data: &std::collections::VecDeque<f32>,
    scale: f32,
    color: Color,
) {
    let d: Vec<u64> = data.iter().map(|v| (v * scale).max(0.0) as u64).collect();
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(title.to_string()))
            .data(d)
            .style(Style::new().fg(color)),
        area,
    );
}

fn draw_detail(f: &mut Frame, area: Rect, c: &ContainerView) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).areas(area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(c.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {}", c.short_id)),
        ]),
        Line::from(format!("image   {}", c.image)),
        Line::from(format!(
            "state   {} {}",
            c.state,
            c.health
                .as_deref()
                .map(|h| format!("({h})"))
                .unwrap_or_default()
        )),
        Line::from(format!("status  {}", c.status)),
        Line::from(format!("memory  {} ({:.1}%)", mem_pair(c), c.mem_percent)),
        Line::from(format!(
            "disk io {} read, {} write",
            rate(c.blk_read_bps),
            rate(c.blk_write_bps)
        )),
        Line::from(format!(
            "net     {} in, {} out (total)",
            human_bytes(c.net_rx_total as f64),
            human_bytes(c.net_tx_total as f64)
        )),
    ];
    if let Some(p) = &c.project {
        lines.push(Line::from(format!("compose {p}")));
    }
    if !c.ports.is_empty() {
        lines.push(Line::from(format!("ports   {}", c.ports.join(", "))));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" details ")),
        left,
    );
    let rows = Layout::vertical([Constraint::Ratio(1, 4); 4]).split(right);
    spark(
        f,
        rows[0],
        &format!(" cpu {:.1}% ", c.cpu_percent),
        &c.history.cpu,
        10.0,
        Color::Green,
    );
    spark(
        f,
        rows[1],
        &format!(" memory {:.1}% ", c.mem_percent),
        &c.history.mem,
        10.0,
        Color::Magenta,
    );
    spark(
        f,
        rows[2],
        &format!(" net rx {} ", rate(c.net_rx_bps)),
        &c.history.rx,
        1.0,
        Color::Cyan,
    );
    spark(
        f,
        rows[3],
        &format!(" net tx {} ", rate(c.net_tx_bps)),
        &c.history.tx,
        1.0,
        Color::Blue,
    );
}

fn ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

fn sev_color(s: Severity) -> Color {
    match s {
        Severity::Critical => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Info => Color::Blue,
    }
}

fn draw_alerts(f: &mut Frame, area: Rect, state: &Arc<AppState>, app: &App, active: &[Alert]) {
    let hist: Vec<Alert> = state
        .engine
        .lock()
        .unwrap()
        .history
        .iter()
        .rev()
        .take(200)
        .cloned()
        .collect();
    let mut rows: Vec<Row> = vec![];
    for a in active {
        rows.push(
            Row::new(vec![
                ts(a.started_at),
                "FIRING".into(),
                a.severity.as_str().into(),
                a.subject.clone(),
                a.rule.clone(),
                a.message.clone(),
            ])
            .style(Style::new().fg(sev_color(a.severity))),
        );
    }
    for a in &hist {
        rows.push(
            Row::new(vec![
                ts(a.started_at),
                "resolved".into(),
                a.severity.as_str().into(),
                a.subject.clone(),
                a.rule.clone(),
                a.message.clone(),
            ])
            .style(Style::new().fg(Color::DarkGray)),
        );
    }
    let rows: Vec<Row> = rows.into_iter().skip(app.scroll).collect();
    let t = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(22),
            Constraint::Length(26),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["SINCE", "STATE", "SEVERITY", "SUBJECT", "RULE", "MESSAGE"])
            .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
    )
    .block(Block::bordered().title(" alerts (active first, then history) "));
    f.render_widget(t, area);
}

fn draw_events(f: &mut Frame, area: Rect, state: &Arc<AppState>, app: &App) {
    let evs: Vec<EventRecord> = state
        .engine
        .lock()
        .unwrap()
        .events
        .iter()
        .rev()
        .cloned()
        .collect();
    let rows: Vec<Row> = evs
        .iter()
        .skip(app.scroll)
        .map(|e| {
            Row::new(vec![
                ts(e.ts),
                e.action.clone(),
                e.subject.clone(),
                e.message.clone(),
                if e.notified_rules.is_empty() {
                    String::new()
                } else {
                    format!("notified: {}", e.notified_rules.join(","))
                },
            ])
        })
        .collect();
    let t = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(18),
            Constraint::Length(22),
            Constraint::Min(30),
            Constraint::Length(36),
        ],
    )
    .header(
        Row::new(["TIME", "EVENT", "CONTAINER", "MESSAGE", ""])
            .style(Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
    )
    .block(Block::bordered().title(" docker events "));
    f.render_widget(t, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &mut App) {
    if app
        .message
        .as_ref()
        .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(6))
    {
        app.message = None;
    }
    let line = if let Some(p) = &app.pending {
        let what = if p.action == Action::Kill {
            "Kill (SIGKILL)"
        } else {
            "Remove"
        };
        Line::from(Span::styled(
            format!("{what} {}?  y = yes, any other key = cancel", p.name),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if app.filtering {
        Line::from(vec![
            Span::styled("filter: ", Style::new().fg(Color::Yellow)),
            Span::raw(format!("{}_", app.filter)),
            Span::styled("   Enter keep, Esc clear", Style::new().fg(Color::DarkGray)),
        ])
    } else if let Some((m, _)) = &app.message {
        Line::from(Span::styled(m.clone(), Style::new().fg(Color::Yellow)))
    } else {
        let f2 = if app.filter.is_empty() {
            String::new()
        } else {
            format!("   filter: {}", app.filter)
        };
        Line::from(Span::styled(
            format!(
                "↑↓ select  Enter details  / filter  s sort field  r reverse  a stopped  S start/stop  R restart  P pause  K kill  D remove  Tab view  q quit{f2}"
            ),
            Style::new().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}
