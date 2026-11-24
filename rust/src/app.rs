//! CLI entry points for MetaBrain: serve, hook, hooks install, note, run,
//! autostart, version. Mirrors metabrain/app.py against the Rust modules
//! (core::Supervisor, server::serve, hooks::*, memory::Memory).

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::util::{data_dir, expand, expand_s, http_json, norm, now, read_json, truthy, J};

const EXIT_PAUSED: i32 = 75;
const DEFAULT_PORT: u16 = 8790;

const HELP: &str = "MetaBrain

  metabrain serve [--port N] [--lan] [--no-browser] [--no-tray]
  metabrain mcp
  metabrain hook <event>
  metabrain hooks install [--dry-run]
  metabrain note <brain> \"text\" [file ...]
  metabrain run <brain> -- cmd ...
  metabrain autostart on|off
  metabrain route [--project P] [--level N] <tarea>
  metabrain outcome --project P --level N --candidate C ok|fail [--tokens T]
  metabrain version
";

// ---------- tiny output helpers (never panic on a closed stdout) ----------
fn emit(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", s);
}
fn emit_raw(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(s.as_bytes());
}
fn emit_err(s: &str) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{}", s);
}
fn emit_err_raw(s: &str) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = err.write_all(s.as_bytes());
}

// ---------- environment operations for `run` (pure, testable) ----------
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvOp {
    Set(String, String),
    Remove(String),
}

/// Find a brain by id, case-insensitively (like _brain_cfg).
fn select_brain(cfg: &Value, bid: &str) -> Option<Value> {
    let target = bid.to_lowercase();
    cfg.arr("brains")
        .into_iter()
        .find(|b| b.s("id").to_lowercase() == target)
}

/// Resolve `master`: a fresh gate wins, otherwise the first configured priority id.
fn resolve_master(cfg: &Value, gate: &Value) -> Option<String> {
    let gid = gate.os("master");
    let fresh = gate.f("ts").map(|t| now() - t <= 120.0).unwrap_or(false);
    if let Some(id) = gid {
        if fresh && select_brain(cfg, &id).is_some() {
            return Some(id);
        }
    }
    for v in cfg
        .get("priority")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let Some(id) = v.as_str() {
            if select_brain(cfg, id).is_some() {
                return Some(id.to_string());
            }
        }
    }
    cfg.arr("brains").into_iter().find_map(|b| b.os("id"))
}

fn is_claude_command(cmd: &[String]) -> bool {
    let first = match cmd.first() {
        Some(f) => f.as_str(),
        None => return false,
    };
    if first.eq_ignore_ascii_case("claude") {
        return true;
    }
    if first.eq_ignore_ascii_case("cmd") {
        return cmd.len() >= 3
            && cmd[1].eq_ignore_ascii_case("/c")
            && cmd[2].eq_ignore_ascii_case("claude");
    }
    false
}

fn prompt_after_print(cmd: &[String]) -> String {
    let ix = match cmd.iter().position(|a| a == "-p" || a == "--print") {
        Some(i) => i,
        None => return String::new(),
    };
    cmd.get(ix + 1..).map(|a| a.join(" ")).unwrap_or_default()
}

fn default_headless() -> Vec<&'static str> {
    if cfg!(windows) {
        vec![
            "powershell",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            "%APPDATA%\\npm\\dsh.ps1",
            "headless",
        ]
    } else {
        vec!["dsh", "headless"]
    }
}

fn command_list(cfg: &Value, key: &str, default: &[&str]) -> Vec<String> {
    let arr = cfg
        .get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let raw: Vec<String> = if arr.is_empty() {
        default.iter().map(|s| s.to_string()).collect()
    } else {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    };
    raw.into_iter().map(|s| expand_s(&s)).collect()
}

/// Same env rules as app.py::run_job, expressed as a list of set/remove ops.
fn job_env_ops(cfg: &Value, b: &Value, g: &Value, state: &Value) -> Vec<EnvOp> {
    let mut ops = Vec::new();
    let id = b.s("id");
    ops.push(EnvOp::Set("METABRAIN_BRAIN".to_string(), id.clone()));
    let kind = b.s("kind");
    if kind == "claude_code" {
        // account = g.account or state.account or b.config_dir or "~/.claude"
        let acct = [g.get("account"), state.get("account"), b.get("config_dir")]
            .into_iter()
            .flatten()
            .find(|v| truthy(Some(*v)))
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "~/.claude".to_string());
        let d = expand(&acct);
        let default_claude = expand("~/.claude");
        if norm(&d) == norm(&default_claude) {
            ops.push(EnvOp::Remove("CLAUDE_CONFIG_DIR".to_string()));
        } else {
            ops.push(EnvOp::Set(
                "CLAUDE_CONFIG_DIR".to_string(),
                d.to_string_lossy().to_string(),
            ));
        }
        let g_model = g.os("model");
        let state_model_or_down = truthy(state.get("model")) || truthy(state.get("downgraded"));
        if let Some(model) = g_model {
            if state_model_or_down {
                ops.push(EnvOp::Set("ANTHROPIC_MODEL".to_string(), model));
            }
        }
        if let Some(t) = state.get("thinking").filter(|v| !v.is_null()) {
            if let Some(n) = t.as_f64() {
                ops.push(EnvOp::Set(
                    "MAX_THINKING_TOKENS".to_string(),
                    format!("{}", n.trunc() as i64),
                ));
            }
        }
    } else {
        let port = cfg
            .f("port")
            .map(|p| p as i64)
            .unwrap_or(DEFAULT_PORT as i64);
        let base = format!("http://127.0.0.1:{port}/proxy/{id}/v1");
        let key = if kind == "anthropic" {
            "ANTHROPIC_BASE_URL"
        } else {
            "OPENAI_BASE_URL"
        };
        ops.push(EnvOp::Set(key.to_string(), base));
    }
    ops
}

// ---------- serve argument parsing (pure, testable) ----------
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeOpts {
    port: Option<u16>,
    browser: bool,
    tray: bool,
    lan: bool,
    /// Also open the web panel (phone / LAN use); the desktop uses the native window.
    web: bool,
}

fn parse_serve_args(argv: &[String]) -> ServeOpts {
    let mut o = ServeOpts {
        port: None,
        browser: true,
        tray: true,
        lan: false,
        web: false,
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--port" => {
                if let Some(v) = argv.get(i + 1) {
                    if let Ok(p) = v.trim().parse::<u16>() {
                        o.port = Some(p);
                    }
                }
                i += 2;
                continue;
            }
            "--lan" => o.lan = true,
            "--no-browser" => o.browser = false,
            "--no-tray" | "--no-window" => o.tray = false,
            "--web" => o.web = true,
            _ => {}
        }
        i += 1;
    }
    o
}

// ---------- subcommands ----------
fn run_hook_cmd(event: Option<&str>) -> i32 {
    let mut input = String::new();
    {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let _ = stdin.read_to_string(&mut input);
    }
    // run_hook returns (exit_code, stdout, stderr).
    let (code, out, err) = crate::hooks::run_hook(event.unwrap_or(""), &input);
    if !out.is_empty() {
        emit_raw(&out);
    }
    if !err.is_empty() {
        emit_err_raw(&err);
    }
    code
}

fn hooks_install(rest: &[String]) -> i32 {
    let dry = rest.iter().any(|a| a == "--dry-run");
    let exe = current_exe_dir()
        .map(|d| {
            d.join(if cfg!(windows) {
                "metabrain-hook.exe"
            } else {
                "metabrain-hook"
            })
        })
        .unwrap_or_else(|| {
            PathBuf::from(if cfg!(windows) {
                "metabrain-hook.exe"
            } else {
                "metabrain-hook"
            })
        });
    let cfg = read_json(&data_dir().join("metabrain.json")).unwrap_or_else(|| json!({}));
    let lines = crate::hooks::install(&cfg, exe.to_string_lossy().as_ref(), dry);
    for line in &lines {
        emit(line);
    }
    0
}

fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn note(bid: &str, text: &str, files: &[String]) -> i32 {
    let cfg = read_json(&data_dir().join("metabrain.json")).unwrap_or_else(|| json!({}));
    // Memory::from_cfg mirrors the Python Memory(cfg).
    let m = crate::memory::Memory::from_cfg(&cfg);
    if files.is_empty() {
        m.note(bid, text, None, None, "note");
    } else {
        for f in files {
            m.note(bid, text, Some(f.as_str()), None, "note");
        }
    }
    emit("noted");
    0
}

fn run_headless(cfg: &Value, cmd: &[String]) -> i32 {
    let mut prompt = String::new();
    {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let _ = stdin.read_to_string(&mut prompt);
    }
    let extra = prompt_after_print(cmd);
    if !extra.is_empty() {
        if !prompt.trim().is_empty() {
            prompt.push('\n');
        }
        prompt.push_str(&extra);
    }

    let argv = command_list(cfg, "deepseek_headless", &default_headless());
    if argv.is_empty() {
        emit_err("run: deepseek_headless is empty");
        return 2;
    }
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]);
    if !prompt.is_empty() {
        c.arg(&prompt);
    }
    match c.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            emit_err(&format!("run: {e}"));
            127
        }
    }
}

fn run_job(bid: &str, cmd: &[String]) -> i32 {
    let cfg = read_json(&data_dir().join("metabrain.json")).unwrap_or_else(|| json!({}));
    let gate = read_json(&data_dir().join("gate.json")).unwrap_or_else(|| json!({}));

    let bid = if bid.eq_ignore_ascii_case("master") {
        match resolve_master(&cfg, &gate) {
            Some(id) => id,
            None => {
                emit_err("run: no master brain configured");
                return 2;
            }
        }
    } else {
        bid.to_string()
    };

    let b = match select_brain(&cfg, &bid) {
        Some(b) => b,
        None => {
            emit_err(&format!("unknown brain '{bid}'"));
            return 2;
        }
    };
    let id = b.s("id");
    let g = gate
        .o("brains")
        .get(id.as_str())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let st = read_json(&data_dir().join("state.json")).unwrap_or_else(|| json!({}));
    let state = st.get(id.as_str()).cloned().unwrap_or_else(|| json!({}));

    if truthy(g.get("paused")) {
        let reason = g.os("reason").or_else(|| g.os("by")).unwrap_or_default();
        emit_err(&format!(
            "MetaBrain: brain '{id}' is paused ({reason}); job not started."
        ));
        return EXIT_PAUSED;
    }
    if cmd.is_empty() {
        emit_err("run: no command given");
        return 2;
    }

    // When the master is not Claude and the job is a Claude call, delegate to DeepSeek.
    if b.s("kind") != "claude_code" && is_claude_command(cmd) {
        return run_headless(&cfg, cmd);
    }

    let ops = job_env_ops(&cfg, &b, &g, &state);
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    for op in &ops {
        match op {
            EnvOp::Set(k, v) => {
                c.env(k.as_str(), v.as_str());
            }
            EnvOp::Remove(k) => {
                c.env_remove(k.as_str());
            }
        }
    }
    match c.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            emit_err(&format!("run: {e}"));
            127
        }
    }
}

fn autostart(on: bool) -> i32 {
    if !cfg!(windows) {
        emit("autostart is only automatic on Windows");
        return 1;
    }
    #[cfg(windows)]
    {
        if let Err(e) = set_autostart(on) {
            emit_err(&format!("autostart: {e}"));
            return 1;
        }
    }
    emit(&format!("autostart {}", if on { "on" } else { "off" }));
    0
}

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "MetaBrain";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutostartStatus {
    Synced {
        command: String,
    },
    Mismatch {
        registered: String,
        expected: String,
    },
    Missing {
        expected: String,
    },
    Error(String),
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
}

pub(crate) fn expected_autostart_cmd(exe: &std::path::Path) -> String {
    let clean = exe.to_string_lossy();
    let clean = clean.strip_prefix(r"\\?\").unwrap_or(&clean);
    format!("\"{clean}\" serve --no-browser")
}

fn parse_autostart_exe(cmd: &str) -> &str {
    let t = cmd.trim();
    if let Some(rest) = t.strip_prefix('"') {
        rest.split('"').next().unwrap_or(t)
    } else {
        t.split_whitespace().next().unwrap_or(t)
    }
}

pub(crate) fn compare_autostart(
    registered: Option<&str>,
    current_exe: &std::path::Path,
) -> AutostartStatus {
    let expected = expected_autostart_cmd(current_exe);
    let Some(reg) = registered.map(str::trim).filter(|s| !s.is_empty()) else {
        return AutostartStatus::Missing { expected };
    };
    let cur_str = current_exe.to_string_lossy();
    let cur_clean = cur_str.strip_prefix(r"\\?\").unwrap_or(&cur_str);
    let reg_exe = parse_autostart_exe(reg);
    let reg_clean = reg_exe.strip_prefix(r"\\?\").unwrap_or(reg_exe);
    if crate::util::norm(std::path::Path::new(reg_clean))
        == crate::util::norm(std::path::Path::new(cur_clean))
    {
        AutostartStatus::Synced {
            command: reg.to_string(),
        }
    } else {
        AutostartStatus::Mismatch {
            registered: reg.to_string(),
            expected,
        }
    }
}

pub(crate) fn autostart_status() -> AutostartStatus {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
        use winreg::RegKey;
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return AutostartStatus::Error(e.to_string()),
        };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey_with_flags(RUN_SUBKEY, KEY_READ) {
            Ok(k) => k,
            Err(e) => return AutostartStatus::Error(e.to_string()),
        };
        match key.get_value::<String, _>(RUN_VALUE) {
            Ok(val) => compare_autostart(Some(&val), &exe),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => compare_autostart(None, &exe),
            Err(e) => AutostartStatus::Error(e.to_string()),
        }
    }
    #[cfg(not(windows))]
    {
        AutostartStatus::Unsupported
    }
}

#[cfg(windows)]
pub(crate) fn set_autostart(on: bool) -> std::io::Result<()> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;
    let exe = std::env::current_exe()?;
    let cmd = expected_autostart_cmd(&exe);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(RUN_SUBKEY, KEY_SET_VALUE)?;
    if on {
        key.set_value(RUN_VALUE, &cmd)
    } else {
        match key.delete_value(RUN_VALUE) {
            Ok(()) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn set_autostart(_on: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "autostart is only automatic on Windows",
    ))
}

pub(crate) fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("explorer");
        c.arg(url);
        crate::util::hidden(&mut c);
        let _ = c.spawn();
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        let _ = c.spawn();
    }
}

fn serve(argv: &[String]) -> i32 {
    let o = parse_serve_args(argv);
    let cfg_path = data_dir().join("metabrain.json");
    let port_num = o
        .port
        .or_else(|| {
            read_json(&cfg_path)
                .as_ref()
                .and_then(crate::config::valid_port)
        })
        .unwrap_or(DEFAULT_PORT);

    // Single instance per port: if the API already answers, just open the browser.
    // Already running: ask that instance to bring its window up (/api/show is instant;
    // /api/state can take seconds while the brains are scanned, so it is not the probe).
    let show = format!("http://127.0.0.1:{port_num}/api/show");
    let host = if o.lan { "0.0.0.0" } else { "127.0.0.1" };
    let server = match tiny_http::Server::http((host, port_num)) {
        Ok(server) => server,
        Err(e) => {
            if http_json(&show, &[], Some(&json!({})), 4)
                .is_ok_and(|r| r["app"] == "MetaBrain" && r["ok"] == true)
            {
                return 0;
            }
            let message = format!("MetaBrain could not bind {host}:{port_num}: {e}");
            crate::util::log(&message);
            emit(&message);
            return 1;
        }
    };

    let config = Arc::new(crate::config::Config::new(None));
    let sup = crate::core::Supervisor::new(config);
    sup.start();

    // The HTTP server blocks; run it in the background so the browser/tray work.
    let sup_srv = sup.clone();
    let lan = o.lan;
    std::thread::spawn(move || {
        if let Err(e) = crate::server::serve(server, sup_srv, port_num, lan) {
            crate::util::log(&format!("server: {e}"));
        }
    });

    emit(&format!("MetaBrain: http://127.0.0.1:{port_num}/ (API)"));
    if o.web {
        open_browser(&format!("http://127.0.0.1:{port_num}/"));
    }
    if o.tray {
        // The native window owns the main thread; closing it hides MetaBrain to the tray.
        match crate::gui::run(sup.clone(), port_num, !o.browser) {
            Ok(()) => {
                crate::util::log("bye");
                std::process::exit(0);
            }
            Err(e) => crate::util::log(&format!("window: {e}")),
        }
    }

    // No window: keep serving until the process is stopped externally.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Tray picture: a 32 px PNG cut from the app icon (the `image` crate only decodes PNG here).
pub(crate) fn tray_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(include_bytes!("../../build/metabrain-tray.png")).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

#[cfg(test)]
mod tray_icon_tests {
    #[test]
    fn tray_picture_decodes_and_is_visible() {
        let (px, w, h) = super::tray_rgba().expect("tray png must decode");
        assert_eq!((w, h), (32, 32));
        let opaque = px.chunks(4).filter(|p| p[3] > 200).count();
        assert!(opaque > 500, "tray icon would be invisible");
    }
}

// ---------- entry point ----------
pub fn main_with(args: Vec<String>, console: bool) -> i32 {
    // The console binary (metabrain-hook) is always a Claude Code hook call:
    // its first argument is the event name.
    // The console binary (metabrain-hook) accepts `hook <event>` (what `hooks install` writes), a bare
    // event name, or any other subcommand (hooks install, note, run, autostart, version).
    if console {
        const EVENTS: [&str; 4] = ["session-start", "pre-tool", "post-tool", "session-end"];
        if let Some(first) = args.first() {
            if EVENTS.contains(&first.as_str()) {
                return run_hook_cmd(Some(first.as_str()));
            }
        }
    }

    let cmd = args.first().cloned().unwrap_or_else(|| "serve".to_string());
    let rest: &[String] = args.get(1..).unwrap_or(&[]);
    match cmd.as_str() {
        "hook" => run_hook_cmd(rest.first().map(|s| s.as_str())),
        "hooks" if rest.first().map(|s| s.as_str()) == Some("install") => hooks_install(&rest[1..]),
        "note" if args.len() >= 3 => note(&args[1], &args[2], &args[3..]),
        "run" if args.len() >= 3 => {
            let mut r: &[String] = &args[2..];
            if r.first().map(|s| s.as_str()) == Some("--") {
                r = &r[1..];
            }
            run_job(&args[1], r)
        }
        "autostart" if matches!(rest.first().map(|s| s.as_str()), Some("on") | Some("off")) => {
            autostart(rest[0] == "on")
        }
        "serve" => serve(rest),
        // Clicking a notification: bring the native window up (starting MetaBrain if needed).
        c if c.starts_with("metabrain:") => serve(&[]),
        "mcp" => crate::mcp::run(),
        "route" => route_cmd(rest),
        "outcome" => outcome_cmd(rest),
        "version" => {
            emit(&format!("MetaBrain {}", env!("CARGO_PKG_VERSION")));
            0
        }
        _ => {
            emit(HELP);
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::J;
    use serde_json::json;

    #[test]
    fn parses_serve_flags() {
        let o = parse_serve_args(&[
            "--port".to_string(),
            "9001".to_string(),
            "--no-browser".to_string(),
            "--lan".to_string(),
        ]);
        assert_eq!(o.port, Some(9001));
        assert!(!o.browser);
        assert!(o.tray);
        assert!(o.lan);
    }

    #[test]
    fn selects_brain_case_insensitive() {
        let cfg = json!({"brains": [{"id": "Work"}, {"id": "Home"}]});
        let b = select_brain(&cfg, "work").expect("brain");
        assert_eq!(b.s("id"), "Work");
        assert!(select_brain(&cfg, "nope").is_none());
    }

    #[test]
    fn claude_default_account_removes_config_dir() {
        let cfg = json!({});
        let b = json!({"id": "x", "kind": "claude_code"});
        let g = json!({});
        let state = json!({});
        let ops = job_env_ops(&cfg, &b, &g, &state);
        assert!(ops.contains(&EnvOp::Remove("CLAUDE_CONFIG_DIR".to_string())));
        assert!(ops.contains(&EnvOp::Set("METABRAIN_BRAIN".to_string(), "x".to_string())));
    }

    #[test]
    fn claude_custom_account_sets_model_and_thinking() {
        let cfg = json!({});
        let b = json!({"id": "work", "kind": "claude_code"});
        let g = json!({"account": "workacct", "model": "opus"});
        let state = json!({"model": "sonnet", "thinking": 4096});
        let ops = job_env_ops(&cfg, &b, &g, &state);
        assert!(ops.contains(&EnvOp::Set(
            "CLAUDE_CONFIG_DIR".to_string(),
            "workacct".to_string()
        )));
        assert!(ops.contains(&EnvOp::Set(
            "ANTHROPIC_MODEL".to_string(),
            "opus".to_string()
        )));
        assert!(ops.contains(&EnvOp::Set(
            "MAX_THINKING_TOKENS".to_string(),
            "4096".to_string()
        )));
    }

    #[test]
    fn proxy_url_uses_default_port() {
        let cfg = json!({});
        let b = json!({"id": "local", "kind": "openai"});
        let g = json!({});
        let state = json!({});
        let ops = job_env_ops(&cfg, &b, &g, &state);
        assert!(ops.contains(&EnvOp::Set(
            "OPENAI_BASE_URL".to_string(),
            "http://127.0.0.1:8790/proxy/local/v1".to_string()
        )));
    }

    #[test]
    fn anthropic_proxy_url() {
        let cfg = json!({"port": 8123});
        let b = json!({"id": "a", "kind": "anthropic"});
        let g = json!({});
        let state = json!({});
        let ops = job_env_ops(&cfg, &b, &g, &state);
        assert!(ops.contains(&EnvOp::Set(
            "ANTHROPIC_BASE_URL".to_string(),
            "http://127.0.0.1:8123/proxy/a/v1".to_string()
        )));
    }

    #[test]
    fn master_prefers_fresh_gate_then_priority() {
        let cfg = json!({"priority": ["B", "A"], "brains": [{"id": "A"}, {"id": "B"}]});
        let fresh = json!({"ts": now(), "master": "A"});
        assert_eq!(resolve_master(&cfg, &fresh).as_deref(), Some("A"));

        let old = json!({"ts": now() - 300.0, "master": "A"});
        assert_eq!(resolve_master(&cfg, &old).as_deref(), Some("B"));

        let missing = json!({});
        assert_eq!(resolve_master(&cfg, &missing).as_deref(), Some("B"));
    }

    #[test]
    fn detects_claude_commands_and_prompts() {
        assert!(is_claude_command(&["claude".to_string()]));
        assert!(is_claude_command(&[
            "cmd".to_string(),
            "/c".to_string(),
            "claude".to_string()
        ]));
        assert!(!is_claude_command(&[
            "echo".to_string(),
            "claude".to_string()
        ]));

        let cmd: Vec<String> = ["claude", "-p", "hello", "world"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(prompt_after_print(&cmd), "hello world");
        assert_eq!(prompt_after_print(&["claude".to_string()]), "");
    }
}

/// `route [--project P] [--level N] <task...>`: ask the router which brain/model/effort to use.
/// Talks to the running MetaBrain (it knows the chain of command); offline it routes without the chain.
fn route_cmd(rest: &[String]) -> i32 {
    let (opts, words) = split_opts(rest);
    let task = words.join(" ");
    if task.trim().is_empty() {
        emit("uso: route [--project P] [--level N] <tarea>");
        return 2;
    }
    let project = opts
        .get("project")
        .cloned()
        .unwrap_or_else(project_from_cwd);
    let level = opts.get("level").and_then(|l| l.parse::<u8>().ok());
    let body = serde_json::json!({"project": project, "task": task, "level": level});
    let out = post_local("/api/route", &body).unwrap_or_else(|| {
        let raw_cfg = crate::util::read_json(&crate::util::data_dir().join("metabrain.json"))
            .unwrap_or(serde_json::json!({}));
        let cfg = crate::router::task_routing_config(raw_cfg);
        let chain: Vec<serde_json::Value> = cfg
            .arr("brains")
            .into_iter()
            .filter(|b| b.b("enabled", true))
            .map(|b| {
                serde_json::json!({
                    "id": b.s("id"),
                    "name": b.s("name"),
                    "kind": b.s("kind"),
                    "available": true
                })
            })
            .collect();
        crate::router::Router::new(None).route(&cfg, &chain, &project, &task, level)
    });
    emit(&serde_json::to_string_pretty(&out).unwrap_or_default());
    0
}

/// `outcome --project P --level N --candidate C ok|fail [--tokens T]`: teach the router how it went.
fn outcome_cmd(rest: &[String]) -> i32 {
    let (opts, words) = split_opts(rest);
    let Some(cand) = opts.get("candidate").cloned() else {
        emit("uso: outcome --project P --level N --candidate C ok|fail [--tokens T]");
        return 2;
    };
    let ok = !words
        .iter()
        .any(|w| w == "fail" || w == "falla" || w == "mal");
    let project = opts
        .get("project")
        .cloned()
        .unwrap_or_else(project_from_cwd);
    let level = opts
        .get("level")
        .and_then(|l| l.parse::<u8>().ok())
        .unwrap_or(3);
    let tokens = opts
        .get("tokens")
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(0);
    let body = serde_json::json!({"project": project, "level": level, "candidate": cand, "ok": ok, "tokens": tokens});
    let out = post_local("/api/outcome", &body).unwrap_or_else(|| {
        crate::router::Router::new(None).record(&project, level, &cand, ok, tokens)
    });
    emit(&serde_json::to_string_pretty(&out).unwrap_or_default());
    0
}

fn split_opts(rest: &[String]) -> (std::collections::HashMap<String, String>, Vec<String>) {
    let mut opts = std::collections::HashMap::new();
    let mut words = vec![];
    let mut i = 0;
    let mut end_opts = false;
    while i < rest.len() {
        if !end_opts && rest[i] == "--" {
            end_opts = true;
            i += 1;
            continue;
        }
        if !end_opts {
            if let Some(k) = rest[i].strip_prefix("--") {
                if matches!(k, "project" | "level" | "candidate" | "tokens") && i + 1 < rest.len() {
                    opts.insert(k.to_string(), rest[i + 1].clone());
                    i += 2;
                    continue;
                }
            }
        }
        words.push(rest[i].clone());
        i += 1;
    }
    (opts, words)
}

/// Project name = the current folder's name (what the user sees in the HMI).
fn project_from_cwd() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "general".into())
}

fn post_local(path: &str, body: &serde_json::Value) -> Option<serde_json::Value> {
    let cfg = crate::util::read_json(&crate::util::data_dir().join("metabrain.json"))
        .unwrap_or(serde_json::json!({}));
    let port = crate::config::valid_port(&cfg).unwrap_or(DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .ok()?;
    serde_json::from_reader(resp.into_reader()).ok()
}

#[cfg(test)]
mod autostart_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn compare_autostart_detects_missing_synced_and_mismatch() {
        let exe = Path::new(r"C:\Users\alice\AppData\Local\Programs\MetaBrain\metabrain.exe");
        assert!(matches!(
            compare_autostart(None, exe),
            AutostartStatus::Missing { .. }
        ));
        assert!(matches!(
            compare_autostart(Some("   "), exe),
            AutostartStatus::Missing { .. }
        ));

        let same =
            r#""c:\users\alice\appdata\local\programs\metabrain\MetaBrain.exe" serve --no-browser"#;
        assert!(matches!(
            compare_autostart(Some(same), exe),
            AutostartStatus::Synced { .. }
        ));

        let verbatim =
            Path::new(r"\\?\C:\Users\alice\AppData\Local\Programs\MetaBrain\metabrain.exe");
        assert!(matches!(
            compare_autostart(Some(same), verbatim),
            AutostartStatus::Synced { .. }
        ));

        let old = r#""C:\Python312\pythonw.exe" -m metabrain serve"#;
        assert!(matches!(
            compare_autostart(Some(old), exe),
            AutostartStatus::Mismatch { .. }
        ));
    }
}
