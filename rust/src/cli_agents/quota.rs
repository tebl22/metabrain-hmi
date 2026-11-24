//! Account discovery, Codex model/effort selection, and live quota readers
//! for ChatGPT (Codex session logs) and Google (Antigravity `/usage`).

use super::{find_bin, is_cmd_script, Agent};
use crate::util::hidden;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// One quota window (5 h session or weekly) shared across Claude, Codex, and Antigravity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaLimit {
    pub kind: String,
    pub group: String,
    pub pct: Option<f64>,
    pub reset: Option<f64>,
}

#[derive(Deserialize, Default)]
struct CodexModelsCatalog {
    #[serde(default)]
    models: Vec<CodexModelEntry>,
}

#[derive(Deserialize, Default)]
struct CodexModelEntry {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Option<Vec<CodexReasoningLevel>>,
}

#[derive(Deserialize, Default)]
struct CodexReasoningLevel {
    #[serde(default)]
    effort: String,
}

#[derive(Deserialize, Default)]
struct CodexRateLimits {
    #[serde(default)]
    primary: Option<CodexRateWindow>,
    #[serde(default)]
    secondary: Option<CodexRateWindow>,
}

#[derive(Deserialize, Default)]
struct CodexRateWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<f64>,
}

#[derive(Deserialize, Default)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: Option<CodexAuthTokens>,
}

#[derive(Deserialize, Default)]
struct CodexAuthTokens {
    #[serde(default)]
    id_token: String,
}

#[derive(Deserialize, Default)]
struct JwtClaims {
    #[serde(default)]
    email: Option<String>,
}

/// The account folder Codex uses: per brain (`codex_home`) or the default ~/.codex.
pub fn codex_home(cfg: &Value) -> PathBuf {
    let h = cfg
        .get("codex_home")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if h.is_empty() {
        crate::util::home().join(".codex")
    } else {
        crate::util::expand(&h)
    }
}

/// Pick Codex settings locally. A missing catalog leaves model discovery to Codex.
pub(crate) fn codex_turn_settings(
    cfg: &Value,
    prompt: &str,
    model: &str,
    effort: &str,
) -> (String, String) {
    let catalog = crate::util::read_json(&codex_home(cfg).join("models_cache.json"));
    pick_codex_turn(
        prompt,
        model,
        effort,
        catalog.as_ref().unwrap_or(&Value::Null),
    )
}

pub(super) fn pick_codex_turn(
    prompt: &str,
    model: &str,
    effort: &str,
    catalog: &Value,
) -> (String, String) {
    let parsed: CodexModelsCatalog = serde_json::from_value(catalog.clone()).unwrap_or_default();
    let request = prompt
        .split("\n\nArchivos adjuntos (")
        .next()
        .unwrap_or(prompt);
    let (mut level, _) = crate::router::classify(request);
    let diagnostic = request.to_lowercase();
    if diagnostic
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| {
            matches!(
                word,
                "error" | "errores" | "falla" | "fallo" | "bug" | "crash"
            )
        })
    {
        level = level.max(3);
    }
    let listed: Vec<&CodexModelEntry> = parsed
        .models
        .iter()
        .filter(|item| item.visibility == "list")
        .collect();
    let chosen_model: &str = if model.is_empty() || model == "auto" {
        let preferred = if level >= 4 {
            ["gpt-6.2-astra", "gpt-6.2-sol"]
        } else {
            ["gpt-6.2-sol", "gpt-6.2-astra"]
        };
        listed
            .iter()
            .find(|item| item.slug == preferred[0])
            .or_else(|| listed.iter().find(|item| item.slug == preferred[1]))
            .map(|item| item.slug.as_str())
            .unwrap_or("")
    } else {
        model
    };
    let wanted = match effort {
        "" | "auto" => match level {
            1 | 2 => "low",
            3 | 4 => "medium",
            _ => "high",
        },
        effort => effort,
    };
    let entry = parsed.models.iter().find(|item| item.slug == chosen_model);
    let supported = entry.and_then(|item| item.supported_reasoning_levels.as_ref());
    let chosen_effort = match supported {
        Some(levels) if !levels.iter().any(|item| item.effort == wanted) => entry
            .and_then(|item| item.default_reasoning_level.as_deref())
            .unwrap_or(""),
        // These levels also work when Codex has yet to cache the catalog on this machine.
        None if !matches!(wanted, "low" | "medium" | "high") => "",
        _ => wanted,
    };
    (chosen_model.to_string(), chosen_effort.to_string())
}

/// Live ChatGPT plan limits (5 h and weekly) from the newest Codex session log in `home`
/// that contains populated rate-limit windows. Cached for 20 s per folder.
pub fn codex_quota(home: &Path) -> Vec<Value> {
    type QuotaCache = Mutex<HashMap<PathBuf, (f64, Vec<Value>)>>;
    static CACHE: OnceLock<QuotaCache> = OnceLock::new();
    let c = CACHE.get_or_init(|| Mutex::new(Default::default()));
    let t = crate::util::now();
    if let Some((ts, v)) = c.lock().ok().and_then(|m| m.get(home).cloned()) {
        if t - ts < 20.0 {
            return v;
        }
    }
    let v: Vec<Value> = read_codex_quota_at(home, t)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|q| serde_json::to_value(q).ok())
        .collect();
    if let Ok(mut m) = c.lock() {
        m.insert(home.to_path_buf(), (t, v.clone()));
    }
    v
}

/// Collect recent Codex `.jsonl` session files from newest day folders (up to 7 days, newest first).
fn recent_codex_session_files(home: &Path, max_days: usize, max_files: usize) -> Vec<PathBuf> {
    let root = home.join("sessions");
    let list_dirs = |dir: &Path| -> Vec<PathBuf> {
        let mut subs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subs.sort();
        subs.reverse();
        subs
    };

    let mut day_dirs = Vec::new();
    'days: for year in list_dirs(&root) {
        for month in list_dirs(&year) {
            for day in list_dirs(&month) {
                day_dirs.push(day);
                if day_dirs.len() >= max_days {
                    break 'days;
                }
            }
        }
    }

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for dir in day_dirs {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "jsonl") {
                    let mt = e
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    files.push((mt, p));
                }
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    files.into_iter().take(max_files).map(|(_, p)| p).collect()
}

fn parse_codex_rate_limits(rl: &Value, now_ts: f64) -> Vec<QuotaLimit> {
    let Ok(parsed) = serde_json::from_value::<CodexRateLimits>(rl.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (win, g) in [(parsed.primary, "session"), (parsed.secondary, "weekly")] {
        if let Some(x) = win {
            // A window that already reset is back to 0 % until Codex reports again.
            let pct = if x.resets_at.is_some_and(|r| r <= now_ts) {
                Some(0.0)
            } else {
                x.used_percent
            };
            if pct.is_some() {
                out.push(QuotaLimit {
                    kind: g.to_string(),
                    group: g.to_string(),
                    pct,
                    reset: x.resets_at,
                });
            }
        }
    }
    out
}

pub(super) fn read_codex_quota_at(home: &Path, now_ts: f64) -> Option<Vec<QuotaLimit>> {
    // sessions/YYYY/MM/DD/rollout-*.jsonl: walk recent day folders newest-first,
    // skipping empty/null rate_limits snapshots (e.g. limit_id: "premium" with null windows).
    // Parallel Codex sessions can overlap in time, so compare the event's ISO timestamp
    // (falling back to file mtime) across recent files and keep the genuinely latest snapshot.
    let mut best: Option<(f64, Vec<QuotaLimit>)> = None;
    for f in recent_codex_session_files(home, 7, 32) {
        let mtime = std::fs::metadata(&f)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        // Once we have a valid snapshot and remaining files are more than 6 hours older, stop scanning.
        if let Some((best_ts, _)) = &best {
            if mtime > 0.0 && *best_ts - mtime > 6.0 * 3600.0 {
                break;
            }
        }
        let Ok(txt) = std::fs::read_to_string(&f) else {
            continue;
        };
        for line in txt.lines().rev() {
            if !line.contains("\"rate_limits\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(rl) = find_key(&v, "rate_limits") else {
                continue;
            };
            let out = parse_codex_rate_limits(&rl, now_ts);
            if !out.is_empty() {
                let ev_ts = v
                    .get("timestamp")
                    .and_then(crate::util::iso_ts)
                    .unwrap_or(mtime);
                if best.as_ref().map(|(t, _)| ev_ts > *t).unwrap_or(true) {
                    best = Some((ev_ts, out));
                }
                break;
            }
        }
    }
    best.map(|(_, out)| out)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for nc in chars.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn split_usage_cols(line: &str) -> Vec<&str> {
    if line.contains('\t') {
        return line
            .split('\t')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
    }
    // Fallback if a CLI version uses 2+ spaces instead of tabs.
    let mut cols = Vec::new();
    let mut start = None;
    let mut spaces = 0usize;
    for (idx, ch) in line.char_indices() {
        if ch == ' ' {
            spaces += 1;
            if spaces == 2 {
                if let Some(s) = start.take() {
                    let piece = line[s..idx - 1].trim();
                    if !piece.is_empty() {
                        cols.push(piece);
                    }
                }
            }
        } else {
            if start.is_none() {
                start = Some(idx);
            }
            spaces = 0;
        }
    }
    if let Some(s) = start {
        let piece = line[s..].trim();
        if !piece.is_empty() {
            cols.push(piece);
        }
    }
    cols
}

fn antigravity_pool_key(model_hint: &str) -> &'static str {
    let hint = model_hint.trim().to_lowercase();
    if hint.starts_with("claude") || hint.starts_with("gpt") {
        "claude_gpt"
    } else {
        "gemini"
    }
}

/// Parse the tab-separated table printed by `agy -p /usage`.
/// Converts "Remaining" percentages into MetaBrain's standard used-percentage format.
pub fn parse_antigravity_usage(raw: &str, model_hint: &str, now_ts: f64) -> Vec<QuotaLimit> {
    let clean = strip_ansi(raw);
    let mut pools: Vec<(String, Vec<QuotaLimit>)> = Vec::new();
    for line in clean.lines() {
        let cols = split_usage_cols(line);
        if cols.len() < 3 {
            continue;
        }
        let pool = cols[0];
        let label = cols[1].to_lowercase();
        let pct_raw = cols[2].trim_end_matches('%').trim();
        let Ok(val) = pct_raw.parse::<f64>() else {
            continue;
        };
        let reset = cols
            .get(3)
            .and_then(|s| crate::util::iso_ts(&Value::String((*s).to_string())));
        let used = if reset.is_some_and(|r| r <= now_ts) {
            0.0
        } else if label.contains("remaining")
            || label.contains("left")
            || label.contains("libre")
            || label.contains("disponible")
        {
            (100.0 - val).clamp(0.0, 100.0)
        } else {
            val.clamp(0.0, 100.0)
        };
        let group = if label.contains("five")
            || label.contains("5 h")
            || label.contains("5h")
            || label.contains("session")
        {
            "session"
        } else if label.contains("week") || label.contains("7 d") || label.contains("seman") {
            "weekly"
        } else {
            "quota"
        };
        let entry = QuotaLimit {
            kind: group.to_string(),
            group: group.to_string(),
            pct: Some(used),
            reset,
        };
        let key = pool.to_lowercase();
        if let Some((_, list)) = pools.iter_mut().find(|(k, _)| *k == key) {
            list.push(entry);
        } else {
            pools.push((key, vec![entry]));
        }
    }
    if pools.is_empty() {
        return Vec::new();
    }
    let want_claude_gpt = antigravity_pool_key(model_hint) == "claude_gpt";
    let chosen = if want_claude_gpt {
        pools
            .iter()
            .find(|(k, _)| k.contains("claude") || k.contains("gpt"))
    } else {
        pools.iter().find(|(k, _)| k.contains("gemini"))
    }
    .or_else(|| pools.first());

    let mut out = chosen.map(|(_, list)| list.clone()).unwrap_or_default();
    out.sort_by_key(|q| match q.group.as_str() {
        "session" => 0,
        "weekly" => 1,
        _ => 2,
    });
    out
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AntigravityQuotaCacheEntry {
    ts: f64,
    limits: Vec<QuotaLimit>,
}

fn antigravity_cache_path() -> PathBuf {
    crate::util::data_dir().join("antigravity_quota_cache.json")
}

pub(super) fn load_antigravity_cache_at(
    path: &Path,
    model_hint: &str,
    now_ts: f64,
) -> Option<(f64, Vec<QuotaLimit>)> {
    let raw = crate::util::read_json(path)?;
    let pool = antigravity_pool_key(model_hint);
    let entry: AntigravityQuotaCacheEntry = serde_json::from_value(raw.get(pool)?.clone()).ok()?;
    // Discard readings older than 7 days.
    if entry.ts <= 0.0 || now_ts - entry.ts > 7.0 * 86400.0 || entry.limits.is_empty() {
        return None;
    }
    let adjusted: Vec<QuotaLimit> = entry
        .limits
        .into_iter()
        .map(|mut q| {
            if q.reset.is_some_and(|r| r <= now_ts) {
                q.pct = Some(0.0);
            }
            q
        })
        .collect();
    Some((entry.ts, adjusted))
}

pub(super) fn save_antigravity_cache_at(
    path: &Path,
    model_hint: &str,
    ts: f64,
    limits: &[QuotaLimit],
) {
    if limits.is_empty() {
        return;
    }
    let _flock = crate::util::FileLock::acquire(path, 1500);
    let mut map = crate::util::read_json(path)
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Ok(entry) = serde_json::to_value(AntigravityQuotaCacheEntry {
        ts,
        limits: limits.to_vec(),
    }) {
        map.insert(antigravity_pool_key(model_hint).to_string(), entry);
        let _ = crate::util::write_json(path, &Value::Object(map));
    }
}

/// Last known Google (Antigravity) quota cached on disk, so startup shows quota immediately
/// before the live `agy -p /usage` call completes.
pub fn cached_antigravity_quota(model_hint: &str) -> Option<(f64, Vec<QuotaLimit>)> {
    load_antigravity_cache_at(&antigravity_cache_path(), model_hint, crate::util::now())
}

/// Live Google (Antigravity) plan limits via `agy -p /usage --print-timeout 25s`.
/// Intended to be called from `slow_poll()` on a background thread (never in the UI hot path).
pub fn antigravity_quota(model_hint: &str) -> Result<Vec<QuotaLimit>, String> {
    let bin = find_bin(Agent::Antigravity)
        .ok_or_else(|| "Antigravity CLI is not installed".to_string())?;
    let mut cmd = if is_cmd_script(&bin) {
        let mut c = Command::new("cmd");
        c.arg("/d").arg("/c").arg(&bin);
        c
    } else {
        Command::new(&bin)
    };
    cmd.args(["-p", "/usage", "--print-timeout", "25s"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _ = hidden(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start /usage query: {e}"))?;
    let _job = crate::util::attach_kill_job(&child).ok();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let h_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stdout_pipe {
            let _ = std::io::Read::read_to_end(&mut r, &mut buf);
        }
        buf
    });
    let h_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stderr_pipe {
            let _ = std::io::Read::read_to_end(&mut r, &mut buf);
        }
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            _ => {
                super::kill_child_tree(&mut child);
                drop(_job);
                return Err("timed out querying Antigravity /usage (30s)".into());
            }
        }
    };
    // Drop the kill-on-close Job Object BEFORE joining pipe reader threads so no
    // lingering grandchild can hold stdout/stderr open and deadlock `poll_slow()`.
    drop(_job);
    let stdout_bytes = h_out.join().unwrap_or_default();
    let stderr_bytes = h_err.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let now_ts = crate::util::now();
    let limits = parse_antigravity_usage(&stdout, model_hint, now_ts);
    if limits.is_empty() {
        let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
        if !stderr.is_empty() {
            return Err(stderr);
        }
        let sample: String = stdout.trim().chars().take(160).collect();
        if !sample.is_empty() {
            return Err(format!("unexpected /usage format: {sample}"));
        }
        return Err(format!(
            "no quota data in /usage (exit {:?})",
            status.and_then(|s| s.code())
        ));
    }
    save_antigravity_cache_at(&antigravity_cache_path(), model_hint, now_ts, &limits);
    Ok(limits)
}

fn find_key(v: &Value, key: &str) -> Option<Value> {
    match v {
        Value::Object(m) => m
            .get(key)
            .cloned()
            .or_else(|| m.values().find_map(|x| find_key(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

/// Signed-in ChatGPT account (e-mail) in a Codex folder, read from the token's claims.
pub fn codex_account(home: &Path) -> Option<String> {
    let auth: CodexAuthFile =
        serde_json::from_str(&std::fs::read_to_string(home.join("auth.json")).ok()?).ok()?;
    let tok = auth.tokens.map(|t| t.id_token).unwrap_or_default();
    let payload = tok.split('.').nth(1).unwrap_or("");
    let email = b64url(payload)
        .and_then(|b| serde_json::from_slice::<JwtClaims>(&b).ok())
        .and_then(|c| c.email);
    Some(email.unwrap_or_default())
}

fn b64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}
