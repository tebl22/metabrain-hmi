//! CLI agents that use the user's consumer accounts (no API keys):
//! ChatGPT via the Codex CLI, Google via the Antigravity CLI (`agy`, which replaced the
//! Gemini CLI for personal accounts) and the legacy Gemini CLI.
//! Both run non-interactively with the prompt delivered on STDIN, so we never
//! hit the Windows command-line length limit.

mod parse;
mod quota;
#[cfg(test)]
mod tests;

pub use parse::{is_limit_error, parse_line, parse_stream};
pub(crate) use quota::codex_turn_settings;
pub use quota::{
    antigravity_quota, cached_antigravity_quota, codex_account, codex_home, codex_quota,
    parse_antigravity_usage, QuotaLimit,
};

use crate::util::hidden;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    Codex,
    Gemini,
    Antigravity,
}

impl Agent {
    /// Map a brain `kind` from config to an agent.
    pub fn from_kind(kind: &str) -> Option<Agent> {
        let k = kind.trim().to_lowercase();
        match k.as_str() {
            "codex" => Some(Agent::Codex),
            "gemini_cli" | "gemini" => Some(Agent::Gemini),
            "antigravity" | "agy" | "google" => Some(Agent::Antigravity),
            _ => None,
        }
    }

    pub fn bin_name(self) -> &'static str {
        match self {
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::Antigravity => "agy",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Agent::Codex => "ChatGPT (Codex CLI)",
            Agent::Gemini => "Google (Gemini CLI)",
            Agent::Antigravity => "Google (Antigravity)",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Agent::Codex => "npm install -g @openai/codex",
            Agent::Gemini => "npm install -g @google/gemini-cli",
            Agent::Antigravity => "irm https://antigravity.google/cli/install.ps1 | iex",
        }
    }
}

/// One decoded output line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Text(String),
    Thinking(String),
    Tool(String),
    ToolStarted(Value),
    ToolFinished(Value),
    Usage {
        input: u64,
        output: u64,
        cached: u64,
    },
    Done {
        ok: bool,
        error: Option<String>,
    },
}

// ---------------------------------------------------------------- discovery

fn candidate_names(agent: Agent) -> Vec<&'static str> {
    if !cfg!(windows) {
        return vec![agent.bin_name()];
    }
    match agent {
        Agent::Codex => vec!["codex.cmd", "codex.exe", "codex.bat", "codex"],
        Agent::Gemini => vec!["gemini.cmd", "gemini.exe", "gemini.bat", "gemini"],
        Agent::Antigravity => vec!["agy.exe", "agy.cmd", "agy"],
    }
}

/// Locate the executable in PATH, then in %APPDATA%\npm (where npm installs .cmd shims).
pub fn find_bin(agent: Agent) -> Option<PathBuf> {
    let names = candidate_names(agent);
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for n in &names {
                let c = dir.join(n);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    // The Antigravity installer puts agy in %LOCALAPPDATA%\agy\bin (or ~/.local/bin).
    if agent == Agent::Antigravity {
        let mut dirs = vec![crate::util::home().join(".local").join("bin")];
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.insert(0, PathBuf::from(l).join("agy").join("bin"));
        }
        for d in dirs {
            for n in &names {
                let c = d.join(n);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let npm = PathBuf::from(appdata).join("npm");
            for n in &names {
                let c = npm.join(n);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    None
}

/// Extract a semver-like version token (e.g. `0.171.0`, `1.5.16-beta.1`) from `--version` output.
pub fn parse_cli_version(raw: &str) -> Option<String> {
    for line in raw.lines() {
        for tok in line.split_whitespace() {
            let t = tok
                .trim_matches(|c: char| matches!(c, 'v' | 'V' | ',' | ';' | '(' | ')' | '[' | ']'));
            let mut parts = t.split('.');
            let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
                continue;
            };
            if !a.is_empty()
                && a.chars().all(|c| c.is_ascii_digit())
                && !b.is_empty()
                && b.chars().take_while(|c| c.is_ascii_digit()).count() > 0
            {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Detect the installed version of a CLI agent (`<bin> --version`), cached once per process.
pub fn cli_version(agent: Agent) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<Agent, Option<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(g) = cache.lock() {
        if let Some(v) = g.get(&agent) {
            return v.clone();
        }
    }
    let bin = find_bin(agent)?;
    let mut cmd = if is_cmd_script(&bin) {
        let mut c = Command::new("cmd");
        c.arg("/d").arg("/c").arg(&bin).arg("--version");
        c
    } else {
        let mut c = Command::new(&bin);
        c.arg("--version");
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _ = hidden(&mut cmd);
    let ver = cmd.output().ok().and_then(|out| {
        let s = String::from_utf8_lossy(&out.stdout);
        parse_cli_version(&s).or_else(|| parse_cli_version(&String::from_utf8_lossy(&out.stderr)))
    });
    if let Ok(mut g) = cache.lock() {
        g.insert(agent, ver.clone());
    }
    ver
}

/// Are the user's credentials present (respecting explicit per-run `env` overrides such as `CODEX_HOME`)?
pub fn logged_in_with_env(agent: Agent, env: &[(String, String)]) -> bool {
    let h = crate::util::home();
    match agent {
        Agent::Codex => {
            let home = env
                .iter()
                .rev()
                .find(|(k, _)| k == "CODEX_HOME")
                .map(|(_, v)| PathBuf::from(v))
                .unwrap_or_else(|| h.join(".codex"));
            home.join("auth.json").is_file()
        }
        Agent::Gemini => h.join(".gemini").join("oauth_creds.json").is_file(),
        // Signed in through MetaBrain (or a credentials file of the CLI itself).
        Agent::Antigravity => {
            crate::util::data_dir().join("agy_login.json").is_file()
                || std::fs::read_dir(h.join(".gemini").join("antigravity-cli"))
                    .map(|rd| {
                        rd.flatten().any(|e| {
                            let n = e.file_name().to_string_lossy().to_lowercase();
                            n.contains("oauth") || n.contains("cred") || n.contains("token")
                        })
                    })
                    .unwrap_or(false)
        }
    }
}

/// Are the user's credentials present in the default account location?
pub fn logged_in(agent: Agent) -> bool {
    logged_in_with_env(agent, &[])
}

/// The command to show the user so they can authenticate.
pub fn login_command(agent: Agent) -> String {
    match agent {
        Agent::Codex => "codex login".to_string(),
        Agent::Gemini => "gemini".to_string(),
        Agent::Antigravity => "Cerebros › Iniciar sesión con Google".to_string(),
    }
}

// ---------------------------------------------------------------- command

/// Typed execution privilege mode shared across CLI runners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionMode {
    /// Read-only analysis and planning.
    Plan,
    /// Workspace write access.
    Write,
}

impl ExecutionMode {
    pub fn from_str_lossy(mode: &str) -> Self {
        match mode.trim().to_ascii_lowercase().as_str() {
            "write" | "build" => ExecutionMode::Write,
            _ => ExecutionMode::Plan,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionMode::Plan => "read",
            ExecutionMode::Write => "write",
        }
    }
}

/// Validate a CLI model name against flag injection and shell metacharacters.
pub fn sanitize_model(model: &str) -> Result<String, String> {
    let m = model.trim();
    if m.is_empty() {
        return Ok(String::new());
    }
    if m.len() > 128 {
        return Err("Model name is too long.".into());
    }
    if m.starts_with('-') {
        return Err("Model name cannot start with '-'.".into());
    }
    if !m
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/'))
    {
        return Err(format!("Model name contains disallowed characters: {m}"));
    }
    Ok(m.to_string())
}

/// Validate a working directory before passing it to a CLI shim (`cmd.exe` on Windows).
pub fn sanitize_cwd(cwd: &str, for_cmd_shim: bool) -> Result<String, String> {
    let c = cwd.trim();
    if c.is_empty() {
        return Ok(String::new());
    }
    if c.chars().any(|ch| ch.is_control() || ch == '\0') {
        return Err("Working directory contains control characters.".into());
    }
    if for_cmd_shim
        && c.chars()
            .any(|ch| matches!(ch, '&' | '|' | '^' | '%' | '!' | '<' | '>' | '"' | ';'))
    {
        return Err(
            "Working directory contains characters unsafe for Windows command shims.".into(),
        );
    }
    Ok(c.to_string())
}

pub(super) fn is_cmd_script(p: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}

/// Argument vector (without program) for an agent invocation using explicit policy.
/// `allow_unsafe` must be explicitly enabled by configuration before bypassing sandboxes/permissions.
pub fn argv_with_policy(
    agent: Agent,
    cwd: &str,
    model: &str,
    mode: ExecutionMode,
    allow_unsafe: bool,
) -> Vec<String> {
    let model = sanitize_model(model).unwrap_or_default();
    let cwd = sanitize_cwd(cwd, false).unwrap_or_default();
    let write = mode == ExecutionMode::Write;
    match agent {
        Agent::Codex => {
            let mut a: Vec<String> = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
            ];
            if !cwd.is_empty() {
                a.push("-C".into());
                a.push(cwd);
            }
            if !model.is_empty() {
                a.push("-m".into());
                a.push(model);
            }
            let write_box = if allow_unsafe {
                "danger-full-access"
            } else {
                "workspace-write"
            };
            a.push("--sandbox".into());
            a.push(if write { write_box } else { "read-only" }.to_string());
            a.push("-".into()); // read prompt from stdin
            a
        }
        Agent::Gemini => {
            let mut a: Vec<String> = vec!["--output-format".into(), "stream-json".into()];
            if !model.is_empty() {
                a.push("-m".into());
                a.push(model);
            }
            a.push("--approval-mode".into());
            a.push(if write { "auto_edit" } else { "plan" }.to_string());
            a.push("-p".into());
            a.push(String::new()); // force non-interactive; real prompt goes on stdin
            a
        }
        Agent::Antigravity => {
            // Prompt as one stream-json user event on stdin (no command-line length limit).
            let mut a: Vec<String> = vec![
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
            ];
            if !model.is_empty() {
                a.push("--model".into());
                a.push(model);
            }
            if write {
                if allow_unsafe {
                    a.push("--dangerously-skip-permissions".into());
                } else {
                    a.push("--mode".into());
                    a.push("auto".into());
                }
            } else {
                a.push("--mode".into());
                a.push("plan".into());
            }
            a
        }
    }
}

/// Argument vector (without program) for an agent invocation (safe defaults: `allow_unsafe = false`).
pub fn argv(agent: Agent, cwd: &str, model: &str, mode: &str) -> Vec<String> {
    argv_with_policy(
        agent,
        cwd,
        model,
        ExecutionMode::from_str_lossy(mode),
        false,
    )
}

/// What goes on the agent's stdin for this prompt.
pub fn stdin_payload(agent: Agent, prompt: &str) -> String {
    match agent {
        Agent::Antigravity => format!(
            "{}\n",
            serde_json::json!({"event": "user", "message": {"content": prompt}})
        ),
        _ => format!("{prompt}\n"),
    }
}

/// Build a hidden process with piped stdio and explicit extra environment variables.
/// The prompt is written by `run_blocking_until`.
pub(super) fn build_cmd_with_policy(
    agent: Agent,
    cwd: &str,
    model: &str,
    effort: &str,
    mode: ExecutionMode,
    allow_unsafe: bool,
    env: &[(String, String)],
) -> Result<Command, String> {
    let bin = find_bin(agent).unwrap_or_else(|| PathBuf::from(agent.bin_name()));
    let cmd_shim = is_cmd_script(&bin);
    let clean_model = sanitize_model(model)?;
    let clean_cwd = sanitize_cwd(cwd, cmd_shim)?;
    let mut args = argv_with_policy(agent, &clean_cwd, &clean_model, mode, allow_unsafe);
    if agent == Agent::Codex
        && matches!(
            effort,
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        )
    {
        args.splice(
            1..1,
            ["-c".into(), format!("model_reasoning_effort={effort}")],
        );
    }

    let mut cmd = if cmd_shim {
        let mut c = Command::new("cmd");
        c.arg("/d").arg("/c").arg(&bin);
        c
    } else {
        Command::new(&bin)
    };
    for a in &args {
        cmd.arg(a);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    if !clean_cwd.is_empty() {
        cmd.current_dir(&clean_cwd);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _ = hidden(&mut cmd);
    Ok(cmd)
}

#[cfg(test)]
pub(super) fn build_cmd(
    agent: Agent,
    cwd: &str,
    model: &str,
    effort: &str,
    mode: &str,
    env: &[(String, String)],
) -> Command {
    build_cmd_with_policy(
        agent,
        cwd,
        model,
        effort,
        ExecutionMode::from_str_lossy(mode),
        false,
        env,
    )
    .expect("valid test command")
}

// ---------------------------------------------------------------- runner

fn wait_status(child: &mut Child, dur: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + dur;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Run an agent to completion, streaming events. No panics; friendly Spanish errors.
pub fn run_blocking(
    agent: Agent,
    prompt: &str,
    cwd: &str,
    model: &str,
    mode: &str,
    timeout_s: u64,
    on_event: &mut dyn FnMut(Event),
) -> Result<String, String> {
    run_blocking_pid(
        agent,
        prompt,
        cwd,
        model,
        "",
        mode,
        timeout_s,
        on_event,
        &mut |_| true,
    )
}

fn kill_child_tree(child: &mut Child) {
    let pid = child.id();
    if cfg!(windows) {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = hidden(&mut cmd);
        let _ = cmd.status();
    }
    let _ = child.kill();
    let _ = wait_status(child, Duration::from_secs(2));
}

/// Reports the child's process ID. Returning false rejects a process started after interruption.
#[allow(clippy::too_many_arguments)]
pub fn run_blocking_pid(
    agent: Agent,
    prompt: &str,
    cwd: &str,
    model: &str,
    effort: &str,
    mode: &str,
    timeout_s: u64,
    on_event: &mut dyn FnMut(Event),
    on_pid: &mut dyn FnMut(u32) -> bool,
) -> Result<String, String> {
    run_blocking_until(
        agent,
        prompt,
        cwd,
        model,
        effort,
        mode,
        &[],
        timeout_s,
        on_event,
        on_pid,
        &mut || false,
    )
}

/// Like `run_blocking_pid`, accepting explicit per-run `env` variables (e.g. `CODEX_HOME`)
/// and polling `should_stop` so user interruptions terminate promptly even if a grandchild holds stdout open.
#[allow(clippy::too_many_arguments)]
pub fn run_blocking_until(
    agent: Agent,
    prompt: &str,
    cwd: &str,
    model: &str,
    effort: &str,
    mode: &str,
    env: &[(String, String)],
    timeout_s: u64,
    on_event: &mut dyn FnMut(Event),
    on_pid: &mut dyn FnMut(u32) -> bool,
    should_stop: &mut dyn FnMut() -> bool,
) -> Result<String, String> {
    if find_bin(agent).is_none() {
        return Err(format!(
            "{} no está instalado. Instálalo con: {}",
            agent.display_name(),
            agent.install_hint()
        ));
    }
    if !logged_in_with_env(agent, env) {
        return Err(format!(
            "Sin sesión. Inicia sesión con: {}",
            login_command(agent)
        ));
    }

    let allow_unsafe = crate::util::read_json(&crate::util::data_dir().join("metabrain.json"))
        .and_then(|v| v.get("allow_unsafe_exec").and_then(|b| b.as_bool()))
        .unwrap_or(false);
    let mut cmd = build_cmd_with_policy(
        agent,
        cwd,
        model,
        effort,
        ExecutionMode::from_str_lossy(mode),
        allow_unsafe,
        env,
    )?;
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not launch {}: {e}", agent.display_name()))?;
    let _job = crate::util::attach_kill_job(&child).ok();
    if !on_pid(child.id()) || should_stop() {
        kill_child_tree(&mut child);
        return Err("Interrupted".into());
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();

    let (tx, rx) = mpsc::channel::<String>();
    if let Some(out) = stdout {
        std::thread::spawn(move || {
            let reader = BufReader::new(out);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let err_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(se) = stderr {
        let buf = Arc::clone(&err_buf);
        std::thread::spawn(move || {
            let reader = BufReader::new(se);
            for l in reader.lines().map_while(Result::ok) {
                if let Ok(mut b) = buf.lock() {
                    b.push_str(&l);
                    b.push('\n');
                    if b.len() > 8000 {
                        let mut cut = b.len() - 8000;
                        while cut < b.len() && !b.is_char_boundary(cut) {
                            cut += 1;
                        }
                        b.drain(..cut);
                    }
                }
            }
        });
    }

    // Deliver the prompt asynchronously AFTER stdout/stderr readers are attached
    // so large prompts (>4 KB) cannot deadlock OS pipes.
    if let Some(mut sin) = stdin {
        let payload = stdin_payload(agent, prompt);
        std::thread::spawn(move || {
            let _ = sin.write_all(payload.as_bytes());
            let _ = sin.flush();
        });
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_s.max(1));
    let mut text = String::new();
    let mut done: Option<(bool, Option<String>)> = None;

    loop {
        if should_stop() {
            kill_child_tree(&mut child);
            return Err("Interrumpida".into());
        }
        let now = Instant::now();
        if now >= deadline {
            kill_child_tree(&mut child);
            return Err("Se agotó el tiempo de espera del agente.".to_string());
        }
        let wait = (deadline - now).min(Duration::from_millis(100));
        match rx.recv_timeout(wait) {
            Ok(line) => parse::absorb(agent, &line, &mut *on_event, &mut text, &mut done),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Process gone but a grandchild may keep the pipe open: drain and stop.
                if let Ok(Some(_)) = child.try_wait() {
                    while let Ok(line) = rx.try_recv() {
                        parse::absorb(agent, &line, &mut *on_event, &mut text, &mut done);
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Small grace period so the reader thread can flush trailing lines.
    std::thread::sleep(Duration::from_millis(30));
    while let Ok(line) = rx.try_recv() {
        parse::absorb(agent, &line, &mut *on_event, &mut text, &mut done);
    }

    let status = match wait_status(&mut child, Duration::from_secs(5)) {
        Some(st) => Some(st),
        None => {
            let _ = child.kill();
            child.wait().ok()
        }
    };
    let exit_ok = status.map(|s| s.success()).unwrap_or(false);

    let err_text = err_buf
        .lock()
        .map(|b| b.trim().to_string())
        .unwrap_or_default();

    let (ok, err) = match done {
        Some((ok, e)) => (ok, e),
        None => (
            exit_ok,
            if exit_ok || err_text.is_empty() {
                None
            } else {
                Some(err_text.clone())
            },
        ),
    };

    let mut combined = String::new();
    if let Some(e) = &err {
        combined.push_str(e);
    }
    combined.push('\n');
    combined.push_str(&err_text);
    if is_limit_error(&combined) {
        let msg = err
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| err_text.clone());
        let msg = msg.trim().to_string();
        return Err(if msg.is_empty() {
            "LIMITE: límite de uso alcanzado.".to_string()
        } else {
            format!("LIMITE: {msg}")
        });
    }

    if ok {
        Ok(text)
    } else {
        let msg = err
            .filter(|s| !s.trim().is_empty())
            .or({
                if err_text.is_empty() {
                    None
                } else {
                    Some(err_text)
                }
            })
            .unwrap_or_else(|| "El agente terminó con error.".to_string());
        Err(msg)
    }
}
