//! Claude Code hooks: the live control path (zero tokens, no HTTP).
//!
//! Port of metabrain/hooks.py. Hooks read gate.json/metabrain.json and never
//! break a Claude session: any error means "allow" with exit code 0.
mod handlers;
mod install;
mod policy;
mod resolve;
#[cfg(test)]
mod tests;

pub use install::install;
pub use resolve::resolve_brain;

use crate::memory::Memory;
use crate::util::{data_dir, now, read_json, write_json, J};
use serde_json::{json, Map, Value};
use std::path::Path;

pub(super) const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
pub(super) const AGENT_TOOLS: [&str; 2] = ["Agent", "Task"];
const SESSIONS_FILE: &str = "hook_sessions.json";
const AGENT_TTL: f64 = 30.0 * 60.0;
const SESSION_TTL: f64 = 3.0 * 24.0 * 3600.0;
pub(super) const MARK: &str = "metabrain";

/// Seconds after which gate.json is ignored: an old gate means MetaBrain is not running,
/// and a stopped supervisor must never keep blocking Claude sessions.
const GATE_MAX_AGE: f64 = 120.0;

pub(super) fn _config() -> Value {
    read_json(&data_dir().join("metabrain.json"))
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

pub(super) fn _gate() -> Value {
    let g = read_json(&data_dir().join("gate.json"))
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    let ts = g.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if crate::util::now() - ts > GATE_MAX_AGE {
        return json!({});
    }
    g
}

pub(super) fn _sessions() -> Value {
    read_json(&data_dir().join(SESSIONS_FILE))
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

pub(super) fn _save_sessions(s: &Value) {
    let t = now();
    let mut out = Map::new();
    if let Some(obj) = s.as_object() {
        for (k, v) in obj {
            let ts = v.get("ts").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if t - ts < SESSION_TTL {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    let _ = write_json(&data_dir().join(SESSIONS_FILE), &Value::Object(out));
}

pub(super) fn _deny(reason: &str) -> String {
    let v = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    });
    let mut s = serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

pub(super) fn _memory(cfg: &Value) -> Option<Memory> {
    let m = cfg.o("memory");
    if !m.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    Some(Memory::from_cfg(cfg))
}

pub(super) fn brain_name(cfg: &Value, bid: &str) -> String {
    cfg.arr("brains")
        .into_iter()
        .find(|b| b.get("id").and_then(|v| v.as_str()) == Some(bid))
        .and_then(|b| b.os("name"))
        .unwrap_or_else(|| bid.to_string())
}

pub(super) fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub(super) fn is_edit_tool(tool: &str) -> bool {
    EDIT_TOOLS.contains(&tool)
}

pub(super) fn is_agent_tool(tool: &str) -> bool {
    AGENT_TOOLS.contains(&tool)
}

pub(super) fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

pub(super) fn session_rec_mut<'a>(sessions: &'a mut Value, sid: &str) -> Option<&'a mut Value> {
    if !sessions.is_object() {
        *sessions = Value::Object(Map::new());
    }
    let map = sessions.as_object_mut()?;
    if !map.get(sid).map(|v| v.is_object()).unwrap_or(false) {
        map.insert(sid.to_string(), json!({"ts": now()}));
    }
    map.get_mut(sid)
}

pub(super) fn filter_agents(rec: &Value, t: f64) -> Map<String, Value> {
    let mut running = Map::new();
    if let Some(agents) = rec.get("agents").and_then(|v| v.as_object()) {
        for (k, v) in agents {
            let ts = v.as_f64().unwrap_or(0.0);
            if t - ts < AGENT_TTL {
                running.insert(k.clone(), v.clone());
            }
        }
    }
    running
}

pub(super) fn _lang(cfg: &Value) -> String {
    let l = cfg.s("lang");
    if l.is_empty() {
        "es".to_string()
    } else {
        l
    }
}

/// Project name the router uses: the session folder's name.
pub(super) fn _project(ev: &Value) -> String {
    let cwd = ev.s("cwd");
    let b = basename(&cwd);
    if b.is_empty() {
        "general".to_string()
    } else {
        b
    }
}

pub(super) fn _router_on(cfg: &Value, key: &str) -> bool {
    cfg.get("router")
        .and_then(|r| r.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Run one hook. Always returns exit code 0 on failure: a hook must never break a Claude session.
pub fn run_hook(name: &str, stdin: &str) -> (i32, String, String) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let raw = stdin.trim_start_matches(|c| {
            c == '\u{feff}' || c == '\u{00ef}' || c == '\u{00bb}' || c == '\u{00bf}'
        });
        let ev: Value = if raw.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(_) => return None,
            }
        };
        let cfg = _config();
        let out = match name {
            "session-start" => handlers::session_start(&ev, &cfg),
            "pre-tool" => handlers::pre_tool(&ev, &cfg),
            "post-tool" => handlers::post_tool(&ev, &cfg),
            "session-end" => handlers::session_end(&ev, &cfg),
            "prompt" => handlers::user_prompt(&ev, &cfg),
            "permission" => handlers::permission(&ev, &cfg),
            "stop" => handlers::stop(&ev, &cfg),
            "shadow-bg" => handlers::shadow_bg(&ev, &cfg),
            _ => return None,
        };
        Some(out)
    }));

    match result {
        Ok(Some(out)) => (0, out, String::new()),
        _ => (0, String::new(), String::new()),
    }
}
