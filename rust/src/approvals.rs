//! Approvals inbox: Claude Code's permission prompts, answered from the HMI or the phone.
//!
//! Claude Code runs the `PermissionRequest` hook right before it shows a permission dialog. The
//! hook files the request here and waits a few seconds for an answer from the panel. An answer
//! ("allow"/"deny") is returned to Claude Code; no answer leaves the normal dialog in the terminal,
//! so nothing is ever blocked by MetaBrain.
use crate::util::{data_dir, now, read_json, write_json, J};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Default seconds the hook waits for an answer before falling back to the terminal dialog.
pub const DEFAULT_WAIT_S: f64 = 20.0;

fn dir() -> PathBuf {
    let d = data_dir().join("approvals");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn path(id: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(dir().join(format!("{id}.json")))
}

/// One readable line for the request ("npm install", "src/app.rs", a URL…).
pub fn summarize(tool: &str, input: &Value) -> String {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
    let s = match tool {
        "Bash" | "PowerShell" => pick("command"),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Read" => {
            pick("file_path").or_else(|| pick("notebook_path"))
        }
        "WebFetch" => pick("url"),
        "WebSearch" => pick("query"),
        "Agent" | "Task" => pick("description"),
        _ => None,
    }
    .or_else(|| {
        input
            .as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str().map(String::from)))
    })
    .unwrap_or_default();
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(300).collect()
}

/// File a new request; returns its id.
pub fn file(brain: &str, brain_name: &str, ev: &Value) -> String {
    let id: String = format!("{:x}{:04x}", (now() * 1000.0) as u64, rand::random::<u16>());
    let tool = ev.s("tool_name");
    let input = ev.get("tool_input").cloned().unwrap_or(Value::Null);
    let cwd = ev.s("cwd");
    let project = cwd
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string();
    let mut clipped = input.to_string();
    if clipped.len() > 4000 {
        clipped = clipped.chars().take(4000).collect();
    }
    let rec = json!({
        "id": id, "ts": now(), "status": "pending",
        "brain": brain, "brain_name": brain_name,
        "session": ev.s("session_id"), "cwd": cwd, "project": project,
        "tool": tool, "summary": summarize(&tool, &input), "input": clipped,
    });
    if let Some(p) = path(&id) {
        let _ = write_json(&p, &rec);
    }
    id
}

pub fn get(id: &str) -> Option<Value> {
    path(id).and_then(|p| read_json(&p))
}

/// Answer a pending request. Returns false when it is no longer pending.
pub fn decide(id: &str, allow: bool, by: &str) -> bool {
    let Some(p) = path(id) else { return false };
    let Some(_flock) = crate::util::FileLock::acquire(&p, 2000) else {
        return false;
    };
    let Some(mut rec) = read_json(&p) else {
        return false;
    };
    if rec.s("status") != "pending" {
        return false;
    }
    rec["status"] = json!(if allow { "allow" } else { "deny" });
    rec["decided_by"] = json!(by);
    rec["decided_ts"] = json!(now());
    write_json(&p, &rec).is_ok()
}

/// The request was answered elsewhere (terminal) or timed out.
pub fn expire(id: &str) {
    if let Some(p) = path(id) {
        let Some(_flock) = crate::util::FileLock::acquire(&p, 1500) else {
            return;
        };
        if let Some(mut rec) = read_json(&p) {
            if rec.s("status") == "pending" {
                rec["status"] = json!("terminal");
                rec["decided_ts"] = json!(now());
                let _ = write_json(&p, &rec);
            }
        }
    }
}

/// Block until the request is answered or `wait_s` passes. Some(true)=allow, Some(false)=deny.
pub fn wait(id: &str, wait_s: f64) -> Option<bool> {
    let t0 = now();
    loop {
        if let Some(rec) = get(id) {
            match rec.s("status").as_str() {
                "allow" => return Some(true),
                "deny" => return Some(false),
                "pending" => {}
                _ => return None,
            }
        }
        if now() - t0 >= wait_s {
            expire(id);
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Claude Code's answer for a PermissionRequest hook.
pub fn hook_output(allow: bool) -> String {
    let decision = if allow {
        json!({"behavior": "allow"})
    } else {
        json!({"behavior": "deny", "message": "Denegado desde MetaBrain."})
    };
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": decision}})
        .to_string()
}

/// Pending requests (oldest first) and the latest answered ones (newest first). Pending ones
/// older than the wait window are stale (the terminal dialog took over) and are expired here.
pub fn list(recent: usize, wait_s: f64) -> Value {
    let mut pending: Vec<Value> = Vec::new();
    let mut done: Vec<Value> = Vec::new();
    let t = now();
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x != "json").unwrap_or(true) {
                continue;
            }
            let Some(rec) = read_json(&p) else { continue };
            let age = t - rec.f("ts").unwrap_or(0.0);
            if rec.s("status") == "pending" {
                if age > wait_s + 5.0 {
                    expire(&rec.s("id"));
                    continue;
                }
                pending.push(rec);
            } else if age < 7.0 * 86400.0 {
                done.push(rec);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let ts = |v: &Value| v.f("ts").unwrap_or(0.0);
    pending.sort_by(|a, b| {
        ts(a)
            .partial_cmp(&ts(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    done.sort_by(|a, b| {
        ts(b)
            .partial_cmp(&ts(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    done.truncate(recent);
    json!({"pending": pending, "recent": done, "wait_s": wait_s})
}

/// Settings: `"approvals": {"enabled": true, "wait_s": 20}`.
pub fn settings(cfg: &Value) -> (bool, f64) {
    let a = cfg.get("approvals").cloned().unwrap_or(Value::Null);
    let on = a.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let w = a
        .get("wait_s")
        .and_then(|v| v.as_f64())
        .filter(|x| x.is_finite() && *x >= 2.0)
        .unwrap_or(DEFAULT_WAIT_S)
        .min(600.0);
    (on, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> std::sync::MutexGuard<'static, ()> {
        let g = crate::util::test_env_lock();
        let d = std::env::temp_dir().join(format!("mb_appr_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::env::set_var("METABRAIN_HOME", &d);
        g
    }

    #[test]
    fn summaries_are_readable() {
        assert_eq!(
            summarize("Bash", &json!({"command": "npm   install\nreact"})),
            "npm install react"
        );
        assert_eq!(
            summarize("Edit", &json!({"file_path": "C:/p/app.rs"})),
            "C:/p/app.rs"
        );
        assert_eq!(
            summarize("mcp__x__y", &json!({"a": 1, "b": "hola"})),
            "hola"
        );
    }

    #[test]
    fn file_decide_wait_and_list() {
        let _g = home("flow");
        let ev = json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build"}, "cwd": "/p/demo", "session_id": "s1"});
        let id = file("A", "Claude 1", &ev);
        let l = list(10, 20.0);
        assert_eq!(l["pending"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(l["pending"][0].s("project"), "demo");
        assert!(decide(&id, true, "panel"));
        assert!(!decide(&id, false, "panel"), "already answered");
        assert_eq!(wait(&id, 1.0), Some(true));
        let id2 = file("A", "Claude 1", &ev);
        assert_eq!(wait(&id2, 0.5), None);
        assert_eq!(
            get(&id2).map(|r| r.s("status")),
            Some("terminal".to_string())
        );
        assert_eq!(
            list(10, 20.0)["recent"].as_array().map(|a| a.len()),
            Some(2)
        );
    }

    #[test]
    fn hook_output_shape_and_settings() {
        let v: Value = serde_json::from_str(&hook_output(false)).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(settings(&json!({})), (true, DEFAULT_WAIT_S));
        assert_eq!(
            settings(&json!({"approvals": {"enabled": false, "wait_s": 45}})),
            (false, 45.0)
        );
        assert!(path("../x").is_none());
    }
}
