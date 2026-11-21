'use strict';
const $ = (s, r = document) => r.querySelector(s);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const HIST = 120;

const S = {
  snap: null,
  hist: {},
  host: { cpu: [], mem: [] },
  page: null,
  arg: null,
  filter: 'all',
  q: '',
  sort: 'name',
  alertHist: [],
  silences: [],
  events: [],
  rules: [],
  channels: [],
  deliveries: [],
};
let token = '';
try { token = localStorage.getItem('ctopx-token') || ''; } catch (e) { /* storage blocked */ }
let es = null;

// ---------- formatting ----------
function bytes(n) {
  n = Math.max(0, Number(n) || 0);
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i === 0 ? n.toFixed(0) : n.toFixed(1)) + ' ' + u[i];
}
const rate = (n) => (n < 1 ? '0 B/s' : bytes(n) + '/s');
const pct = (n) => (Number(n) || 0).toFixed(1) + '%';
function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  if (s < 86400) return Math.floor(s / 3600) + 'h ' + Math.floor((s % 3600) / 60) + 'm ago';
  return Math.floor(s / 86400) + 'd ago';
}
const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const level = (p) => (p >= 90 ? 'crit' : p >= 70 ? 'warn' : '');
function uptime(s) {
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
}

// ---------- api ----------
async function api(path, opt = {}) {
  const headers = { ...(opt.headers || {}) };
  if (token) headers.Authorization = 'Bearer ' + token;
  if (opt.body && !headers['Content-Type']) headers['Content-Type'] = 'application/json';
  const r = await fetch(path, { ...opt, headers });
  if (r.status === 401) { askToken(); throw new Error('Access token required'); }
  const text = await r.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch (e) { /* not json */ }
  if (!r.ok) throw new Error((json && json.error) || r.statusText || 'Request failed (' + r.status + ')');
  return json;
}
const post = (path, body) => api(path, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) });

function askToken() {
  const dlg = $('#dlg-token');
  if (!dlg.open) dlg.showModal();
}
$('#f-token').addEventListener('submit', () => {
  token = $('#token-input').value.trim();
  try { localStorage.setItem('ctopx-token', token); } catch (e) { /* ignore */ }
  location.reload();
});

// ---------- toasts ----------
function toast(title, text, kind = 'info', ms = 7000) {
  const el = document.createElement('div');
  el.className = 'toast ' + kind;
  el.innerHTML = `<b>${esc(title)}</b><span>${esc(text || '')}</span>`;
  $('#toasts').appendChild(el);
  setTimeout(() => el.remove(), ms);
}

// ---------- theme ----------
$('#theme').addEventListener('click', () => {
  const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('ctopx-theme', next); } catch (e) { /* ignore */ }
});

// ---------- charts ----------
function spark(values, max) {
  if (!values || values.length < 2) return '<svg class="spark" viewBox="0 0 100 26" preserveAspectRatio="none"></svg>';
  const m = max || Math.max(1, ...values);
  const step = 100 / (HIST - 1);
  const off = (HIST - values.length) * step;
  const pts = values.map((v, i) => [off + i * step, 24 - Math.min(1, v / m) * 22]);
  const line = pts.map((p, i) => (i ? 'L' : 'M') + p[0].toFixed(1) + ' ' + p[1].toFixed(1)).join('');
  const fill = line + `L${pts[pts.length - 1][0].toFixed(1)} 26L${pts[0][0].toFixed(1)} 26Z`;
  return `<svg class="spark" viewBox="0 0 100 26" preserveAspectRatio="none" aria-hidden="true"><path class="fill" d="${fill}"/><path class="line" d="${line}"/></svg>`;
}

function chart(title, values, fmt, color, fixedMax, left = 34) {
  const W = 300, H = 90, L = left, B = 6;
  const max = fixedMax || Math.max(1, ...(values.length ? values : [1])) * 1.15;
  const x = (i) => L + (i / (HIST - 1)) * (W - L - 4) + ((HIST - values.length) / (HIST - 1)) * (W - L - 4);
  const y = (v) => 6 + (H - 6 - B) * (1 - Math.min(1, v / max));
  const grid = [0, .5, 1].map((f) => `<line class="grid" x1="${L}" x2="${W - 4}" y1="${y(max * f)}" y2="${y(max * f)}"/><text x="${L - 4}" y="${y(max * f) + 3}" text-anchor="end">${esc(fmt(max * f))}</text>`).join('');
  const d = values.map((v, i) => (i ? 'L' : 'M') + x(i).toFixed(1) + ' ' + y(v).toFixed(1)).join('');
  const last = values.length ? values[values.length - 1] : 0;
  return `<div class="chart"><div class="chart-top"><span>${esc(title)}</span><b class="num">${esc(fmt(last))}</b></div><svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(title)} over the last few minutes">${grid}<path class="a" d="${d}" stroke="${color}"/></svg></div>`;
}

// ---------- state updates ----------
function pushHist(arr, v) { arr.push(v); if (arr.length > HIST) arr.shift(); }

function onSnapshot(snap) {
  S.snap = snap;
  const names = new Set();
  for (const c of snap.containers) {
    names.add(c.name);
    if (!c.state || c.state !== 'running') continue;
    const h = (S.hist[c.name] ||= { cpu: [], mem: [], rx: [], tx: [] });
    pushHist(h.cpu, c.cpu_percent); pushHist(h.mem, c.mem_percent); pushHist(h.rx, c.net_rx_bps); pushHist(h.tx, c.net_tx_bps);
  }
  for (const k of Object.keys(S.hist)) if (!names.has(k)) delete S.hist[k];
  if (snap.host) { pushHist(S.host.cpu, snap.host.cpu_percent); pushHist(S.host.mem, snap.host.mem_percent); }
  $('#instance').textContent = snap.instance + ' · v' + snap.version;
  const n = snap.alerts.length;
  const badge = $('#alert-badge');
  badge.hidden = n === 0;
  badge.textContent = n;
  document.title = (n ? `(${n}) ` : '') + 'ctopx · ' + snap.instance;
  refresh();
}

function onAlert(a) {
  if (a.state === 'firing') {
    if (a.notified || !a.silenced) toast(`${a.subject}: ${a.rule}`, a.message, a.severity);
  } else {
    toast(`Resolved: ${a.subject}`, a.rule, 'ok', 4500);
  }
  if (S.page === 'alerts') loadAlerts();
}

function onDockerEvent(e) {
  S.events.unshift(e);
  if (S.events.length > 500) S.events.pop();
  if (e.notified_rules && e.notified_rules.length) toast(e.subject + ': ' + e.action, e.message, 'info', 5000);
  if (S.page === 'events') renderEvents();
  if (S.page === 'containers' && S.arg === e.subject) renderDrawer();
}

function onDelivery(d) {
  S.deliveries.unshift(d);
  if (!d.ok) toast(`Notification to ${d.channel} failed`, d.error, 'error', 10000);
  if (S.page === 'channels') renderDeliveries();
}

function setConn(state, text) {
  const el = $('#conn');
  el.className = 'conn ' + state;
  $('span', el).textContent = text;
}

function connect() {
  if (es) es.close();
  es = new EventSource('/api/stream' + (token ? '?token=' + encodeURIComponent(token) : ''));
  es.addEventListener('snapshot', (e) => onSnapshot(JSON.parse(e.data)));
  es.addEventListener('alert', (e) => onAlert(JSON.parse(e.data)));
  es.addEventListener('docker_event', (e) => onDockerEvent(JSON.parse(e.data)));
  es.addEventListener('delivery', (e) => onDelivery(JSON.parse(e.data)));
  es.onopen = () => setConn('live', 'Live');
  es.onerror = () => {
    setConn('down', 'Reconnecting');
    api('/api/state').catch(() => { /* 401 opens the token dialog */ });
  };
}

// ---------- routing ----------
function route() {
  const h = location.hash.replace(/^#\//, '') || 'containers';
  const [page, ...rest] = h.split('/');
  const arg = rest.length ? decodeURIComponent(rest.join('/')) : null;
  const changed = page !== S.page;
  S.page = ['containers', 'alerts', 'events', 'rules', 'channels'].includes(page) ? page : 'containers';
  S.arg = arg;
  document.querySelectorAll('.nav a').forEach((a) => {
    if (a.dataset.page === S.page) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  });
  if (changed) buildPage();
  refresh();
}
window.addEventListener('hashchange', route);

function buildPage() {
  const m = $('#main');
  if (S.page === 'containers') {
    m.innerHTML = `
      <div class="page-head"><h1>Containers</h1><p class="sub" id="sub"></p></div>
      <div id="banner"></div>
      <div class="host" id="host"></div>
      <div class="toolbar">
        <input id="q" type="search" placeholder="Filter by name or image" aria-label="Filter containers" value="${esc(S.q)}">
        <div class="seg" role="group" aria-label="Show">
          ${[['all', 'All'], ['running', 'Running'], ['stopped', 'Stopped'], ['problem', 'Needs attention']].map(([k, l]) => `<button type="button" data-f="${k}" aria-pressed="${S.filter === k}">${l}</button>`).join('')}
        </div>
        <label>Sort by
          <select id="sort">${[['name', 'Name'], ['cpu', 'CPU'], ['mem', 'Memory'], ['net', 'Network']].map(([k, l]) => `<option value="${k}" ${S.sort === k ? 'selected' : ''}>${l}</option>`).join('')}</select>
        </label>
      </div>
      <div class="tablewrap">
        <table class="ctable"><thead><tr><th>Container</th><th>CPU</th><th>Memory</th><th class="col-net">Network</th><th class="col-status">Status</th></tr></thead><tbody id="rows"></tbody></table>
      </div>`;
    $('#q').addEventListener('input', (e) => { S.q = e.target.value; renderRows(); });
    $('#sort').addEventListener('change', (e) => { S.sort = e.target.value; renderRows(); });
    m.querySelectorAll('[data-f]').forEach((b) => b.addEventListener('click', () => {
      S.filter = b.dataset.f;
      m.querySelectorAll('[data-f]').forEach((x) => x.setAttribute('aria-pressed', String(x === b)));
      renderRows();
    }));
    $('#rows').addEventListener('click', (e) => {
      const tr = e.target.closest('tr[data-name]');
      if (tr) location.hash = '#/containers/' + encodeURIComponent(tr.dataset.name);
    });
    $('#rows').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { const tr = e.target.closest('tr[data-name]'); if (tr) location.hash = '#/containers/' + encodeURIComponent(tr.dataset.name); }
    });
  } else if (S.page === 'alerts') {
    m.innerHTML = `<div class="page-head"><h1>Alerts</h1><p class="sub" id="sub"></p></div>
      <section><h2>Active</h2><div id="active"></div></section>
      <section><h2>Silences</h2><div id="silences"></div></section>
      <section><h2>History</h2><div id="ahist"></div></section>`;
    loadAlerts();
  } else if (S.page === 'events') {
    m.innerHTML = `<div class="page-head"><h1>Events</h1><p class="sub">Container lifecycle events since ctopx started.</p></div><div id="events"></div>`;
    api('/api/events').then((v) => { S.events = v; renderEvents(); }).catch(() => {});
  } else if (S.page === 'rules') {
    m.innerHTML = `<div class="page-head"><h1>Rules</h1><p class="sub">Defined in the config file. Reload it with <code>POST /api/reload</code> after editing.</p></div><div id="rules"></div>`;
    api('/api/rules').then((v) => { S.rules = v; renderRules(); }).catch(() => {});
  } else if (S.page === 'channels') {
    m.innerHTML = `<div class="page-head"><h1>Channels</h1><p class="sub">Where notifications are delivered.</p></div>
      <section><div id="channels"></div></section>
      <section><h2>Recent deliveries</h2><div id="deliveries"></div></section>`;
    Promise.all([api('/api/channels'), api('/api/deliveries')]).then(([c, d]) => { S.channels = c; S.deliveries = d; renderChannels(); renderDeliveries(); }).catch(() => {});
  }
}

function refresh() {
  if (!S.snap) return;
  if (S.page === 'containers') { renderBanner(); renderHost(); renderRows(); renderDrawer(); }
  else if (S.page === 'alerts') renderActive();
  closeDrawerIfNotRouted();
}

// ---------- containers page ----------
function renderBanner() {
  const d = S.snap.docker;
  $('#banner').innerHTML = d.up ? '' : `<div class="banner" role="alert"><b>Docker is not reachable.</b> ${esc(d.error || '')} Container data below is the last known state.</div>`;
  const cs = S.snap.containers;
  $('#sub').textContent = `${cs.filter((c) => c.state === 'running').length} of ${cs.length} running`;
}

function meter(label, valueText, p, sub, hist, color) {
  return `<div class="hm"><div class="hm-top"><span>${esc(label)}</span><span class="hm-val">${esc(valueText)}</span></div>
    <div class="meter ${level(p)}"><i style="width:${Math.min(100, p).toFixed(1)}%"></i></div>
    <div class="hm-sub">${esc(sub)}</div>${hist ? spark(hist, 100) : ''}</div>`;
}

function renderHost() {
  const h = S.snap.host;
  const el = $('#host');
  if (!h) { el.hidden = true; return; }
  el.hidden = false;
  const disk = h.disks.slice().sort((a, b) => b.percent - a.percent)[0];
  el.innerHTML =
    meter('CPU', pct(h.cpu_percent), h.cpu_percent, `${h.cpus} cores`, S.host.cpu) +
    meter('Memory', pct(h.mem_percent), h.mem_percent, `${bytes(h.mem_used)} of ${bytes(h.mem_total)}`, S.host.mem) +
    meter('Disk', disk ? pct(disk.percent) : 'n/a', disk ? disk.percent : 0, disk ? `${disk.mount}: ${bytes(disk.used)} of ${bytes(disk.total)}` : 'No disks found') +
    `<div class="hm"><div class="hm-top"><span>Load</span><span class="hm-val">${h.load1.toFixed(2)}</span></div><div class="hm-sub">5 min ${h.load5.toFixed(2)}, 15 min ${h.load15.toFixed(2)}</div><div class="hm-sub">${esc(h.os || h.hostname)}</div><div class="hm-sub">up ${uptime(h.uptime_secs)}</div></div>`;
}

function stateClass(c) {
  if (c.health === 'unhealthy') return 's-unhealthy';
  return 's-' + (c.state || 'exited');
}
function statePill(c) {
  if (c.health === 'unhealthy') return '<span class="pill crit">unhealthy</span>';
  const k = { running: 'ok', paused: 'warn', restarting: 'warn' }[c.state] || '';
  return `<span class="pill ${k}">${esc(c.state || 'unknown')}</span>${c.health === 'starting' ? ' <span class="pill">health check starting</span>' : ''}`;
}

function filtered() {
  const q = S.q.trim().toLowerCase();
  let v = S.snap.containers.filter((c) => {
    if (q && !(c.name.toLowerCase().includes(q) || c.image.toLowerCase().includes(q))) return false;
    if (S.filter === 'running') return c.state === 'running';
    if (S.filter === 'stopped') return c.state !== 'running';
    if (S.filter === 'problem') return c.health === 'unhealthy' || ['restarting', 'dead', 'paused'].includes(c.state) || (c.state === 'exited' && /Exited \((?!0\))/.test(c.status)) || c.cpu_percent >= 90 || c.mem_percent >= 90;
    return true;
  });
  const key = { name: (c) => c.name, cpu: (c) => -c.cpu_percent, mem: (c) => -c.mem_bytes, net: (c) => -(c.net_rx_bps + c.net_tx_bps) }[S.sort];
  v.sort((a, b) => { const x = key(a), y = key(b); return x < y ? -1 : x > y ? 1 : a.name.localeCompare(b.name); });
  return v;
}

function renderRows() {
  const rows = $('#rows');
  if (!rows || !S.snap) return;
  const v = filtered();
  if (!v.length) {
    const none = S.snap.containers.length === 0;
    rows.innerHTML = `<tr><td colspan="5"><div class="empty"><b>${none ? 'No containers found' : 'Nothing matches this filter'}</b>${none ? (S.snap.docker.up ? 'Start a container and it will show up here.' : 'Check that the Docker daemon is running and that ctopx can reach it.') : 'Clear the filter to see all containers.'}</div></td></tr>`;
    return;
  }
  rows.innerHTML = v.map((c) => {
    const run = c.state === 'running';
    const h = S.hist[c.name];
    return `<tr data-name="${esc(c.name)}" class="${stateClass(c)}" tabindex="0">
      <td class="name"><span class="edge"></span><strong>${esc(c.name)}</strong><small>${esc(c.image)}${c.project ? ' · ' + esc(c.project) : ''}</small></td>
      <td class="cpu ${run ? '' : 'dim'}">${run ? `<b>${pct(c.cpu_percent)}</b>${spark(h && h.cpu, 100)}` : '<b>-</b>'}</td>
      <td class="mem ${run ? '' : 'dim'}">${run ? `<b>${bytes(c.mem_bytes)}</b><div class="meter ${level(c.mem_percent)}"><i style="width:${Math.min(100, c.mem_percent).toFixed(1)}%"></i></div><small>${c.mem_limit ? pct(c.mem_percent) + ' of ' + bytes(c.mem_limit) : 'no limit'}</small>` : '<b>-</b>'}</td>
      <td class="net col-net ${run ? '' : 'dim'}">${run ? `↓ ${rate(c.net_rx_bps)}<br>↑ ${rate(c.net_tx_bps)}` : '-'}</td>
      <td class="col-status">${statePill(c)}<small>${esc(c.status)}</small></td></tr>`;
  }).join('');
}

// ---------- drawer ----------
const drawer = $('#drawer'), scrim = $('#scrim');
function closeDrawer() { if (S.arg) location.hash = '#/containers'; }
scrim.addEventListener('click', closeDrawer);
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !drawer.hidden) closeDrawer(); });

function closeDrawerIfNotRouted() {
  if (S.page !== 'containers' || !S.arg) { drawer.hidden = true; scrim.hidden = true; }
}

function renderDrawer() {
  if (S.page !== 'containers' || !S.arg || !S.snap) return;
  const c = S.snap.containers.find((x) => x.name === S.arg);
  const wasOpen = !drawer.hidden;
  drawer.hidden = false; scrim.hidden = false;
  if (!c) {
    drawer.innerHTML = `<div class="drawer-head"><h2>${esc(S.arg)}</h2><button class="btn small" id="d-close">Close</button></div><div class="empty"><b>This container no longer exists</b>It may have been removed.</div>`;
    $('#d-close').onclick = closeDrawer;
    return;
  }
  const run = c.state === 'running';
  const h = S.hist[c.name] || { cpu: [], mem: [], rx: [], tx: [] };
  const evs = S.events.filter((e) => e.subject === c.name).slice(0, 6);
  const acts = S.snap.allow_actions
    ? `<div class="actions">${run ? '<button class="btn" data-act="stop">Stop</button>' : '<button class="btn primary" data-act="start">Start</button>'}<button class="btn" data-act="restart">Restart</button></div>`
    : '<p class="muted">Start, stop and restart are turned off. Set <code>server.allow_actions = true</code> to enable them.</p>';
  const scrollTop = drawer.scrollTop;
  drawer.innerHTML = `
    <div class="drawer-head"><div><h2>${esc(c.name)}</h2><div class="muted num">${esc(c.short_id)}</div></div><div>${statePill(c)} <button class="btn small" id="d-close">Close</button></div></div>
    ${acts}
    ${run ? `<div class="charts">
      ${chart('CPU', h.cpu, (v) => v.toFixed(0) + '%', 'var(--accent)', Math.max(100, ...h.cpu))}
      ${chart('Memory', h.mem, (v) => v.toFixed(0) + '%', 'var(--ok)', 100)}
      ${chart('Network in', h.rx, rate, 'var(--info)', 0, 62)}
      ${chart('Network out', h.tx, rate, 'var(--warn)', 0, 62)}
    </div>` : '<div class="empty"><b>Not running</b>Live charts appear while the container runs.</div>'}
    <dl class="kv">
      <dt>Image</dt><dd>${esc(c.image)}</dd>
      <dt>Status</dt><dd>${esc(c.status)}</dd>
      ${c.health ? `<dt>Health</dt><dd>${esc(c.health)}</dd>` : ''}
      ${c.project ? `<dt>Compose project</dt><dd>${esc(c.project)}</dd>` : ''}
      <dt>Memory</dt><dd>${bytes(c.mem_bytes)}${c.mem_limit ? ' of ' + bytes(c.mem_limit) : ' (no limit)'}</dd>
      <dt>Disk I/O</dt><dd>${rate(c.blk_read_bps)} read, ${rate(c.blk_write_bps)} write</dd>
      <dt>Network total</dt><dd>${bytes(c.net_rx_total)} in, ${bytes(c.net_tx_total)} out</dd>
      <dt>Processes</dt><dd>${c.pids}</dd>
      ${c.ports.length ? `<dt>Ports</dt><dd>${c.ports.map(esc).join('<br>')}</dd>` : ''}
      <dt>Created</dt><dd>${c.created ? new Date(c.created * 1000).toLocaleString() : 'unknown'}</dd>
    </dl>
    ${evs.length ? `<h2>Recent events</h2><div class="list">${evs.map((e) => `<div class="row"><span class="pill">${esc(e.action)}</span><div class="what">${esc(e.message)}</div><span class="when">${clock(e.ts)}</span></div>`).join('')}</div>` : ''}`;
  if (wasOpen) drawer.scrollTop = scrollTop; else drawer.focus?.();
  $('#d-close').onclick = closeDrawer;
  drawer.querySelectorAll('[data-act]').forEach((b) => b.addEventListener('click', async () => {
    b.disabled = true;
    try { await post(`/api/containers/${encodeURIComponent(c.name)}/${b.dataset.act}`); toast(c.name, `${b.dataset.act} sent`, 'ok', 3500); }
    catch (err) { toast(`Could not ${b.dataset.act} ${c.name}`, err.message, 'error', 9000); }
    b.disabled = false;
  }));
}

// ---------- alerts page ----------
async function loadAlerts() {
  try {
    const [h, s] = await Promise.all([api('/api/alerts/history'), api('/api/silences')]);
    S.alertHist = h; S.silences = s;
    renderActive(); renderAlertHist(); renderSilences();
  } catch (e) { /* surfaced by api() */ }
}

function renderActive() {
  const el = $('#active');
  if (!el || !S.snap) return;
  const a = S.snap.alerts;
  $('#sub').textContent = a.length ? `${a.length} firing` : 'Nothing firing';
  el.innerHTML = a.length
    ? `<div class="list">${a.map((x) => `<div class="row firing ${x.severity}"><span class="pill ${x.severity}">${x.severity}</span>
        <div class="what"><strong>${esc(x.subject)}</strong> <span class="muted">${esc(x.rule)}</span><div>${esc(x.message)}${x.silenced ? ' (notifications silenced)' : ''}</div></div>
        <div class="when">since ${ago(x.started_at)}<br><button class="btn small" data-sil="${esc(x.subject)}" data-rule="${esc(x.rule)}">Silence</button></div></div>`).join('')}</div>`
    : '<div class="list"><div class="empty"><b>All clear</b>Rules are being checked every few seconds.</div></div>';
  el.querySelectorAll('[data-sil]').forEach((b) => b.addEventListener('click', () => openSilence(b.dataset.sil, b.dataset.rule)));
}

function renderAlertHist() {
  const el = $('#ahist');
  if (!el) return;
  el.innerHTML = S.alertHist.length
    ? `<div class="tablewrap"><table><thead><tr><th>Started</th><th>Subject</th><th>Rule</th><th>Severity</th><th>Message</th><th>Lasted</th></tr></thead><tbody>${S.alertHist.slice(0, 100).map((a) => `<tr><td class="num">${clock(a.started_at)}</td><td>${esc(a.subject)}</td><td>${esc(a.rule)}</td><td><span class="pill ${a.severity}">${a.severity}</span></td><td>${esc(a.message)}</td><td class="num">${a.resolved_at ? Math.max(1, Math.round((a.resolved_at - a.started_at) / 1000)) + 's' : ''}</td></tr>`).join('')}</tbody></table></div>`
    : '<div class="list"><div class="empty"><b>No resolved alerts yet</b>Alerts that clear will be listed here.</div></div>';
}

function renderSilences() {
  const el = $('#silences');
  if (!el) return;
  el.innerHTML = S.silences.length
    ? `<div class="list">${S.silences.map((s) => `<div class="row"><span class="pill">silenced</span><div class="what"><strong>${esc(s.pattern)}</strong> <span class="muted">${s.rule ? esc(s.rule) : 'all rules'}</span><div>${esc(s.reason || '')}</div></div><div class="when">until ${new Date(s.until).toLocaleTimeString()}<br><button class="btn small" data-del="${s.id}">Remove</button></div></div>`).join('')}</div>`
    : '<p class="muted">No active silences.</p>';
  el.querySelectorAll('[data-del]').forEach((b) => b.addEventListener('click', async () => {
    try { await api('/api/silences/' + b.dataset.del, { method: 'DELETE' }); loadAlerts(); } catch (e) { toast('Could not remove silence', e.message, 'error'); }
  }));
}

function openSilence(subject, rule) {
  $('#sil-pattern').value = subject;
  $('#sil-rule').innerHTML = `<option value="${esc(rule)}">Only the rule ${esc(rule)}</option><option value="">All rules</option>`;
  $('#sil-reason').value = '';
  $('#dlg-silence').showModal();
}
$('#sil-cancel').addEventListener('click', () => $('#dlg-silence').close());
$('#f-silence').addEventListener('submit', async () => {
  try {
    await post('/api/silences', { pattern: $('#sil-pattern').value.trim(), rule: $('#sil-rule').value || null, duration_secs: Number($('#sil-dur').value), reason: $('#sil-reason').value.trim() || null });
    toast('Silenced', $('#sil-pattern').value, 'ok', 3500);
    loadAlerts();
  } catch (e) { toast('Could not silence', e.message, 'error'); }
});

// ---------- events, rules, channels ----------
function renderEvents() {
  const el = $('#events');
  if (!el) return;
  el.innerHTML = S.events.length
    ? `<div class="tablewrap"><table><thead><tr><th>Time</th><th>Event</th><th>Container</th><th>Message</th><th>Notified by</th></tr></thead><tbody>${S.events.slice(0, 200).map((e) => `<tr><td class="num">${clock(e.ts)}</td><td><span class="pill">${esc(e.action)}</span></td><td>${esc(e.subject)}</td><td>${esc(e.message)}</td><td class="muted">${esc((e.notified_rules || []).join(', '))}</td></tr>`).join('')}</tbody></table></div>`
    : '<div class="list"><div class="empty"><b>No events yet</b>Starts, stops, crashes and health changes will appear here.</div></div>';
}

function renderRules() {
  const el = $('#rules');
  if (!el) return;
  el.innerHTML = `<div class="tablewrap"><table><thead><tr><th>Rule</th><th>Condition</th><th>Severity</th><th>Applies to</th><th>Sends to</th><th>Cooldown</th></tr></thead><tbody>${S.rules.map((r) => `<tr class="${r.enabled ? '' : 'dim'}"><td><strong>${esc(r.name)}</strong>${r.enabled ? '' : '<small>disabled</small>'}</td><td>${esc(r.description)}</td><td><span class="pill ${r.severity}">${r.severity}</span></td><td>${r.kind === 'event' || r.kind === 'restart_loop' || (r.kind === 'metric' && !String(r.metric).startsWith('host_')) ? esc((r.scope || []).join(', ')) : 'n/a'}</td><td>${r.channels && r.channels.length ? esc(r.channels.join(', ')) : 'all channels'}</td><td class="num">${r.cooldown_secs}s</td></tr>`).join('')}</tbody></table></div>`;
}

function renderChannels() {
  const el = $('#channels');
  if (!el) return;
  if (!S.channels.length) {
    el.innerHTML = '<div class="list"><div class="empty"><b>No channels configured</b>Alerts are visible here, but nothing is sent. Add a <code>[[channels]]</code> block to your config and reload.</div></div>';
    return;
  }
  el.innerHTML = `<div class="list">${S.channels.map((c) => `<div class="row"><span class="pill info">${esc(c.type)}</span><div class="what"><strong>${esc(c.name)}</strong> <span class="muted">${esc(c.target)}</span><div>Sends ${esc(c.min_severity)} and above${c.enabled ? '' : ', currently disabled'}</div></div><div class="when"><button class="btn small" data-test="${esc(c.name)}">Send test</button></div></div>`).join('')}</div>
    <p><button class="btn" id="test-all">Send test to all channels</button></p>`;
  el.querySelectorAll('[data-test]').forEach((b) => b.addEventListener('click', async () => {
    b.disabled = true;
    try { await post('/api/channels/' + encodeURIComponent(b.dataset.test) + '/test'); toast('Test sent', b.dataset.test, 'ok', 4000); }
    catch (e) { toast(`Test to ${b.dataset.test} failed`, e.message, 'error', 12000); }
    b.disabled = false;
  }));
  $('#test-all').addEventListener('click', async () => { try { await post('/api/notify/test'); toast('Test queued', 'Results appear under recent deliveries.', 'info', 4000); } catch (e) { toast('Test failed', e.message, 'error'); } });
}

function renderDeliveries() {
  const el = $('#deliveries');
  if (!el) return;
  el.innerHTML = S.deliveries.length
    ? `<div class="tablewrap"><table><thead><tr><th>Time</th><th>Channel</th><th>Notification</th><th>Result</th></tr></thead><tbody>${S.deliveries.slice(0, 50).map((d) => `<tr><td class="num">${clock(d.ts)}</td><td>${esc(d.channel)}</td><td>${esc(d.title)}</td><td>${d.ok ? '<span class="pill ok">delivered</span>' : `<span class="pill crit">failed</span> <small>${esc(d.error)}</small>`}</td></tr>`).join('')}</tbody></table></div>`
    : '<p class="muted">Nothing has been sent since ctopx started.</p>';
}

// ---------- boot ----------
(async function boot() {
  route();
  try {
    const h = await api('/api/history');
    for (const [k, v] of Object.entries(h.containers || {})) S.hist[k] = v;
    if (h.host) { S.host.cpu = h.host.cpu || []; S.host.mem = h.host.mem || []; }
    S.events = await api('/api/events');
  } catch (e) { /* the token dialog or the banner explains */ }
  connect();
})();
