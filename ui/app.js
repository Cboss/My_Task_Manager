"use strict";

// ---------- backend bridge (falls back to fake data when opened in a plain browser) ----------
const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : mockInvoke;

// ---------- state ----------
const HIST = 90;
const hist = { cpu: [], mem: [], dr: [], dw: [], rx: [], tx: [] };
const state = {
  snap: null,
  sortKey: "cpu",
  sortDir: -1,
  mode: "list",
  filter: "",
  sel: null, // { pid } or { group: name }
  details: null,
  paused: false,
  collapsed: new Set(),
  info: null,
  tab: "procs",
  conns: null,
  startup: null,
};

const $ = (id) => document.getElementById(id);
const tbody = document.querySelector("#procs tbody");

// ---------- formatting ----------
function fmtBytes(n, digits = 1) {
  if (!n || n < 1) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(u.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  return (n / 1024 ** i).toFixed(i === 0 ? 0 : digits) + " " + u[i];
}
const fmtRate = (n) => (n < 1 ? "–" : fmtBytes(n) + "/s");
const fmtPct = (n) => (n < 0.05 ? "0%" : n.toFixed(1) + "%");
function fmtDur(s) {
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  return d ? `${d}d ${h}h ${m}m` : h ? `${h}h ${m}m` : `${m}m ${Math.floor(s % 60)}s`;
}
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

// Background tint scaled to intensity, like the stock Task Manager heat-map.
function heat(v, max, rgb) {
  const a = Math.min(1, v / max) * 0.45;
  return a < 0.02 ? "" : `background: rgba(${rgb}, ${a.toFixed(3)})`;
}

// ---------- sparkline ----------
function spark(canvas, series, colors, max) {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth, h = canvas.clientHeight;
  if (canvas.width !== w * dpr) { canvas.width = w * dpr; canvas.height = h * dpr; }
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  ctx.strokeStyle = "#ffffff0a";
  ctx.lineWidth = 1;
  for (let i = 1; i < 4; i++) {
    const y = Math.round((h * i) / 4) + 0.5;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
  }

  const top = max ?? Math.max(1, ...series.flat()) * 1.15;
  series.forEach((data, si) => {
    if (data.length < 2) return;
    const step = w / (HIST - 1);
    const x0 = w - (data.length - 1) * step;
    const pts = data.map((v, i) => [x0 + i * step, h - (v / top) * (h - 2) - 1]);
    const color = colors[si];

    ctx.beginPath();
    pts.forEach(([x, y], i) => (i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)));
    ctx.lineTo(pts[pts.length - 1][0], h); ctx.lineTo(pts[0][0], h); ctx.closePath();
    const g = ctx.createLinearGradient(0, 0, 0, h);
    g.addColorStop(0, color + "55"); g.addColorStop(1, color + "00");
    ctx.fillStyle = g; ctx.fill();

    ctx.beginPath();
    pts.forEach(([x, y], i) => (i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)));
    ctx.strokeStyle = color; ctx.lineWidth = 1.5; ctx.lineJoin = "round"; ctx.stroke();
  });
}

const css = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();
const COLORS = { cpu: css("--cpu"), mem: css("--mem"), disk: css("--disk"), net: css("--net"), net2: css("--net-2") };

function push(arr, v) { arr.push(v); if (arr.length > HIST) arr.shift(); }

// ---------- metrics ----------
function renderMetrics(s) {
  push(hist.cpu, s.cpu);
  push(hist.mem, (s.memUsed / s.memTotal) * 100);
  push(hist.dr, s.diskRead); push(hist.dw, s.diskWrite);
  push(hist.rx, s.netRx); push(hist.tx, s.netTx);

  $("m-cpu").textContent = s.cpu.toFixed(0) + "%";
  $("m-mem").textContent = ((s.memUsed / s.memTotal) * 100).toFixed(0) + "%";
  $("m-mem-sub").textContent = `${fmtBytes(s.memUsed)} / ${fmtBytes(s.memTotal)} · swap ${fmtBytes(s.swapUsed)}`;
  $("m-disk").textContent = fmtRate(s.diskRead + s.diskWrite);
  $("m-disk-sub").textContent = `R ${fmtRate(s.diskRead)} · W ${fmtRate(s.diskWrite)}`;
  $("m-net").textContent = fmtRate(s.netRx + s.netTx);
  $("m-net-sub").textContent = `↓ ${fmtRate(s.netRx)} · ↑ ${fmtRate(s.netTx)}`;

  spark($("c-cpu"), [hist.cpu], [COLORS.cpu], 100);
  spark($("c-mem"), [hist.mem], [COLORS.mem], 100);
  spark($("c-disk"), [hist.dr, hist.dw], [COLORS.disk, COLORS.net2]);
  spark($("c-net"), [hist.rx, hist.tx], [COLORS.net, COLORS.net2]);

  const cores = $("cores");
  if (cores.children.length !== s.cores.length) cores.innerHTML = "<i></i>".repeat(s.cores.length);
  s.cores.forEach((v, i) => {
    cores.children[i].style.background = `rgba(79, 209, 197, ${(0.08 + (v / 100) * 0.92).toFixed(2)})`;
    cores.children[i].title = `Core ${i}: ${v.toFixed(0)}%`;
  });

  $("s-count").textContent = `${s.procs.length} processes`;
  $("s-uptime").textContent = `Up ${fmtDur(s.uptime)}`;
}

// ---------- table ----------
const cmp = (key, dir) => (a, b) => {
  const x = a[key], y = b[key];
  return (typeof x === "string" ? x.localeCompare(y, undefined, { sensitivity: "base" }) : x - y) * dir;
};

function matches(p, f) {
  return !f || p.name.toLowerCase().includes(f) || String(p.pid).startsWith(f);
}

function buildRows(procs) {
  const f = state.filter;
  const sorter = cmp(state.sortKey, state.sortDir);

  if (state.mode === "list") {
    return procs.filter((p) => matches(p, f)).sort(sorter).map((p) => ({ p, depth: 0 }));
  }

  if (state.mode === "group") {
    const groups = new Map();
    for (const p of procs) {
      if (!matches(p, f)) continue;
      let g = groups.get(p.name);
      if (!g) groups.set(p.name, (g = { name: p.name, pid: p.pid, pids: [], cpu: 0, mem: 0, diskRead: 0, diskWrite: 0, conns: 0, listening: 0, net: 0, netRx: 0, netTx: 0 }));
      g.pids.push(p.pid); g.cpu += p.cpu; g.mem += p.mem; g.diskRead += p.diskRead; g.diskWrite += p.diskWrite;
      g.conns += p.conns; g.listening += p.listening;
      g.net += p.net; g.netRx += p.netRx; g.netTx += p.netTx;
      g.pid = Math.min(g.pid, p.pid);
    }
    return [...groups.values()].sort(sorter).map((g) => ({ p: g, depth: 0, group: true }));
  }

  // tree
  const byPid = new Map(procs.map((p) => [p.pid, p]));
  const kids = new Map();
  const roots = [];
  for (const p of procs) {
    if (p.parent != null && byPid.has(p.parent)) {
      if (!kids.has(p.parent)) kids.set(p.parent, []);
      kids.get(p.parent).push(p);
    } else roots.push(p);
  }
  // When filtering, keep matches plus their ancestors so context is visible.
  let visible = null;
  if (f) {
    visible = new Set();
    for (const p of procs) {
      if (!matches(p, f)) continue;
      for (let c = p; c && !visible.has(c.pid); c = byPid.get(c.parent)) visible.add(c.pid);
    }
  }
  const rows = [];
  const walk = (list, depth) => {
    for (const p of list.sort(sorter)) {
      if (visible && !visible.has(p.pid)) continue;
      const children = kids.get(p.pid) || [];
      rows.push({ p, depth, hasKids: children.length > 0 });
      if (children.length && (f || !state.collapsed.has(p.pid))) walk(children, depth + 1);
    }
  };
  walk(roots, 0);
  return rows;
}

function renderTable() {
  const s = state.snap;
  if (!s) return;
  const rows = buildRows(s.procs);
  const memMax = s.memTotal * 0.1;
  const html = [];
  for (const r of rows) {
    const p = r.p;
    const selected = state.sel && (r.group ? state.sel.group === p.name : state.sel.pid === p.pid);
    const twist = state.mode === "tree"
      ? `<span class="twist" data-twist="${p.pid}">${r.hasKids ? (state.collapsed.has(p.pid) && !state.filter ? "▸" : "▾") : ""}</span>`
      : "";
    const count = r.group && p.pids.length > 1 ? `<span class="count">×${p.pids.length}</span>` : "";
    html.push(
      `<tr data-pid="${p.pid}"${r.group ? ` data-group="${esc(p.name)}"` : ""} class="${selected ? "sel" : ""}">` +
        `<td style="padding-left:${12 + r.depth * 16}px">${twist}${esc(p.name)}${count}</td>` +
        `<td class="num">${r.group ? "" : p.pid}</td>` +
        `<td class="num${p.cpu > 1 ? " hot" : ""}" style="${heat(p.cpu, 50, "79,209,197")}">${fmtPct(p.cpu)}</td>` +
        `<td class="num${p.mem > 2e8 ? " hot" : ""}" style="${heat(p.mem, memMax, "167,139,250")}">${fmtBytes(p.mem)}</td>` +
        `<td class="num" style="${heat(p.diskRead, 2e7, "246,173,85")}">${fmtRate(p.diskRead)}</td>` +
        `<td class="num" style="${heat(p.diskWrite, 2e7, "246,173,85")}">${fmtRate(p.diskWrite)}</td>` +
        `<td class="num${p.net > 1024 ? " hot" : ""}" style="${heat(p.net, 5e6, "99,179,237")}" title="↓ ${fmtRate(p.netRx)} · ↑ ${fmtRate(p.netTx)}">${s.bandwidth ? fmtRate(p.net) : "–"}</td>` +
        `<td class="num narrow${p.conns ? " hot" : ""}" style="${heat(p.conns, 40, "99,179,237")}" title="${p.conns} established · ${p.listening} listening">${p.conns || (p.listening ? "·" : "–")}</td>` +
      `</tr>`
    );
  }
  tbody.innerHTML = html.join("");

  document.querySelectorAll("thead th").forEach((th) => {
    th.classList.toggle("sorted", th.dataset.sort === state.sortKey);
    th.classList.toggle("asc", th.dataset.sort === state.sortKey && state.sortDir === 1);
  });
}

// ---------- details panel ----------
function selectedPids() {
  if (!state.sel || !state.snap) return [];
  if (state.sel.group) return state.snap.procs.filter((p) => p.name === state.sel.group).map((p) => p.pid);
  return [state.sel.pid];
}

async function select(sel) {
  state.sel = sel;
  state.details = null;
  state.conns = null;
  renderTable();
  if (!sel) { $("panel").hidden = true; return; }
  $("panel").hidden = false;
  renderConns();
  if (sel.pid != null) {
    try { state.details = await invoke("details", { pid: sel.pid }); }
    catch (e) { toast(e, true); }
    refreshConns();
  }
  renderPanel();
}

async function refreshConns() {
  const pid = state.sel?.pid;
  if (pid == null) return;
  try {
    const conns = await invoke("connections", { pid });
    if (state.sel?.pid === pid) { state.conns = conns; renderConns(); }
  } catch (e) { /* process may have exited */ }
}

function renderConns() {
  const wrap = $("d-conns-wrap");
  wrap.hidden = state.sel?.pid == null;
  if (wrap.hidden) return;
  const list = state.conns;
  if (!list) { $("d-conns").innerHTML = `<div class="hint-line">Loading…</div>`; $("d-conns-count").textContent = ""; return; }
  $("d-conns-count").textContent = list.length ? `(${list.length})` : "";
  if (!list.length) { $("d-conns").innerHTML = `<div class="hint-line">No network connections.</div>`; return; }
  const cls = (s) => (s === "Established" ? "est" : s === "Listen" ? "listen" : "");
  $("d-conns").innerHTML = list.slice(0, 80).map((c) =>
    `<div class="conn"><span class="proto">${c.proto}</span>` +
    `<span class="addr">${c.remote && !/^0\.0\.0\.0:0$|^\[::\]:0$/.test(c.remote) ? esc(c.remote) : esc(c.local)}</span>` +
    `<span class="state ${cls(c.state)}">${c.state ? esc(c.state) : "bound"}${c.remote && !/:0$/.test(c.remote) ? ` · local ${esc(c.local)}` : ""}</span></div>`
  ).join("") + (list.length > 80 ? `<div class="hint-line">+${list.length - 80} more</div>` : "");
}

function renderPanel() {
  const sel = state.sel;
  if (!sel || !state.snap) return;
  const live = state.snap.procs.filter((p) => (sel.group ? p.name === sel.group : p.pid === sel.pid));
  if (!live.length && !sel.group) { $("d-sub").textContent = `PID ${sel.pid} · exited`; }

  const sum = (k) => live.reduce((a, p) => a + p[k], 0);
  $("d-cpu").textContent = fmtPct(sum("cpu"));
  $("d-mem").textContent = fmtBytes(sum("mem"));
  $("d-disk").textContent = fmtRate(sum("diskRead") + sum("diskWrite"));
  $("d-net").textContent = state.snap.bandwidth ? `↓ ${fmtRate(sum("netRx"))} ↑ ${fmtRate(sum("netTx"))}` : "needs admin";

  const d = state.details;
  const props = $("d-props");
  if (sel.group) {
    $("d-name").textContent = sel.group;
    $("d-sub").textContent = `${live.length} processes`;
    props.innerHTML = `<dt>PIDs</dt><dd>${live.map((p) => p.pid).join(", ")}</dd>`;
    $("d-prio-wrap").hidden = true;
    $("d-open").hidden = true;
    $("d-kill").firstChild.textContent = `End all ${live.length} `;
    $("d-killtree").hidden = true;
    return;
  }
  $("d-prio-wrap").hidden = false;
  $("d-open").hidden = false;
  $("d-killtree").hidden = false;
  $("d-kill").firstChild.textContent = "End task ";
  if (!d) return;

  $("d-name").textContent = d.name;
  $("d-sub").textContent = live.length ? `PID ${d.pid} · ${d.status}` : `PID ${d.pid} · exited`;
  const row = (k, v) => (v ? `<dt>${k}</dt><dd>${v}</dd>` : "");
  props.innerHTML =
    row("Path", d.exe ? esc(d.exe) : '<span style="color:var(--dim)">access denied</span>') +
    row("Command", d.cmd && esc(d.cmd)) +
    row("Working dir", d.cwd && esc(d.cwd)) +
    row("Parent", d.parent != null ? `<a data-goto="${d.parent}">${esc(d.parentName || "?")} (${d.parent})</a>` : "") +
    row("Started", d.startTime ? new Date(d.startTime * 1000).toLocaleString() : "") +
    row("Running", fmtDur(d.runTime)) +
    row("Virtual mem", fmtBytes(d.virt));
  $("d-prio").value = d.priority || "";
  $("d-prio").disabled = !d.priority;
}

// ---------- actions ----------
function confirmBox(title, body, yes = "End") {
  return new Promise((resolve) => {
    $("modal-title").textContent = title;
    $("modal-body").textContent = body;
    $("modal-yes").textContent = yes;
    $("modal").hidden = false;
    $("modal-yes").focus();
    const done = (v) => { $("modal").hidden = true; $("modal-yes").onclick = $("modal-no").onclick = null; resolve(v); };
    $("modal-yes").onclick = () => done(true);
    $("modal-no").onclick = () => done(false);
    $("modal").onkeydown = (e) => { if (e.key === "Escape") done(false); };
  });
}

async function endSelected(tree) {
  const pids = selectedPids();
  if (!pids.length) return;
  const name = state.sel.group || state.details?.name || `PID ${pids[0]}`;
  const what = state.sel.group ? `all ${pids.length} "${name}" processes` : tree ? `${name} and all its child processes` : name;
  if (!(await confirmBox(`End ${tree ? "process tree" : "task"}?`, `This will end ${what}. Unsaved work will be lost.`))) return;
  try {
    const n = await invoke("kill", { pids, tree });
    toast(`Ended ${n} process${n === 1 ? "" : "es"}`);
    select(null);
    tick(true);
  } catch (e) { toast(e, true); }
}

let toastTimer;
function toast(msg, err = false) {
  const t = $("toast");
  t.textContent = String(msg);
  t.className = "toast" + (err ? " err" : "");
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 3200);
}

function setMode(m) {
  state.mode = m;
  document.querySelectorAll("#modes button").forEach((b) => b.classList.toggle("on", b.dataset.mode === m));
  if (state.sel?.group && m !== "group") select(null);
  renderTable();
}

// ---------- tabs ----------
function setTab(tab) {
  state.tab = tab;
  document.querySelectorAll("#tabs button").forEach((b) => b.classList.toggle("on", b.dataset.tab === tab));
  $("view-procs").hidden = tab !== "procs";
  $("view-startup").hidden = tab !== "startup";
  $("view-rules").hidden = tab !== "rules";
  $("modes").style.visibility = tab === "procs" ? "" : "hidden";
  $("search").parentElement.style.visibility = tab === "rules" ? "hidden" : "";
  $("search").placeholder = tab === "startup" ? "Filter startup apps" : "Filter by name or PID";
  if (tab === "procs") renderTable();
  if (tab === "startup") loadStartup();
  if (tab === "rules") renderRules();
}

// ---------- startup apps ----------
async function loadStartup() {
  try { state.startup = await invoke("startup_items"); }
  catch (e) { toast(e, true); state.startup = []; }
  renderStartup();
}

function renderStartup() {
  const items = (state.startup || []).filter((i) =>
    !state.filter || i.name.toLowerCase().includes(state.filter) || i.command.toLowerCase().includes(state.filter));
  $("startup-empty").hidden = items.length > 0 || state.startup == null;
  document.querySelector("#startup tbody").innerHTML = items.map((i) =>
    `<tr class="${i.enabled ? "" : "off"}">` +
      `<td class="col-toggle"><button class="switch${i.enabled ? " on" : ""}" data-idx="${state.startup.indexOf(i)}" title="${i.enabled ? "Disable" : "Enable"}"></button></td>` +
      `<td title="${esc(i.name)}">${esc(i.name)}</td>` +
      `<td class="cmd" title="${esc(i.command)}">${esc(i.command)}</td>` +
      `<td class="loc col-loc">${esc(i.location)}</td>` +
    `</tr>`).join("");
}

document.querySelector("#startup tbody").addEventListener("click", async (e) => {
  const sw = e.target.closest(".switch");
  if (!sw) return;
  const item = state.startup[+sw.dataset.idx];
  const enabled = !item.enabled;
  try {
    await invoke("set_startup", { scope: item.scope, approved: item.approved, name: item.name, enabled });
    item.enabled = enabled;
    toast(`${item.name} ${enabled ? "enabled" : "disabled"} at startup`);
    renderStartup();
  } catch (err) { toast(err, true); }
});

// ---------- rules (evaluated by the Rust sampler; this is just the editor) ----------
const LEGACY_RULES_KEY = "overseer.rules.v1"; // rules lived in localStorage before v0.3
const rules = { list: [], log: [], lastFired: {} };

async function loadRules() {
  try {
    const v = await invoke("get_rules");
    rules.list = v.rules; rules.log = v.log; rules.lastFired = v.lastFired;
  } catch (e) { toast(e, true); }
  updateRulesBadge();
}

async function migrateLegacyRules() {
  let legacy = null;
  try { legacy = JSON.parse(localStorage.getItem(LEGACY_RULES_KEY)); } catch {}
  if (!Array.isArray(legacy) || !legacy.length) return;
  await loadRules();
  if (!rules.list.length) await saveRules(legacy);
  try { localStorage.removeItem(LEGACY_RULES_KEY); } catch {}
}

async function saveRules(list) {
  try {
    await invoke("set_rules", { rules: list });
    rules.list = list;
  } catch (e) { toast(e, true); }
  updateRulesBadge();
  renderRules();
}

function updateRulesBadge() {
  const n = rules.list.filter((r) => r.enabled).length;
  $("rules-count").hidden = !n;
  $("rules-count").textContent = n;
}

const ACTION_TEXT = { notify: "notify me", lower: "lower its priority", kill: "end it" };
const metricText = (r) => (r.metric === "cpu" ? `${r.threshold}% CPU` : `${r.threshold} MB memory`);

function renderRules() {
  $("rules-list").innerHTML = rules.list.length
    ? rules.list.map((r) => {
        const last = rules.lastFired[r.id];
        return `<div class="rule${r.enabled ? "" : " off"}">` +
          `<button class="switch${r.enabled ? " on" : ""}" data-toggle="${r.id}" title="${r.enabled ? "Pause" : "Resume"} rule"></button>` +
          `<div class="rule-text">If <b>${esc(r.match)}</b> uses more than <b>${metricText(r)}</b> for <b>${r.duration}s</b>, ` +
          `<span class="act-${r.action}">${ACTION_TEXT[r.action]}</span></div>` +
          (last ? `<span class="rule-fired">last fired ${new Date(last).toLocaleTimeString()}</span>` : "") +
          `<button class="icon-btn" data-del="${r.id}" title="Delete rule">✕</button></div>`;
      }).join("")
    : `<div class="hint-line">No rules yet. Add one above.</div>`;
  $("rules-log").innerHTML = rules.log.length
    ? rules.log.map((l) => `<div class="log-row"><time>${new Date(l.time).toLocaleTimeString()}</time><span>${esc(l.text)}</span></div>`).join("")
    : `<div class="hint-line">Nothing has triggered yet.</div>`;
}

$("r-add").addEventListener("click", async () => {
  const match = $("r-match").value.trim();
  const threshold = +$("r-threshold").value;
  const duration = Math.round(+$("r-duration").value);
  const metric = $("r-metric").value, action = $("r-action").value;
  if (!match) { toast("Enter part of a process name", true); $("r-match").focus(); return; }
  if (!(threshold > 0) || !(duration > 0)) { toast("Threshold and duration must be positive", true); return; }
  if (action === "kill" && !(await confirmBox("Add an auto-end rule?",
      `Any process whose name contains "${match}" will be ended automatically, without asking, when the rule triggers. Unsaved work in it will be lost.`, "Add rule"))) return;
  await saveRules([...rules.list, { id: Date.now().toString(36), match, metric, threshold, duration, action, enabled: true }]);
  $("r-match").value = "";
  toast("Rule added");
});

$("rules-list").addEventListener("click", (e) => {
  const t = e.target.closest("[data-toggle]"), d = e.target.closest("[data-del]");
  if (t) saveRules(rules.list.map((x) => (x.id === t.dataset.toggle ? { ...x, enabled: !x.enabled } : x)));
  else if (d) saveRules(rules.list.filter((x) => x.id !== d.dataset.del));
});
$("r-clear-log").addEventListener("click", async () => {
  await invoke("clear_rule_log").catch((e) => toast(e, true));
  rules.log = [];
  renderRules();
});

// ---------- events ----------
$("tabs").addEventListener("click", (e) => { const t = e.target.closest("[data-tab]"); if (t) setTab(t.dataset.tab); });
document.querySelector("thead").addEventListener("click", (e) => {
  const k = e.target.closest("th")?.dataset.sort;
  if (!k) return;
  if (state.sortKey === k) state.sortDir *= -1;
  else { state.sortKey = k; state.sortDir = k === "name" || k === "pid" ? 1 : -1; }
  renderTable();
});

tbody.addEventListener("click", (e) => {
  const tw = e.target.closest("[data-twist]");
  if (tw) {
    const pid = +tw.dataset.twist;
    state.collapsed.has(pid) ? state.collapsed.delete(pid) : state.collapsed.add(pid);
    renderTable();
    return;
  }
  const tr = e.target.closest("tr");
  if (!tr) return;
  select(tr.dataset.group ? { group: tr.dataset.group } : { pid: +tr.dataset.pid });
});

$("d-props").addEventListener("click", (e) => {
  const a = e.target.closest("[data-goto]");
  if (a) select({ pid: +a.dataset.goto });
});

$("search").addEventListener("input", (e) => {
  state.filter = e.target.value.trim().toLowerCase();
  state.tab === "startup" ? renderStartup() : renderTable();
});
$("modes").addEventListener("click", (e) => { const m = e.target.dataset.mode; if (m) setMode(m); });
$("pause").addEventListener("click", togglePause);
$("d-close").addEventListener("click", () => select(null));
$("d-kill").addEventListener("click", () => endSelected(false));
$("d-killtree").addEventListener("click", () => endSelected(true));
$("d-open").addEventListener("click", () => invoke("open_location", { pid: state.sel.pid }).catch((e) => toast(e, true)));
$("d-prio").addEventListener("change", async (e) => {
  const level = e.target.value;
  if (level === "realtime" && !(await confirmBox("Set realtime priority?", "Realtime can starve the rest of the system, including your mouse and keyboard.", "Set realtime"))) {
    e.target.value = state.details?.priority || "normal";
    return;
  }
  try { await invoke("set_priority", { pid: state.sel.pid, level }); toast("Priority updated"); state.details.priority = level; }
  catch (err) { toast(err, true); e.target.value = state.details?.priority || "normal"; }
});
$("admin").addEventListener("click", async () => {
  if (state.info?.admin) return;
  if (await confirmBox("Restart as administrator?", "Admin rights let Overseer see and control system processes and services.", "Restart"))
    invoke("restart_as_admin").catch((e) => toast(e, true));
});

function togglePause() {
  state.paused = !state.paused;
  $("pause").classList.toggle("on", state.paused);
  $("pause").textContent = state.paused ? "▶" : "❚❚";
  toast(state.paused ? "Paused" : "Resumed");
}

document.addEventListener("keydown", (e) => {
  if (!$("modal").hidden) return;
  if (e.ctrlKey && ["1", "2", "3"].includes(e.key)) { e.preventDefault(); setTab(["procs", "startup", "rules"][+e.key - 1]); return; }
  if (e.target.matches("input:not(#search), select")) return; // typing in the rules form
  const typing = e.target === $("search");
  if ((e.key === "/" && !typing) || (e.key === "f" && e.ctrlKey)) { e.preventDefault(); $("search").focus(); $("search").select(); return; }
  if (e.key === "Escape") {
    if (typing && state.filter) { $("search").value = ""; state.filter = ""; state.tab === "startup" ? renderStartup() : renderTable(); }
    else if (typing) $("search").blur();
    else select(null);
    return;
  }
  if (typing) return;
  if (e.key === " ") { e.preventDefault(); togglePause(); }
  else if (state.tab !== "procs") return;
  else if (e.key === "Delete") endSelected(e.shiftKey);
  else if (e.key === "1") setMode("list");
  else if (e.key === "2") setMode("tree");
  else if (e.key === "3") setMode("group");
  else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const rows = [...tbody.rows];
    const i = rows.findIndex((r) => r.classList.contains("sel"));
    const next = rows[Math.max(0, Math.min(rows.length - 1, i + (e.key === "ArrowDown" ? 1 : -1)))];
    if (next) { next.click(); next.scrollIntoView({ block: "nearest" }); }
  }
});

// ---------- loop ----------
let ticking = false;
let tickCount = 0;
async function tick(force = false) {
  if (ticking || (state.paused && !force)) return;
  ticking = true;
  try {
    const s = await invoke("snapshot");
    if (!s) return; // sampler hasn't produced its first sample yet
    for (const p of s.procs) p.net = p.netRx + p.netTx;
    state.snap = s;
    renderMetrics(s);
    renderBandwidthStatus(s.bandwidth);
    if (state.tab === "procs") {
      renderTable();
      renderPanel();
      if (++tickCount % 3 === 0) refreshConns();
    } else if (state.tab === "rules") {
      await loadRules();
      renderRules();
    }
  } catch (e) {
    toast(e, true);
  } finally {
    ticking = false;
  }
}

(async function init() {
  try {
    const info = (state.info = await invoke("system_info"));
    $("sysline").textContent = `${info.host} · ${info.cpuBrand} · ${info.cores} threads`;
    $("sysline").title = info.os;
    const b = $("admin");
    b.textContent = info.admin ? "● Admin" : "● Limited, click to elevate";
    b.className = "badge " + (info.admin ? "admin" : "limited");
    $("s-hotkey").textContent = `${info.hotkey} shows/hides Overseer`;
  } catch (e) { toast(e, true); }
  await migrateLegacyRules();
  await loadRules();
  await tick(true);
  setInterval(tick, 1000);
})();

function renderBandwidthStatus(on) {
  const el = $("s-bw");
  el.textContent = on ? "Per-process bandwidth: on" : "Per-process bandwidth: needs admin";
  el.className = on ? "" : "off";
  el.title = on ? "" : "Click to restart as administrator";
  $("th-net").title = on ? "Network bandwidth (download + upload)" : "Restart as admin to see per-process bandwidth";
}
$("s-bw").addEventListener("click", () => { if (!state.snap?.bandwidth) $("admin").click(); });

// ---------- mock backend for previewing the UI in a normal browser ----------
function mockInvoke(cmd, args) {
  const m = (mockInvoke.s ??= (() => {
    const names = ["System", "Registry", "smss.exe", "csrss.exe", "wininit.exe", "services.exe", "lsass.exe", "svchost.exe", "explorer.exe", "chrome.exe", "Code.exe", "Discord.exe", "Spotify.exe", "steam.exe", "dwm.exe", "RuntimeBroker.exe", "SearchHost.exe", "Teams.exe", "node.exe", "cargo.exe", "MsMpEng.exe", "audiodg.exe"];
    const procs = [{ pid: 4, parent: null, name: "System" }];
    let pid = 100;
    const add = (name, parent, n = 1) => { const out = []; for (let i = 0; i < n; i++) { procs.push({ pid: (pid += 4 + Math.floor(Math.random() * 40)), parent, name }); out.push(pid); } return out; };
    const [wininit] = add("wininit.exe", null); const [services] = add("services.exe", wininit);
    add("lsass.exe", wininit); add("svchost.exe", services, 24); add("MsMpEng.exe", services);
    const [explorer] = add("explorer.exe", null);
    const [chrome] = add("chrome.exe", explorer); add("chrome.exe", chrome, 18);
    const [code] = add("Code.exe", explorer); add("Code.exe", code, 7); const [node] = add("node.exe", code); add("cargo.exe", node);
    for (const n of names.slice(10)) if (!procs.some((p) => p.name === n)) add(n, Math.random() > 0.5 ? explorer : null);
    procs.forEach((p) => {
      p.base = Math.random() ** 3 * 8; p.memBase = Math.random() ** 2 * 6e8 + 4e6;
      p.conns = /chrome|Discord|Spotify|Teams|steam/.test(p.name) ? Math.floor(Math.random() * 30) : 0;
      p.listening = p.name === "svchost.exe" && Math.random() > 0.7 ? 1 : 0;
    });
    return { procs, t: 0 };
  })());
  m.t++;
  const wave = (k) => Math.max(0, Math.sin(m.t / 6 + k) * 0.5 + Math.random() * 0.8);
  if (cmd === "system_info") return Promise.resolve({ host: "PREVIEW-PC", os: "Windows 11 (mock data)", cpuBrand: "Mock CPU 16-core", cores: 16, admin: false, hotkey: "Ctrl+Alt+O" });
  if (cmd === "get_rules") return Promise.resolve({ rules: m.rules ?? [], log: [{ time: Date.now(), text: "Alert: chrome.exe (1234) at 62% CPU for 10s+ (mock)" }], lastFired: {} });
  if (cmd === "set_rules") { m.rules = args.rules; return Promise.resolve(); }
  if (cmd === "clear_rule_log") return Promise.resolve();
  if (cmd === "snapshot") {
    const procs = m.procs.map((p, i) => ({ pid: p.pid, parent: p.parent, name: p.name, conns: p.conns, listening: p.listening,
      netRx: p.conns ? Math.random() * p.conns * 4e4 : 0, netTx: p.conns ? Math.random() * p.conns * 6e3 : 0, cpu: p.base * wave(i), mem: p.memBase * (0.9 + Math.random() * 0.2), diskRead: Math.random() > 0.85 ? Math.random() * 3e6 : 0, diskWrite: Math.random() > 0.9 ? Math.random() * 1e6 : 0 }));
    const cpu = Math.min(100, procs.reduce((a, p) => a + p.cpu, 0));
    return Promise.resolve({
      cpu, cores: Array.from({ length: 16 }, () => Math.min(100, cpu * (0.4 + Math.random() * 1.2))),
      memUsed: procs.reduce((a, p) => a + p.mem, 0) + 4e9, memTotal: 32 * 1024 ** 3, swapUsed: 1.2e9, swapTotal: 8e9,
      netRx: Math.random() * 4e6 * wave(1), netTx: Math.random() * 6e5, diskRead: procs.reduce((a, p) => a + p.diskRead, 0), diskWrite: procs.reduce((a, p) => a + p.diskWrite, 0),
      uptime: 3600 * 27 + m.t, bandwidth: true, procs,
    });
  }
  if (cmd === "details") {
    const p = m.procs.find((x) => x.pid === args.pid); const par = m.procs.find((x) => x.pid === p?.parent);
    return Promise.resolve({ pid: p.pid, name: p.name, exe: `C:\\Program Files\\${p.name.replace(".exe", "")}\\${p.name}`, cmd: `"${p.name}" --type=renderer`, cwd: "C:\\Users\\you", parent: par?.pid ?? null, parentName: par?.name, status: "Run", startTime: Date.now() / 1000 - 5000, runTime: 5000, virt: 2.1e9, priority: "normal" });
  }
  if (cmd === "kill") return Promise.resolve(args.pids.length);
  if (cmd === "set_priority" || cmd === "set_startup") return Promise.resolve();
  if (cmd === "connections") {
    const n = m.procs.find((x) => x.pid === args.pid)?.conns || 0;
    return Promise.resolve([
      ...Array.from({ length: n }, (_, i) => ({ pid: args.pid, proto: "TCP", local: `192.168.1.20:${50000 + i}`, remote: `142.250.${i}.14:443`, state: "Established" })),
      { pid: args.pid, proto: "UDP", local: "0.0.0.0:5353", remote: "", state: "" },
    ]);
  }
  if (cmd === "startup_items") {
    const mk = (name, command, location, scope, approved, enabled = true) => ({ name, command, location, scope, approved, enabled });
    return Promise.resolve([
      mk("Discord", "C:\\Users\\you\\AppData\\Local\\Discord\\Update.exe --processStart Discord.exe", "Registry · current user", "user", "Run"),
      mk("Spotify", "C:\\Users\\you\\AppData\\Roaming\\Spotify\\Spotify.exe /minimized", "Registry · current user", "user", "Run", false),
      mk("SecurityHealth", "%windir%\\system32\\SecurityHealthSystray.exe", "Registry · all users", "machine", "Run"),
      mk("Steam", "\"C:\\Program Files (x86)\\Steam\\steam.exe\" -silent", "Registry · all users (32-bit)", "machine", "Run32"),
      mk("OneNote.lnk", "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Startup\\OneNote.lnk", "Startup folder · current user", "user", "StartupFolder"),
    ]);
  }
  return Promise.reject(`"${cmd}" isn't available in preview mode`);
}
