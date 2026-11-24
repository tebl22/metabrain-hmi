//! Event handlers for Claude Code hooks (SessionStart, PreToolUse, PostToolUse, Permission, etc.).
use super::policy::{_subagent_policy, is_readonly_command};
use super::resolve::session_brain;
use super::{
    _deny, _gate, _lang, _memory, _project, _router_on, _save_sessions, _sessions, as_int,
    basename, brain_name, filter_agents, is_agent_tool, is_edit_tool, session_rec_mut,
};
use crate::util::{now, truthy, J};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// PermissionRequest: put the prompt in MetaBrain's inbox and wait briefly for an answer from the
/// panel or the phone. No answer (or the inbox turned off) keeps Claude Code's own dialog.
pub(super) fn permission(ev: &Value, cfg: &Value) -> String {
    let (on, wait_s) = crate::approvals::settings(cfg);
    if !on {
        return String::new();
    }
    let (bid, _, _) = session_brain(cfg, ev, None);
    let bid = bid.unwrap_or_default();
    let name = if bid.is_empty() {
        "Claude".to_string()
    } else {
        brain_name(cfg, &bid)
    };
    let id = crate::approvals::file(&bid, &name, ev);
    let what = crate::approvals::get(&id)
        .map(|r| r.s("summary"))
        .unwrap_or_default();
    crate::insights::notify(
        "MetaBrain",
        &format!(
            "{name} pide permiso: {} {}",
            ev.s("tool_name"),
            what.chars().take(90).collect::<String>()
        ),
    );
    match crate::approvals::wait(&id, wait_s) {
        Some(allow) => crate::approvals::hook_output(allow),
        None => String::new(),
    }
}

pub(super) fn session_start(ev: &Value, cfg: &Value) -> String {
    let (bid, sessions, _) = session_brain(cfg, ev, None);
    _save_sessions(&sessions);

    let mut lines: Vec<String> = Vec::new();
    let mem = _memory(cfg);
    if let Some(bid) = bid.as_deref() {
        if let Some(mem) = mem.as_ref() {
            let b = cfg
                .arr("brains")
                .into_iter()
                .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(bid))
                .unwrap_or_else(|| json!({}));
            let name = b.os("name").unwrap_or_else(|| bid.to_string());
            lines.push(mem.briefing(bid, &name));
        }
        let gate = _gate();
        let brains = gate.o("brains");
        let g = brains.get(bid).cloned().unwrap_or_else(|| json!({}));
        if truthy(g.get("paused")) {
            let why = g.os("reason").or_else(|| g.os("by")).unwrap_or_default();
            lines.push(format!(
                "[METABRAIN] This brain is PAUSED ({why}). Do not start work; tell the user."
            ));
        } else if let Some(model) = g.os("model") {
            let effort = g.os("effort").unwrap_or_else(|| "-".to_string());
            lines.push(format!(
                "[METABRAIN] Budget plan for this brain: model {model}, effort {effort}."
            ));
        }
    }
    if let Some(bid) = bid.as_deref() {
        let cwd = ev.s("cwd");
        if !cwd.is_empty() {
            if let Some(txt) = crate::relay::inject(&cwd, bid, &_lang(cfg)) {
                crate::savings::mark_relayed(&ev.s("session_id"));
                lines.push(txt);
            }
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

/// Keep the continuous relay of this session up to date (throttled inside relay::update).
fn _relay(ev: &Value, cfg: &Value, bid: Option<&str>, force: bool) {
    let tp = ev.s("transcript_path");
    let cwd = ev.s("cwd");
    let Some(bid) = bid else { return };
    if tp.is_empty() || cwd.is_empty() {
        return;
    }
    let name = brain_name(cfg, bid);
    let _ = crate::relay::update(
        Path::new(&tp),
        &cwd,
        &ev.s("session_id"),
        bid,
        &name,
        &_lang(cfg),
        force,
    );
}

/// What a delegation actually used: the subagent's model, or DeepSeek when the command calls it.
fn _actual_model(ev: &Value) -> Option<String> {
    let tool = ev.s("tool_name");
    let inp = ev.o("tool_input");
    if is_agent_tool(&tool) {
        let m = inp.s("model");
        return Some(if m.is_empty() {
            format!("subagente {}", inp.s("subagent_type"))
        } else {
            m
        });
    }
    if tool == "Bash" {
        let c = inp.s("command").to_lowercase();
        if c.contains("ds_worker") || c.contains("dsh ") || c.contains("deepseek") {
            return Some("deepseek".to_string());
        }
    }
    None
}

fn _task_of(ev: &Value) -> String {
    let inp = ev.o("tool_input");
    let t = inp
        .os("prompt")
        .or_else(|| inp.os("description"))
        .unwrap_or_else(|| inp.s("command"));
    t.chars().take(1200).collect()
}

/// Run the shadow comparison in a detached copy of ourselves so the session never waits for the network.
fn _spawn_shadow(ev: &Value) {
    use std::io::Write;
    // Under `cargo test` current_exe() is the test harness: re-running it would spawn tests recursively
    // and keep the binary locked on Windows.
    if cfg!(test) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["hook", "shadow-bg"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::util::hidden(&mut cmd);
    if let Ok(mut child) = cmd.spawn() {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(ev.to_string().as_bytes());
        }
    }
}

pub(super) fn shadow_bg(ev: &Value, cfg: &Value) -> String {
    let Some(actual) = _actual_model(ev) else {
        return String::new();
    };
    let chain: Vec<Value> = cfg
        .arr("brains")
        .into_iter()
        .filter(|b| b.b("enabled", true))
        .map(|b| json!({"id": b.s("id"), "available": true, "kind": b.s("kind")}))
        .collect();
    let router = crate::router::Router::new(None);
    crate::learn::shadow(&router, cfg, &chain, &_project(ev), &_task_of(ev), &actual);
    String::new()
}

/// UserPromptSubmit: a correction from the user is a failure signal for the last routed attempt.
pub(super) fn user_prompt(ev: &Value, cfg: &Value) -> String {
    let p = ev.s("prompt");
    if !p.is_empty() {
        let router = crate::router::Router::new(None);
        crate::learn::signal_from_prompt(&router, &_project(ev), &p);
    }
    let (bid, _, _) = session_brain(cfg, ev, None);
    _relay(ev, cfg, bid.as_deref(), false);
    String::new()
}

/// Stop: the turn ended, so refresh the relay right away.
pub(super) fn stop(ev: &Value, cfg: &Value) -> String {
    let (bid, _, _) = session_brain(cfg, ev, None);
    _relay(ev, cfg, bid.as_deref(), true);
    String::new()
}

pub(super) fn pre_tool(ev: &Value, cfg: &Value) -> String {
    let (bid, mut sessions, mut changed) = session_brain(cfg, ev, None);
    let out = pre_tool_inner(ev, cfg, bid.as_deref(), &mut sessions, &mut changed);
    if changed {
        _save_sessions(&sessions);
    }
    out
}

fn pre_tool_inner(
    ev: &Value,
    cfg: &Value,
    bid: Option<&str>,
    sessions: &mut Value,
    changed: &mut bool,
) -> String {
    let tool = ev.s("tool_name");
    let inp = ev.o("tool_input");
    let sid = ev.s("session_id");

    if let Some(bid) = bid {
        let gate = _gate();
        let brains = gate.o("brains");
        let g = brains.get(bid).cloned().unwrap_or_else(|| json!({}));

        if truthy(g.get("paused")) {
            let reason = g
                .os("reason")
                .or_else(|| g.os("by"))
                .unwrap_or_else(|| "budget".to_string());
            return _deny(&format!(
                "MetaBrain paused brain '{bid}': {reason}. Stop now, do not retry tools; tell the user. It resumes automatically."
            ));
        }

        // Read-only brains may inspect but never modify.
        if truthy(g.get("readonly")) {
            let name = brain_name(cfg, bid);
            let msg = format!(
                "MetaBrain: {name} está en modo SOLO LECTURA desde el panel. Analiza y propone; no edites ni ejecutes comandos que cambien archivos."
            );
            if is_edit_tool(&tool) {
                return _deny(&msg);
            }
            if tool == "Bash" || tool == "PowerShell" {
                let cmd = inp.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if !is_readonly_command(cmd) {
                    return _deny(&msg);
                }
            }
        }

        if is_agent_tool(&tool) {
            let atype = inp
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("general-purpose")
                .to_string();
            let pol = _subagent_policy(cfg, bid, &atype);

            if pol.get("allow").and_then(|v| v.as_bool()) == Some(false) {
                return _deny(&format!(
                    "MetaBrain policy: subagent type '{atype}' is blocked for brain '{bid}'. Do the work yourself."
                ));
            }
            if let Some(want) = pol.os("model") {
                let got = inp.get("model").and_then(|v| v.as_str()).unwrap_or("");
                if got != want {
                    return _deny(&format!(
                        "MetaBrain policy: subagents of type '{atype}' must use model '{want}'. Relaunch the same Agent call with model: \"{want}\"."
                    ));
                }
            }

            let mx = pol.get("max_parallel").and_then(as_int);
            let t = now();
            if let Some(rec) = session_rec_mut(sessions, &sid) {
                let mut running = filter_agents(rec, t);
                if let Some(mx) = mx {
                    if (running.len() as i64) >= mx {
                        return _deny(&format!(
                            "MetaBrain policy: at most {mx} subagents in parallel for brain '{bid}'. Wait for one to finish, then relaunch."
                        ));
                    }
                }
                if let Some(tid) = ev.os("tool_use_id") {
                    running.insert(tid, json!(t));
                }
                if let Some(o) = rec.as_object_mut() {
                    o.insert("agents".to_string(), Value::Object(running));
                }
                *changed = true;
            }
        }

        if is_edit_tool(&tool) {
            let mem = _memory(cfg);
            let path = inp.os("file_path").or_else(|| inp.os("notebook_path"));
            if let (Some(path), Some(mem)) = (path, mem) {
                if let Some(block) = mem.claim(&path, bid, Some(sid.as_str())) {
                    let b = block.get("brain").and_then(|v| v.as_str()).unwrap_or("");
                    return _deny(&format!(
                        "File locked by brain '{b}' (another session is editing {}). Do not insist: leave a ticket in {} (first line 'to: {b}').",
                        basename(&path),
                        mem.tickets.display()
                    ));
                }
            }
        }
    }
    String::new()
}

pub(super) fn post_tool(ev: &Value, cfg: &Value) -> String {
    let tool = ev.s("tool_name");
    let sid = ev.s("session_id");

    if tool == "Bash" {
        let cmd = ev.o("tool_input").s("command");
        let r = ev.o("tool_response");
        let out = format!("{}\n{}", r.s("stdout"), r.s("stderr"));
        let router = crate::router::Router::new(None);
        crate::learn::signal_from_bash(&router, &_project(ev), &cmd, &out);
    }
    if _router_on(cfg, "shadow") && _actual_model(ev).is_some() {
        _spawn_shadow(ev);
    }
    {
        let (bid, _, _) = session_brain(cfg, ev, None);
        _relay(ev, cfg, bid.as_deref(), false);
    }

    if is_agent_tool(&tool) {
        if let Some(tid) = ev.os("tool_use_id") {
            let mut sessions = _sessions();
            let mut changed = false;
            if let Some(rec) = sessions
                .as_object_mut()
                .and_then(|o| o.get_mut(&sid))
                .and_then(|v| v.as_object_mut())
            {
                if let Some(agents) = rec.get_mut("agents").and_then(|v| v.as_object_mut()) {
                    if agents.remove(&tid).is_some() {
                        changed = true;
                    }
                }
            }
            if changed {
                _save_sessions(&sessions);
            }
            return String::new();
        }
    }

    if is_edit_tool(&tool) {
        let inp = ev.o("tool_input");
        let path = inp.os("file_path").or_else(|| inp.os("notebook_path"));
        let mem = _memory(cfg);
        if let (Some(path), Some(mem)) = (path, mem) {
            let (bid, sessions, changed) = session_brain(cfg, ev, None);
            if changed {
                _save_sessions(&sessions);
            }
            mem.note(
                bid.as_deref().unwrap_or("?"),
                "",
                Some(path.as_str()),
                Some(sid.as_str()),
                "edit",
            );
            let idx = mem.root.join("INDEX.md");
            let stale = fs::metadata(&idx)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| now() - d.as_secs_f64() > 600.0)
                .unwrap_or(true);
            if stale {
                let _ = mem.write_index();
            }
        }
    }
    String::new()
}

pub(super) fn session_end(ev: &Value, cfg: &Value) -> String {
    let sid = ev.s("session_id");
    if let Some(mem) = _memory(cfg) {
        mem.release(Some(sid.as_str()));
    }

    let mut sessions = _sessions();
    let mut changed = false;
    if let Some(rec) = sessions
        .as_object_mut()
        .and_then(|o| o.get_mut(&sid))
        .and_then(|v| v.as_object_mut())
    {
        rec.insert("ended".to_string(), json!(now()));
        rec.remove("agents");
        changed = true;
    }
    if changed {
        _save_sessions(&sessions);
    }
    String::new()
}
