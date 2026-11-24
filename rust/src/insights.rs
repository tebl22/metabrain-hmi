//! Insights: what consumed the most, first-use doctor and native notifications.
use crate::util::{data_dir, expand_s, hidden, local_date, log, now, read_json, secret, J};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// Cached aggregation: (timestamp, key, value), 120 s TTL.
static CACHE: Mutex<Option<(f64, String, Value)>> = Mutex::new(None);
/// Anti-repetition for notifications: "<title>\u{1}<body>\u{1}<view>" -> last time shown.
static NOTIFY_SEEN: Mutex<Option<HashMap<String, f64>>> = Mutex::new(None);

const CACHE_TTL: f64 = 120.0;
const NOTIFY_TTL: f64 = 600.0;
const MAX_LINES: usize = 400_000;

struct Q {
    text: String,
    topic: String,
    account: String,
    when: String,
    tokens: i64,
}

fn pct(part: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        ((part / total) * 1000.0).round() / 10.0
    }
}

fn last_folder(p: &str) -> Option<String> {
    p.split(['/', '\\'])
        .rfind(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn fmt_when(ts: f64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(ts as i64, 0).single() {
        Some(d) => d.format("%Y-%m-%d %H:%M").to_string(),
        None => local_date(ts),
    }
}

/// Remove `<command-...>` / `<system-reminder...>` fragments, trim and cut to 300 chars.
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with("<command-") || rest.starts_with("<system-reminder") {
            match rest.find('>') {
                Some(off) => {
                    i += off + 1;
                    continue;
                }
                None => break,
            }
        }
        match rest.chars().next() {
            Some(c) => {
                out.push(c);
                i += c.len_utf8();
            }
            None => break,
        }
    }
    let cut: String = out.trim().chars().take(300).collect();
    cut.trim().to_string()
}

/// A "user" record is a new question when its content is text and has no tool_result block.
fn user_question(rec: &Value) -> Option<String> {
    let content = rec.get("message")?.get("content")?;
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let mut has_tool_result = false;
            let mut texts: Vec<String> = Vec::new();
            for b in arr {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("tool_result") => has_tool_result = true,
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                            texts.push(t.to_string());
                        } else if let Some(t) = b.get("content").and_then(|v| v.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
            if has_tool_result || texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

fn usage_tokens(u: &Value) -> i64 {
    let mut sum = 0i64;
    for k in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if let Some(v) = u.get(k) {
            if let Some(i) = v.as_i64() {
                sum += i;
            } else if let Some(f) = v.as_f64() {
                sum += f as i64;
            }
        }
    }
    sum
}

/// Parse one transcript file; questions keep order, tokens attached to the current question.
fn scan_file(path: &Path, fallback_topic: &str, mtime: f64) -> Vec<Q> {
    let raw = match fs::read(path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let txt = String::from_utf8_lossy(&raw);
    let mut qs: Vec<Q> = Vec::new();
    let mut cur: Option<usize> = None;
    let mut topic: Option<String> = None;
    let mut seen: HashSet<String> = HashSet::new();
    let mut n = 0usize;
    for line in txt.split('\n') {
        n += 1;
        if n > MAX_LINES {
            break;
        }
        let lt = line.trim();
        if lt.is_empty() || !lt.starts_with('{') {
            continue;
        }
        let rec: Value = match serde_json::from_str(lt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if topic.is_none() {
            if let Some(cwd) = rec.get("cwd").and_then(|v| v.as_str()) {
                if let Some(f) = last_folder(cwd) {
                    if !f.is_empty() {
                        topic = Some(f);
                    }
                }
            }
        }
        match rec.s("type").as_str() {
            "user" => {
                let text = match user_question(&rec) {
                    Some(t) => t,
                    None => continue,
                };
                let cleaned = clean_text(&text);
                if cleaned.is_empty() {
                    continue;
                }
                let ts = rec
                    .get("timestamp")
                    .and_then(crate::util::iso_ts)
                    .unwrap_or(mtime);
                qs.push(Q {
                    text: cleaned,
                    topic: String::new(),
                    account: String::new(),
                    when: fmt_when(ts),
                    tokens: 0,
                });
                cur = Some(qs.len() - 1);
            }
            "assistant" => {
                let msg = match rec.get("message") {
                    Some(m) if m.is_object() => m,
                    _ => continue,
                };
                let usage = match msg.get("usage") {
                    Some(u) if u.is_object() => u,
                    _ => continue,
                };
                let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
                // Count each message.id only once per file.
                if !id.is_empty() && !seen.insert(id.to_string()) {
                    continue;
                }
                let add = usage_tokens(usage);
                if add != 0 {
                    if let Some(ix) = cur {
                        if let Some(q) = qs.get_mut(ix) {
                            q.tokens += add;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let t = topic.unwrap_or_else(|| fallback_topic.to_string());
    for q in qs.iter_mut() {
        q.topic = t.clone();
    }
    qs
}

fn jsonl_in(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file()
                && p.extension()
                    .map(|x| x.eq_ignore_ascii_case("jsonl"))
                    .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
}

/// Top questions by tokens consumed, grouped by topic and account.
pub fn top_questions(brains: &[Value], days: i64, limit: usize) -> Value {
    let claudes: Vec<(String, String)> = brains
        .iter()
        .filter(|b| b.s("kind") == "claude_code")
        .map(|b| {
            (
                b.s("name"),
                b.os("config_dir")
                    .unwrap_or_else(|| "~/.claude".to_string()),
            )
        })
        .collect();

    let ids: Vec<String> = claudes
        .iter()
        .map(|(n, d)| format!("{}@{}", n, d))
        .collect();
    let key = format!("{}|{}|{}", ids.join(","), days, limit);
    {
        if let Ok(g) = CACHE.lock() {
            if let Some((ts, k, v)) = g.as_ref() {
                if *k == key && now() - *ts < CACHE_TTL {
                    return v.clone();
                }
            }
        }
    }

    let mut all: Vec<Q> = Vec::new();
    let cutoff = if days > 0 {
        now() - (days as f64) * 86_400.0
    } else {
        0.0
    };
    for (account, cfgdir) in &claudes {
        let root = PathBuf::from(expand_s(cfgdir)).join("projects");
        let rd = match fs::read_dir(&root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let pd = e.path();
            if !pd.is_dir() {
                continue;
            }
            let fallback = pd
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut files: Vec<PathBuf> = Vec::new();
            jsonl_in(&pd, &mut files);
            // Also <session>/subagents/*.jsonl inside each project.
            if let Ok(sub) = fs::read_dir(&pd) {
                for s in sub.flatten() {
                    let sp = s.path();
                    if sp.is_dir() {
                        jsonl_in(&sp.join("subagents"), &mut files);
                    }
                }
            }
            for f in files {
                let mtime = fs::metadata(&f)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or_else(now);
                if cutoff > 0.0 && mtime < cutoff {
                    continue;
                }
                let mut qs = scan_file(&f, &fallback, mtime);
                for q in qs.iter_mut() {
                    q.account = account.clone();
                }
                all.append(&mut qs);
            }
        }
    }

    let total: i64 = all.iter().map(|q| q.tokens).sum();
    let tf = total as f64;

    let mut sorted: Vec<&Q> = all.iter().collect();
    sorted.sort_by_key(|q| std::cmp::Reverse(q.tokens));
    let n = if limit == 0 {
        sorted.len()
    } else {
        limit.min(sorted.len())
    };
    let mut top: Vec<Value> = Vec::with_capacity(n);
    for q in sorted.iter().take(n) {
        top.push(json!({
            "text": q.text,
            "topic": q.topic,
            "account": q.account,
            "brain": q.account,
            "when": q.when,
            "tokens": q.tokens,
            "pct": pct(q.tokens as f64, tf),
        }));
    }

    let mut tm: HashMap<String, i64> = HashMap::new();
    let mut am: HashMap<String, i64> = HashMap::new();
    for q in &all {
        *tm.entry(q.topic.clone()).or_insert(0) += q.tokens;
        *am.entry(q.account.clone()).or_insert(0) += q.tokens;
    }
    let mut tv: Vec<(String, i64)> = tm.into_iter().collect();
    tv.sort_by_key(|x| std::cmp::Reverse(x.1));
    let by_topic: Vec<Value> = tv
        .into_iter()
        .map(|(k, v)| json!({"topic": k, "tokens": v, "pct": pct(v as f64, tf)}))
        .collect();
    let mut av: Vec<(String, i64)> = am.into_iter().collect();
    av.sort_by_key(|x| std::cmp::Reverse(x.1));
    let by_account: Vec<Value> = av
        .into_iter()
        .map(|(k, v)| json!({"account": k, "tokens": v, "pct": pct(v as f64, tf)}))
        .collect();

    let out = json!({
        "days": days,
        "total": total,
        "top_questions": top,
        "by_topic": by_topic,
        "by_account": by_account,
    });
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((now(), key, out.clone()));
    }
    out
}

// ---------- doctor ----------

fn login_fix(dir: &Path) -> String {
    if cfg!(windows) {
        format!(
            "Abre una terminal y ejecuta:  set CLAUDE_CONFIG_DIR={}  y luego  claude  y escribe /login",
            dir.display()
        )
    } else {
        format!(
            "Abre una terminal y ejecuta:  export CLAUDE_CONFIG_DIR={}  y luego  claude  y escribe /login",
            dir.display()
        )
    }
}

fn hooks_installed(settings: &Path) -> bool {
    match read_json(settings) {
        Some(v) => {
            let hooks = v.o("hooks");
            match hooks.as_object() {
                Some(m) if !m.is_empty() => serde_json::to_string(&hooks)
                    .map(|s| s.to_lowercase().contains("metabrain"))
                    .unwrap_or(false),
                _ => false,
            }
        }
        None => false,
    }
}

fn git_version() -> (bool, String) {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("--version");
    hidden(&mut cmd);
    match cmd.output() {
        Ok(o) => {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                (
                    true,
                    if s.is_empty() {
                        "git disponible.".to_string()
                    } else {
                        s
                    },
                )
            } else {
                (false, "git --version falló.".to_string())
            }
        }
        Err(_) => (
            false,
            "git no está instalado o no está en el PATH.".to_string(),
        ),
    }
}

/// Locate the Claude Code executable (MB_CLAUDE, PATH, ~/.local/bin).
pub fn find_claude() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("MB_CLAUDE") {
        if !v.trim().is_empty() {
            let p = PathBuf::from(expand_s(v.trim()));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd", "claude"]
    } else {
        &["claude"]
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for n in names {
                let c = dir.join(n);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    let local = if cfg!(windows) {
        PathBuf::from(expand_s("~/.local/bin/claude.exe"))
    } else {
        PathBuf::from(expand_s("~/.local/bin/claude"))
    };
    if local.is_file() {
        return Some(local);
    }
    None
}

/// First-use diagnosis: one {id, ok, title, detail, fix} per check.
pub fn doctor(cfg: &Value) -> Value {
    let mut checks: Vec<Value> = Vec::new();
    let brains = cfg.arr("brains");

    for b in &brains {
        let kind = b.s("kind");
        let name = b.s("name");
        if kind == "claude_code" {
            let dir = b
                .os("config_dir")
                .unwrap_or_else(|| "~/.claude".to_string());
            let dp = PathBuf::from(expand_s(&dir));
            let exists = dp.is_dir();
            let creds = dp.join(".credentials.json").is_file();
            let token = secret("CLAUDE_CODE_OAUTH_TOKEN").is_some();
            let ok = exists && (creds || token);
            let detail = if !exists {
                format!("No existe la carpeta {}.", dp.display())
            } else if ok {
                if creds {
                    format!("Credenciales encontradas en {}.", dp.display())
                } else {
                    format!(
                        "Sesión detectada por CLAUDE_CODE_OAUTH_TOKEN en {}.",
                        dp.display()
                    )
                }
            } else {
                format!("La carpeta {} existe pero no hay .credentials.json ni CLAUDE_CODE_OAUTH_TOKEN.", dp.display())
            };
            checks.push(json!({
                "id": format!("account:{}", name),
                "ok": ok,
                "title": format!("Cuenta {}", name),
                "detail": detail,
                "fix": login_fix(&dp),
            }));

            let hooks_ok = hooks_installed(&dp.join("settings.json"));
            checks.push(json!({
                "id": format!("hooks:{}", name),
                "ok": hooks_ok,
                "title": format!("Hooks de {}", name),
                "detail": if hooks_ok {
                    "Los hooks de MetaBrain están configurados en settings.json.".to_string()
                } else {
                    format!("No se encontraron hooks de MetaBrain en {}.", dp.join("settings.json").display())
                },
                "fix": "Configuración → Instalar hooks",
            }));
        } else if kind == "openai" {
            let var = b
                .os("key_env")
                .unwrap_or_else(|| "OPENAI_API_KEY".to_string());
            let ok = secret(&var).is_some();
            checks.push(json!({
                "id": format!("key:{}", name),
                "ok": ok,
                "title": format!("Clave de {}", name),
                "detail": if ok {
                    format!("Variable de entorno {} detectada.", var)
                } else {
                    format!("No se encontró la variable de entorno {}.", var)
                },
                "fix": format!("Crea la variable de entorno {} con tu clave y reinicia MetaBrain.", var),
            }));
        }
    }

    match find_claude() {
        Some(p) => checks.push(json!({
            "id": "claude_exe",
            "ok": true,
            "title": "Claude Code instalado",
            "detail": p.display().to_string(),
            "fix": "Instala Claude Code: npm install -g @anthropic-ai/claude-code",
        })),
        None => checks.push(json!({
            "id": "claude_exe",
            "ok": false,
            "title": "Claude Code instalado",
            "detail": "No se encontró el ejecutable claude en PATH ni en ~/.local/bin.",
            "fix": "Instala Claude Code: npm install -g @anthropic-ai/claude-code",
        })),
    }

    let (git_ok, git_detail) = git_version();
    checks.push(json!({
        "id": "git",
        "ok": git_ok,
        "title": "Git",
        "detail": git_detail,
        "fix": "Instala Git para usar ramas aisladas en Tareas.",
    }));

    // Check configured external CLI agents (binary + detected version).
    let mut seen_agents = std::collections::HashSet::new();
    for b in cfg.arr("brains") {
        let Some(agent) = crate::cli_agents::Agent::from_kind(&b.s("kind")) else {
            continue;
        };
        if !seen_agents.insert(agent.bin_name()) {
            continue;
        }
        let bin = crate::cli_agents::find_bin(agent);
        let ver = if bin.is_some() {
            crate::cli_agents::cli_version(agent)
        } else {
            None
        };
        let ok = bin.is_some();
        let detail = match (bin, ver) {
            (Some(p), Some(v)) => format!("v{v} ({})", p.display()),
            (Some(p), None) => format!("instalado en {} (versión no detectada)", p.display()),
            (None, _) => format!("No se encontró {} en PATH.", agent.bin_name()),
        };
        checks.push(json!({
            "id": format!("cli:{}", agent.bin_name()),
            "ok": ok,
            "title": format!("CLI {}", agent.display_name()),
            "detail": detail,
            "fix": format!("Instala {} y reinicia MetaBrain.", agent.display_name()),
        }));
    }

    let ok = checks
        .iter()
        .all(|c| c.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    json!({"ok": ok, "checks": checks})
}

// ---------- notifications ----------

/// Per-user AppUserModelID so Windows shows "MetaBrain" (logo + name) instead of PowerShell.
#[allow(dead_code)]
const TOAST_AUMID: &str = "MetaBrain";
#[cfg_attr(not(windows), allow(dead_code))]
const TOAST_APP_NAME: &str = "MetaBrain";
const DEFAULT_PORT: u16 = 8790;

#[cfg_attr(not(windows), allow(dead_code))]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Keep the view fragment to safe URL characters (never panic, always something usable).
#[cfg_attr(not(windows), allow(dead_code))]
fn view_slug(view: &str) -> String {
    let v: String = view
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if v.is_empty() {
        "alarms".to_string()
    } else {
        v
    }
}

/// Build the toast XML (pure: easy to test without Windows).
#[cfg_attr(not(windows), allow(dead_code))]
fn toast_xml(title: &str, body: &str, port: u16, view: &str) -> String {
    // metabrain:// opens the native window (registered next to the toast identity), never the browser.
    let _ = port;
    let link = xml_escape(&format!("metabrain://{}", view_slug(view)));
    let t = xml_escape(title);
    let b = xml_escape(body);
    let app = TOAST_APP_NAME;
    // The logo inside the toast itself shows even when Windows ignores the registered icon.
    let png = data_dir().join("metabrain-toast.png");
    let logo = if png.exists() {
        format!(
            "<image placement=\"appLogoOverride\" src=\"{}\"/>",
            xml_escape(&format!(
                "file:///{}",
                png.display().to_string().replace('\\', "/")
            ))
        )
    } else {
        String::new()
    };
    format!(
        "<toast launch=\"{link}\" activationType=\"protocol\">\
         <visual><binding template=\"ToastGeneric\">\
         {logo}<text>{t}</text><text>{b}</text>\
         <text placement=\"attribution\">{app}</text>\
         </binding></visual>\
         <actions><action content=\"Abrir MetaBrain\" arguments=\"{link}\" activationType=\"protocol\"/></actions>\
         </toast>"
    )
}

/// Registry subkey under HKCU that holds the app identity for one AUMID.
#[allow(dead_code)]
fn app_id_subkey(aumid: &str) -> String {
    format!(r"Software\Classes\AppUserModelId\{}", aumid)
}

/// Command line registered under `HKCU\Software\Classes\metabrain\shell\open\command`.
#[allow(dead_code)]
fn protocol_command_line(exe: &Path) -> String {
    let clean = exe.to_string_lossy();
    let clean = clean.strip_prefix(r"\\?\").unwrap_or(&clean);
    format!("\"{clean}\" \"%1\"")
}

/// Port the local server listens on (same config the server reads; 8790 when unknown).
#[allow(dead_code)]
fn configured_port() -> u16 {
    fn pick(v: &Value) -> Option<u16> {
        let n = v
            .as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
        if n > 0 && n <= 65535 {
            Some(n as u16)
        } else {
            None
        }
    }
    let cfg = match read_json(&data_dir().join("metabrain.json")) {
        Some(v) => v,
        None => return DEFAULT_PORT,
    };
    for k in ["port", "web_port", "http_port", "server_port"] {
        if let Some(v) = cfg.get(k) {
            if let Some(p) = pick(v) {
                return p;
            }
        }
    }
    for k in ["server", "web", "http"] {
        if let Some(v) = cfg.get(k).and_then(|s| s.get("port")) {
            if let Some(p) = pick(v) {
                return p;
            }
        }
    }
    DEFAULT_PORT
}

/// Write the logo and register the app identity under HKCU (no admin needed).
#[cfg(windows)]
const TOAST_LOGO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../metabrain/web/icon-192.png"
));

#[cfg(windows)]
fn setup_toast_identity() -> Result<PathBuf, String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let dir = data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("no se pudo crear {}: {e}", dir.display()))?;
    let png = dir.join("metabrain-toast.png");
    let stale = match fs::metadata(&png) {
        Ok(m) => m.len() != TOAST_LOGO.len() as u64,
        Err(_) => true,
    };
    if stale {
        fs::write(&png, TOAST_LOGO)
            .map_err(|e| format!("no se pudo escribir el logo {}: {e}", png.display()))?;
    }
    let icon = png.to_string_lossy().to_string();
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let subkey = app_id_subkey(TOAST_AUMID);
    let (key, _) = hkcu
        .create_subkey(&subkey)
        .map_err(|e| format!("no se pudo abrir/crear HKCU\\{subkey}: {e}"))?;
    key.set_value("DisplayName", &TOAST_APP_NAME)
        .map_err(|e| format!("no se pudo escribir DisplayName: {e}"))?;
    key.set_value("IconUri", &icon)
        .map_err(|e| format!("no se pudo escribir IconUri: {e}"))?;
    key.set_value("IconBackgroundColor", &"FF0E1015")
        .map_err(|e| format!("no se pudo escribir IconBackgroundColor: {e}"))?;
    key.set_value("ShowInSettings", &1u32)
        .map_err(|e| format!("no se pudo escribir ShowInSettings: {e}"))?;

    // metabrain:// → this exe, so clicking a toast shows the window.
    if let Ok(exe) = std::env::current_exe() {
        let base = r"Software\Classes\metabrain";
        if let Ok((proto, _)) = hkcu.create_subkey(base) {
            let _ = proto.set_value("", &"URL:MetaBrain");
            let _ = proto.set_value("URL Protocol", &"");
        }
        let cmd_line = protocol_command_line(&exe);
        if let Ok((cmd_key, _)) = hkcu.create_subkey(format!(r"{base}\shell\open\command")) {
            let _ = cmd_key.set_value("", &cmd_line);
        }
    }
    Ok(png)
}

/// Run the identity setup once per process; a failure is logged once and ignored.
#[cfg(windows)]
fn ensure_toast_identity() -> Option<PathBuf> {
    static ONCE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| match setup_toast_identity() {
        Ok(p) => Some(p),
        Err(e) => {
            log(&format!("toast: {e}"));
            None
        }
    })
    .clone()
}

#[cfg(windows)]
fn windows_toast(title: &str, body: &str, view: &str) {
    // Identity is best effort: notifications still work if the registry/logo step fails.
    let _ = ensure_toast_identity();
    let xml = toast_xml(title, body, configured_port(), view);
    // The XML lives inside a PowerShell single-quoted string: double the quotes.
    let xml_ps = xml.replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue';\
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;\
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null;\
$xml = '{xml_ps}';\
$doc = New-Object Windows.Data.Xml.Dom.XmlDocument;\
$doc.LoadXml($xml);\
$toast = New-Object Windows.UI.Notifications.ToastNotification $doc;\
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{aumid}').Show($toast);",
        aumid = TOAST_AUMID
    );
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "Hidden",
        "-Command",
        &script,
    ]);
    hidden(&mut cmd);
    if cmd.spawn().is_err() {
        log(&format!(
            "notify: no se pudo lanzar PowerShell para '{}'",
            title
        ));
    }
}

/// Native desktop notification that opens the given panel view when clicked.
pub fn notify_with(title: &str, body: &str, view: &str) {
    if let Some(v) = read_json(&data_dir().join("metabrain.json")) {
        if !v.b("notifications", true) {
            return;
        }
    }

    let key = format!("{}\u{1}{}\u{1}{}", title, body, view);
    let t = now();
    if let Ok(mut g) = NOTIFY_SEEN.lock() {
        let map = g.get_or_insert_with(HashMap::new);
        if let Some(prev) = map.get(&key) {
            if t - *prev < NOTIFY_TTL {
                return;
            }
        }
        map.insert(key, t);
        if map.len() > 200 {
            map.retain(|_, v| t - *v < NOTIFY_TTL);
        }
    }

    #[cfg(windows)]
    {
        windows_toast(title, body, view);
    }
    #[cfg(not(windows))]
    {
        let spawned = std::process::Command::new("notify-send")
            .arg(title)
            .arg(body)
            .spawn()
            .is_ok();
        if !spawned {
            log(&format!("notify: {} — {}", title, body));
        }
    }
}

/// Routine changes (who has command, reorganisations). They always go to the diary; a toast
/// only when `notify_routine` is on, and then at most one every 30 minutes.
pub fn notify_routine(title: &str, body: &str, view: &str) {
    static LAST: Mutex<f64> = Mutex::new(0.0);
    let on = read_json(&data_dir().join("metabrain.json"))
        .map(|v| v.b("notify_routine", false))
        .unwrap_or(false);
    if !on {
        return;
    }
    if let Ok(mut g) = LAST.lock() {
        let t = now();
        if t - *g < 1800.0 {
            return;
        }
        *g = t;
    }
    notify_with(title, body, view);
}

/// Native desktop notification (no wait, no repeat within 10 min).
pub fn notify(title: &str, body: &str) {
    notify_with(title, body, "alarms");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::J;

    fn tmp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let n = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("mb_insights_{}_{}_{}", tag, std::process::id(), n));
        let _ = fs::create_dir_all(&p);
        p
    }

    #[test]
    fn top_questions_sums_sorts_and_topics() {
        let _g = crate::util::test_env_lock();
        let home = tmp_home("top");
        std::env::set_var("METABRAIN_HOME", &home);
        let proj = home.join("projects").join("p1");
        fs::create_dir_all(&proj).expect("project dir");
        let lines = [
            r#"{"type":"user","message":{"content":"Hola primera pregunta"},"timestamp":"2024-01-01T10:00:00Z","cwd":"C:\\work\\proyectoX"}"#,
            r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
            r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"salida"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"Segunda pregunta"}]},"timestamp":"2024-01-02T11:00:00Z"}"#,
            r#"{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":5,"output_tokens":5}}}"#,
            r#"esto no es json"#,
        ];
        fs::write(proj.join("s.jsonl"), lines.join("\n")).expect("write transcript");

        let brains = vec![json!({
            "kind": "claude_code",
            "name": "CuentaTest",
            "config_dir": home.display().to_string()
        })];
        let r = top_questions(&brains, 1, 10);

        assert_eq!(r.get("days").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(r.get("total").and_then(|v| v.as_i64()), Some(40));
        let top = r.arr("top_questions");
        assert_eq!(top.len(), 2, "tool_result y duplicados no crean preguntas");
        assert_eq!(top[0].s("text"), "Hola primera pregunta");
        assert_eq!(top[0].f("tokens"), Some(30.0));
        assert_eq!(top[0].f("pct"), Some(75.0));
        assert_eq!(top[0].s("topic"), "proyectoX");
        assert_eq!(top[0].s("account"), "CuentaTest");
        let want = chrono::DateTime::parse_from_rfc3339("2024-01-01T10:00:00Z")
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        assert_eq!(top[0].s("when"), want);
        assert_eq!(top[1].s("text"), "Segunda pregunta");
        assert_eq!(top[1].f("tokens"), Some(10.0));
        assert_eq!(top[1].f("pct"), Some(25.0));

        let bt = r.arr("by_topic");
        assert_eq!(bt.len(), 1);
        assert_eq!(bt[0].s("topic"), "proyectoX");
        assert_eq!(bt[0].f("tokens"), Some(40.0));
        assert_eq!(bt[0].f("pct"), Some(100.0));

        let ba = r.arr("by_account");
        assert_eq!(ba.len(), 1);
        assert_eq!(ba[0].s("account"), "CuentaTest");
        assert_eq!(ba[0].f("tokens"), Some(40.0));
    }

    #[test]
    fn doctor_flags_missing_credentials() {
        let _g = crate::util::test_env_lock();
        let home = tmp_home("doc");
        std::env::set_var("METABRAIN_HOME", &home);
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        let missing = home.join("no_existe");
        let cfg = json!({"brains": [{
            "kind": "claude_code",
            "name": "SinCred",
            "config_dir": missing.display().to_string()
        }]});
        let r = doctor(&cfg);
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(false));
        let checks = r.arr("checks");
        let c = checks
            .iter()
            .find(|c| c.s("title") == "Cuenta SinCred")
            .expect("check de cuenta");
        assert_eq!(c.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert!(c.s("fix").contains("CLAUDE_CONFIG_DIR"));
        assert!(checks.iter().any(|c| c.s("title") == "Hooks de SinCred"));
    }

    #[test]
    fn xml_escape_covers_specials() {
        assert_eq!(
            xml_escape("Tom & Jerry <x> \"y\" 'z'"),
            "Tom &amp; Jerry &lt;x&gt; &quot;y&quot; &apos;z&apos;"
        );
    }

    #[test]
    fn toast_xml_has_attribution_action_and_escaping() {
        let x = toast_xml("Cuota & alta", "semanal <92%> \"x\" 'y'", 9123, "alarms");
        assert!(x.contains("launch=\"metabrain://alarms\""), "launch: {x}");
        assert!(
            x.contains("<text placement=\"attribution\">MetaBrain</text>"),
            "attribution: {x}"
        );
        assert!(x.contains("content=\"Abrir MetaBrain\""), "action: {x}");
        assert!(
            x.contains("arguments=\"metabrain://alarms\""),
            "action args: {x}"
        );
        assert!(
            x.contains("<text>Cuota &amp; alta</text>"),
            "titulo escapado: {x}"
        );
        assert!(
            x.contains("<text>semanal &lt;92%&gt; &quot;x&quot; &apos;y&apos;</text>"),
            "cuerpo escapado: {x}"
        );
        assert!(!x.contains('\n'), "el XML va en una sola linea");
    }

    #[test]
    fn toast_xml_sanitizes_view_and_defaults() {
        let x = toast_xml("t", "b", 8790, "../../evil?x=1");
        assert!(x.contains("metabrain://evilx1"), "{x}");
        let d = toast_xml("t", "b", 8790, "");
        assert!(d.contains("metabrain://alarms"), "{d}");
    }

    #[test]
    fn toast_registry_subkey_and_protocol_cmd() {
        let subkey = app_id_subkey(TOAST_AUMID);
        assert_eq!(subkey, r"Software\Classes\AppUserModelId\MetaBrain");
        let cmd = protocol_command_line(Path::new(r"\\?\C:\Programs\MetaBrain\metabrain.exe"));
        assert_eq!(cmd, r#""C:\Programs\MetaBrain\metabrain.exe" "%1""#);
    }
}
