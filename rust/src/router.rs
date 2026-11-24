//! Task router: pick the cheapest brain + model + effort that still gets the job done, and learn
//! from the recorded outcomes (port of router.py). No unsafe; never panics on disk/config data.
use crate::util::{data_dir, log, read_json, write_json, J};
use rand::Rng;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;

/// Human names for levels 1..5.
pub const LEVEL_NAMES: [&str; 5] = ["mecánico", "rutina", "desarrollo", "difícil", "crítico"];
/// Relative cost of the always-opus baseline (per 1M tokens).
pub const OPUS_COST: f64 = 150.0;
/// Weight of a new sample in the token EWMA.
const TOKEN_ALPHA: f64 = 0.3;
/// Beta prior: mean = (wins + 2) / (n + 3).
const PRIOR_WINS: f64 = 2.0;
const PRIOR_N: f64 = 3.0;
/// Exploration rate for cheaper candidates that still need samples.
const EXPLORE_P: f64 = 0.1;
const EXPLORE_MIN_N: f64 = 5.0;

// ---------- keyword tables (classify() folds accents first, so they are plain ASCII) ----------
const KW1: &[&str] = &[
    "renombrar",
    "renombra",
    "rename",
    "formatear",
    "formatea",
    "format",
    "mover",
    "move",
    "copiar",
    "copy",
    "leer log",
    "leer logs",
    "read log",
    "read logs",
    "boilerplate",
    "lee",
    "leer",
    "log",
    "logs",
    "convertir",
    "convert",
    "listar",
    "ordenar archivos",
    "borrar archivo",
    "delete file",
    "grep",
    "sed",
    "awk",
    "zip",
    "unzip",
    "csv a json",
    "json a csv",
];
const KW2: &[&str] = &[
    "script",
    "scripts",
    "bash",
    "shell",
    "powershell",
    "test",
    "tests",
    "testear",
    "unit",
    "ffmpeg",
    "blender",
    "plantilla",
    "template",
    "typo",
    "lint",
    "instalar",
    "install",
    "backup",
    "copia de seguridad",
    "cron",
    "tarea programada",
    "ajuste",
    "configurar",
    "config",
    "css",
    "html",
    "markdown",
    "readme",
];
const KW3: &[&str] = &[
    "implementar",
    "implementa",
    "implement",
    "funcion",
    "function",
    "metodo",
    "method",
    "endpoint",
    "api",
    "integrar",
    "integracion",
    "integrate",
    "componente",
    "component",
    "refactor",
    "refactorizar",
    "refactoriza",
    "anadir",
    "anade",
    "agregar",
    "agrega",
    "add",
    "crea",
    "crear",
    "modulo",
    "module",
    "clase",
    "class",
    "consulta",
    "query",
    "parsear",
    "parser",
    "formulario",
    "form",
    "crud",
    "login",
    "cliente",
    "client",
    "servicio",
    "bug",
    "crash",
    "fallo",
    "falla",
];
const KW4: &[&str] = &[
    "arquitectura",
    "architecture",
    "disenar",
    "diseno",
    "design",
    "concurrencia",
    "concurrency",
    "deadlock",
    "race condition",
    "seguridad",
    "security",
    "vulnerabilidad",
    "vulnerability",
    "rendimiento",
    "performance",
    "optimizar",
    "optimizacion",
    "optimize",
    "fuga de memoria",
    "memory leak",
    "depurar",
    "depura",
    "debug",
    "debugging",
    "flaky",
    "intermitente",
    "al azar",
    "aleatorio",
    "aleatoriamente",
    "a veces falla",
    "no se por que",
    "por que falla",
    "raro",
    "extrano",
    "cifrado",
    "crypto",
    "criptografia",
    "auditoria",
    "audit",
    "escalabilidad",
    "scalability",
    "hilos",
    "threads",
    "async",
    "unsafe",
    "compilador",
    "compiler",
    "migracion",
    "migration",
    "code review",
    "revision de codigo",
    "refactor grande",
];
const KW5: &[&str] = &[
    "publicar",
    "publica",
    "publicalo",
    "publish",
    "desplegar",
    "despliega",
    "deploy",
    "release",
    "lanzar",
    "lanza",
    "lanzamiento",
    "aprueba",
    "apruebalo",
    "decide",
    "produccion",
    "production",
    "aprobar",
    "approve",
    "aprobacion",
    "irreversible",
    "firmar",
    "sign off",
    "revision final",
    "final review",
    "decision de producto",
    "product decision",
    "decidir",
    "decision",
    "merge a main",
    "merge to main",
    "go live",
    "drop table",
    "borrar base de datos",
    "delete database",
    "dinero real",
    "pago real",
    "facturacion",
];

/// Default ladder of candidates per level (cheapest first).
pub fn default_levels() -> Value {
    json!({
        "1": ["ds:deepseek-flash:none", "claude:haiku:low"],
        "2": ["ds:deepseek-flash:high", "claude:haiku:medium", "claude:sonnet:low"],
        "3": ["claude:sonnet:medium", "ds:deepseek-v4-pro:high", "claude:opus:medium"],
        "4": ["claude:opus:medium", "claude:opus:high"],
        "5": ["claude:opus:high"]
    })
}

/// The task runner can launch these harnesses directly. Keep its defaults independent of
/// the Claude hooks, which use the legacy ladder for delegation.
pub(crate) fn task_routing_config(mut cfg: Value) -> Value {
    let levels = json!({
        "1": ["ds:deepseek-flash:none", "codex:auto:low", "gemini_cli:flash:low", "antigravity:flash:low", "claude:haiku:low"],
        "2": ["codex:auto:low", "gemini_cli:flash:low", "antigravity:flash:low", "ds:deepseek-flash:high", "claude:haiku:medium"],
        "3": ["codex:auto:medium", "claude:sonnet:medium", "antigravity:medium:medium", "gemini_cli:pro:medium", "ds:deepseek-v4-pro:high"],
        "4": ["codex:auto:high", "claude:opus:medium", "antigravity:high:high", "gemini_cli:pro:high", "ds:deepseek-v4-pro:high"],
        "5": ["claude:opus:high", "codex:auto:high", "antigravity:high:high", "gemini_cli:pro:high", "ds:deepseek-v4-pro:high"]
    });
    if !cfg["router"].is_object() {
        cfg["router"] = json!({});
    }
    if !cfg["router"]["levels"].is_object() {
        cfg["router"]["levels"] = json!({});
    }
    cfg["router"]["task_harnesses"] = json!(true);
    for (level, defaults) in levels.as_object().unwrap() {
        if !cfg["router"]["levels"][level].as_array().is_some_and(|v| {
            v.iter()
                .any(|entry| entry.as_str().is_some_and(|s| !s.trim().is_empty()))
        }) {
            cfg["router"]["levels"][level] = defaults.clone();
        }
    }
    cfg
}

pub struct Router {
    path: PathBuf,
    data: Mutex<Value>,
}

impl Router {
    pub fn new(path: Option<PathBuf>) -> Router {
        let path = path.unwrap_or_else(|| data_dir().join("router.json"));
        let mut data = read_json(&path)
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({"version": 1, "projects": {}}));
        let has_projects = data.get("projects").map(|v| v.is_object()).unwrap_or(false);
        if !has_projects {
            if let Some(m) = data.as_object_mut() {
                m.insert("projects".to_string(), json!({}));
            }
        }
        Router {
            path,
            data: Mutex::new(data),
        }
    }

    /// Re-read router.json: hooks run in their own processes and also record outcomes.
    pub fn refresh(&self) {
        if let Some(v) = read_json(&self.path)
            .filter(|v| v.get("projects").map(|p| p.is_object()).unwrap_or(false))
        {
            match self.data.lock() {
                Ok(mut g) => *g = v,
                Err(p) => *p.into_inner() = v,
            }
        }
    }

    /// Heuristic classification (same as the free `classify`), handy as `Router::classify`.
    pub fn classify(task: &str) -> (u8, String) {
        self::classify(task)
    }

    /// Decide which candidate to use for this task, respecting the availability of the chain.
    pub fn route(
        &self,
        cfg: &Value,
        chain: &[Value],
        project: &str,
        task: &str,
        level_hint: Option<u8>,
    ) -> Value {
        let (auto_level, auto_why) = classify(task);
        let level = match level_hint {
            Some(l) => l.clamp(1, 5),
            None => auto_level,
        };
        let why = match level_hint {
            Some(_) => format!("nivel indicado por el usuario; heuristica: {}", auto_why),
            None => auto_why,
        };
        self.refresh();
        let target = target_of(cfg);
        let data = match self.data.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };

        let mut rows: Vec<Row> = vec![];
        for c in candidates_for(cfg, project, level) {
            let (n, ok, toks) = stat_of(&data, project, level, &c);
            let mut pick = resolve(&c, chain, cfg);
            if let Some(p) = &mut pick {
                if let Some(brain) = cfg
                    .arr("brains")
                    .into_iter()
                    .find(|b| b["id"] == p["brain"] && b.s("kind") == "codex")
                {
                    let (model, effort) = crate::cli_agents::codex_turn_settings(
                        &brain,
                        task,
                        &p.s("model"),
                        &p.s("effort"),
                    );
                    p["model"] = json!(model);
                    p["effort"] = json!(effort);
                }
            }
            let cost = pick
                .as_ref()
                .map(|p| {
                    let model = p.s("model");
                    let brain = cfg
                        .arr("brains")
                        .into_iter()
                        .find(|b| b["id"] == p["brain"])
                        .unwrap_or(Value::Null);
                    // Normalize USD-per-1M-input prices onto the same relative scale as OPUS_COST (150.0 ≈ $15/1M)
                    // by multiplying USD by 10.0, so configured prices and candidate_cost() share one unit.
                    brain["prices"][&model]["input"]
                        .as_f64()
                        .filter(|x| *x >= 0.0)
                        .or_else(|| {
                            brain["pricing"][&model]["input"]
                                .as_f64()
                                .filter(|x| *x >= 0.0)
                        })
                        .map(|usd| usd * 10.0)
                        .unwrap_or_else(|| candidate_cost(&c))
                })
                .unwrap_or_else(|| candidate_cost(&c));
            let p = (ok + PRIOR_WINS) / (n + PRIOR_N);
            rows.push(Row {
                cand: c,
                cost,
                p,
                n,
                tokens: toks,
                pick,
            });
        }

        let avail: Vec<usize> = (0..rows.len())
            .filter(|i| rows[*i].pick.is_some())
            .collect();
        let mut chosen = choose(&rows, &avail, target);

        // Exploration: sometimes sample the closest cheaper candidate that still has little data.
        let explore = cfg["router"].b("explore", !cfg["router"].b("task_harnesses", false));
        if let Some(cur) = chosen.filter(|_| explore && level <= 3) {
            if avail.len() > 1 && rand::thread_rng().gen_bool(EXPLORE_P) {
                let cur_cost = rows[cur].cost;
                let mut cheaper: Vec<usize> = avail
                    .iter()
                    .copied()
                    .filter(|i| rows[*i].cost < cur_cost && rows[*i].n < EXPLORE_MIN_N)
                    .collect();
                cheaper.sort_by(|a, b| {
                    rows[*b]
                        .cost
                        .partial_cmp(&rows[*a].cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                if let Some(&i) = cheaper.first() {
                    chosen = Some(i);
                }
            }
        }

        let mut alts: Vec<Value> = vec![];
        for r in &rows {
            let pick = match &r.pick {
                Some(p) => p.clone(),
                None => {
                    json!({"candidate": r.cand.as_str(), "brain": "", "model": "", "effort": ""})
                }
            };
            let brain = pick.s("brain");
            let model = pick.s("model");
            let effort = pick.s("effort");
            let available = r.pick.is_some();
            let succ = round3(r.p);
            alts.push(json!({
                "candidate": r.cand.as_str(),
                "brain": brain,
                "model": model,
                "effort": effort,
                "cost": r.cost,
                "available": available,
                "n": round1(r.n),
                "success_rate": succ
            }));
        }

        let (pick_out, exp_succ, exp_tokens, saving) = match chosen {
            Some(i) => {
                let r = &rows[i];
                let pick = r.pick.clone().unwrap_or(Value::Null);
                let toks = if r.n > 0.0 && r.tokens > 0.0 {
                    r.tokens.round() as i64
                } else {
                    (2000.0 + r.cost * 200.0).round() as i64
                };
                (
                    pick,
                    round3(r.p),
                    toks,
                    round1((100.0 * (1.0 - r.cost / OPUS_COST)).clamp(0.0, 100.0)),
                )
            }
            None => (
                json!({"candidate": "", "brain": "", "model": "", "effort": ""}),
                0.0,
                0i64,
                0.0,
            ),
        };

        json!({
            "project": project,
            "level": level,
            "level_name": LEVEL_NAMES[(level - 1) as usize],
            "why": why,
            "pick": pick_out,
            "alternatives": alts,
            "expected_success": exp_succ,
            "expected_tokens": exp_tokens,
            "saving_vs_opus_pct": saving
        })
    }

    /// Record one attempt. Returns the candidate to escalate to when `ok` is false.
    pub fn record(
        &self,
        project: &str,
        level: u8,
        candidate: &str,
        ok: bool,
        tokens: i64,
    ) -> Value {
        self.record_weighted(project, level, candidate, ok, 1.0, tokens)
    }

    /// Record one attempt with the strength of its evidence (0..=1]: verified results count fully,
    /// heuristic ones (a retry, silence for 30 min) only partly.
    pub fn record_weighted(
        &self,
        project: &str,
        level: u8,
        candidate: &str,
        ok: bool,
        weight: f64,
        tokens: i64,
    ) -> Value {
        let w = if weight.is_finite() {
            weight.clamp(0.05, 1.0)
        } else {
            1.0
        };
        let level = level.clamp(1, 5);
        let lv = level.to_string();
        let cand = candidate.trim().to_string();
        let updated = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let _flock = crate::util::FileLock::acquire(&self.path, 2000);
        self.refresh();
        let snapshot = {
            let mut d = match self.data.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            with_path_mut(
                &mut d,
                &["projects", project, lv.as_str(), cand.as_str()],
                |e| {
                    let n = e.get("n").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let wins = e.get("ok").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let prev = e.get("tokens").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let t = tokens.max(0) as f64;
                    let avg = if n <= 0.0 || prev <= 0.0 || t <= 0.0 {
                        if t > 0.0 {
                            t
                        } else {
                            prev
                        }
                    } else {
                        TOKEN_ALPHA * t + (1.0 - TOKEN_ALPHA) * prev
                    };
                    *e = json!({
                        "n": round3(n + w),
                        "ok": round3(wins + if ok { w } else { 0.0 }),
                        "tokens": round1(avg),
                        "updated": updated.as_str()
                    });
                },
            );
            d.clone()
        };
        if let Err(e) = write_json(&self.path, &snapshot) {
            log(&format!(
                "router: no se pudo guardar {}: {}",
                self.path.display(),
                e
            ));
        }
        let next = if ok {
            Value::Null
        } else {
            json!(escalate(level, &cand))
        };
        json!({"candidate": cand.as_str(), "level": level, "ok": ok, "next": next})
    }

    /// HMI view: per project and level, every candidate with its statistics and the current pick.
    pub fn table(&self, cfg: &Value, project: Option<&str>) -> Value {
        self.refresh();
        let target = target_of(cfg);
        let data = match self.data.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        };
        let mut names: Vec<String> = vec![];
        if let Some(ps) = cfg
            .get("router")
            .and_then(|r| r.get("projects"))
            .and_then(|v| v.as_object())
        {
            for k in ps.keys() {
                names.push(k.clone());
            }
        }
        if let Some(ps) = data.get("projects").and_then(|v| v.as_object()) {
            for k in ps.keys() {
                if !names.iter().any(|n| n == k) {
                    names.push(k.clone());
                }
            }
        }
        if let Some(p) = project {
            if !names.iter().any(|n| n == p) {
                names.push(p.to_string());
            }
            names.retain(|n| n == p);
        }
        names.sort();

        let mut projects: Vec<Value> = vec![];
        let mut sum_w = 0.0;
        let mut sum_v = 0.0;
        for pname in &names {
            let mut levels: Vec<Value> = vec![];
            for level in 1..=5u8 {
                let mut rows: Vec<Row> = vec![];
                for c in candidates_for(cfg, pname, level) {
                    let (n, ok, toks) = stat_of(&data, pname, level, &c);
                    let cost = candidate_cost(&c);
                    let p = (ok + PRIOR_WINS) / (n + PRIOR_N);
                    rows.push(Row {
                        cand: c,
                        cost,
                        p,
                        n,
                        tokens: toks,
                        pick: None,
                    });
                }
                let all: Vec<usize> = (0..rows.len()).collect();
                let chosen = choose(&rows, &all, target);
                let mut items: Vec<Value> = vec![];
                let mut total_n: f64 = 0.0;
                for (i, r) in rows.iter().enumerate() {
                    total_n += r.n;
                    let avg = if r.n > 0.0 {
                        json!(round1(r.tokens))
                    } else {
                        Value::Null
                    };
                    let is_chosen = chosen == Some(i);
                    items.push(json!({
                        "candidate": r.cand.as_str(),
                        "n": round1(r.n),
                        "success_rate": round3(r.p),
                        "avg_tokens": avg,
                        "cost": r.cost,
                        "chosen": is_chosen
                    }));
                }
                let chosen_cand = match chosen {
                    Some(i) => rows[i].cand.clone(),
                    None => String::new(),
                };
                let chosen_cost = match chosen {
                    Some(i) => rows[i].cost,
                    None => OPUS_COST,
                };
                let w = total_n.max(1.0);
                sum_w += w;
                sum_v += w * (1.0 - chosen_cost / OPUS_COST);
                levels.push(json!({
                    "level": level,
                    "level_name": LEVEL_NAMES[(level - 1) as usize],
                    "chosen": chosen_cand,
                    "candidates": items
                }));
            }
            projects.push(json!({"project": pname.as_str(), "levels": levels}));
        }
        let saving = if sum_w > 0.0 {
            round1(100.0 * sum_v / sum_w)
        } else {
            0.0
        };
        json!({"projects": projects, "saving_pct": saving})
    }

    /// Replace the candidate ladder of one project/level (validated; ignored if malformed).
    pub fn set_levels(&self, cfg: &mut Value, project: &str, level: u8, candidates: Vec<String>) {
        if !(1..=5).contains(&level) {
            log(&format!("router: nivel invalido {}", level));
            return;
        }
        if candidates.is_empty() {
            log(&format!(
                "router: lista de candidatos vacia para {} nivel {}",
                project, level
            ));
            return;
        }
        if let Some(bad) = candidates.iter().find(|c| !valid_cand(c)) {
            log(&format!("router: candidato invalido '{}'", bad));
            return;
        }
        let clean: Vec<Value> = candidates.iter().map(|c| json!(c.trim())).collect();
        let lv = level.to_string();
        with_path_mut(
            cfg,
            &["router", "projects", project, "levels", lv.as_str()],
            |v| {
                *v = Value::Array(clean);
            },
        );
    }
}

// ---------- classification ----------

/// Strip accents and lowercase so the keyword tables can stay ASCII.
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
            c => c,
        })
        .collect()
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn first_hit(tokens: &[String], text: &str, kws: &[&str]) -> Option<String> {
    for k in kws {
        let found = if k.contains(' ') {
            text.contains(k)
        } else {
            tokens.iter().any(|t| t.as_str() == *k)
        };
        if found {
            return Some((*k).to_string());
        }
    }
    None
}

/// Level from the size of the task (used when no keyword matches, and as a mild booster).
fn base_level(words: usize) -> u8 {
    match words {
        0..=8 => 1,
        9..=25 => 2,
        26..=80 => 3,
        _ => 4,
    }
}

/// Heuristic (ES + EN): keywords plus task length -> (level 1..5, short reason).
pub fn classify(task: &str) -> (u8, String) {
    let text = fold(task);
    let tokens = tokenize(&text);
    let groups: [&[&str]; 5] = [KW1, KW2, KW3, KW4, KW5];
    let mut hit_level: u8 = 0;
    let mut reason = String::new();
    for (i, list) in groups.iter().enumerate() {
        if let Some(k) = first_hit(&tokens, &text, list) {
            let lv = (i as u8) + 1;
            if lv >= hit_level {
                hit_level = lv;
                reason = format!("palabra clave: {}", k);
            }
        }
    }
    let base = base_level(tokens.len());
    if hit_level == 0 {
        return (base, format!("longitud: {} palabras", tokens.len()));
    }
    // Length can raise the level by at most one step above the strongest keyword.
    let level = hit_level.max(base.min(hit_level + 1));
    (level, reason)
}

// ---------- candidates, costs and chain resolution ----------

fn split_cand(c: &str) -> (String, String, String) {
    let mut it = c.trim().split(':');
    let fam = it.next().unwrap_or("").trim().to_lowercase();
    let model = it.next().unwrap_or("").trim().to_string();
    let effort = it.next().unwrap_or("").trim().to_lowercase();
    (fam, model, effort)
}

fn valid_cand(c: &str) -> bool {
    let parts: Vec<&str> = c.trim().split(':').collect();
    if parts.len() != 3 {
        return false;
    }
    let fam = parts[0].trim().to_lowercase();
    if !matches!(
        fam.as_str(),
        "claude" | "ds" | "codex" | "gemini_cli" | "antigravity"
    ) {
        return false;
    }
    if parts[1].trim().is_empty() {
        return false;
    }
    let eff = parts[2].trim().to_lowercase();
    matches!(eff.as_str(), "none" | "low" | "medium" | "high" | "max")
}

/// Relative cost per 1M tokens (ds-flash 1, ds-pro 4, haiku 8, sonnet 30, opus 150).
fn candidate_cost(cand: &str) -> f64 {
    let (fam, model, _eff) = split_cand(cand);
    let m = model.to_lowercase();
    if m.contains("flash") {
        1.0
    } else if m.contains("haiku") {
        8.0
    } else if fam == "ds" || (fam == "openai" && m.contains("deepseek")) {
        4.0
    } else if m.contains("sonnet") {
        30.0
    } else if m.contains("opus") || m.contains("astra") {
        OPUS_COST
    } else {
        30.0
    }
}

/// Find an available brain for the candidate family and its config (for the model ladder).
fn brain_for(family: &str, model: &str, chain: &[Value], cfg: &Value) -> Option<(String, Value)> {
    let want = model.trim().to_lowercase();
    let mut best: Option<(i32, Value)> = None;
    for b in chain {
        if !b.b("available", true) {
            continue;
        }
        let kind = b.s("kind").to_lowercase();
        let hay = format!("{} {}", b.s("id"), b.s("name")).to_lowercase();
        let score = match family {
            // "claude" = a frontier/subscription brain. Claude accounts first; when they are out,
            // ChatGPT (Codex) and Google (Antigravity) accounts take the same work, then the API.
            "claude" => {
                if kind == "claude_code" {
                    4
                } else if !cfg["router"].b("task_harnesses", false)
                    && (kind == "codex" || kind == "antigravity")
                {
                    3
                } else if kind == "anthropic" {
                    2
                } else {
                    continue;
                }
            }
            "ds" => {
                if kind != "openai" {
                    continue;
                }
                let in_ladder = !want.is_empty()
                    && cfg
                        .arr("brains")
                        .iter()
                        .find(|x| x.s("id") == b.s("id"))
                        .map(|x| {
                            x.arr("ladder").iter().any(|m| {
                                m.as_str()
                                    .map(|m| m.to_lowercase() == want)
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false);
                if in_ladder {
                    8
                } else if hay.contains("deepseek") {
                    4
                } else {
                    2
                }
            }
            "codex" | "gemini_cli" | "antigravity" if kind == family => 4,
            _ => continue,
        };
        let better = match &best {
            Some((s, _)) => score > *s,
            None => true,
        };
        if better {
            best = Some((score, b.clone()));
        }
    }
    let (_, entry) = best?;
    let id = entry.s("id");
    let full = cfg
        .arr("brains")
        .into_iter()
        .find(|b| b.s("id") == id)
        .unwrap_or(entry);
    Some((id, full))
}

/// Map "haiku"/"sonnet"/"opus" (or a full model name) onto the brain's real ladder entry.
fn resolve_model(brain: &Value, want: &str) -> String {
    let ladder: Vec<String> = brain
        .arr("ladder")
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    let w = want.trim().to_lowercase();
    if w == "auto" {
        return String::new();
    }
    if ladder.is_empty() {
        // Account agents (ChatGPT, Google) pick their own default when they have no ladder;
        // a Claude alias like "haiku" means nothing to them.
        let kind = brain.s("kind");
        return if kind == "claude_code" || kind == "anthropic" {
            want.to_string()
        } else {
            String::new()
        };
    }
    if w.is_empty() {
        return want.to_string();
    }
    for m in &ladder {
        if m.to_lowercase() == w {
            return m.clone();
        }
    }
    for m in &ladder {
        if m.to_lowercase().contains(&w) {
            return m.clone();
        }
    }
    for m in &ladder {
        if w.contains(&m.to_lowercase()) {
            return m.clone();
        }
    }
    // Another provider's model name: map by tier (cheap names → cheapest rung, else the top rung).
    let cheap = [
        "flash", "lite", "mini", "haiku", "small", "hydrogen", "luna",
    ];
    if cheap.iter().any(|c| w.contains(c)) {
        return ladder.last().cloned().unwrap_or_else(|| want.to_string());
    }
    ladder.first().cloned().unwrap_or_else(|| want.to_string())
}

fn resolve(cand: &str, chain: &[Value], cfg: &Value) -> Option<Value> {
    let (fam, model, effort) = split_cand(cand);
    let (bid, brain) = brain_for(&fam, &model, chain, cfg)?;
    let real = resolve_model(&brain, &model);
    Some(json!({"candidate": cand, "brain": bid, "model": real, "effort": effort}))
}

fn candidates_for(cfg: &Value, project: &str, level: u8) -> Vec<String> {
    let lv = level.to_string();
    let read = |v: Option<&Value>| -> Vec<String> {
        v.and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default()
    };
    let router = cfg.get("router");
    let from_project = read(
        router
            .and_then(|r| r.get("projects"))
            .and_then(|p| p.get(project))
            .and_then(|p| p.get("levels"))
            .and_then(|l| l.get(lv.as_str())),
    );
    if !from_project.is_empty() {
        return from_project;
    }
    let from_global = read(
        router
            .and_then(|r| r.get("levels"))
            .and_then(|l| l.get(lv.as_str())),
    );
    if !from_global.is_empty() {
        return from_global;
    }
    read(default_levels().get(lv.as_str()))
}

fn target_of(cfg: &Value) -> f64 {
    cfg.get("router")
        .and_then(|r| r.get("target"))
        .and_then(|v| v.as_f64())
        .filter(|t| *t > 0.0 && *t <= 1.0)
        .unwrap_or(0.85)
}

/// Next step of the ladder after `candidate` (same level, else the cheapest of the next level).
pub fn escalate(level: u8, candidate: &str) -> Option<String> {
    let lv = level.clamp(1, 5);
    let levels = default_levels();
    let key = lv.to_string();
    let list: Vec<String> = levels
        .get(key.as_str())
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(i) = list.iter().position(|c| c == candidate) {
        if i + 1 < list.len() {
            return Some(list[i + 1].clone());
        }
    }
    if lv >= 5 {
        return None;
    }
    let next = (lv + 1).to_string();
    levels
        .get(next.as_str())
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ---------- statistics helpers ----------

struct Row {
    cand: String,
    cost: f64,
    p: f64,
    n: f64,
    tokens: f64,
    pick: Option<Value>,
}

/// (evidence weight seen, weighted successes, average tokens). Counts are fractional: a verified
/// result (tests, build) weighs 1, weaker signals (no retry in 30 min) weigh less.
fn stat_of(data: &Value, project: &str, level: u8, cand: &str) -> (f64, f64, f64) {
    let lv = level.to_string();
    let e = data
        .get("projects")
        .and_then(|p| p.get(project))
        .and_then(|p| p.get(lv.as_str()))
        .and_then(|p| p.get(cand));
    match e {
        Some(e) => (
            e.get("n").and_then(|v| v.as_f64()).unwrap_or(0.0),
            e.get("ok").and_then(|v| v.as_f64()).unwrap_or(0.0),
            e.get("tokens").and_then(|v| v.as_f64()).unwrap_or(0.0),
        ),
        None => (0.0, 0.0, 0.0),
    }
}

/// Minimize expected spend among candidates with enough evidence of success.
fn choose(rows: &[Row], avail: &[usize], target: f64) -> Option<usize> {
    if avail.is_empty() {
        return None;
    }
    let mut idx: Vec<usize> = avail.to_vec();
    let rank = |r: &Row| {
        let tokens = if r.tokens > 0.0 {
            r.tokens
        } else {
            2000.0 + r.cost * 200.0
        };
        r.cost * tokens / r.p.max(0.01)
    };
    idx.sort_by(|a, b| {
        let ca = rank(&rows[*a]).total_cmp(&rank(&rows[*b]));
        let pa = rows[*b]
            .p
            .partial_cmp(&rows[*a].p)
            .unwrap_or(std::cmp::Ordering::Equal);
        ca.then(pa).then_with(|| rows[*a].cand.cmp(&rows[*b].cand))
    });
    if let Some(&i) = idx.iter().find(|i| rows[**i].p >= target) {
        return Some(i);
    }
    // Nobody has proven itself yet: use the level's default (first in the ladder, as configured),
    // and let exploration try the cheaper ones now and then.
    avail
        .iter()
        .copied()
        .find(|i| rows[*i].n < EXPLORE_MIN_N)
        .or_else(|| {
            avail.iter().copied().max_by(|a, b| {
                rows[*a]
                    .p
                    .total_cmp(&rows[*b].p)
                    .then_with(|| rank(&rows[*b]).total_cmp(&rank(&rows[*a])))
            })
        })
}

fn round1(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    (x * 10.0).round() / 10.0
}

fn round3(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    (x * 1000.0).round() / 1000.0
}

/// Walk (and create) an object path, then run `f` on the leaf (always an object).
fn with_path_mut<F: FnOnce(&mut Value)>(v: &mut Value, keys: &[&str], f: F) {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    match keys.split_first() {
        None => f(v),
        Some((k, rest)) => {
            let child = match v.as_object_mut() {
                Some(m) => m.entry((*k).to_string()).or_insert(Value::Null),
                None => return,
            };
            with_path_mut(child, rest, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "metabrain-router-{}-{}-{}.json",
            name,
            std::process::id(),
            nanos
        ))
    }

    fn cfg() -> Value {
        json!({
            "router": {"target": 0.85},
            "brains": [
                {"id": "claude", "kind": "claude_code",
                 "ladder": ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]},
                {"id": "deepseek", "kind": "openai",
                 "ladder": ["deepseek-v4-pro", "deepseek-flash"]}
            ]
        })
    }

    fn chain() -> Vec<Value> {
        vec![
            json!({"id": "claude", "kind": "claude_code", "available": true}),
            json!({"id": "deepseek", "kind": "openai", "name": "DeepSeek", "available": true}),
        ]
    }

    #[test]
    fn classify_covers_every_level() {
        assert_eq!(classify("renombrar los archivos y leer logs").0, 1);
        assert_eq!(classify("rename files and move them").0, 1);
        assert_eq!(classify("write a bash script with ffmpeg").0, 2);
        assert_eq!(classify("fix typo in the template").0, 2);
        assert_eq!(classify("implementar la funcion de login en la api").0, 3);
        assert_eq!(
            classify("depurar el deadlock de concurrencia en el servidor").0,
            4
        );
        assert_eq!(classify("deploy to production").0, 5);
        assert_eq!(classify("publicar la version final a produccion").0, 5);
    }

    #[test]
    fn route_picks_cheapest_by_default() {
        let r = Router::new(Some(tmp("cheap")));
        let out = r.route(&cfg(), &chain(), "demo", "renombrar archivos", None);
        assert_eq!(out["level"], 1);
        assert_eq!(out["level_name"], "mecánico");
        assert_eq!(out["pick"]["candidate"], "ds:deepseek-flash:none");
        assert_eq!(out["pick"]["brain"], "deepseek");
        assert_eq!(out["pick"]["model"], "deepseek-flash");
        assert_eq!(out["pick"]["effort"], "none");
        assert!(out["saving_vs_opus_pct"].as_f64().unwrap_or(0.0) > 90.0);
    }

    #[test]
    fn route_prefers_prices_and_falls_back_to_legacy_pricing() {
        let r = Router::new(Some(tmp("prices")));
        let chain = chain();
        let mut cfg = cfg();
        let route_cost = |cfg: &Value| {
            let route = r.route(cfg, &chain, "demo", "renombrar archivos", Some(1));
            let option = route["alternatives"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["candidate"] == "ds:deepseek-flash:none")
                .unwrap();
            option["cost"].as_f64().unwrap()
        };

        assert_eq!(route_cost(&cfg), candidate_cost("ds:deepseek-flash:none"));
        // Configured USD prices are normalized onto the OPUS_COST (150.0 = $15/1M) scale (* 10.0).
        cfg["brains"][1]["pricing"] = json!({"deepseek-flash": {"input": 2.0}});
        assert_eq!(route_cost(&cfg), 20.0);
        cfg["brains"][1]["prices"] = json!({"deepseek-flash": {"input": 3.0}});
        assert_eq!(route_cost(&cfg), 30.0);
    }

    #[test]
    fn route_learns_from_records() {
        let r = Router::new(Some(tmp("learn")));
        for _ in 0..(EXPLORE_MIN_N as i64) {
            r.record("demo", 1, "ds:deepseek-flash:none", false, 0);
        }
        for _ in 0..6 {
            r.record("demo", 1, "claude:haiku:low", true, 1200);
        }
        let out = r.route(&cfg(), &chain(), "demo", "renombrar archivos", None);
        assert_eq!(out["pick"]["candidate"], "claude:haiku:low");
        assert_eq!(out["pick"]["model"], "claude-haiku-5-5");
        assert!(out["expected_success"].as_f64().unwrap_or(0.0) >= 0.85);
    }

    #[test]
    fn record_failure_escalates() {
        let r = Router::new(Some(tmp("esc")));
        let a = r.record("demo", 1, "ds:deepseek-flash:none", false, 800);
        assert_eq!(a["next"], "claude:haiku:low");
        let b = r.record("demo", 4, "claude:opus:medium", false, 9000);
        assert_eq!(b["next"], "claude:opus:high");
        let c = r.record("demo", 5, "claude:opus:high", false, 9000);
        assert!(c["next"].is_null());
        assert_eq!(c["ok"], false);
    }

    #[test]
    fn unavailable_brains_are_skipped() {
        let r = Router::new(Some(tmp("chain")));
        let no_claude = vec![
            json!({"id": "claude", "kind": "claude_code", "available": false}),
            json!({"id": "deepseek", "kind": "openai", "name": "DeepSeek", "available": true}),
        ];
        let out = r.route(
            &cfg(),
            &no_claude,
            "demo",
            "implementar la funcion de login en la api",
            None,
        );
        assert_eq!(out["level"], 3);
        assert_eq!(out["pick"]["candidate"], "ds:deepseek-v4-pro:high");
        assert_eq!(out["pick"]["model"], "deepseek-v4-pro");

        let second_account = vec![
            json!({"id": "claude", "kind": "claude_code", "available": false}),
            json!({"id": "claude_b", "kind": "claude_code", "available": true}),
            json!({"id": "deepseek", "kind": "openai", "name": "DeepSeek", "available": true}),
        ];
        let out2 = r.route(
            &cfg(),
            &second_account,
            "demo",
            "deploy to production",
            None,
        );
        assert_eq!(out2["level"], 5);
        assert_eq!(out2["pick"]["brain"], "claude_b");
    }

    #[test]
    fn set_levels_validates_and_is_used() {
        let r = Router::new(Some(tmp("levels")));
        let mut c = cfg();
        r.set_levels(
            &mut c,
            "demo",
            2,
            vec!["ds:deepseek-flash:low".into(), "claude:sonnet:low".into()],
        );
        let n = c["router"]["projects"]["demo"]["levels"]["2"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        assert_eq!(n, 2);
        r.set_levels(&mut c, "demo", 3, vec!["nope".into()]);
        assert!(c["router"]["projects"]["demo"]["levels"].get("3").is_none());
        let out = r.route(&c, &chain(), "demo", "escribir test", None);
        assert_eq!(out["level"], 2);
        assert_eq!(out["pick"]["candidate"], "ds:deepseek-flash:low");
    }

    #[test]
    fn task_routing_changes_harness_with_complexity_and_availability() {
        let r = Router::new(Some(tmp("harnesses")));
        let mut config = cfg();
        config["brains"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "codex", "kind": "codex"}));
        config["brains"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "gemini", "kind": "gemini_cli",
            "ladder": ["gemini-pro", "gemini-flash"]}));
        let config = task_routing_config(config);
        let mut active = chain();
        active.push(json!({"id": "codex", "kind": "codex", "available": true}));
        active.push(json!({"id": "gemini", "kind": "gemini_cli", "available": true}));
        let pick = |task: &str, chain: &[Value]| {
            r.route(&config, chain, "demo", task, None)["pick"].clone()
        };
        assert_eq!(pick("resume y lee los logs", &active)["brain"], "deepseek");
        assert_eq!(pick("escribe un script", &active)["brain"], "codex");
        assert_eq!(pick("deploy to production", &active)["brain"], "claude");
        active[2]["available"] = json!(false);
        assert_eq!(pick("escribe un script", &active)["model"], "gemini-flash");
    }
}

#[cfg(test)]
mod provider_fallback_tests {
    use super::*;

    #[test]
    fn claude_work_goes_to_chatgpt_or_google_when_claude_is_out() {
        let cfg = json!({"brains": [
            {"id": "A", "kind": "claude_code", "name": "Claude", "ladder": ["claude-opus-5-5", "claude-haiku-5-5"]},
            {"id": "C", "kind": "codex", "name": "ChatGPT", "ladder": ["gpt-6.2-sol"]},
            {"id": "G", "kind": "antigravity", "name": "Gemini", "ladder": []}
        ]});
        let chain = vec![
            json!({"id": "A", "kind": "claude_code", "available": false}),
            json!({"id": "C", "kind": "codex", "available": true}),
            json!({"id": "G", "kind": "antigravity", "available": true}),
        ];
        let r = resolve("claude:opus:high", &chain, &cfg).unwrap();
        assert_eq!(r.s("brain"), "C");
        assert_eq!(r.s("model"), "gpt-6.2-sol");
        let chain2 = vec![
            json!({"id": "A", "kind": "claude_code", "available": false}),
            json!({"id": "C", "kind": "codex", "available": false}),
            json!({"id": "G", "kind": "antigravity", "available": true}),
        ];
        let r = resolve("claude:haiku:low", &chain2, &cfg).unwrap();
        assert_eq!(r.s("brain"), "G");
        assert_eq!(r.s("model"), "", "no ladder: let the agent choose");
    }

    #[test]
    fn ds_candidate_falls_back_to_gemini_by_tier() {
        let cfg = json!({"brains": [
            {"id": "DS", "kind": "openai", "name": "DeepSeek", "ladder": ["deepseek-v4-pro", "deepseek-flash"]},
            {"id": "GM", "kind": "openai", "name": "Gemini", "ladder": ["gemini-3.5-flash", "gemini-3.5-flash-lite"]}
        ]});
        let only_gm = vec![
            json!({"id": "DS", "kind": "openai", "name": "DeepSeek", "available": false}),
            json!({"id": "GM", "kind": "openai", "name": "Gemini", "available": true}),
        ];
        let r = resolve("ds:deepseek-flash:none", &only_gm, &cfg).unwrap();
        assert_eq!(r.s("brain"), "GM");
        assert_eq!(r.s("model"), "gemini-3.5-flash-lite");
        let r = resolve("ds:deepseek-v4-pro:high", &only_gm, &cfg).unwrap();
        assert_eq!(r.s("model"), "gemini-3.5-flash");
        let both = vec![
            json!({"id": "DS", "kind": "openai", "name": "DeepSeek", "available": true}),
            json!({"id": "GM", "kind": "openai", "name": "Gemini", "available": true}),
        ];
        assert_eq!(
            resolve("ds:deepseek-flash:none", &both, &cfg)
                .unwrap()
                .s("brain"),
            "DS"
        );
        assert_eq!(
            resolve("ds:gemini-3.5-flash:none", &both, &cfg)
                .unwrap()
                .s("brain"),
            "GM"
        );
    }
}
