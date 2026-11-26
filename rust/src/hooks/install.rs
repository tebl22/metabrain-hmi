//! Idempotent installer for MetaBrain hooks in Claude Code `settings.json` files.
use super::{AGENT_TOOLS, EDIT_TOOLS, MARK};
use crate::util::{expand, read_json, write_json, J};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn hook_spec(exe: &str) -> Value {
    let c = |event: &str| format!("\"{}\" hook {}", exe, event);
    let matcher = EDIT_TOOLS
        .iter()
        .chain(AGENT_TOOLS.iter())
        .copied()
        .collect::<Vec<&str>>()
        .join("|");
    json!({
        "SessionStart": [{"hooks": [{"type": "command", "command": c("session-start")}]}],
        "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": c("pre-tool")}]}],
        "PostToolUse": [{"matcher": format!("{matcher}|Bash"), "hooks": [{"type": "command", "command": c("post-tool")}]}],
        "UserPromptSubmit": [{"hooks": [{"type": "command", "command": c("prompt")}]}],
        "PermissionRequest": [{"matcher": "*", "hooks": [{"type": "command", "command": c("permission"), "timeout": 650}]}],
        "Stop": [{"hooks": [{"type": "command", "command": c("stop")}]}],
        "SessionEnd": [{"hooks": [{"type": "command", "command": c("session-end")}]}],
    })
}

fn _is_ours_or_legacy(entry: &Value) -> bool {
    let s = serde_json::to_string(entry).unwrap_or_default();
    if s.contains("cerebros.py") {
        return true;
    }
    let replaced = s.replace("\\\\", "/");
    if replaced.contains("metabrain/hooks.py") {
        return true;
    }
    let lower = s.to_lowercase();
    (s.contains(" hook ") && lower.contains(MARK)) || lower.contains("metabrain-hook")
}

fn ensure_map_field<'a>(v: &'a mut Value, key: &str) -> Option<&'a mut Map<String, Value>> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    let obj = v.as_object_mut()?;
    if !obj.get(key).map(|x| x.is_object()).unwrap_or(false) {
        obj.insert(key.to_string(), Value::Object(Map::new()));
    }
    obj.get_mut(key).and_then(|x| x.as_object_mut())
}

/// Add MetaBrain hooks to settings.json of every claude_code config dir; drop old hooks.
pub fn install(cfg: &Value, exe: &str, dry_run: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for b in cfg.arr("brains") {
        if b.s("kind") == "claude_code" {
            let d = expand(
                &b.os("config_dir")
                    .unwrap_or_else(|| "~/.claude".to_string()),
            );
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }

    let spec = hook_spec(exe);
    for d in dirs {
        if !d.is_dir() {
            out.push(format!("skip (missing): {}", d.display()));
            continue;
        }
        let p = d.join("settings.json");
        let exists = p.exists();
        let mut s = match read_json(&p) {
            Some(v) => v,
            None => {
                if exists {
                    out.push(format!(
                        "skip (cannot parse, not touching it): {}",
                        p.display()
                    ));
                    continue;
                }
                json!({})
            }
        };
        if s.is_null() {
            s = json!({});
        }
        if !s.is_object() {
            if exists {
                out.push(format!(
                    "skip (cannot parse, not touching it): {}",
                    p.display()
                ));
                continue;
            }
            s = json!({});
        }

        let mut events: BTreeSet<String> = BTreeSet::new();
        if let Some(h) = s.get("hooks").and_then(|v| v.as_object()) {
            for k in h.keys() {
                events.insert(k.clone());
            }
        }
        if let Some(o) = spec.as_object() {
            for k in o.keys() {
                events.insert(k.clone());
            }
        }

        if let Some(hooks) = ensure_map_field(&mut s, "hooks") {
            for ev in events {
                let keep = hooks
                    .get(ev.as_str())
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut new_list: Vec<Value> = keep
                    .into_iter()
                    .filter(|x| !_is_ours_or_legacy(x))
                    .collect();
                if let Some(arr) = spec.get(ev.as_str()).and_then(|v| v.as_array()) {
                    new_list.extend(arr.iter().cloned());
                }
                if new_list.is_empty() {
                    hooks.remove(ev.as_str());
                } else {
                    hooks.insert(ev.clone(), Value::Array(new_list));
                }
            }
        }

        if !dry_run {
            if exists {
                let bak = PathBuf::from(format!("{}.metabrain.bak", p.display()));
                if !bak.exists() {
                    if let Ok(bytes) = fs::read(&p) {
                        let _ = fs::write(&bak, bytes);
                    }
                }
            }
            let _ = write_json(&p, &s);
        }
        out.push(format!(
            "{}: {}",
            if dry_run {
                "would install"
            } else {
                "hooks installed in"
            },
            p.display()
        ));
    }
    out
}

/// Remove only MetaBrain hooks from every Claude Code `settings.json`, preserving all user hooks.
pub fn uninstall(cfg: &Value, dry_run: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for b in cfg.arr("brains") {
        if b.s("kind") == "claude_code" {
            let d = expand(
                &b.os("config_dir")
                    .unwrap_or_else(|| "~/.claude".to_string()),
            );
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    if dirs.is_empty() {
        dirs.push(expand("~/.claude"));
    }

    for d in dirs {
        let p = d.join("settings.json");
        if !p.exists() {
            out.push(format!("skip (missing): {}", p.display()));
            continue;
        }
        let Some(mut s) = read_json(&p) else {
            out.push(format!(
                "skip (cannot parse, not touching it): {}",
                p.display()
            ));
            continue;
        };
        let Some(obj) = s.as_object_mut() else {
            out.push(format!(
                "skip (cannot parse, not touching it): {}",
                p.display()
            ));
            continue;
        };
        let mut removed = 0usize;
        let mut drop_hooks_obj = false;
        if let Some(hooks) = obj.get_mut("hooks").and_then(|v| v.as_object_mut()) {
            let keys: Vec<String> = hooks.keys().cloned().collect();
            for ev in keys {
                if let Some(arr) = hooks.get(&ev).and_then(|v| v.as_array()).cloned() {
                    let before = arr.len();
                    let kept: Vec<Value> =
                        arr.into_iter().filter(|x| !_is_ours_or_legacy(x)).collect();
                    removed += before.saturating_sub(kept.len());
                    if kept.is_empty() {
                        hooks.remove(&ev);
                    } else {
                        hooks.insert(ev, Value::Array(kept));
                    }
                }
            }
            drop_hooks_obj = hooks.is_empty();
        }
        if drop_hooks_obj {
            obj.remove("hooks");
        }
        if removed == 0 {
            out.push(format!("clean (no MetaBrain hooks): {}", p.display()));
            continue;
        }
        if !dry_run {
            let _ = write_json(&p, &s);
        }
        out.push(format!(
            "{} ({removed} entries): {}",
            if dry_run {
                "would remove hooks from"
            } else {
                "hooks removed from"
            },
            p.display()
        ));
    }
    out
}
