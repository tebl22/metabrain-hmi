//! Pure task helpers: event formatting, Claude stream-json parser, Git wrappers,
//! follow-up context compaction, and cached project briefs.

use crate::util::{data_dir, expand_s, hidden, now, read_json, truthy, J};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn can_edit(kind: &str) -> bool {
    kind == "claude_code" || crate::cli_agents::Agent::from_kind(kind).is_some()
}

pub(crate) fn can_run_task_on(kind: &str, mode: &str) -> bool {
    can_edit(kind) || (mode == "plan" && kind == "openai")
}

pub fn turn_busy(status: &str) -> bool {
    matches!(
        status,
        "queued" | "running" | "verifying" | "stopping" | "planning" | "synthesizing"
    )
}

pub(crate) fn mk_event_pub(kind: &str, extra: Value) -> Value {
    mk_event(kind, extra)
}

pub(crate) fn brain_parallel_limit_pub(cfg: &Value) -> usize {
    brain_parallel_limit(cfg)
}

pub(super) fn put(v: &mut Value, k: &str, val: Value) {
    if v.is_null() {
        *v = Value::Object(Map::new());
    }
    if let Some(o) = v.as_object_mut() {
        o.insert(k.to_string(), val);
    }
}

/// Build an event object: ts + type + extra fields.
pub(super) fn mk_event(kind: &str, extra: Value) -> Value {
    let mut m = Map::new();
    m.insert("ts".to_string(), json!(now()));
    m.insert("type".to_string(), json!(kind));
    if let Value::Object(o) = extra {
        for (k, v) in o {
            m.insert(k, v);
        }
    }
    Value::Object(m)
}

pub(super) fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect()
}

pub(super) fn terminate_tree(pid: u32) {
    if cfg!(windows) {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/T", "/F", "/PID", &pid.to_string()]);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        hidden(&mut cmd);
        let _ = cmd.status();
    } else {
        // Kill the child's entire process group created by `util::hidden` (`process_group(0)`),
        // then also signal the leader PID directly as a fallback.
        let pgid = format!("-{pid}");
        let mut gcmd = Command::new("kill");
        gcmd.args(["-9", &pgid])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = gcmd.status();
        let mut pcmd = Command::new("kill");
        pcmd.args(["-9", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = pcmd.status();
    }
}

pub(super) fn new_id() -> String {
    // 10 random hex digits
    format!("{:010x}", rand::random::<u64>() & 0xff_ffff_ffff)
}

pub(super) fn brain_kind(cfg: &Value) -> String {
    let k = cfg.s("kind").to_lowercase();
    if crate::cli_agents::Agent::from_kind(&k).is_some() {
        return k;
    }
    if k.contains("openai") || k.contains("deepseek") || k.contains("compatible") {
        return "openai".to_string();
    }
    if k.contains("claude") {
        return "claude_code".to_string();
    }
    let base = cfg.s("base_url").to_lowercase();
    if !base.is_empty() && !base.contains("anthropic") {
        return "openai".to_string();
    }
    "claude_code".to_string()
}

/// Parallel slot count for a brain config.
pub(super) fn brain_parallel_limit(cfg: &Value) -> usize {
    if let Some(n) = cfg.get("max_parallel").and_then(|v| v.as_f64()) {
        let n = n as usize;
        if n >= 1 {
            return n;
        }
    }
    let k = cfg.s("kind").to_lowercase();
    let k = if k.is_empty() { brain_kind(cfg) } else { k };
    if k.contains("claude") || k == "codex" || k == "gemini_cli" || k == "antigravity" {
        2
    } else if k.contains("openai") || k.contains("deepseek") || k.contains("compatible") {
        4
    } else if k.contains("anthropic") {
        2
    } else if k.contains("ollama") {
        1
    } else if k.contains("opencode") {
        2
    } else {
        1
    }
}

pub type StreamLineParsed = (Vec<Value>, Option<String>, Option<(i64, f64, bool)>);

/// Parse one line of `claude --output-format stream-json`.
/// Returns (events, session_id, (tokens, cost_usd, is_error)).
pub fn events_from_line(line: &str) -> StreamLineParsed {
    let mut evs: Vec<Value> = Vec::new();
    let mut session: Option<String> = None;
    let mut usage: Option<(i64, f64, bool)> = None;

    let v: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return (evs, session, usage),
    };

    match v.s("type").as_str() {
        "system" => {
            if v.s("subtype") == "init" {
                if let Some(sid) = v.os("session_id") {
                    session = Some(sid);
                }
            }
        }
        "assistant" => {
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            for b in content {
                match b.s("type").as_str() {
                    "text" => {
                        let t = b.s("text");
                        if !t.is_empty() {
                            evs.push(mk_event("text", json!({ "text": t })));
                        }
                    }
                    "thinking" => {
                        let raw = b.s("thinking");
                        let t = if raw.is_empty() { b.s("text") } else { raw };
                        if !t.is_empty() {
                            evs.push(mk_event("thinking", json!({ "text": clip(&t, 2000) })));
                        }
                    }
                    "tool_use" => {
                        let name = b.s("name");
                        let input = b.get("input").cloned().unwrap_or(Value::Null);
                        let desc = input.s("description");
                        let is_sub = name == "Agent" || name == "Task";
                        evs.push(mk_event(
                            "tool_use",
                            json!({ "name": name, "input": input, "tool_id": b["id"] }),
                        ));
                        if is_sub && !desc.is_empty() {
                            evs.push(mk_event("subagent", json!({ "text": desc })));
                        }
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            for b in content {
                if b.s("type") != "tool_result" {
                    continue;
                }
                let c = b.get("content").cloned().unwrap_or(Value::Null);
                let text = match c {
                    Value::String(s) => s,
                    Value::Array(a) => a
                        .iter()
                        .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                evs.push(mk_event("tool_result", json!({ "text": clip(&text, 4000), "tool_id": b["tool_use_id"], "error": b["is_error"] == true })));
            }
        }
        "result" => {
            let u = v.get("usage");
            let g = |k: &str| {
                u.and_then(|x| x.get(k))
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0)
            };
            let tokens = (g("input_tokens")
                + g("output_tokens")
                + g("cache_read_input_tokens")
                + g("cache_creation_input_tokens")) as i64;
            let cost = v
                .get("total_cost_usd")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0);
            let is_err = truthy(v.get("is_error"));
            if is_err {
                let t = v.s("result");
                let txt = if t.is_empty() {
                    "La tarea terminó con error.".to_string()
                } else {
                    clip(&t, 2000)
                };
                evs.push(mk_event("error", json!({ "text": txt })));
            }
            usage = Some((tokens, cost, is_err));
        }
        _ => {}
    }
    (evs, session, usage)
}

pub(super) fn git_cmd(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for a in args {
        c.arg(a);
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hidden(&mut c);
    c.output().ok()
}

pub(super) fn checked_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_cmd(dir, args).ok_or_else(|| "No se pudo ejecutar Git.".to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "Git {}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Locate the claude executable: MB_CLAUDE, PATH, then ~/.local/bin.
pub(super) fn resolve_claude() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("MB_CLAUDE") {
        let p = expand_s(&v);
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd", "claude"]
    } else {
        &["claude"]
    };
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            for n in names {
                let c = d.join(n);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    let home = crate::util::home();
    let n = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let c = home.join(".local").join("bin").join(n);
    if c.is_file() {
        return Some(c);
    }
    None
}

/// Brain config for a stored task (used when continuing a conversation).
pub(super) fn find_brain_cfg(brain_id: &str) -> Value {
    let cfg = read_json(&data_dir().join("metabrain.json"))
        .or_else(|| read_json(&data_dir().join("config.json")))
        .unwrap_or(Value::Null);
    if brain_id.is_empty() {
        return Value::Null;
    }
    for b in cfg.arr("brains") {
        if b.s("id") == brain_id || b.s("name") == brain_id {
            return b;
        }
    }
    for b in cfg.arr("ladder") {
        if b.s("id") == brain_id || b.s("name") == brain_id {
            return b;
        }
    }
    Value::Null
}

/// Keeps recent exchanges and older excerpts within a fixed context budget.
pub(super) fn follow_up_prompt(original: &str, events: &[Value], text: &str) -> String {
    let clip_boundary = |text: &str, max_chars: usize, suffix: &str| -> String {
        let trimmed = text.trim();
        if trimmed.chars().count() <= max_chars {
            return trimmed.to_string();
        }
        let mut end_byte = trimmed.len();
        let mut last_para = None;
        let mut last_line = None;
        let mut last_space = None;
        let mut prev_nl = false;
        for (count, (idx, ch)) in trimmed.char_indices().enumerate() {
            if count >= max_chars {
                end_byte = idx;
                break;
            }
            if ch == '\n' {
                if prev_nl && count >= max_chars / 2 {
                    last_para = Some(idx);
                }
                if count >= max_chars / 2 {
                    last_line = Some(idx);
                }
                prev_nl = true;
            } else {
                prev_nl = false;
                if ch.is_whitespace() && count >= max_chars * 2 / 3 {
                    last_space = Some(idx);
                }
            }
        }
        let cut = last_para.or(last_line).or(last_space).unwrap_or(end_byte);
        format!("{}{suffix}", trimmed[..cut].trim_end())
    };

    let summarize_older = |turns: &[(String, String)]| -> String {
        let mut lines = Vec::new();
        let mut files = Vec::new();
        for (user, answer) in turns {
            if !user.is_empty() {
                lines.push(format!("- Usuario: {}", clip_boundary(user, 160, "…")));
            }
            for raw in answer.lines() {
                let l = raw.trim();
                if l.is_empty() {
                    continue;
                }
                let low = l.to_lowercase();
                if (l.starts_with('#')
                    || low.starts_with("decisión:")
                    || low.starts_with("decision:")
                    || low.starts_with("pendiente:")
                    || low.starts_with("todo")
                    || low.starts_with("- **"))
                    && lines.len() < 12
                {
                    lines.push(format!("  · {}", clip_boundary(l, 160, "…")));
                }
                for tok in l.split_whitespace() {
                    let clean = tok.trim_matches(|c: char| {
                        matches!(
                            c,
                            '`' | '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '.'
                        )
                    });
                    if clean.len() > 3
                        && (clean.contains('/')
                            || clean.contains('\\')
                            || clean.ends_with(".rs")
                            || clean.ends_with(".js")
                            || clean.ends_with(".toml")
                            || clean.ends_with(".json")
                            || clean.ends_with(".ps1"))
                        && !clean.starts_with("http")
                        && !files.iter().any(|f| f == clean)
                        && files.len() < 10
                    {
                        files.push(clean.to_string());
                    }
                }
            }
            if answer.lines().count() <= 3 && !answer.trim().is_empty() && lines.len() < 12 {
                lines.push(format!(
                    "  · Asistente: {}",
                    clip_boundary(answer, 200, "…")
                ));
            }
        }
        if lines.is_empty() && files.is_empty() {
            return String::new();
        }
        let mut out = String::from("Resumen de turnos anteriores:\n");
        for l in lines {
            out.push_str(&l);
            out.push('\n');
        }
        if !files.is_empty() {
            out.push_str(&format!("- Archivos mencionados: {}\n", files.join(", ")));
        }
        clip_boundary(&out, 2_400, "\n… [resumen recortado]")
    };

    let mut turns = vec![(String::new(), String::new())];
    for event in events.iter().filter(|event| event.s("type") == "text") {
        let value = event.s("text");
        if let Some(user) = value.strip_prefix("Tú: ") {
            turns.push((clip_boundary(user, 1_800, "…"), String::new()));
        } else if let Some((_, answer)) = turns.last_mut() {
            if answer.chars().count() < 8_000 {
                answer.push_str(&value);
                if !event.b("delta", false) && !value.ends_with('\n') {
                    answer.push('\n');
                }
            }
        }
    }
    // Drop empty initial placeholder if there were no assistant messages before the first follow-up.
    if turns.len() > 1 && turns[0].0.is_empty() && turns[0].1.trim().is_empty() {
        turns.remove(0);
    }

    const HISTORY_BUDGET: usize = 16_000;
    const RECENT_TURNS: usize = 4;
    let recent = turns.len().saturating_sub(RECENT_TURNS);
    let mut context = String::new();
    if recent > 0 {
        let older_start = recent.saturating_sub(8);
        let summary = summarize_older(&turns[older_start..recent]);
        if !summary.is_empty() {
            context.push_str(&summary);
            context.push('\n');
        }
    }
    let last_idx = turns.len().saturating_sub(1);
    for (offset, (user, answer)) in turns[recent..].iter().enumerate() {
        let is_last = recent + offset == last_idx;
        if !user.is_empty() {
            let cap = if is_last { 1_800 } else { 900 };
            context.push_str(&format!("Usuario: {}\n", clip_boundary(user, cap, "…")));
        }
        let ans = answer.trim();
        if !ans.is_empty() {
            let (cap, suffix) = if is_last {
                (6_000, "\n… [recortado]")
            } else {
                (1_600, "…")
            };
            context.push_str(&format!("Asistente: {}\n", clip_boundary(ans, cap, suffix)));
        }
    }
    let context = clip_boundary(&context, HISTORY_BUDGET, "\n… [historial recortado]");
    format!(
        "Continúa esta conversación en la misma carpeta.\n\nPedido original:\n{}\n\nHistorial:\n{context}\n\nNuevo mensaje del usuario:\n{text}",
        clip_boundary(original, 2_500, "…")
    )
}

/// Build the initial system prompt from the fixed instructions and the project brief.
pub fn cached_system(cwd: &str) -> String {
    let mut s = String::from(
        "Eres un ingeniero de software que trabaja dentro de MetaBrain. Responde en español, directo y completo. \
Cuando propongas cambios de código, muestra el archivo y el bloque exacto a cambiar.",
    );
    let brief = project_brief(cwd);
    if !brief.is_empty() {
        s.push_str("\n\n## Proyecto\n");
        s.push_str(&brief);
    }
    s
}

fn project_brief(cwd: &str) -> String {
    let dir = PathBuf::from(cwd);
    if cwd.trim().is_empty() || !dir.is_dir() {
        return String::new();
    }
    let key: u64 = cwd.bytes().fold(1469598103934665603u64, |h, b| {
        (h ^ b as u64).wrapping_mul(1099511628211)
    });
    let cache = crate::util::data_dir()
        .join("briefs")
        .join(format!("{key:x}.txt"));
    if let Ok(md) = fs::metadata(&cache) {
        if md
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|e| e.as_secs() < 86400)
            .unwrap_or(false)
        {
            if let Ok(t) = fs::read_to_string(&cache) {
                return t;
            }
        }
    }
    let mut out = format!("Carpeta: {cwd}\n");
    // Top-level layout (two levels, skipping noise).
    let skip = [
        "target",
        "node_modules",
        ".git",
        "__pycache__",
        "dist",
        "build",
        ".venv",
        "venv",
    ];
    let mut lines = vec![];
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut ents: Vec<_> = rd.flatten().collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents.iter().take(60) {
            let n = e.file_name().to_string_lossy().to_string();
            if skip.contains(&n.as_str()) || n.starts_with('.') {
                continue;
            }
            if e.path().is_dir() {
                let mut kids: Vec<String> = fs::read_dir(e.path())
                    .map(|r| {
                        r.flatten()
                            .map(|k| k.file_name().to_string_lossy().to_string())
                            .filter(|k| !k.starts_with('.'))
                            .collect()
                    })
                    .unwrap_or_default();
                kids.sort();
                let more = if kids.len() > 12 {
                    format!(", … ({} más)", kids.len() - 12)
                } else {
                    String::new()
                };
                kids.truncate(12);
                lines.push(format!("{n}/: {}{more}", kids.join(", ")));
            } else {
                lines.push(n);
            }
        }
    }
    out.push_str(&lines.join("\n"));
    for readme in [
        "README.md",
        "readme.md",
        "README.txt",
        "HANDOFF_METABRAIN.md",
        "CLAUDE.md",
    ] {
        if let Ok(t) = fs::read_to_string(dir.join(readme)) {
            let t: String = t.chars().take(3000).collect();
            out.push_str(&format!("\n\n### {readme}\n{t}"));
            break;
        }
    }
    let _ = fs::create_dir_all(cache.parent().unwrap_or(&dir));
    let _ = fs::write(&cache, &out);
    out
}
