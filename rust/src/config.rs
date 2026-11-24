//! Configuration: one JSON file describes every brain, service and option (port of config.py).
//! Kept as free-form JSON so unknown keys survive hand edits.
use crate::util::{data_dir, home, truthy, write_json, J};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

pub const EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];

pub fn templates() -> Value {
    json!({
        "claude_code": {
            "kind": "claude_code", "name": "Claude", "role": "director",
            "config_dir": "~/.claude",
            "match": {},
            "apply": "hook",
            "max_parallel": 2,
            "ladder": ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
            "budget": {"tokens_day": null, "usd_day": null, "quota_pct": 90},
            "delegates_to": {"mcp__deepseek": "deepseek"}
        },
        "openai": {
            "kind": "openai", "name": "DeepSeek", "role": "worker",
            "base_url": "https://api.deepseek.com", "key_env": "DEEPSEEK_API_KEY", "balance": "deepseek",
            "max_parallel": 4,
            "ladder": ["deepseek-v4-pro", "deepseek-flash"],
            "reasoning": "reasoning_effort",
            "prices": {},
            "usage_file": "",
            "budget": {"tokens_day": 300000, "usd_day": 1.0, "quota_pct": null}
        },
        "ollama": {
            "kind": "ollama", "name": "Ollama", "role": "local",
            "base_url": "http://127.0.0.1:11434", "ladder": [],
            "max_parallel": 1,
            "budget": {"tokens_day": null, "usd_day": null, "quota_pct": null}
        },
        "anthropic": {
            "kind": "anthropic", "name": "Claude API", "role": "api",
            "base_url": "https://api.anthropic.com", "key_env": "ANTHROPIC_API_KEY",
            "max_parallel": 2,
            "ladder": ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
            "prices": {}, "budget": {"tokens_day": null, "usd_day": 5.0, "quota_pct": null}
        },
        "opencode": {
            "kind": "opencode", "name": "OpenCode", "role": "agent",
            "base_url": "http://127.0.0.1:4096", "password_env": "OPENCODE_SERVER_PASSWORD", "username": "opencode",
            "max_parallel": 2,
            "ladder": [], "budget": {"tokens_day": null, "usd_day": null, "quota_pct": null}
        },
        "codex": {
            "kind": "codex", "name": "ChatGPT", "role": "producer",
            "ladder": ["gpt-6.2-sol"], "max_parallel": 2,
            "budget": {"tokens_day": null, "usd_day": null, "quota_pct": null}
        },
        "antigravity": {
            "kind": "antigravity", "name": "Gemini (Google)", "role": "producer",
            "ladder": [], "max_parallel": 2,
            "budget": {"tokens_day": null, "usd_day": null, "quota_pct": null}
        },
        "gemini_cli": {
            "kind": "gemini_cli", "name": "Google", "role": "producer",
            "ladder": ["gemini-3-pro", "gemini-3-flash"], "max_parallel": 2,
            "budget": {"tokens_day": null, "usd_day": null, "quota_pct": null}
        },
        "gemini": {
            "kind": "openai", "name": "Gemini", "role": "reader", "provider": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "key_env": "GEMINI_API_KEY",
            "ladder": [],
            "reasoning": "reasoning_effort",
            "vision": true,
            "max_parallel": 2,
            "prices": {},
            "budget": {"tokens_day": null, "usd_day": 0.5, "quota_pct": null}
        }
    })
}

pub fn default_cfg() -> Value {
    json!({
        "lang": "en",
        "port": 8790,
        "lan": {"enabled": false, "pin": ""},
        "mode": "normal",
        "rules": {"downgrade_at": 80, "pause_at": 100, "auto_resume": true},
        "history_days": 30,
        "max_parallel_total": 8,
        "brains": [],
        "services": [],
        "memory": {"enabled": true, "dir": "~/metabrain-memory", "roots": [], "lock_minutes": 20},
        "telegram": {"enabled": false, "token_env": "METABRAIN_TELEGRAM_TOKEN", "chat_id": ""}
    })
}

/// OpenAI-compatible brains expose a models endpoint relative to their base URL.
pub fn models_url(brain: &Value) -> Option<String> {
    if brain.s("kind") != "openai" {
        return None;
    }
    let base = brain.s("base_url");
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        None
    } else {
        Some(format!("{base}/models"))
    }
}

/// Extract model ids from an OpenAI-compatible /models response.
/// Gemini ids are cleaned, filtered and ordered by usefulness.
pub fn parse_models(provider: &str, body: &Value) -> Vec<String> {
    let ids: Vec<String> = body
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    if provider != "gemini" {
        return ids;
    }

    let mut out: Vec<String> = ids
        .into_iter()
        .filter_map(|id| {
            let id = strip_models_prefix(id);
            let lower = id.to_lowercase();
            let blocked = ["embedding", "aqa", "imagen", "tts", "veo"]
                .into_iter()
                .any(|bad| lower.contains(bad));
            if blocked {
                None
            } else {
                Some(id)
            }
        })
        .collect();

    out.sort_by(|a, b| {
        model_group(a)
            .cmp(&model_group(b))
            .then_with(|| version_numbers(b).cmp(&version_numbers(a)))
            .then_with(|| a.cmp(b))
    });
    out
}

fn strip_models_prefix(id: String) -> String {
    if let Some(rest) = id.strip_prefix("models/") {
        return rest.to_string();
    }
    id
}

fn model_group(id: &str) -> u8 {
    let lower = id.to_lowercase();
    if lower.contains("pro") {
        0
    } else if lower.contains("flash") {
        1
    } else {
        2
    }
}

fn version_numbers(id: &str) -> Vec<u64> {
    let mut nums = Vec::new();
    let mut cur: Option<u64> = None;
    for ch in id.chars() {
        if let Some(d) = ch.to_digit(10) {
            let d = u64::from(d);
            cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
        } else if let Some(n) = cur.take() {
            nums.push(n);
        }
    }
    if let Some(n) = cur {
        nums.push(n);
    }
    nums
}

pub struct Config {
    pub path: PathBuf,
    pub data: Mutex<Value>,
}

/// Parse and validate a port value (`1..=65535`) from a JSON config object.
pub fn valid_port(cfg: &Value) -> Option<u16> {
    let v = cfg.get("port")?;
    if let Some(n) = v.as_u64() {
        return (1..=65535).contains(&n).then_some(n as u16);
    }
    if let Some(n) = v.as_i64() {
        return (1..=65535).contains(&n).then_some(n as u16);
    }
    if let Some(f) = v.as_f64() {
        if f.is_finite() && f.fract() == 0.0 && (1.0..=65535.0).contains(&f) {
            return Some(f as u16);
        }
    }
    crate::util::log(&format!("config: ignoring invalid port value: {v}"));
    None
}

impl Config {
    pub fn new(path: Option<PathBuf>) -> Config {
        let path = path.unwrap_or_else(|| data_dir().join("metabrain.json"));
        let data = Self::load(&path);
        Config {
            path,
            data: Mutex::new(data),
        }
    }

    /// Build from an in-memory value (tests only).
    #[cfg(test)]
    pub fn from_value(path: PathBuf, raw: Value) -> Config {
        let _ = write_json(&path, &raw);
        Config::new(Some(path))
    }

    fn load(path: &std::path::Path) -> Value {
        let mut read_res = std::fs::read(path);
        for attempt in 0..4u64 {
            match &read_res {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                    ) || e.raw_os_error() == Some(32) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(25 * (attempt + 1)));
                    read_res = std::fs::read(path);
                }
                _ => break,
            }
        }
        let mut persist_allowed = true;
        let raw = match read_res {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v @ Value::Object(_)) => v,
                _ => {
                    let ts = crate::util::now() as u64;
                    let bak = path.with_file_name(format!(
                        "{}.corrupt.{ts}",
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "metabrain.json".into())
                    ));
                    match std::fs::copy(path, &bak) {
                        Ok(_) => {
                            crate::util::log(&format!(
                                "config: corrupted {} backed up to {}",
                                path.display(),
                                bak.display()
                            ));
                            let r = first_run_config();
                            if let Err(e) = write_json(path, &r) {
                                crate::util::log(&format!(
                                    "config: failed writing initial config {}: {e}",
                                    path.display()
                                ));
                            }
                            r
                        }
                        Err(e) => {
                            // Never overwrite the user's file if we couldn't preserve a backup first.
                            persist_allowed = false;
                            crate::util::log(&format!(
                                "config: failed to back up corrupted {} ({e}); refusing to overwrite on disk",
                                path.display()
                            ));
                            first_run_config()
                        }
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let r = first_run_config();
                if let Err(err) = write_json(path, &r) {
                    crate::util::log(&format!(
                        "config: failed writing initial config {}: {err}",
                        path.display()
                    ));
                }
                r
            }
            Err(e) => {
                // Transient I/O / permission lock error: fall back in memory without overwriting disk.
                persist_allowed = false;
                crate::util::log(&format!(
                    "config: transient error reading {} ({e}); using in-memory defaults without overwriting disk",
                    path.display()
                ));
                first_run_config()
            }
        };
        let mut cfg = default_cfg();
        merge_shallow(&mut cfg, &raw);
        if cfg["lan"].s("pin").is_empty() {
            use rand::Rng;
            let pin = format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32));
            if !cfg["lan"].is_object() {
                cfg["lan"] = json!({});
            }
            cfg["lan"]["pin"] = json!(pin);
            if persist_allowed {
                if let Err(e) = write_json(path, &cfg) {
                    crate::util::log(&format!(
                        "config: failed persisting generated PIN {}: {e}",
                        path.display()
                    ));
                }
            }
        }
        if let Some(bs) = cfg.get_mut("brains").and_then(|b| b.as_array_mut()) {
            for b in bs.iter_mut() {
                normalize_brain(b);
            }
        }
        cfg
    }

    /// Snapshot of the whole config.
    pub fn get(&self) -> Value {
        self.data.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn save(&self) {
        let d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = write_json(&self.path, &d) {
            crate::util::log(&format!(
                "config: save failed for {}: {e}",
                self.path.display()
            ));
        }
    }

    /// Validate and swap the whole config (from the HMI's editor).
    pub fn replace(&self, mut new: Value) -> Vec<String> {
        let errs = validate(&new);
        if !errs.is_empty() {
            return errs;
        }
        if let Some(bs) = new.get_mut("brains").and_then(|b| b.as_array_mut()) {
            for b in bs.iter_mut() {
                normalize_brain(b);
            }
        }
        let mut merged = default_cfg();
        merge_shallow(&mut merged, &new);
        {
            let mut cur = self.data.lock().unwrap_or_else(|e| e.into_inner());
            let prev_pin = cur["lan"].s("pin");
            if merged["lan"].s("pin").is_empty() && !prev_pin.is_empty() {
                if !merged["lan"].is_object() {
                    merged["lan"] = json!({});
                }
                merged["lan"]["pin"] = json!(prev_pin);
            }
            *cur = merged;
        }
        self.save();
        vec![]
    }

    pub fn brain(&self, bid: &str) -> Option<Value> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())["brains"]
            .as_array()?
            .iter()
            .find(|b| b.s("id") == bid)
            .cloned()
    }

    pub fn brains(&self) -> Vec<Value> {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .arr("brains")
    }

    /// Mutate one brain's config in place (false if not found). Caller saves.
    pub fn with_brain<F: FnOnce(&mut Value)>(&self, bid: &str, f: F) -> bool {
        let mut d = self.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = d
            .get_mut("brains")
            .and_then(|b| b.as_array_mut())
            .and_then(|a| a.iter_mut().find(|b| b.s("id") == bid))
        {
            f(b);
            true
        } else {
            false
        }
    }

    pub fn key(&self, k: &str) -> Value {
        self.data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(k)
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub fn set_key(&self, k: &str, v: Value) {
        self.data.lock().unwrap_or_else(|e| e.into_inner())[k] = v;
    }
}

/// cfg[k].update(v) for dicts, else cfg[k] = v  (Config._load).
fn merge_shallow(cfg: &mut Value, raw: &Value) {
    let Some(r) = raw.as_object() else { return };
    for (k, v) in r {
        match (cfg.get_mut(k), v) {
            (Some(Value::Object(dst)), Value::Object(src)) => {
                for (kk, vv) in src {
                    dst.insert(kk.clone(), vv.clone());
                }
            }
            _ => {
                cfg[k] = v.clone();
            }
        }
    }
}

pub fn normalize_brain(b: &mut Value) {
    if !b.is_object() {
        return;
    }
    let base = templates().get(b.s("kind")).cloned().unwrap_or(json!({}));
    if let Some(base) = base.as_object() {
        for (k, v) in base {
            match b.get_mut(k) {
                None => {
                    b[k] = v.clone();
                }
                Some(Value::Object(cur)) => {
                    if let Some(vo) = v.as_object() {
                        for (kk, vv) in vo {
                            cur.entry(kk.clone()).or_insert_with(|| vv.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let o = b.as_object_mut().unwrap();
    o.entry("enabled").or_insert(json!(true));
    let has_valid_id = o
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_valid_id {
        let n = o
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .unwrap_or_else(|| {
                o.get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("brain")
                    .to_string()
            });
        let slug: String = n
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let slug = slug.trim_matches('_').to_string();
        o.insert(
            "id".into(),
            json!(if slug.is_empty() {
                "brain".into()
            } else {
                slug
            }),
        );
    }
}

pub fn validate(cfg: &Value) -> Vec<String> {
    let mut errs = vec![];
    if !cfg.is_object() {
        return vec!["config must be a JSON object".into()];
    }
    let t = templates();
    let kinds: Vec<String> = t.as_object().unwrap().keys().cloned().collect();
    let mut ids: Vec<String> = vec![];
    for (i, b) in cfg.arr("brains").iter().enumerate() {
        if !kinds.contains(&b.s("kind")) {
            errs.push(format!(
                "brain #{}: kind must be one of {}",
                i + 1,
                kinds.join(", ")
            ));
        }
        if let Some(id) = b.os("id") {
            if !crate::util::valid_id(&id) {
                errs.push(format!(
                    "brain #{}: id '{}' must contain only letters, digits, '_' or '-'",
                    i + 1,
                    id
                ));
            }
        }
        match b.os("id").or_else(|| b.os("name")) {
            None => errs.push(format!("brain #{}: needs an id or name", i + 1)),
            Some(id) => {
                if ids.contains(&id) {
                    errs.push(format!("brain #{}: duplicated id '{}'", i + 1, id));
                }
                ids.push(id);
            }
        }
    }
    for (i, s) in cfg.arr("services").iter().enumerate() {
        if s.os("id").is_none() || !truthy(s.get("cmd")) {
            errs.push(format!("service #{}: needs id and cmd", i + 1));
        }
    }
    if cfg.get("port").is_some() && valid_port(cfg).is_none() {
        errs.push("port must be an integer between 1 and 65535".into());
    }
    errs
}

fn env_nonempty(name: &str) -> bool {
    std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Detect what this machine already has and propose brains for it.
pub fn first_run_config() -> Value {
    let mut cfg = default_cfg();
    let t = templates();
    let h = home();
    let mut brains = vec![];
    if h.join(".claude").is_dir() {
        let mut b = t["claude_code"].clone();
        b["id"] = json!("claude");
        b["name"] = json!("Claude Code");
        brains.push(b);
    }
    let mut extras: Vec<String> = std::fs::read_dir(&h)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    extras.sort();
    for extra in extras {
        if extra.starts_with(".claude-") && h.join(&extra).join(".credentials.json").is_file() {
            let mut b = t["claude_code"].clone();
            b["id"] = json!(extra[1..].replace('-', "_"));
            b["name"] = json!(format!("Claude {}", extra[8..].to_uppercase()));
            b["role"] = json!("producer");
            b["config_dir"] = json!(format!("~/{extra}"));
            brains.push(b);
        }
    }
    if env_nonempty("DEEPSEEK_API_KEY") {
        let mut b = t["openai"].clone();
        b["id"] = json!("deepseek");
        brains.push(b);
    }

    let has_gemini = env_nonempty("GEMINI_API_KEY");
    let has_google = env_nonempty("GOOGLE_API_KEY");
    if (has_gemini || has_google) && !brains.iter().any(|b| b.s("provider") == "gemini") {
        if let Some(tmpl) = t.get("gemini") {
            let mut b = tmpl.clone();
            b["id"] = json!("gemini");
            b["key_env"] = json!(if has_gemini {
                "GEMINI_API_KEY"
            } else {
                "GOOGLE_API_KEY"
            });
            brains.push(b);
        }
    }

    cfg["brains"] = Value::Array(brains);
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::J;
    use serde_json::json;

    #[test]
    fn templates_include_gemini_and_parallelism() {
        let t = templates();
        assert_eq!(t["claude_code"].f("max_parallel"), Some(2.0));
        assert_eq!(t["openai"].f("max_parallel"), Some(4.0));
        assert_eq!(t["anthropic"].f("max_parallel"), Some(2.0));
        assert_eq!(t["ollama"].f("max_parallel"), Some(1.0));
        assert_eq!(t["opencode"].f("max_parallel"), Some(2.0));

        let g = &t["gemini"];
        assert_eq!(g.s("kind"), "openai");
        assert_eq!(g.s("provider"), "gemini");
        assert_eq!(g.s("key_env"), "GEMINI_API_KEY");
        assert!(g.b("vision", false));
        assert_eq!(g.f("max_parallel"), Some(2.0));
        assert_eq!(g["budget"]["usd_day"].as_f64(), Some(0.5));
    }

    #[test]
    fn models_url_only_for_openai_brains() {
        let brain = json!({"kind": "openai", "base_url": "https://example.com/v1/"});
        assert_eq!(
            models_url(&brain).as_deref(),
            Some("https://example.com/v1/models")
        );
        assert_eq!(models_url(&json!({"kind": "openai", "base_url": ""})), None);
        assert_eq!(
            models_url(&json!({"kind": "ollama", "base_url": "http://127.0.0.1:11434"})),
            None
        );
    }

    #[test]
    fn parse_models_cleans_and_orders_gemini_ids() {
        let body = json!({"data": [
            {"id": "models/gemini-3.8-flash"},
            {"id": "models/gemini-3.5-pro"},
            {"id": "models/text-embedding-004"},
            {"id": "models/gemini-3.6-flash"}
        ]});
        let got = parse_models("gemini", &body);
        let got_refs: Vec<&str> = got.iter().map(String::as_str).collect();
        assert_eq!(
            got_refs,
            vec!["gemini-3.5-pro", "gemini-3.8-flash", "gemini-3.6-flash"]
        );
    }

    #[test]
    fn parse_models_keeps_other_providers_as_is() {
        let body = json!({"data": [{"id": "models/b"}, {"id": "a"}]});
        assert_eq!(
            parse_models("openai", &body),
            vec!["models/b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn default_config_has_global_parallelism() {
        assert_eq!(default_cfg().f("max_parallel_total"), Some(8.0));
    }

    #[test]
    fn valid_port_checks_bounds_and_corrupt_config_creates_backup() {
        assert_eq!(valid_port(&json!({"port": 8790})), Some(8790));
        assert_eq!(valid_port(&json!({"port": 0})), None);
        assert_eq!(valid_port(&json!({"port": 70000})), None);
        assert_eq!(valid_port(&json!({"port": -5})), None);

        let dir = std::env::temp_dir().join(format!("mb_cfg_corrupt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("metabrain.json");
        std::fs::write(&cfg_path, "{broken json").unwrap();

        let _cfg = Config::new(Some(cfg_path.clone()));
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("metabrain.json.corrupt.")
            })
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "corrupted config must be backed up before regeneration"
        );
        assert!(crate::util::read_json(&cfg_path).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
