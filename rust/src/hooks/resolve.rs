//! Brain resolution for Claude Code hook events (env vars, config_dir, transcript prompt & rules).
use super::_sessions;
use crate::util::{expand, norm, now, truthy, J};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};

fn _current_config_dir() -> String {
    let raw = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "~/.claude".to_string());
    norm(&expand(&raw))
}

fn _first_prompt(transcript: &str, max_lines: usize) -> String {
    let Ok(f) = fs::File::open(transcript) else {
        return String::new();
    };
    for (i, line) in BufReader::new(f).lines().enumerate() {
        if i >= max_lines {
            break;
        }
        let Ok(line) = line else { continue };
        let Ok(d) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let mut text = String::new();
        match d.get("type").and_then(|v| v.as_str()) {
            Some("queue-operation") => {
                text = d
                    .get("content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| d.get("prompt").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string();
            }
            Some("user") => match d.get("message").and_then(|m| m.get("content")) {
                Some(Value::String(s)) => text = s.clone(),
                Some(Value::Array(a)) => {
                    let parts: Vec<String> = a
                        .iter()
                        .filter_map(|x| {
                            if x.get("type").and_then(|v| v.as_str()) == Some("text") {
                                x.get("text")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                    text = parts.join(" ");
                }
                _ => {}
            },
            _ => {}
        }
        let t = text.trim();
        if !t.is_empty() && !t.starts_with('<') {
            return t.to_string();
        }
    }
    String::new()
}

fn _rule_ok(rule: &Value, prompt: &str, cwd: &str) -> bool {
    let Some(rule) = rule.as_object() else {
        return false;
    };
    for (key, text) in [("prompt", prompt), ("cwd", cwd)] {
        if let Some(pat) = rule.get(key) {
            if truthy(Some(pat)) {
                let Some(pat) = pat.as_str() else {
                    return false;
                };
                let Ok(re) = regex::RegexBuilder::new(pat).case_insensitive(true).build() else {
                    return false;
                };
                if !re.is_match(text) {
                    return false;
                }
            }
        }
    }
    true
}

/// -> (brain_id or None, sure). Env METABRAIN_BRAIN / CEREBRO wins; else config dir + match rules.
pub fn resolve_brain(cfg: &Value, ev: &Value) -> (Option<String>, bool) {
    resolve_brain_with_env(cfg, ev, |k| std::env::var(k).ok())
}

fn resolve_brain_with_env<F>(cfg: &Value, ev: &Value, env: F) -> (Option<String>, bool)
where
    F: Fn(&str) -> Option<String>,
{
    let brains: Vec<Value> = cfg
        .arr("brains")
        .into_iter()
        .filter(|b| b.s("kind") == "claude_code" && b.b("enabled", true))
        .collect();

    let mut ids: HashMap<String, String> = HashMap::new();
    for b in &brains {
        if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
            ids.insert(id.to_lowercase(), id.to_string());
        }
    }
    for var in ["METABRAIN_BRAIN", "CEREBRO"] {
        if let Some(v) = env(var) {
            let key = v.trim().to_lowercase();
            if !key.is_empty() {
                if let Some(id) = ids.get(&key) {
                    return (Some(id.clone()), true);
                }
            }
        }
    }

    let cfg_dir = env("CLAUDE_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "~/.claude".to_string());
    let here = norm(&expand(&cfg_dir));
    let mine: Vec<&Value> = brains
        .iter()
        .filter(|b| {
            let d = b
                .os("config_dir")
                .unwrap_or_else(|| "~/.claude".to_string());
            norm(&expand(&d)) == here
        })
        .collect();
    if mine.is_empty() {
        return (None, true);
    }

    let transcript = ev.s("transcript_path");
    let prompt = if transcript.is_empty() {
        String::new()
    } else {
        _first_prompt(&transcript, 60)
    };
    let cwd = ev.s("cwd");
    let mut needs_prompt = false;
    for b in &mine {
        let rule = match b.get("match") {
            Some(v @ Value::Object(o)) if !o.is_empty() => v,
            _ => continue,
        };
        if truthy(rule.get("prompt")) && prompt.is_empty() {
            needs_prompt = true;
            continue;
        }
        if _rule_ok(rule, &prompt, &cwd) {
            return (
                b.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                true,
            );
        }
    }
    let default = mine.iter().find(|b| match b.get("match") {
        None | Some(Value::Null) => true,
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    });
    let out = default.and_then(|b| b.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()));
    (out, !needs_prompt)
}

pub(super) fn session_brain(
    cfg: &Value,
    ev: &Value,
    sessions: Option<Value>,
) -> (Option<String>, Value, bool) {
    let mut sessions = sessions.unwrap_or_else(_sessions);
    if !sessions.is_object() {
        sessions = json!({});
    }
    let sid = ev.s("session_id");
    let existing = sessions.get(&sid).cloned();
    if let Some(rec) = &existing {
        if rec.get("sure").and_then(|v| v.as_bool()).unwrap_or(false) {
            let brain = rec
                .get("brain")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return (brain, sessions, false);
        }
    }

    let (bid, sure) = resolve_brain(cfg, ev);
    let mut rec = match existing {
        Some(Value::Object(o)) => Value::Object(o),
        _ => json!({}),
    };
    if let Some(o) = rec.as_object_mut() {
        o.insert(
            "brain".to_string(),
            bid.clone().map(Value::String).unwrap_or(Value::Null),
        );
        o.insert("sure".to_string(), Value::Bool(sure));
        o.insert("ts".to_string(), json!(now()));
        o.insert(
            "cwd".to_string(),
            ev.get("cwd").cloned().unwrap_or(Value::Null),
        );
        o.insert("config_dir".to_string(), json!(_current_config_dir()));
    }
    if let Some(obj) = sessions.as_object_mut() {
        obj.insert(sid, rec);
    }
    (bid, sessions, true)
}
