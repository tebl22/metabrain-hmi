//! learn.rs — smart classification (heuristic cache + DeepSeek), automatic learning from
//! outcomes and shadow mode. Everything is best-effort: on error we fall back to defaults,
//! never panic and never log secrets.
use crate::router::{classify, escalate, Router};
use crate::util::{data_dir, http_json, log, now, read_json, secret, truthy, write_json, J};
use serde_json::{json, Value};
use sha1_smol::Sha1;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Classification cache TTL (30 days).
const CACHE_TTL: f64 = 30.0 * 86_400.0;
/// Retry / implicit-success window (30 minutes).
const RETRY_WINDOW: f64 = 30.0 * 60.0;
/// Evidence weights: an executable check or the user's own correction counts fully; a retry of
/// similar wording is a fair hint; silence for 30 minutes is a weak hint of success.
pub const W_VERIFIED: f64 = 1.0;
pub const W_USER: f64 = 1.0;
pub const W_RETRY: f64 = 0.6;
pub const W_IMPLICIT: f64 = 0.25;
/// User-correction window (60 minutes).
const PROMPT_WINDOW: f64 = 60.0 * 60.0;
const MAX_CACHE: usize = 2000;
const MAX_PENDING: usize = 200;
const SHADOW_MAX_BYTES: u64 = 5_000_000;

const SYSTEM_PROMPT: &str = "Clasifica la dificultad de una tarea de programación/producción para elegir el modelo de IA más barato que la haga bien. Niveles: 1 mecánico (renombrar, mover, formatear, leer logs, convertir formatos); 2 rutina (script simple, test, plantilla ffmpeg/blender, arreglo pequeño conocido); 3 desarrollo (implementar una función o vista, refactor acotado, integrar una API); 4 difícil (arquitectura, depurar errores raros, rendimiento, seguridad, concurrencia); 5 crítico (decisión de producto, revisión final, publicar, algo irreversible). Responde SOLO JSON: {\"level\":n,\"why\":\"motivo de máximo 8 palabras\"}";

const VERIFY_CMDS: &[&str] = &[
    "cargo test",
    "cargo build",
    "cargo check",
    "pytest",
    "npm test",
    "npm run build",
    "go test",
    "ffmpeg",
    "blender",
];
const FAIL_WORDS: &[&str] = &[
    "test result: failed",
    "error[e",
    "error:",
    "traceback",
    "exception",
    "panicked at",
];
const OK_WORDS: &[&str] = &["test result: ok", "passed", "finished", "build succeeded"];
const CORRECTION_WORDS: &[&str] = &[
    "esta mal",
    "rehaz",
    "no funciona",
    "no sirve",
    "sigue fallando",
    "otra vez lo mismo",
    "wrong",
    "doesn't work",
    "redo",
];

// ---------- text helpers ----------

/// Lowercase and strip accents (keyword tables stay ASCII).
fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' | 'ã' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            '’' | '‘' => '\'',
            c => c,
        })
        .collect()
}

/// Lowercase, collapse whitespace and cap at 600 chars (cache key material).
fn normalize(task: &str) -> String {
    let joined = task.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.to_lowercase().chars().take(600).collect()
}

fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    h.digest().to_string()
}

/// Normalised word set: lowercase, no accents, >= 3 letters, deduped, max 40 words.
fn words_of(task: &str) -> Vec<String> {
    let text = fold(task);
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        if w.chars().count() < 3 {
            continue;
        }
        if out.iter().any(|x| x.as_str() == w) {
            continue;
        }
        out.push(w.to_string());
        if out.len() >= 40 {
            break;
        }
    }
    out
}

/// Jaccard similarity between two word sets (0.0 when either side is empty).
fn similar(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.iter().filter(|x| b.iter().any(|y| y == *x)).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

// ---------- paths / files ----------

fn cache_path() -> PathBuf {
    data_dir().join("learn_cache.json")
}

fn pending_path() -> PathBuf {
    data_dir().join("learn_pending.json")
}

fn shadow_path() -> PathBuf {
    data_dir().join("learn_shadow.jsonl")
}

/// Append one JSON line; rotate the file to ".1" when it grows past 5 MB.
fn append_line(path: &Path, value: &Value) {
    if let Ok(m) = fs::metadata(path) {
        if m.len() > SHADOW_MAX_BYTES {
            let bak = PathBuf::from(format!("{}.1", path.display()));
            let _ = fs::remove_file(&bak);
            let _ = fs::rename(path, &bak);
        }
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", value);
    }
}

fn shadow_line(value: Value) {
    append_line(&shadow_path(), &value);
}

// ---------- classification cache ----------

fn cache_get(key: &str) -> Option<(u8, String)> {
    let v = read_json(&cache_path())?;
    let e = v.get(key)?;
    let ts = e.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0);
    if now() - ts > CACHE_TTL {
        return None;
    }
    let level = e.get("level").and_then(|l| l.as_u64())?;
    if !(1..=5).contains(&level) {
        return None;
    }
    let why = e
        .get("why")
        .and_then(|w| w.as_str())
        .unwrap_or("")
        .to_string();
    Some((level as u8, why))
}

fn cache_put(key: &str, level: u8, why: &str) {
    let p = cache_path();
    let mut v = read_json(&p)
        .filter(|x| x.is_object())
        .unwrap_or_else(|| json!({}));
    let obj = match v.as_object_mut() {
        Some(m) => m,
        None => return,
    };
    obj.insert(
        key.to_string(),
        json!({"level": level, "why": why, "ts": now()}),
    );
    if obj.len() > MAX_CACHE {
        // Drop the oldest entries until we are back at the cap.
        let mut keys: Vec<(String, f64)> = obj
            .iter()
            .map(|(k, e)| {
                (
                    k.clone(),
                    e.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0),
                )
            })
            .collect();
        keys.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let excess = obj.len() - MAX_CACHE;
        for (k, _) in keys.into_iter().take(excess) {
            obj.remove(&k);
        }
    }
    if let Err(e) = write_json(&p, &v) {
        log(&format!(
            "learn: no se pudo guardar la cache de clasificacion: {}",
            e
        ));
    }
}

fn llm_enabled(cfg: &Value) -> bool {
    cfg.get("router")
        .and_then(|r| r.get("llm_classify"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn parse_json_loose(s: &str) -> Option<Value> {
    let t = s.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    // Some providers wrap the JSON object in prose (or double-encode it).
    let start = t.find('{')?;
    let end = t.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&t[start..=end]).ok()
}

/// Ask DeepSeek for the difficulty level. Returns None on any problem (no brains, no key,
/// network, bad payload) so the caller can fall back to the heuristic.
fn deepseek_classify(cfg: &Value, norm: &str, chain: &[Value]) -> Option<(u8, String)> {
    let brain = cfg.arr("brains").into_iter().find(|b| {
        if !chain
            .iter()
            .any(|e| e["id"] == b["id"] && e.b("available", true))
            || !b.b("enabled", true)
            || b.s("kind").to_lowercase() != "openai"
        {
            return false;
        }
        let id = b.s("id").to_lowercase();
        let base = b.s("base_url").to_lowercase();
        id.contains("deepseek") || base.contains("deepseek")
    })?;
    let key = secret(&brain.s("key_env"))?;
    let base = brain.os("base_url")?;
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let ladder = brain.arr("ladder");
    let model = ladder
        .iter()
        .filter_map(|v| v.as_str())
        .find(|m| m.to_lowercase().contains("flash") || m.to_lowercase().contains("chat"))
        .or_else(|| ladder.last().and_then(|v| v.as_str()))
        .unwrap_or("deepseek-flash")
        .to_string();
    let body = json!({
        "model": model,
        "max_tokens": 80,
        "temperature": 0,
        "thinking": {"type": "disabled"},
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": norm}
        ]
    });
    let auth = format!("Bearer {}", key);
    let resp = http_json(&url, &[("Authorization", auth.as_str())], Some(&body), 6).ok()?;
    let content = resp
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;
    let parsed = parse_json_loose(content)?;
    let level = parsed.get("level").and_then(|v| v.as_u64())?;
    if !(1..=5).contains(&level) {
        return None;
    }
    let why = parsed
        .get("why")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().chars().take(80).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "clasificado por deepseek".to_string());
    Some((level as u8, why))
}

/// (level 1..5, short reason, source in {"cache","deepseek","palabras"}).
pub fn classify_smart(cfg: &Value, task: &str, chain: &[Value]) -> (u8, String, String) {
    let norm = normalize(task);
    if !norm.is_empty() {
        let key = sha1_hex(&norm);
        if let Some((level, why)) = cache_get(&key) {
            return (level, why, "cache".to_string());
        }
        let (level, why) = classify(task);
        if level == 1 || why.starts_with("palabra clave:") {
            return (level, why, "palabras".into());
        }
        if llm_enabled(cfg) {
            if let Some((level, why)) = deepseek_classify(cfg, &norm, chain) {
                cache_put(&key, level, &why);
                return (level, why, "deepseek".to_string());
            }
        }
    }
    let (level, why) = classify(task);
    (level, why, "palabras".to_string())
}

// ---------- pending attempts ----------

#[derive(Clone)]
struct Pending {
    id: String,
    ts: f64,
    project: String,
    level: u8,
    candidate: String,
    task: String,
    words: Vec<String>,
    source: String,
}

impl Pending {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id.clone(),
            "ts": self.ts,
            "project": self.project.clone(),
            "level": self.level,
            "candidate": self.candidate.clone(),
            "task": self.task.clone(),
            "words": self.words.clone(),
            "source": self.source.clone()
        })
    }

    fn from_value(v: &Value) -> Option<Pending> {
        let id = v.os("id")?;
        let level = v
            .get("level")
            .and_then(|x| x.as_u64())
            .unwrap_or(1)
            .clamp(1, 5) as u8;
        let words: Vec<String> = v
            .arr("words")
            .iter()
            .filter_map(|w| w.as_str().map(|s| s.to_string()))
            .collect();
        Some(Pending {
            id,
            ts: v.f("ts").unwrap_or(0.0),
            project: v.s("project"),
            level,
            candidate: v.s("candidate"),
            task: v.s("task"),
            words,
            source: v.s("source"),
        })
    }
}

fn load_pending() -> Vec<Pending> {
    let v = read_json(&pending_path()).unwrap_or(Value::Null);
    v.get("items")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(Pending::from_value).collect())
        .unwrap_or_default()
}

fn save_pending(items: &[Pending]) {
    let arr: Vec<Value> = items.iter().map(|p| p.to_value()).collect();
    if let Err(e) = write_json(&pending_path(), &json!({"items": arr})) {
        log(&format!("learn: no se pudo guardar learn_pending: {}", e));
    }
}

fn new_id(ts: f64, task: &str) -> String {
    sha1_hex(&format!("{:.6}|{}", ts, task))
        .chars()
        .take(12)
        .collect()
}

/// Locate a pending attempt: by exact id, else the most recent one for the project (or global).
fn find_pending(items: &[Pending], project: Option<&str>, id: Option<&str>) -> Option<usize> {
    if let Some(id) = id {
        if let Some(i) = items.iter().position(|p| p.id == id) {
            return Some(i);
        }
    }
    let mut best: Option<usize> = None;
    for (i, p) in items.iter().enumerate() {
        if let Some(pr) = project {
            if p.project != pr {
                continue;
            }
        }
        let newer = match best {
            Some(b) => p.ts > items[b].ts,
            None => true,
        };
        if newer {
            best = Some(i);
        }
    }
    best
}

// ---------- automatic routing + learning ----------

/// Expire old pending attempts: nobody retried nor failed them in 30 min => implicit success.
pub fn sweep(router: &Router) {
    let tnow = now();
    let mut items = load_pending();
    if items.is_empty() {
        return;
    }
    let mut keep: Vec<Pending> = Vec::with_capacity(items.len());
    for p in items.drain(..) {
        if tnow - p.ts > RETRY_WINDOW {
            let _ = router.record_weighted(&p.project, p.level, &p.candidate, true, W_IMPLICIT, 0);
            shadow_line(json!({
                "ts": tnow,
                "kind": "outcome",
                "project": p.project.as_str(),
                "level": p.level,
                "candidate": p.candidate.as_str(),
                "ok": true,
                "why": "sin reintento en 30 min"
            }));
        } else {
            keep.push(p);
        }
    }
    if keep.len() > MAX_PENDING {
        keep.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let cut = keep.len() - MAX_PENDING;
        keep.drain(0..cut);
    }
    save_pending(&keep);
}

/// Classify + route, detect retries, escalate them and remember the attempt as pending.
pub fn route_auto(
    router: &Router,
    cfg: &Value,
    chain: &[Value],
    project: &str,
    task: &str,
) -> Value {
    route_auto_in(router, cfg, chain, project, task, false)
}

/// Like `route_auto`; with `verified` (the project has an automatic check) the first attempt
/// starts one level cheaper: the classification is only a prior, and a failed check escalates.
pub fn route_auto_in(
    router: &Router,
    cfg: &Value,
    chain: &[Value],
    project: &str,
    task: &str,
    verified: bool,
) -> Value {
    sweep(router);

    let (mut level, mut why, source) = classify_smart(cfg, task, chain);
    let classified = level;
    let starts_cheaper = verified && matches!(level, 2 | 3);
    if starts_cheaper {
        level -= 1;
        why = format!("{why}; empieza un nivel más barato porque hay verificación automática");
    }
    let words = words_of(task);
    let tnow = now();
    let mut items = load_pending();

    // Retry detection: same project, < 30 min old, similar wording.
    let mut retry: Option<Pending> = None;
    for p in items
        .iter()
        .filter(|_| !cfg["router"].b("task_harnesses", false))
    {
        if p.project != project || tnow - p.ts >= RETRY_WINDOW {
            continue;
        }
        if similar(&words, &p.words) < 0.5 {
            continue;
        }
        let newer = match &retry {
            Some(r) => p.ts > r.ts,
            None => true,
        };
        if newer {
            retry = Some(p.clone());
        }
    }

    let mut escalated = false;
    let mut retry_of = String::new();
    let mut forced: Option<String> = None;

    if let Some(prev) = retry {
        // The previous attempt failed; record it and force the next rung of the ladder.
        let _ = router.record_weighted(
            &prev.project,
            prev.level,
            &prev.candidate,
            false,
            W_RETRY,
            0,
        );
        items.retain(|p| p.id != prev.id);
        retry_of = prev.id.clone();
        escalated = true;
        match escalate(prev.level, &prev.candidate) {
            Some(c) => {
                level = level.max(prev.level);
                forced = Some(c);
            }
            None => {
                level = (prev.level.saturating_add(1)).min(5);
            }
        }
    }

    let mut v = router.route(cfg, chain, project, task, Some(level));

    // The next rung may live on the next level: route there so it appears among the alternatives.
    if let Some(c) = forced.as_ref() {
        let present = v
            .get("alternatives")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().any(|x| &x.s("candidate") == c))
            .unwrap_or(false);
        if !present && level < 5 {
            level += 1;
            v = router.route(cfg, chain, project, task, Some(level));
        }
    }

    if let Some(c) = forced {
        let alt = v
            .get("alternatives")
            .and_then(|a| a.as_array())
            .and_then(|a| {
                a.iter().find(|x| {
                    x.s("candidate") == c
                        && x.get("available")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false)
                })
            })
            .cloned();
        if let Some(alt) = alt {
            let pick = json!({
                "candidate": alt.s("candidate"),
                "brain": alt.s("brain"),
                "model": alt.s("model"),
                "effort": alt.s("effort")
            });
            let cost = alt.get("cost").and_then(|x| x.as_f64()).unwrap_or(30.0);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("pick".to_string(), pick);
                obj.insert(
                    "saving_vs_opus_pct".to_string(),
                    json!(round1(100.0 * (1.0 - cost / 150.0))),
                );
            }
        }
    }

    let id = new_id(now(), task);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("why".to_string(), json!(why));
        obj.insert("source".to_string(), json!(source.clone()));
        obj.insert("id".to_string(), json!(id));
        obj.insert("classified_level".to_string(), json!(classified));
        obj.insert("verified_start".to_string(), json!(starts_cheaper));
        if escalated {
            obj.insert("retry_of".to_string(), json!(retry_of));
            obj.insert("escalated".to_string(), json!(true));
        }
    }

    let candidate = v.get("pick").map(|p| p.s("candidate")).unwrap_or_default();
    if !candidate.is_empty() {
        items.push(Pending {
            id: id.clone(),
            ts: now(),
            project: project.to_string(),
            level: level.clamp(1, 5),
            candidate,
            task: task.to_string(),
            words,
            source,
        });
    }
    if items.len() > MAX_PENDING {
        items.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let cut = items.len() - MAX_PENDING;
        items.drain(0..cut);
    }
    save_pending(&items);
    v
}

/// Resolve a pending attempt (by id or most recent) and record the real outcome.
pub fn outcome(
    router: &Router,
    project: Option<&str>,
    id: Option<&str>,
    ok: bool,
    why: &str,
) -> Value {
    outcome_w(router, project, id, ok, W_USER, why)
}

/// `outcome` with an explicit evidence weight (see W_*).
pub fn outcome_w(
    router: &Router,
    project: Option<&str>,
    id: Option<&str>,
    ok: bool,
    weight: f64,
    why: &str,
) -> Value {
    outcome_with_tokens(router, project, id, ok, weight, why, 0)
}

pub(crate) fn outcome_with_tokens(
    router: &Router,
    project: Option<&str>,
    id: Option<&str>,
    ok: bool,
    weight: f64,
    why: &str,
    tokens: i64,
) -> Value {
    let mut items = load_pending();
    let idx = match find_pending(&items, project, id) {
        Some(i) => i,
        None => return json!({"resolved": false}),
    };
    let p = items.remove(idx);
    save_pending(&items);

    let rec = router.record_weighted(&p.project, p.level, &p.candidate, ok, weight, tokens);
    shadow_line(json!({
        "ts": now(),
        "kind": "outcome",
        "weight": weight,
        "project": p.project.as_str(),
        "level": p.level,
        "candidate": p.candidate.as_str(),
        "ok": ok,
        "why": why
    }));

    let mut out = json!({"resolved": true});
    if let (Some(dst), Some(src)) = (out.as_object_mut(), rec.as_object()) {
        for (k, val) in src {
            dst.insert(k.clone(), val.clone());
        }
    }
    out
}

/// After a failed check: route the same task to the next rung of the ladder (or the next level)
/// and remember that attempt as pending. Returns the routing, or Null when nothing is above.
pub fn escalate_after(
    router: &Router,
    cfg: &Value,
    chain: &[Value],
    project: &str,
    task: &str,
    level: u8,
    candidate: &str,
) -> Value {
    let mut next = None;
    for lvl in level.clamp(1, 5)..=5 {
        let mut route = router.route(cfg, chain, project, task, Some(lvl));
        let alternatives = route.arr("alternatives");
        let start = if lvl == level {
            alternatives
                .iter()
                .position(|alt| alt.s("candidate") == candidate)
                .map_or(alternatives.len(), |index| index + 1)
        } else {
            0
        };
        if let Some(pick) = alternatives
            .iter()
            .skip(start)
            .find(|alt| alt.b("available", false) && alt.s("candidate") != candidate)
        {
            route["pick"] = json!({"candidate": pick.s("candidate"), "brain": pick.s("brain"),
                "model": pick.s("model"), "effort": pick.s("effort")});
            next = Some((lvl, route));
            break;
        }
    }
    let Some((lvl, mut v)) = next else {
        return Value::Null;
    };
    let cand = v["pick"].s("candidate");
    let id = new_id(now(), task);
    v["id"] = json!(id);
    v["level"] = json!(lvl);
    v["escalated"] = json!(true);
    v["why"] = json!(format!(
        "la verificación falló con {candidate}; sube a {cand}"
    ));
    let mut items = load_pending();
    items.push(Pending {
        id,
        ts: now(),
        project: project.to_string(),
        level: lvl,
        candidate: cand,
        task: task.to_string(),
        words: words_of(task),
        source: "verificacion".into(),
    });
    if items.len() > MAX_PENDING {
        items.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
        let cut = items.len() - MAX_PENDING;
        items.drain(0..cut);
    }
    save_pending(&items);
    v
}

// ---------- outcome signals ----------

/// Drop pending routing attempts for a task that crashed, failed, or was stopped,
/// so `sweep()` does not falsely reward it as an implicit success 30 minutes later.
pub fn discard_pending(project: &str, route_id: Option<&str>) {
    let _flock = crate::util::FileLock::acquire(&pending_path(), 1500);
    let mut items = load_pending();
    if items.is_empty() {
        return;
    }
    let before = items.len();
    if let Some(rid) = route_id.filter(|s| !s.is_empty()) {
        items.retain(|p| p.id != rid);
    } else if !project.is_empty() {
        if let Some(ix) = items
            .iter()
            .enumerate()
            .filter(|(_, p)| p.project == project)
            .max_by(|(_, a), (_, b)| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
        {
            items.remove(ix);
        }
    }
    if items.len() != before {
        save_pending(&items);
    }
}

/// Inspect a shell command + output for verification evidence about a recent pending attempt.
pub fn signal_from_bash(router: &Router, project: &str, command: &str, output: &str) {
    let cmd = fold(command);
    if !VERIFY_CMDS.iter().any(|k| cmd.contains(k)) {
        return;
    }
    let tnow = now();
    let items = load_pending();
    let recent = items
        .iter()
        .any(|p| p.project == project && tnow - p.ts < RETRY_WINDOW);
    if !recent {
        return;
    }
    let out = fold(output);
    static NONZERO_FAIL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let nonzero_fail = NONZERO_FAIL_RE
        .get_or_init(|| regex::Regex::new(r"\b[1-9]\d*\s+failed\b").expect("valid regex"))
        .is_match(&out);
    let go_fail =
        out.contains("--- fail:") || out.contains("\nfail\t") || out.starts_with("fail\t");
    let bad = nonzero_fail || go_fail || FAIL_WORDS.iter().any(|k| out.contains(k));
    if bad {
        outcome_w(
            router,
            Some(project),
            None,
            false,
            W_VERIFIED,
            "verification failed",
        );
        return;
    }
    let good = OK_WORDS.iter().any(|k| out.contains(k));
    if good {
        outcome_w(
            router,
            Some(project),
            None,
            true,
            W_VERIFIED,
            "verification ok",
        );
    }
}

/// A user message that complains about the previous answer means that attempt failed.
pub fn signal_from_prompt(router: &Router, project: &str, prompt: &str) {
    let text = fold(prompt);
    if !CORRECTION_WORDS.iter().any(|k| text.contains(k)) {
        return;
    }
    let tnow = now();
    let items = load_pending();
    let recent = items
        .iter()
        .any(|p| p.project == project && tnow - p.ts < PROMPT_WINDOW);
    if !recent {
        return;
    }
    outcome(router, Some(project), None, false, "el usuario lo corrigió");
}

// ---------- shadow mode ----------

fn round1(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    (x * 10.0).round() / 10.0
}

fn pct(a: f64, n: f64) -> f64 {
    if n <= 0.0 {
        0.0
    } else {
        round1(100.0 * a / n)
    }
}

/// Relative cost per 1M tokens: ds-flash 1, ds-pro 4, haiku 8, sonnet 30, opus 150.
fn rel_cost(model: &str) -> f64 {
    let m = model.to_lowercase();
    if m.contains("flash") {
        1.0
    } else if m.contains("haiku") {
        8.0
    } else if m.contains("pro") {
        4.0
    } else if m.contains("sonnet") {
        30.0
    } else if m.contains("opus") {
        150.0
    } else if m.contains("deepseek") {
        4.0
    } else {
        30.0
    }
}

/// Same model family between the router's pick and what actually ran?
fn family_agree(pick_model: &str, actual: &str) -> bool {
    let p = pick_model.to_lowercase();
    let a = actual.to_lowercase();
    for f in ["opus", "sonnet", "haiku", "deepseek"] {
        if p.contains(f) {
            return a.contains(f);
        }
    }
    !p.is_empty() && a.contains(&p)
}

fn shadow_enabled() -> bool {
    // Best-effort read of the portable config; shadow mode is on unless explicitly disabled.
    match read_json(&data_dir().join("metabrain.json")) {
        Some(cfg) => match cfg.get("router").and_then(|r| r.get("shadow")) {
            Some(v) => truthy(Some(v)),
            None => true,
        },
        None => true,
    }
}

/// Log what the router would have chosen versus what actually ran (no pending is created).
pub fn shadow(
    router: &Router,
    cfg: &Value,
    chain: &[Value],
    project: &str,
    task: &str,
    actual_model: &str,
) {
    let (level, _why, source) = classify_smart(cfg, task, chain);
    let v = router.route(cfg, chain, project, task, Some(level));
    let pick = v.get("pick").cloned().unwrap_or(Value::Null);
    let cand = pick.s("candidate");
    let pmodel = pick.s("model");
    let agree = family_agree(&pmodel, actual_model);
    shadow_line(json!({
        "ts": now(),
        "kind": "shadow",
        "project": project,
        "task": task.chars().take(160).collect::<String>(),
        "level": level,
        "source": source,
        "pick": cand,
        "pick_model": pmodel,
        "actual": actual_model,
        "agree": agree,
        "cost_pick": rel_cost(&pmodel),
        "cost_actual": rel_cost(actual_model)
    }));
}

/// Aggregate the last `limit` shadow events (optionally for one project).
pub fn shadow_stats(project: Option<&str>, limit: usize) -> Value {
    let raw = fs::read_to_string(shadow_path()).unwrap_or_default();
    let mut entries: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    if let Some(p) = project {
        entries.retain(|e| e.s("project") == p);
    }
    if limit > 0 && entries.len() > limit {
        let cut = entries.len() - limit;
        entries.drain(0..cut);
    }

    let mut n_shadow: i64 = 0;
    let mut agree: i64 = 0;
    let mut sum_pick = 0.0;
    let mut sum_actual = 0.0;
    let mut ok: i64 = 0;
    let mut fail: i64 = 0;
    for e in &entries {
        match e.s("kind").as_str() {
            "shadow" => {
                n_shadow += 1;
                if e.get("agree").and_then(|v| v.as_bool()).unwrap_or(false) {
                    agree += 1;
                }
                sum_pick += e.get("cost_pick").and_then(|v| v.as_f64()).unwrap_or(30.0);
                sum_actual += e
                    .get("cost_actual")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(30.0);
            }
            "outcome" => {
                if e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    ok += 1;
                } else {
                    fail += 1;
                }
            }
            _ => {}
        }
    }

    let agree_pct = pct(agree as f64, n_shadow as f64);
    let saving_pct = if sum_actual > 0.0 {
        round1(100.0 * (1.0 - sum_pick / sum_actual))
    } else {
        0.0
    };

    let mut by_level: Vec<Value> = Vec::new();
    for lvl in 1..=5u8 {
        let mut ev: i64 = 0;
        let mut ag: i64 = 0;
        for e in &entries {
            if e.s("kind") != "shadow" {
                continue;
            }
            if e.get("level").and_then(|v| v.as_u64()) != Some(lvl as u64) {
                continue;
            }
            ev += 1;
            if e.get("agree").and_then(|v| v.as_bool()).unwrap_or(false) {
                ag += 1;
            }
        }
        by_level.push(json!({"level": lvl, "events": ev, "agree_pct": pct(ag as f64, ev as f64)}));
    }

    let recent: Vec<Value> = entries.iter().rev().take(30).cloned().collect();

    json!({
        "enabled": shadow_enabled(),
        "events": n_shadow,
        "agree_pct": agree_pct,
        "saving_pct": saving_pct,
        "outcomes": {"ok": ok, "fail": fail},
        "by_level": by_level,
        "recent": recent
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::MutexGuard;

    fn lock() -> MutexGuard<'static, ()> {
        crate::util::test_env_lock()
    }

    fn test_home(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "metabrain-learn-{}-{}-{}",
            name,
            std::process::id(),
            nanos
        ));
        let _ = fs::create_dir_all(&d);
        std::env::set_var("METABRAIN_HOME", &d);
        d
    }

    fn test_cfg() -> Value {
        json!({
            "router": {"target": 0.6, "llm_classify": false},
            "brains": [
                {"id": "claude", "kind": "claude_code",
                 "ladder": ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]},
                {"id": "deepseek", "kind": "openai", "name": "DeepSeek",
                 "base_url": "https://api.deepseek.com/v1", "key_env": "METABRAIN_TEST_NO_KEY",
                 "ladder": ["deepseek-v4-pro", "deepseek-flash"]}
            ]
        })
    }

    fn test_chain() -> Vec<Value> {
        vec![
            json!({"id": "claude", "kind": "claude_code", "available": true}),
            json!({"id": "deepseek", "kind": "openai", "name": "DeepSeek", "available": true}),
        ]
    }

    #[test]
    fn retry_similar_escalates_and_records_failure() {
        let _g = lock();
        let d = test_home("retry");
        let r = Router::new(Some(d.join("router.json")));
        let cfg = test_cfg();
        let chain = test_chain();
        let task = "implementar la funcion de login en la api";

        let (lvl, _why, src) = classify_smart(
            &json!({"brains": []}),
            "renombrar archivos y leer logs",
            &[],
        );
        assert_eq!(src, "palabras");
        assert_eq!(lvl, 1);

        let v1 = route_auto(&r, &cfg, &chain, "demo", task);
        assert_eq!(v1["source"], "palabras");
        assert_eq!(v1["level"], 3);
        assert_eq!(v1["pick"]["candidate"], "ds:deepseek-v4-pro:high");
        let id1 = v1["id"].as_str().unwrap_or("").to_string();
        assert_eq!(id1.len(), 12);
        assert_eq!(load_pending().len(), 1);

        let v2 = route_auto(&r, &cfg, &chain, "demo", task);
        assert_eq!(v2["escalated"], true);
        assert_eq!(v2["retry_of"], id1.as_str());
        assert_eq!(v2["pick"]["candidate"], "claude:opus:medium");
        assert_eq!(load_pending().len(), 1);

        let rj = read_json(&d.join("router.json")).unwrap_or(Value::Null);
        assert!(
            (rj["projects"]["demo"]["3"]["ds:deepseek-v4-pro:high"]["n"]
                .as_f64()
                .unwrap_or(-1.0)
                - W_RETRY)
                .abs()
                < 1e-9,
            "n"
        );
        assert!(
            (rj["projects"]["demo"]["3"]["ds:deepseek-v4-pro:high"]["ok"]
                .as_f64()
                .unwrap_or(-1.0)
                - 0.0)
                .abs()
                < 1e-9,
            "ok"
        );
    }

    #[test]
    fn different_task_is_not_a_retry() {
        let _g = lock();
        let d = test_home("not-retry");
        let r = Router::new(Some(d.join("router.json")));
        let cfg = test_cfg();
        let chain = test_chain();
        let v1 = route_auto(&r, &cfg, &chain, "demo", "renombrar archivos y leer logs");
        assert_eq!(v1["level"], 1);
        let v2 = route_auto(
            &r,
            &cfg,
            &chain,
            "demo",
            "implementar la funcion de login en la api",
        );
        assert_eq!(v2["level"], 3);
        assert!(v2.get("escalated").is_none());
        assert!(v2.get("retry_of").is_none());
        assert_eq!(load_pending().len(), 2);
    }

    #[test]
    fn sweep_marks_old_pendings_as_success() {
        let _g = lock();
        let d = test_home("sweep");
        let r = Router::new(Some(d.join("router.json")));
        let v = route_auto(
            &r,
            &test_cfg(),
            &test_chain(),
            "demo",
            "renombrar archivos y leer logs",
        );
        let cand = v["pick"]["candidate"].as_str().unwrap_or("").to_string();
        assert!(!cand.is_empty());

        let mut items = load_pending();
        assert_eq!(items.len(), 1);
        items[0].ts = now() - 7_200.0;
        save_pending(&items);

        sweep(&r);
        assert!(load_pending().is_empty());
        let rj = read_json(&d.join("router.json")).unwrap_or(Value::Null);
        assert!(
            (rj["projects"]["demo"]["1"][cand.as_str()]["n"]
                .as_f64()
                .unwrap_or(-1.0)
                - W_IMPLICIT)
                .abs()
                < 1e-9,
            "n"
        );
        assert!(
            (rj["projects"]["demo"]["1"][cand.as_str()]["ok"]
                .as_f64()
                .unwrap_or(-1.0)
                - W_IMPLICIT)
                .abs()
                < 1e-9,
            "ok"
        );
    }

    #[test]
    fn bash_failed_verification_records_failure() {
        let _g = lock();
        let d = test_home("bash");
        let r = Router::new(Some(d.join("router.json")));
        let v = route_auto(
            &r,
            &test_cfg(),
            &test_chain(),
            "demo",
            "renombrar archivos y leer logs",
        );
        let cand = v["pick"]["candidate"].as_str().unwrap_or("").to_string();
        assert!(!cand.is_empty());

        signal_from_bash(
            &r,
            "demo",
            "cargo test --all",
            "running 3 tests\ntest result: FAILED. 2 passed; 1 failed",
        );
        assert!(load_pending().is_empty());
        let rj = read_json(&d.join("router.json")).unwrap_or(Value::Null);
        assert!(
            (rj["projects"]["demo"]["1"][cand.as_str()]["n"]
                .as_f64()
                .unwrap_or(-1.0)
                - W_VERIFIED)
                .abs()
                < 1e-9,
            "n"
        );
        assert!(
            (rj["projects"]["demo"]["1"][cand.as_str()]["ok"]
                .as_f64()
                .unwrap_or(-1.0)
                - 0.0)
                .abs()
                < 1e-9,
            "ok"
        );
    }

    #[test]
    fn prompt_correction_records_failure() {
        let _g = lock();
        let d = test_home("prompt");
        let r = Router::new(Some(d.join("router.json")));
        let v = route_auto(
            &r,
            &test_cfg(),
            &test_chain(),
            "demo",
            "renombrar archivos y leer logs",
        );
        let cand = v["pick"]["candidate"].as_str().unwrap_or("").to_string();
        assert!(!cand.is_empty());

        signal_from_prompt(&r, "demo", "Esto está mal, rehazlo por favor");
        assert!(load_pending().is_empty());
        let rj = read_json(&d.join("router.json")).unwrap_or(Value::Null);
        assert!(
            (rj["projects"]["demo"]["1"][cand.as_str()]["n"]
                .as_f64()
                .unwrap_or(-1.0)
                - W_USER)
                .abs()
                < 1e-9,
            "n"
        );
        assert!(
            (rj["projects"]["demo"]["1"][cand.as_str()]["ok"]
                .as_f64()
                .unwrap_or(-1.0)
                - 0.0)
                .abs()
                < 1e-9,
            "ok"
        );
    }

    #[test]
    fn shadow_stats_agree_and_saving() {
        let _g = lock();
        let _d = test_home("shadow");
        let r = Router::new(Some(_d.join("router.json")));
        let cfg = test_cfg();
        let chain = test_chain();
        let task = "implementar la funcion de login en la api";

        shadow(&r, &cfg, &chain, "demo", task, "claude-opus-5-5");
        shadow(&r, &cfg, &chain, "demo", task, "deepseek-v4-pro");

        let st = shadow_stats(Some("demo"), 100);
        assert_eq!(st["events"], 2);
        assert_eq!(st["agree_pct"].as_f64().unwrap_or(-1.0), 50.0);
        let saving = st["saving_pct"].as_f64().unwrap_or(0.0);
        assert!((saving - 94.8).abs() < 0.2, "saving={}", saving);
        assert_eq!(st["by_level"][2]["level"], 3);
        assert_eq!(st["by_level"][2]["events"], 2);
        assert_eq!(st["recent"].as_array().map(|a| a.len()).unwrap_or(0), 2);
        assert_eq!(st["outcomes"]["ok"], 0);
        assert_eq!(st["outcomes"]["fail"], 0);
    }

    #[test]
    fn similar_is_jaccard() {
        let a = words_of("implementar login en la api");
        let b = words_of("implementar login en la api nueva");
        assert!((similar(&a, &b) - 0.75).abs() < 1e-9);
        assert!((similar(&a, &a) - 1.0).abs() < 1e-9);
        let c = words_of("renombrar archivos");
        assert!(similar(&a, &c).abs() < 1e-9);
        assert!(similar(&[], &[]).abs() < 1e-9);
    }
}

#[cfg(test)]
mod escalate_tests {
    use super::*;

    #[test]
    fn retry_at_top_of_level_moves_to_next_level() {
        let _g = crate::util::test_env_lock();
        let d = std::env::temp_dir().join(format!("mb_learn_esc_{}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        std::env::set_var("METABRAIN_HOME", &d);
        let _ = fs::remove_file(pending_path());
        let router = Router::new(Some(d.join("router.json")));
        let cfg = json!({"router": {"llm_classify": false}});
        let chain = vec![
            json!({"id": "A", "kind": "claude_code", "available": true, "ladder": ["claude-haiku-4-5", "claude-sonnet-5-5", "claude-opus-5-5"]}),
        ];
        let first = route_auto(
            &router,
            &cfg,
            &chain,
            "p",
            "arregla el bug raro de concurrencia en el planificador",
        );
        let second = route_auto(
            &router,
            &cfg,
            &chain,
            "p",
            "arregla el bug raro de concurrencia en el planificador otra vez",
        );
        assert_eq!(
            second.get("escalated").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_ne!(first["pick"]["candidate"], second["pick"]["candidate"]);
    }
}

#[cfg(test)]
mod verified_routing_tests {
    use super::*;

    fn home(tag: &str) -> (std::sync::MutexGuard<'static, ()>, std::path::PathBuf) {
        let g = crate::util::test_env_lock();
        let d = std::env::temp_dir().join(format!("mb_vr_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::create_dir_all(&d);
        std::env::set_var("METABRAIN_HOME", &d);
        (g, d)
    }

    fn chain() -> Vec<Value> {
        vec![
            json!({"id": "A", "kind": "claude_code", "available": true, "ladder": ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]}),
            json!({"id": "DS", "kind": "openai", "available": true, "ladder": ["deepseek-v4-pro", "deepseek-flash"]}),
        ]
    }

    #[test]
    fn verified_projects_start_one_level_cheaper() {
        let (_g, d) = home("start");
        let router = Router::new(Some(d.join("router.json")));
        let cfg = json!({"router": {"llm_classify": false}});
        let task = "implementa la vista de historial con filtros y paginacion en el panel";
        let plain = route_auto_in(&router, &cfg, &chain(), "p1", task, false);
        let verified = route_auto_in(&router, &cfg, &chain(), "p2", task, true);
        let lp = plain["level"].as_u64().unwrap();
        let lv = verified["level"].as_u64().unwrap();
        assert_eq!(lv + 1, lp.max(2), "verified start is one level lower");
        assert_eq!(verified["verified_start"], json!(true));
    }

    #[test]
    fn escalate_after_moves_up_and_never_repeats_the_candidate() {
        let (_g, d) = home("esc");
        let router = Router::new(Some(d.join("router.json")));
        let cfg = json!({"router": {"llm_classify": false}});
        let first = router.route(&cfg, &chain(), "p", "arregla el test", Some(2));
        let cand = first["pick"].s("candidate");
        let v = escalate_after(&router, &cfg, &chain(), "p", "arregla el test", 2, &cand);
        assert!(!v.is_null());
        assert_ne!(v["pick"].s("candidate"), cand);
        assert_eq!(v["escalated"], json!(true));
        // the new attempt is pending, so a verified result can be attributed to it
        let r = outcome_w(&router, Some("p"), Some(&v.s("id")), true, W_VERIFIED, "ok");
        assert_eq!(r["resolved"], json!(true));
    }

    #[test]
    fn weak_evidence_counts_less_than_verified() {
        let (_g, d) = home("w");
        let router = Router::new(Some(d.join("router.json")));
        router.record_weighted("p", 2, "ds:deepseek-flash:high", true, W_IMPLICIT, 0);
        router.record_weighted("p", 2, "ds:deepseek-flash:high", false, W_VERIFIED, 0);
        let rj = read_json(&d.join("router.json")).unwrap();
        let e = &rj["projects"]["p"]["2"]["ds:deepseek-flash:high"];
        assert!((e["n"].as_f64().unwrap() - (W_IMPLICIT + W_VERIFIED)).abs() < 1e-9);
        assert!((e["ok"].as_f64().unwrap() - W_IMPLICIT).abs() < 1e-9);
    }
}
