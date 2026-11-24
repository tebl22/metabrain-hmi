"use strict";

// Minimalist read-only mobile/LAN monitor for MetaBrain v1.4.
// Uses DOM textContent/createElement exclusively (zero innerHTML sinks)
// and pauses polling automatically while the tab is hidden.

const $ = (id) => document.getElementById(id);
let timer = null;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function showLogin(msg) {
  $("dash-view").classList.add("hidden");
  $("login-view").classList.remove("hidden");
  const err = $("login-err");
  if (msg) {
    err.textContent = msg;
    err.classList.remove("hidden");
  } else {
    err.classList.add("hidden");
  }
}

function showDash() {
  $("login-view").classList.add("hidden");
  $("dash-view").classList.remove("hidden");
}

function maxQuotaPct(brain) {
  const limits = Array.isArray(brain.quota) ? brain.quota : [];
  let maxPct = null;
  for (const l of limits) {
    if (typeof l.pct === "number" && Number.isFinite(l.pct)) {
      maxPct = maxPct === null ? l.pct : Math.max(maxPct, l.pct);
    }
  }
  return maxPct;
}

function renderState(snap, tasks) {
  const overall = String(snap.overall || "OK").toUpperCase();
  const dot = $("overall-dot");
  dot.className = "dot" + (overall === "ALARM" ? " alarm" : overall === "WARNING" ? " warn" : "");
  $("version-badge").textContent = "v" + String(snap.version || "1.4.0");
  $("mode-pill").textContent = "Mode: " + String(snap.mode || "normal").toUpperCase();

  const mach = snap.machine || {};
  const cpu = typeof mach.cpu === "number" ? Math.round(mach.cpu) + "%" : "—";
  const ram = typeof mach.ram === "number" ? Math.round(mach.ram) + "%" : "—";
  $("host-stats").textContent = "CPU " + cpu + " · RAM " + ram;
  $("updated-at").textContent = new Date().toLocaleTimeString();

  // Brains
  const brainsBox = $("brains-list");
  clear(brainsBox);
  const chainMap = new Map();
  for (const c of Array.isArray(snap.chain) ? snap.chain : []) {
    chainMap.set(c.id, c);
  }
  const brains = Array.isArray(snap.brains) ? snap.brains : [];
  if (brains.length === 0) {
    brainsBox.appendChild(el("div", "muted", "No brains configured."));
  } else {
    for (const b of brains) {
      const ch = chainMap.get(b.id) || {};
      const card = el("div", "row-card");
      const top = el("div", "row-top");
      top.appendChild(el("span", "row-title", b.name || b.id));

      const statusText = ch.master
        ? "In command"
        : b.active
        ? "Active"
        : ch.available === false
        ? ch.reason || "Unavailable"
        : "Standby";
      const tagClass = ch.master
        ? "tag master"
        : b.active
        ? "tag ok"
        : ch.available === false
        ? "tag warn"
        : "tag";
      top.appendChild(el("span", tagClass, statusText));
      card.appendChild(top);

      const eff = b.effective || {};
      const model = eff.model || b.model || b.kind || "";
      const pct = maxQuotaPct(b);
      const subParts = [];
      if (model) subParts.push(model);
      if (pct !== null) subParts.push("Quota " + Math.round(pct) + "%");
      if (b.pilot && b.pilot !== "off") subParts.push("Pilot: " + b.pilot);
      card.appendChild(el("div", "row-sub", subParts.join(" · ") || "Ready"));

      if (pct !== null) {
        const wrap = el("div", "bar-wrap");
        const fill = el(
          "div",
          "bar-fill" + (pct >= 90 ? " alarm" : pct >= 75 ? " warn" : "")
        );
        fill.style.width = Math.max(0, Math.min(100, pct)) + "%";
        wrap.appendChild(fill);
        card.appendChild(wrap);
      }
      brainsBox.appendChild(card);
    }
  }

  // Active tasks queue
  const tasksBox = $("tasks-list");
  clear(tasksBox);
  const taskArr = Array.isArray(tasks) ? tasks : [];
  const activeStatuses = new Set(["running", "queued", "verifying", "planning", "synthesizing", "starting"]);
  const active = taskArr.filter((t) => activeStatuses.has(String(t.status || "")));
  const recent = active.length > 0 ? active : taskArr.slice(0, 6);
  $("queue-count").textContent = active.length + " active";

  if (recent.length === 0) {
    tasksBox.appendChild(el("div", "muted", "No recent tasks."));
  } else {
    for (const t of recent.slice(0, 10)) {
      const card = el("div", "row-card");
      const top = el("div", "row-top");
      const title = String(t.prompt || t.id || "Task").slice(0, 72);
      top.appendChild(el("span", "row-title", title));
      const st = String(t.status || "unknown");
      const stClass =
        st === "running" || st === "verifying"
          ? "tag ok"
          : st === "failed"
          ? "tag alarm"
          : st === "queued"
          ? "tag warn"
          : "tag";
      top.appendChild(el("span", stClass, st));
      card.appendChild(top);
      const brainName = t.brain_name || t.brain || "auto";
      const toks = typeof t.tokens === "number" && t.tokens > 0 ? " · " + t.tokens.toLocaleString() + " tok" : "";
      card.appendChild(el("div", "row-sub", brainName + toks));
      tasksBox.appendChild(card);
    }
  }

  // Alarms & diary
  const alarmsBox = $("alarms-list");
  clear(alarmsBox);
  const alarms = Array.isArray(snap.alarms) ? snap.alarms : [];
  $("alarm-count").textContent = alarms.length + (alarms.length === 1 ? " alarm" : " alarms");
  if (alarms.length === 0) {
    alarmsBox.appendChild(el("div", "muted", "No active alarms."));
  } else {
    for (const a of alarms.slice(0, 8)) {
      const card = el("div", "row-card");
      const top = el("div", "row-top");
      top.appendChild(el("span", "row-title", a.brain || "MetaBrain"));
      const lvl = String(a.level || "warning");
      top.appendChild(el("span", lvl === "alarm" ? "tag alarm" : "tag warn", lvl));
      card.appendChild(top);
      card.appendChild(el("div", "row-sub", a.text || ""));
      alarmsBox.appendChild(card);
    }
  }

  const diaryBox = $("diary-list");
  clear(diaryBox);
  const diary = Array.isArray(snap.diary) ? snap.diary.slice(-8).reverse() : [];
  for (const d of diary) {
    const line = (d.who ? d.who + ": " : "") + (d.text || "");
    diaryBox.appendChild(el("div", "diary-line", line));
  }
}

async function pollOnce() {
  if (document.visibilityState === "hidden") return;
  try {
    const [stRes, tkRes] = await Promise.all([
      fetch("/api/state", { cache: "no-store" }),
      fetch("/api/tasks", { cache: "no-store" }),
    ]);
    if (stRes.status === 401 || tkRes.status === 401) {
      showLogin("");
      return;
    }
    if (!stRes.ok) return;
    const snap = await stRes.json();
    const tasks = tkRes.ok ? await tkRes.json() : [];
    showDash();
    renderState(snap, tasks);
  } catch {
    $("updated-at").textContent = "Reconnecting…";
  }
}

function startPolling() {
  if (timer) clearInterval(timer);
  pollOnce();
  timer = setInterval(pollOnce, 4000);
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const pin = $("pin-input").value.trim();
  if (!pin) return;
  try {
    const res = await fetch("/api/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ pin }),
    });
    const body = await res.json().catch(() => ({}));
    if (res.ok && body.ok) {
      $("pin-input").value = "";
      showDash();
      startPolling();
    } else {
      showLogin(body.error || "Invalid PIN");
    }
  } catch {
    showLogin("Connection failed");
  }
});

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") pollOnce();
});

startPolling();
