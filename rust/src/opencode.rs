//! OpenCode brain adapter (local HTTP server supervision).
//!
//! `opencode serve` exposes a small REST API. We poll `GET /session` and
//! `GET /session/{id}/message` every 3 s at most, keep the last good picture in
//! memory (a network error only sets a warning) and expose it as a snapshot
//! with the same shape the Claude Code brains produce in `brains.rs`.
//! Every parse is tolerant: renamed / missing fields are simply ignored.
use crate::util::{http_json, log, secret, J};
use serde_json::{json, Map, Value};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

/// Minimum seconds between two polls of the OpenCode server.
const POLL_EVERY: f64 = 3.0;
/// Only sessions updated in this window are fetched (8 h).
const KEEP_SECS: f64 = 8.0 * 3600.0;
/// A message without `completed` younger than this is live.
const ACTIVE_SECS: f64 = 120.0;
/// A subagent is shown alive for this long after its session was touched.
const ALIVE_SECS: f64 = 90.0;
/// Window used to light up the lobes.
const LOBE_SECS: f64 = 25.0;
const HTTP_TIMEOUT: u64 = 5;
/// Sessions polled per round (most recently updated first).
const MAX_FETCH: usize = 24;
/// `/session/{id}/children` probes per round when the listing has no parentID.
const MAX_CHILD_FETCH: usize = 6;
/// Cap on the kept message records.
const MSG_KEEP: usize = 4000;
const DEFAULT_BASE: &str = "http://127.0.0.1:4096";

// =====================================================================================
// State
// =====================================================================================

#[derive(Clone, Debug, Default)]
struct Sess {
    id: String,
    parent: String,
    title: String,
    project: String,
    agent: String,
    created: f64,
    updated: f64,
}

#[derive(Clone, Debug)]
struct Msg {
    /// Unique key: "<session id>:<message id>" (falls back to the timestamp).
    key: String,
    sid: String,
    ts: f64,
    /// 0.0 while the message has no `completed` time.
    done: f64,
    model: String,
    /// [input, output, cache_read, cache_write]
    tokens: [i64; 4],
    reasoning: i64,
    usd: f64,
    status: String,
}

impl Msg {
    fn active(&self, now_ts: f64) -> bool {
        self.done == 0.0 && self.ts != 0.0 && now_ts - self.ts < ACTIVE_SECS
    }
    fn total(&self) -> i64 {
        self.tokens[0] + self.tokens[1] + self.reasoning + self.tokens[3]
    }
}

/// Per-session roll-up computed from the messages.
#[derive(Default)]
struct Agg {
    tokens: i64,
    cost: f64,
    last: f64,
    model: String,
    status: String,
    active: bool,
}

#[derive(Default)]
struct OcInner {
    last_poll: f64,
    ready: bool,
    warning: String,
    sessions: HashMap<String, Sess>,
    msgs: HashMap<String, Msg>,
    /// parent id -> children discovered through `/session/{id}/children`
    children: HashMap<String, Vec<String>>,
}

pub struct OpenCode {
    inner: Mutex<OcInner>,
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCode {
    pub fn new() -> OpenCode {
        OpenCode {
            inner: Mutex::new(OcInner::default()),
        }
    }

    /// Poison-tolerant lock: a panic elsewhere never takes the adapter down.
    fn lock(&self) -> MutexGuard<'_, OcInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn warn(&self, msg: String) {
        let mut i = self.lock();
        if i.warning != msg {
            log(&format!("opencode: {msg}"));
        }
        i.warning = msg;
    }

    // ------------------------------------------------------------------ perception
    /// Read the server (>= 3 s apart). Network errors keep the last good picture.
    pub fn poll(&self, cfg: &Value, now_ts: f64) {
        {
            let mut i = self.lock();
            if now_ts - i.last_poll < POLL_EVERY {
                return;
            }
            i.last_poll = now_ts;
        }
        let base = base_url(cfg);
        let auth = basic_auth(cfg);
        let listing = match req_json(&format!("{base}/session"), &auth, None) {
            Ok(v) => v,
            Err(_) => {
                self.warn(format!("OpenCode no responde en {base}"));
                return;
            }
        };

        // Sessions touched inside the window, newest first.
        let mut found: Vec<Sess> = Vec::new();
        for raw in obj_list(&listing) {
            let Some(id) = session_id(&raw) else { continue };
            let (created, updated) = session_times(&raw);
            let last = if updated != 0.0 { updated } else { created };
            if last != 0.0 && now_ts - last > KEEP_SECS {
                continue;
            }
            found.push(Sess {
                id,
                parent: parent_id(&raw),
                title: session_title(&raw),
                project: session_project(&raw),
                agent: session_agent(&raw),
                created,
                updated: last,
            });
        }
        found.sort_by(|a, b| {
            b.updated
                .partial_cmp(&a.updated)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        found.truncate(MAX_FETCH);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        // Subagents: explicit parentID when the server sends it, else /children.
        let mut parents: HashMap<String, String> = HashMap::new();
        let mut kids: HashMap<String, Vec<String>> = HashMap::new();
        if found.iter().any(|s| !s.parent.is_empty()) {
            for s in &found {
                if !s.parent.is_empty() {
                    kids.entry(s.parent.clone()).or_default().push(s.id.clone());
                    parents.insert(s.id.clone(), s.parent.clone());
                }
            }
        } else {
            for s in found.iter().take(MAX_CHILD_FETCH) {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                let Ok(v) = req_json(&format!("{base}/session/{}/children", s.id), &auth, None)
                else {
                    continue;
                };
                let list: Vec<String> = obj_list(&v).iter().filter_map(session_id).collect();
                if list.is_empty() {
                    continue;
                }
                for k in &list {
                    parents.insert(k.clone(), s.id.clone());
                }
                kids.entry(s.id.clone()).or_default().extend(list);
            }
        }

        // Messages of every session in the window (bounded by time budget so poll_fast never stalls).
        let mut msgs: HashMap<String, Msg> = HashMap::new();
        let mut failed = 0usize;
        for s in &found {
            if std::time::Instant::now() >= deadline {
                break;
            }
            match req_json(&format!("{base}/session/{}/message", s.id), &auth, None) {
                Ok(v) => {
                    for raw in obj_list(&v) {
                        if let Some(m) = parse_msg(&s.id, &raw) {
                            msgs.insert(m.key.clone(), m);
                        }
                    }
                }
                Err(_) => failed += 1,
            }
        }
        let got_msgs = msgs.len();

        // ---- merge (the UI must never see a half-written picture) ----
        let mut i = self.lock();
        for s in found {
            let Sess {
                id,
                parent,
                title,
                project,
                agent,
                created,
                updated,
            } = s;
            let p = if parent.is_empty() {
                parents.get(&id).cloned().unwrap_or_default()
            } else {
                parent
            };
            let e = i.sessions.entry(id.clone()).or_insert_with(|| Sess {
                id: id.clone(),
                ..Default::default()
            });
            if !title.is_empty() {
                e.title = title;
            }
            if !project.is_empty() {
                e.project = project;
            }
            if !agent.is_empty() {
                e.agent = agent;
            }
            if !p.is_empty() {
                e.parent = p;
            }
            if created != 0.0 && (e.created == 0.0 || created < e.created) {
                e.created = created;
            }
            if updated > e.updated {
                e.updated = updated;
            }
        }
        for (p, list) in kids {
            let e = i.children.entry(p).or_default();
            for k in list {
                if !e.contains(&k) {
                    e.push(k);
                }
            }
        }
        for (k, m) in msgs {
            match i.msgs.entry(k) {
                Entry::Occupied(mut o) => merge_msg(o.get_mut(), m),
                Entry::Vacant(v) => {
                    v.insert(m);
                }
            }
        }

        // ---- prune ----
        let cutoff = now_ts - KEEP_SECS;
        i.sessions
            .retain(|_, s| s.updated == 0.0 || s.updated >= cutoff);
        let live: HashSet<String> = i.sessions.keys().cloned().collect();
        i.msgs
            .retain(|_, m| live.contains(&m.sid) && (m.ts == 0.0 || m.ts >= cutoff));
        i.children.retain(|parent, list| {
            list.retain(|c| live.contains(c));
            !list.is_empty() && live.contains(parent)
        });
        if i.msgs.len() > MSG_KEEP {
            let excess = i.msgs.len() - MSG_KEEP;
            let mut by_ts: Vec<(f64, String)> =
                i.msgs.iter().map(|(k, m)| (m.ts, k.clone())).collect();
            by_ts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            for (_, k) in by_ts.into_iter().take(excess) {
                i.msgs.remove(&k);
            }
        }

        i.ready = true;
        if failed > 0 && got_msgs == 0 {
            let msg = format!("OpenCode no responde en {base}");
            if i.warning != msg {
                log(&format!("opencode: {msg}"));
            }
            i.warning = msg;
        } else {
            i.warning.clear();
        }
    }

    /// Ledger rows: (unique key, ts, model, [input, output, cache_read, cache_write], usd).
    pub fn usages(&self) -> Vec<(String, f64, String, [i64; 4], f64)> {
        let i = self.lock();
        let mut out: Vec<(String, f64, String, [i64; 4], f64)> = i
            .msgs
            .values()
            .filter(|m| m.tokens.iter().any(|t| *t != 0) || m.usd != 0.0)
            .map(|m| (m.key.clone(), m.ts, m.model.clone(), m.tokens, m.usd))
            .collect();
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    // ------------------------------------------------------------------ view
    /// Same shape as `Brain::snapshot_claude`.
    pub fn snapshot(&self, now_ts: f64) -> Value {
        let i = self.lock();

        // Per-session roll-up.
        let mut agg: HashMap<String, Agg> = HashMap::new();
        for m in i.msgs.values() {
            let a = agg.entry(m.sid.clone()).or_default();
            a.tokens += m.total();
            a.cost += m.usd;
            if m.ts >= a.last {
                a.last = m.ts;
                if !m.model.is_empty() {
                    a.model = m.model.clone();
                }
                a.status = m.status.clone();
            }
            if m.active(now_ts) {
                a.active = true;
            }
        }

        // Children (subagents), from parentID and from the /children probes.
        let mut kids: HashMap<String, Vec<String>> = HashMap::new();
        for s in i.sessions.values() {
            if !s.parent.is_empty() {
                kids.entry(s.parent.clone()).or_default().push(s.id.clone());
            }
        }
        for (p, list) in &i.children {
            let e = kids.entry(p.clone()).or_default();
            for k in list {
                if !e.contains(k) {
                    e.push(k.clone());
                }
            }
        }
        let child_ids: HashSet<String> = kids.values().flatten().cloned().collect();

        let live = i
            .msgs
            .values()
            .filter(|m| m.active(now_ts))
            .max_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let active = live.is_some();
        let status = match live {
            Some(m) if !m.status.is_empty() => m.status.clone(),
            Some(_) => "working".to_string(),
            None if i.sessions.is_empty() => "no sessions".to_string(),
            None => "idle".to_string(),
        };

        // Lobes: plan = message in flight, act = message just finished,
        // delegate = a subagent is working (or was just touched).
        let mut plan = false;
        let mut act = false;
        let mut delegate = false;
        for m in i.msgs.values() {
            if m.ts == 0.0 || now_ts - m.ts > LOBE_SECS {
                continue;
            }
            if m.done == 0.0 {
                plan = true;
            } else {
                act = true;
            }
            if child_ids.contains(&m.sid) {
                delegate = true;
            }
        }
        if !delegate {
            delegate = i.sessions.values().any(|s| {
                child_ids.contains(&s.id) && s.updated != 0.0 && now_ts - s.updated < LOBE_SECS
            });
        }
        let mut lobes = Map::new();
        lobes.insert("plan".to_string(), json!(plan));
        lobes.insert("act".to_string(), json!(act));
        lobes.insert("delegate".to_string(), json!(delegate));

        // Subagents.
        let mut subs: Vec<Value> = Vec::new();
        for list in kids.values() {
            for id in list {
                let Some(s) = i.sessions.get(id) else {
                    continue;
                };
                let a = agg.get(id);
                let last = last_of(s, &agg);
                if last != 0.0 && now_ts - last > KEEP_SECS {
                    continue;
                }
                let is_alive = alive(a, Some(s), now_ts);
                let desc = if s.title.is_empty() {
                    short(id)
                } else {
                    s.title.clone()
                };
                let typ = if s.agent.is_empty() {
                    "subagent".to_string()
                } else {
                    s.agent.clone()
                };
                subs.push(json!({
                    "id": short(id),
                    "desc": desc,
                    "type": typ,
                    "model": a.map(|a| a.model.clone()).unwrap_or_default(),
                    "tokens": a.map(|a| a.tokens).unwrap_or(0),
                    "alive": is_alive,
                    "status": if is_alive { a.map(|a| a.status.clone()).unwrap_or_default() } else { String::new() },
                    "last": last,
                }));
            }
        }
        subs.sort_by(|a, b| {
            let ka = (!a.b("alive", false), -a.f("last").unwrap_or(0.0));
            let kb = (!b.b("alive", false), -b.f("last").unwrap_or(0.0));
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        });
        subs.truncate(16);

        // Sessions.
        let mut order: Vec<&Sess> = i.sessions.values().collect();
        order.sort_by(|a, b| {
            let la = last_of(a, &agg);
            let lb = last_of(b, &agg);
            lb.partial_cmp(&la).unwrap_or(std::cmp::Ordering::Equal)
        });
        let top = order.first().copied();
        let sessions: Vec<Value> = order
            .iter()
            .filter(|s| {
                let l = last_of(s, &agg);
                l == 0.0 || now_ts - l < KEEP_SECS
            })
            .take(12)
            .map(|s| {
                let a = agg.get(&s.id);
                let is_active = a.map(|a| a.active).unwrap_or(false);
                let subs_alive = kids
                    .get(&s.id)
                    .map(|l| l.iter().filter(|c| alive(agg.get(*c), i.sessions.get(*c), now_ts)).count())
                    .unwrap_or(0);
                json!({
                    "id": short(&s.id),
                    "project": s.project.clone(),
                    "title": s.title.clone(),
                    "model": a.map(|a| a.model.clone()).unwrap_or_default(),
                    "effort": "",
                    "status": if is_active { a.map(|a| a.status.clone()).unwrap_or_default() } else { String::new() },
                    "tokens": a.map(|a| a.tokens).unwrap_or(0),
                    "cost": a.map(|a| a.cost).unwrap_or(0.0),
                    "last": last_of(s, &agg),
                    "active": is_active,
                    "subs_alive": subs_alive,
                })
            })
            .collect();

        let model = top
            .and_then(|s| agg.get(&s.id))
            .map(|a| a.model.clone())
            .unwrap_or_default();
        let project = top.map(|s| s.project.clone()).unwrap_or_default();
        let last = i
            .sessions
            .values()
            .map(|s| last_of(s, &agg))
            .fold(0.0f64, f64::max);

        json!({
            "active": active,
            "status": status,
            "lobes": Value::Object(lobes),
            "lobe_names": ["plan", "act", "delegate"],
            "model": model,
            "effort": "",
            "project": project,
            "subagents": subs,
            "jobs": [],
            "sessions": sessions,
            "quota": [],
            "balance": Value::Null,
            "warning": i.warning.clone(),
            "ready": i.ready,
            "last": if last != 0.0 { json!(last) } else { Value::Null },
        })
    }

    // ------------------------------------------------------------------ control
    /// POST /session/{id}/abort. A short (8 chars) id is resolved against the known sessions.
    pub fn abort(&self, cfg: &Value, session: &str) -> Result<String, String> {
        let wanted = session.trim();
        if wanted.is_empty() {
            return Err("no session id".to_string());
        }
        let full = {
            let i = self.lock();
            if i.sessions.contains_key(wanted) {
                wanted.to_string()
            } else {
                i.sessions
                    .keys()
                    .find(|k| k.starts_with(wanted))
                    .cloned()
                    .unwrap_or_else(|| wanted.to_string())
            }
        };
        let base = base_url(cfg);
        let auth = basic_auth(cfg);
        let url = format!("{base}/session/{full}/abort");
        match req_json(&url, &auth, Some(&json!({}))) {
            Ok(_) => {
                log(&format!("opencode: aborted {full}"));
                Ok(format!("aborted {full}"))
            }
            Err(e) => Err(format!("OpenCode no responde en {base} ({e})")),
        }
    }
}

// =====================================================================================
// Helpers
// =====================================================================================

fn last_of(s: &Sess, agg: &HashMap<String, Agg>) -> f64 {
    agg.get(&s.id).map(|a| a.last).unwrap_or(0.0).max(s.updated)
}

fn alive(a: Option<&Agg>, s: Option<&Sess>, now_ts: f64) -> bool {
    if a.map(|a| a.active).unwrap_or(false) {
        return true;
    }
    match s {
        Some(s) => s.updated != 0.0 && now_ts - s.updated < ALIVE_SECS,
        None => false,
    }
}

fn merge_msg(old: &mut Msg, new: Msg) {
    if new.done != 0.0 {
        old.done = new.done;
    }
    if new.ts != 0.0 {
        old.ts = new.ts;
    }
    for (ix, v) in new.tokens.iter().enumerate() {
        if *v > old.tokens[ix] {
            old.tokens[ix] = *v;
        }
    }
    if new.reasoning > old.reasoning {
        old.reasoning = new.reasoning;
    }
    if new.usd > old.usd {
        old.usd = new.usd;
    }
    if !new.model.is_empty() {
        old.model = new.model;
    }
    if !new.status.is_empty() {
        old.status = new.status;
    }
}

fn base_url(cfg: &Value) -> String {
    let b = cfg
        .os("base_url")
        .unwrap_or_else(|| DEFAULT_BASE.to_string());
    let b = b.trim_end_matches('/').to_string();
    if b.is_empty() {
        DEFAULT_BASE.to_string()
    } else {
        b
    }
}

/// HTTP Basic credentials, only when `password_env` resolves to a secret.
fn basic_auth(cfg: &Value) -> Option<String> {
    let env = cfg.s("password_env");
    let pass = secret(&env)?;
    let user = cfg.os("username").unwrap_or_else(|| "opencode".to_string());
    Some(format!(
        "Basic {}",
        b64(format!("{user}:{pass}").as_bytes())
    ))
}

fn req_json(url: &str, auth: &Option<String>, body: Option<&Value>) -> Result<Value, String> {
    let mut hv: Vec<(&str, &str)> = vec![("Accept", "application/json")];
    if let Some(a) = auth.as_deref() {
        hv.push(("Authorization", a));
    }
    http_json(url, &hv, body, HTTP_TIMEOUT)
}

/// Minimal RFC 4648 base64 (no extra crates available).
fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = (chunk[0] as u32) << 16 | (b1 as u32) << 8 | b2 as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn pretty(p: &str) -> String {
    let t = p.trim_end_matches(['\\', '/']);
    t.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

// ---------- tolerant JSON accessors ----------

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn get_num(v: &Value, keys: &[&str]) -> Option<f64> {
    for k in keys {
        if let Some(x) = v.get(*k).and_then(num) {
            return Some(x);
        }
    }
    None
}

fn pick(v: Option<&Value>, keys: &[&str]) -> f64 {
    let Some(o) = v else { return 0.0 };
    for k in keys {
        if let Some(x) = o.get(*k).and_then(num) {
            return x;
        }
    }
    0.0
}

/// OpenCode reports milliseconds; tolerate seconds too.
fn secs(x: f64) -> f64 {
    if x > 1e11 {
        x / 1000.0
    } else {
        x
    }
}

fn get_ts(v: &Value, keys: &[&str]) -> Option<f64> {
    get_num(v, keys).map(secs).filter(|t| *t > 0.0)
}

/// A list of objects, either a bare array or the usual wrapper keys.
fn obj_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.iter().filter(|x| x.is_object()).cloned().collect(),
        Value::Object(_) => {
            for k in [
                "sessions", "messages", "children", "data", "items", "result",
            ] {
                if let Some(Value::Array(a)) = v.get(k) {
                    return a.iter().filter(|x| x.is_object()).cloned().collect();
                }
            }
            if session_id(v).is_some() || v.get("info").is_some() || v.get("role").is_some() {
                vec![v.clone()]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

fn session_id(v: &Value) -> Option<String> {
    for k in ["id", "sessionID", "sessionId", "session_id"] {
        if let Some(s) = v
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(s.to_string());
        }
    }
    None
}

fn parent_id(v: &Value) -> String {
    for k in ["parentID", "parentId", "parent_id", "parent"] {
        if let Some(s) = v
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    String::new()
}

fn session_title(v: &Value) -> String {
    for k in ["title", "summary", "name", "slug"] {
        if let Some(s) = v
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return s.chars().take(90).collect();
        }
    }
    String::new()
}

fn session_project(v: &Value) -> String {
    for k in ["directory", "path", "cwd", "worktree", "project"] {
        if let Some(s) = v
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return pretty(s);
        }
    }
    String::new()
}

fn session_agent(v: &Value) -> String {
    for k in ["agent", "agentID", "agentId", "mode", "type"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() && s != "session" {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn session_times(v: &Value) -> (f64, f64) {
    let t = v.get("time");
    let created = match t {
        Some(x) if x.is_object() => get_ts(x, &["created", "createdAt"]),
        Some(x) => num(x).map(secs),
        None => None,
    }
    .or_else(|| get_ts(v, &["created", "createdAt", "time_created"]))
    .unwrap_or(0.0);
    let updated = match t {
        Some(x) if x.is_object() => get_ts(x, &["updated", "updatedAt"]),
        _ => None,
    }
    .or_else(|| get_ts(v, &["updated", "updatedAt", "time_updated"]))
    .unwrap_or(created);
    (created, updated)
}

fn msg_times(info: &Value) -> (f64, f64) {
    let t = info.get("time");
    let created = match t {
        Some(x) if x.is_object() => get_ts(x, &["created", "createdAt"]),
        Some(x) => num(x).map(secs),
        None => None,
    }
    .or_else(|| get_ts(info, &["created", "createdAt"]))
    .unwrap_or(0.0);
    let completed = match t {
        Some(x) if x.is_object() => get_ts(x, &["completed", "completedAt", "finished"]),
        _ => None,
    }
    .or_else(|| get_ts(info, &["completed", "completedAt"]))
    .unwrap_or(0.0);
    (created, completed)
}

fn msg_model(info: &Value) -> String {
    for k in ["modelID", "model_id", "model"] {
        if let Some(s) = info
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    if let Some(m) = info.get("model").filter(|x| x.is_object()) {
        for k in ["id", "modelID", "name"] {
            if let Some(s) = m
                .get(k)
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn msg_tokens(info: &Value) -> [i64; 4] {
    let tk = info.get("tokens");
    let cache = tk.and_then(|t| t.get("cache"));
    let mut inp = pick(tk, &["input", "input_tokens", "prompt", "prompt_tokens"]);
    let mut out = pick(
        tk,
        &["output", "output_tokens", "completion", "completion_tokens"],
    );
    let mut cr = pick(cache, &["read", "read_tokens", "cache_read"]);
    let mut cw = pick(cache, &["write", "write_tokens", "cache_write", "creation"]);
    if inp == 0.0 {
        inp = pick(
            Some(info),
            &["input_tokens", "inputTokens", "prompt_tokens"],
        );
    }
    if out == 0.0 {
        out = pick(
            Some(info),
            &["output_tokens", "outputTokens", "completion_tokens"],
        );
    }
    if cr == 0.0 {
        cr = pick(tk, &["cache_read", "cache_read_input_tokens"]);
        if cr == 0.0 {
            cr = pick(Some(info), &["cache_read", "cache_read_input_tokens"]);
        }
    }
    if cw == 0.0 {
        cw = pick(tk, &["cache_write", "cache_creation_input_tokens"]);
        if cw == 0.0 {
            cw = pick(Some(info), &["cache_write", "cache_creation_input_tokens"]);
        }
    }
    [
        inp.max(0.0) as i64,
        out.max(0.0) as i64,
        cr.max(0.0) as i64,
        cw.max(0.0) as i64,
    ]
}

fn msg_status(parts: &[Value]) -> String {
    for p in parts.iter().rev() {
        match p.s("type").as_str() {
            "tool" => {
                let name = p
                    .os("tool")
                    .or_else(|| p.os("name"))
                    .or_else(|| p.get("state").and_then(|s| s.os("title")))
                    .unwrap_or_else(|| "tool".to_string());
                return format!("using {name}");
            }
            "reasoning" | "thinking" => return "thinking".to_string(),
            "text" => return "writing".to_string(),
            _ => {}
        }
    }
    "thinking".to_string()
}

fn parse_msg(sid: &str, raw: &Value) -> Option<Msg> {
    let info = match raw.get("info") {
        Some(x @ Value::Object(_)) => x,
        _ => raw,
    };
    let role = info.s("role");
    if !role.is_empty() && role != "assistant" {
        return None;
    }
    let (created, completed) = msg_times(info);
    let ts = if created != 0.0 { created } else { completed };
    let id = info.os("id").or_else(|| raw.os("id")).unwrap_or_default();
    if ts == 0.0 && id.is_empty() {
        return None;
    }
    let key = if id.is_empty() {
        format!("{sid}:{ts:.3}")
    } else {
        format!("{sid}:{id}")
    };
    let parts = raw
        .get("parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let reasoning = pick(
        info.get("tokens"),
        &["reasoning", "reasoning_tokens", "reasoningTokens"],
    );
    Some(Msg {
        key,
        sid: sid.to_string(),
        ts,
        done: completed,
        model: msg_model(info),
        tokens: msg_tokens(info),
        reasoning: reasoning.max(0.0) as i64,
        usd: get_num(info, &["cost", "costUSD", "cost_usd"]).unwrap_or(0.0),
        status: msg_status(&parts),
    })
}

// =====================================================================================
// Tests (pure parsing, no network)
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &str = r#"{
        "info": {
            "id": "msg_1", "role": "assistant", "modelID": "claude-sonnet-4",
            "cost": 0.012,
            "tokens": {"input": 100, "output": 50, "reasoning": 10, "cache": {"read": 1000, "write": 200}},
            "time": {"created": 1700000000000, "completed": 1700000010000}
        },
        "parts": [{"type": "tool", "tool": "bash"}]
    }"#;

    #[test]
    fn parses_an_assistant_message() {
        let v: Value = serde_json::from_str(MSG).expect("valid json");
        let m = parse_msg("ses_1", &v).expect("assistant message");
        assert_eq!(m.model, "claude-sonnet-4");
        assert_eq!(m.tokens, [100, 50, 1000, 200]);
        assert_eq!(m.reasoning, 10);
        assert!((m.usd - 0.012).abs() < 1e-9);
        assert!((m.ts - 1_700_000_000.0).abs() < 0.001);
        assert!((m.done - 1_700_000_010.0).abs() < 0.001);
        assert_eq!(m.status, "using bash");
        assert!(!m.active(1_700_000_005.0));
    }

    #[test]
    fn tolerates_renamed_fields() {
        let v = json!({"role": "assistant", "model": "gpt-4o", "input_tokens": 5, "output_tokens": 7,
                       "time": {"created": 1_700_000_000}});
        let m = parse_msg("ses_2", &v).expect("tolerant parse");
        assert_eq!(m.model, "gpt-4o");
        assert_eq!(m.tokens, [5, 7, 0, 0]);
        assert_eq!(m.done, 0.0);
        assert!(m.active(1_700_000_050.0));
        assert_eq!(m.key, "ses_2:1700000000.000");
    }

    #[test]
    fn ignores_user_messages_and_unwraps_lists() {
        assert!(parse_msg(
            "ses_3",
            &json!({"info": {"role": "user", "time": {"created": 1_700_000_000}}})
        )
        .is_none());
        let list = obj_list(&json!({"sessions": [{"id": "ses_a"}, {"id": "ses_b"}]}));
        assert_eq!(list.len(), 2);
        assert_eq!(session_id(&list[1]), Some("ses_b".to_string()));
        let (created, updated) = session_times(
            &json!({"time": {"created": 1_700_000_000_000u64, "updated": 1_700_000_500_000u64}}),
        );
        assert!((created - 1_700_000_000.0).abs() < 0.001);
        assert!((updated - 1_700_000_500.0).abs() < 0.001);
    }

    #[test]
    fn basic_auth_is_base64() {
        assert_eq!(b64(b"opencode:secret"), "b3BlbmNvZGU6c2VjcmV0");
        assert_eq!(b64(b"a"), "YQ==");
        assert_eq!(b64(b""), "");
    }

    #[test]
    fn snapshot_has_the_brain_shape_and_usages_are_dated() {
        let oc = OpenCode::new();
        let t = 1_700_000_000.0;
        {
            let mut i = oc.lock();
            i.sessions.insert(
                "ses_abcdef123".to_string(),
                Sess {
                    id: "ses_abcdef123".to_string(),
                    title: "Fix the build".to_string(),
                    project: "repo".to_string(),
                    created: t - 60.0,
                    updated: t - 5.0,
                    ..Default::default()
                },
            );
            i.msgs.insert(
                "ses_abcdef123:m1".to_string(),
                Msg {
                    key: "ses_abcdef123:m1".to_string(),
                    sid: "ses_abcdef123".to_string(),
                    ts: t - 5.0,
                    done: 0.0,
                    model: "claude-sonnet-4".to_string(),
                    tokens: [10, 20, 0, 0],
                    reasoning: 0,
                    usd: 0.5,
                    status: "using bash".to_string(),
                },
            );
            i.ready = true;
        }
        let s = oc.snapshot(t);
        assert_eq!(s["active"], json!(true));
        assert_eq!(s["status"], json!("using bash"));
        assert_eq!(s["model"], json!("claude-sonnet-4"));
        assert_eq!(s["lobe_names"], json!(["plan", "act", "delegate"]));
        assert_eq!(s["lobes"]["plan"], json!(true));
        assert_eq!(s["sessions"][0]["id"], json!("ses_abcd"));
        assert_eq!(s["sessions"][0]["tokens"], json!(30));
        assert_eq!(s["jobs"], json!([]));
        assert_eq!(s["quota"], json!([]));
        assert!(s["balance"].is_null());
        assert!(s["warning"].as_str().unwrap_or("x").is_empty());

        let u = oc.usages();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0, "ses_abcdef123:m1");
        assert_eq!(u[0].3, [10, 20, 0, 0]);
        assert!((u[0].4 - 0.5).abs() < 1e-9);
    }
}
