//! API brains (OpenAI-compatible, Ollama, Anthropic) and subscription CLI adapters (Codex, Gemini, Antigravity).
use super::scanner::mtime;
use super::{Brain, Imp};
use crate::store::Usage;
use crate::util::{expand, http_json, log, midnight, now, read_json, secret, J};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

#[derive(Default)]
pub struct ApiInner {
    /// request id -> {app, model, phase, ts}, in arrival order
    pub inflight: Vec<(String, Value)>,
    pub calls: VecDeque<Value>,
    pub lobe_ts: [f64; 3], // receive, think, respond
    pub balance: Value,
    pub warning: String,
    pub file_mtime: f64,
    pub loaded: Vec<String>,
    /// Live plan limits for subscription CLIs (Codex / Antigravity), updated in slow_poll().
    pub quota: Vec<Value>,
    /// Epoch seconds of the last quota refresh (None = never fetched).
    pub quota_ts: Option<f64>,
    /// Models discovered live from the provider's /models endpoint (never persisted to disk).
    pub live_models: Vec<String>,
    /// Epoch seconds of the last /models attempt (0.0 = never tried).
    pub models_at: f64,
    /// Set once we warned that the API key is missing.
    pub key_warned: bool,
    /// Set once we logged a models fetch failure.
    pub model_err_logged: bool,
}

const API_LOBES: [&str; 3] = ["receive", "think", "respond"];

/// Minimum seconds between live model refreshes (6 hours).
const MODELS_INTERVAL: f64 = 6.0 * 3600.0;

/// Resolve the visible model list: the configured ladder wins, else the live list.
pub(super) fn resolve_models(ladder: &[Value], live: &[String]) -> Vec<String> {
    let l: Vec<String> = ladder
        .iter()
        .filter_map(|m| m.as_str().map(String::from))
        .collect();
    if l.is_empty() {
        live.to_vec()
    } else {
        l
    }
}

/// Is a live refresh due, given the last attempt time?
pub(super) fn models_due(models_at: f64, t: f64) -> bool {
    models_at == 0.0 || t - models_at >= MODELS_INTERVAL
}

/// Only OpenAI-compatible gemini brains (or brains with an empty ladder) need live models.
fn wants_live_models(kind: &str, provider: &str, ladder_empty: bool) -> bool {
    kind == "openai" && (provider == "gemini" || ladder_empty)
}

pub(super) enum ModelsFetch {
    Skip,
    MissingKey,
    Fetch,
}

/// Decide what the next /models poll should do, without doing any IO.
pub(super) fn models_fetch_plan(
    kind: &str,
    provider: &str,
    ladder_empty: bool,
    models_at: f64,
    t: f64,
    has_key: bool,
) -> ModelsFetch {
    if !wants_live_models(kind, provider, ladder_empty) {
        return ModelsFetch::Skip;
    }
    if !models_due(models_at, t) {
        return ModelsFetch::Skip;
    }
    if !has_key {
        return ModelsFetch::MissingKey;
    }
    ModelsFetch::Fetch
}

impl Brain {
    pub(super) fn slow_poll_api(&self, inner: &Mutex<ApiInner>) {
        let cfg = self.cfg();
        // Consumer-account CLIs (Codex / Gemini CLI / Antigravity): check install, sign-in & quota.
        if let Some(agent) = crate::cli_agents::Agent::from_kind(&self.kind) {
            let mut env_pairs: Vec<(String, String)> = Vec::new();
            if agent == crate::cli_agents::Agent::Codex {
                let home = crate::cli_agents::codex_home(&cfg);
                if !home.as_os_str().is_empty() {
                    env_pairs.push(("CODEX_HOME".into(), home.to_string_lossy().into_owned()));
                }
            }
            let w = if crate::cli_agents::find_bin(agent).is_none() {
                format!("{} no está instalado", agent.display_name())
            } else if !crate::cli_agents::logged_in_with_env(agent, &env_pairs) {
                if agent == crate::cli_agents::Agent::Antigravity {
                    "Sin sesión: inicia sesión desde Cerebros".to_string()
                } else {
                    format!(
                        "Sin sesión: ejecuta {}",
                        crate::cli_agents::login_command(agent)
                    )
                }
            } else {
                String::new()
            };
            let due = {
                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                i.warning = w.clone();
                i.quota_ts.map(|ts| now() - ts > 90.0).unwrap_or(true)
            };
            if w.is_empty() && due {
                match agent {
                    crate::cli_agents::Agent::Codex => {
                        let home = crate::cli_agents::codex_home(&cfg);
                        let limits = crate::cli_agents::codex_quota(&home);
                        let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                        if !limits.is_empty() {
                            i.quota = limits;
                        }
                        i.quota_ts = Some(now());
                    }
                    crate::cli_agents::Agent::Antigravity => {
                        let hint = cfg
                            .arr("ladder")
                            .first()
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        match crate::cli_agents::antigravity_quota(&hint) {
                            Ok(limits) => {
                                let vals: Vec<Value> = limits
                                    .into_iter()
                                    .filter_map(|q| serde_json::to_value(q).ok())
                                    .collect();
                                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                                i.quota = vals;
                                i.quota_ts = Some(now());
                            }
                            Err(e) => {
                                log(&format!("{}: quota /usage no disponible: {e}", self.id));
                                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                                // Retry sooner (30 s) if we still have no quota reading yet.
                                i.quota_ts = Some(if i.quota.is_empty() {
                                    now() - 60.0
                                } else {
                                    now()
                                });
                            }
                        }
                    }
                    crate::cli_agents::Agent::Gemini => {}
                }
            }
            return;
        }
        if self.kind == "ollama" {
            let base = cfg.s("base_url").trim_end_matches('/').to_string();
            let ps = http_json(&format!("{base}/api/ps"), &[], None, 4);
            let tags = ps
                .as_ref()
                .ok()
                .and_then(|_| http_json(&format!("{base}/api/tags"), &[], None, 4).ok());
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            match (ps, tags) {
                (Ok(ps), Some(tags)) => {
                    i.loaded = ps.arr("models").iter().map(|m| m.s("name")).collect();
                    if !crate::util::truthy(cfg.get("ladder")) {
                        let names: Vec<Value> = tags
                            .arr("models")
                            .iter()
                            .map(|m| json!(m.s("name")))
                            .collect();
                        self.config
                            .with_brain(&self.id, |b| b["ladder"] = Value::Array(names));
                    }
                    i.warning.clear();
                }
                _ => {
                    i.loaded.clear();
                    i.warning = "Ollama is not running".into();
                }
            }
            return;
        }
        // Gemini (and OpenAI-compatible brains with an empty ladder): refresh live models.
        if self.kind == "openai" {
            self.refresh_models(inner);
        }
        if self.kind != "openai" || cfg.s("balance") != "deepseek" {
            return;
        }
        let Some(key) = self.key() else { return };
        let auth = format!("Bearer {key}");
        let url = format!("{}/user/balance", cfg.s("base_url").trim_end_matches('/'));
        let r = http_json(
            &url,
            &[("Authorization", &auth), ("Accept", "application/json")],
            None,
            15,
        );
        let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Ok(r) => {
                let b = r.arr("balance_infos").first().cloned().unwrap_or(json!({}));
                let amount = match b.get("total_balance") {
                    Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
                    Some(v) => v.as_f64().unwrap_or(0.0),
                    None => 0.0,
                };
                i.balance = json!({
                    "amount": amount,
                    "currency": b.get("currency").cloned().unwrap_or(json!(""))
                });
                i.warning = if r.b("is_available", true) {
                    String::new()
                } else {
                    "balance not available".into()
                };
            }
            Err(e) => {
                let name = if e.starts_with("HTTPError") {
                    "HTTPError"
                } else {
                    "URLError"
                };
                i.warning = format!("balance not available ({name})");
            }
        }
    }

    /// Fetch the provider's model list at most every 6 h (and on the very first poll).
    /// Stores the result in memory only; the config file is never written.
    fn refresh_models(&self, inner: &Mutex<ApiInner>) {
        let cfg = self.cfg();
        let provider = cfg.s("provider");
        let ladder_empty = !crate::util::truthy(cfg.get("ladder"));
        let has_key = self.key().is_some();
        let models_at = inner.lock().unwrap_or_else(|e| e.into_inner()).models_at;
        match models_fetch_plan(
            &self.kind,
            &provider,
            ladder_empty,
            models_at,
            now(),
            has_key,
        ) {
            ModelsFetch::Skip => {}
            ModelsFetch::MissingKey => {
                let env = cfg.s("key_env");
                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                if !i.key_warned {
                    i.warning = format!("Falta la clave {env}");
                    i.key_warned = true;
                }
            }
            ModelsFetch::Fetch => {
                let Some(key) = self.key() else { return };
                let Some(url) = crate::config::models_url(&cfg) else {
                    return;
                };
                let auth = format!("Bearer {key}");
                let r = http_json(
                    &url,
                    &[("Authorization", &auth), ("Accept", "application/json")],
                    None,
                    15,
                );
                let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
                match r {
                    Ok(body) => {
                        let models = crate::config::parse_models(&provider, &body);
                        i.models_at = now();
                        if !models.is_empty() {
                            i.live_models = models;
                            i.warning.clear();
                            i.key_warned = false;
                            i.model_err_logged = false;
                        }
                    }
                    Err(e) => {
                        // Keep the previous list; log the failure only once.
                        if !i.model_err_logged {
                            log(&format!("{}: models fetch failed: {e}", self.id));
                            i.model_err_logged = true;
                        }
                        i.models_at = now();
                    }
                }
            }
        }
    }

    pub(super) fn quota_pct_api(&self, inner: &Mutex<ApiInner>) -> Option<f64> {
        let mut limits = inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .quota
            .clone();
        if limits.is_empty() && self.kind == "codex" {
            let home = crate::cli_agents::codex_home(&self.cfg());
            limits = crate::cli_agents::codex_quota(&home);
        }
        limits
            .iter()
            .filter(|l| matches!(l.s("group").as_str(), "session" | "weekly" | "quota"))
            .filter_map(|l| l.f("pct"))
            .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))
    }

    pub fn key(&self) -> Option<String> {
        secret(&self.cfg().s("key_env"))
    }

    pub fn price(&self, model: &str, inp: i64, out: i64, cache_read: i64) -> f64 {
        let prices = self.cfg().o("prices");
        let p = prices
            .get(model)
            .filter(|p| crate::util::truthy(Some(p)))
            .or_else(|| prices.get("*"))
            .cloned();
        let Some(p) = p.filter(|p| crate::util::truthy(Some(p))) else {
            return 0.0;
        };
        let fresh = (inp - cache_read).max(0) as f64;
        let pin = p.f("in").unwrap_or(0.0);
        (fresh * pin
            + out as f64 * p.f("out").unwrap_or(0.0)
            + cache_read as f64 * p.f("cache").unwrap_or(pin))
            / 1e6
    }

    pub fn begin(&self, rid: &str, app: &str, model: &str) {
        if let Imp::Api(inner) = &self.imp {
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            i.inflight.push((
                rid.to_string(),
                json!({"app": app, "model": model, "phase": "receive", "ts": now()}),
            ));
            i.lobe_ts[0] = now();
        }
    }

    pub fn phase(&self, rid: &str, phase: &str) {
        if let Imp::Api(inner) = &self.imp {
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, r)) = i.inflight.iter_mut().find(|(k, _)| k == rid) {
                r["phase"] = json!(phase);
            }
            if let Some(ix) = API_LOBES.iter().position(|l| *l == phase) {
                i.lobe_ts[ix] = now();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn end(
        &self,
        rid: &str,
        model: &str,
        inp: i64,
        out: i64,
        cache_read: i64,
        cache_write: i64,
        reasoning: i64,
        error: Option<String>,
    ) {
        let Imp::Api(inner) = &self.imp else { return };
        let req = {
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            match i.inflight.iter().position(|(k, _)| k == rid) {
                Some(ix) => i.inflight.remove(ix).1,
                None => json!({"app": "?", "ts": now()}),
            }
        };
        let usd = self.price(model, inp, out, cache_read);
        let t = now();
        let secs = ((t - req.f("ts").unwrap_or(t)) * 10.0).round() / 10.0;
        {
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            i.calls.push_back(json!({
                "ts": t,
                "app": req.s("app"),
                "model": model,
                "tokens": inp + out,
                "usd": usd,
                "secs": secs,
                "error": error,
                "reasoning": reasoning
            }));
            while i.calls.len() > 400 {
                i.calls.pop_front();
            }
        }
        if inp != 0 || out != 0 {
            self.ledger.add(
                Usage {
                    ts: now(),
                    brain: self.id.clone(),
                    model: Some(model.to_string()),
                    inp,
                    out,
                    cache_read,
                    cache_write,
                    usd,
                },
                true,
            );
        }
    }

    pub(super) fn poll_usage_file(&self, inner: &Mutex<ApiInner>) {
        let f = self.cfg().s("usage_file");
        if f.is_empty() {
            return;
        }
        let p = expand(&f);
        let Some(mt) = mtime(&p) else { return };
        {
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            if mt == i.file_mtime {
                return;
            }
            i.file_mtime = mt;
        }
        let data = read_json(&p).unwrap_or(json!({}));
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let fid = format!("{}:file", self.id);
        let mut vol = HashMap::new();
        if let Some(days) = data.as_object() {
            for (day, models) in days {
                let Some(models) = models.as_object() else {
                    continue;
                };
                if day.chars().count() != 10 {
                    continue;
                }
                let Ok(nd) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
                    continue;
                };
                let base = {
                    use chrono::TimeZone;
                    let dt = nd.and_hms_opt(0, 0, 0).unwrap();
                    let ts = chrono::Local
                        .from_local_datetime(&dt)
                        .earliest()
                        .map(|x| x.timestamp() as f64)
                        .unwrap_or(0.0);
                    midnight(ts)
                };
                let ts = if *day == today {
                    mt
                } else {
                    base + 12.0 * 3600.0
                };
                for (model, v) in models {
                    if model.starts_with('_') || !v.is_object() {
                        continue;
                    }
                    let n = |a: &str, b: &str| -> i64 {
                        v.get(a)
                            .or_else(|| v.get(b))
                            .and_then(|x| x.as_f64())
                            .unwrap_or(0.0) as i64
                    };
                    let inp = n("input", "entrada");
                    let out = n("output", "salida");
                    let cache = n("cache", "cache_read");
                    vol.insert(
                        format!("file|{day}|{model}"),
                        Usage {
                            ts,
                            brain: fid.clone(),
                            model: Some(model.clone()),
                            inp,
                            out,
                            cache_read: cache,
                            cache_write: 0,
                            usd: self.price(model, inp, out, cache),
                        },
                    );
                }
            }
        }
        self.ledger.set_volatile(&fid, vol);
    }

    pub(super) fn snapshot_api(&self, inner: &Mutex<ApiInner>, now_ts: f64) -> Value {
        let i = inner.lock().unwrap_or_else(|e| e.into_inner());
        let live: Vec<Value> = i.inflight.iter().map(|(_, r)| r.clone()).collect();
        let recent: Vec<Value> = i
            .calls
            .iter()
            .filter(|c| now_ts - c.f("ts").unwrap_or(0.0) < 600.0)
            .cloned()
            .collect();
        let mut apps: Vec<(String, Value)> = vec![];
        for c in &recent {
            let app = c.s("app");
            let ix = match apps.iter().position(|(k, _)| *k == app) {
                Some(ix) => ix,
                None => {
                    apps.push((
                        app.clone(),
                        json!({"id": app, "desc": app, "type": "app", "model": c.get("model"), "tokens": 0, "alive": false, "status": "", "last": 0}),
                    ));
                    apps.len() - 1
                }
            };
            let a = &mut apps[ix].1;
            a["tokens"] =
                json!(a["tokens"].as_i64().unwrap_or(0) + c["tokens"].as_i64().unwrap_or(0));
            a["last"] = json!(a.f("last").unwrap_or(0.0).max(c.f("ts").unwrap_or(0.0)));
            a["model"] = c["model"].clone();
        }
        for r in &live {
            let app = r.s("app");
            match apps.iter_mut().find(|(k, _)| *k == app) {
                Some((_, a)) => {
                    a["alive"] = json!(true);
                    a["status"] = r["phase"].clone();
                }
                None => apps.push((
                    app.clone(),
                    json!({"id": app, "desc": app, "type": "app", "model": r.get("model"), "tokens": 0, "alive": true, "status": r["phase"], "last": r["ts"]}),
                )),
            }
        }
        let file_recent = i.file_mtime != 0.0 && now_ts - i.file_mtime < 120.0;
        let status = if !live.is_empty() {
            format!("{} request(s) in flight", live.len())
        } else if file_recent {
            "reported usage just now".into()
        } else {
            "standing by".into()
        };
        let mut lobes = Map::new();
        for (ix, k) in API_LOBES.iter().enumerate() {
            lobes.insert(
                k.to_string(),
                json!(Self::lobe(i.lobe_ts[ix], now_ts) || (*k == "respond" && file_recent)),
            );
        }
        let mut model = live
            .last()
            .or(recent.last())
            .map(|x| x.get("model").cloned().unwrap_or(json!("")))
            .unwrap_or(json!(""));
        if !crate::util::truthy(Some(&model)) {
            // No chosen/reported model yet: fall back to the first visible model.
            let ladder = self.cfg().arr("ladder");
            let def = resolve_models(&ladder, &i.live_models);
            model = json!(def.first().cloned().unwrap_or_default());
        }
        let calls: Vec<Value> = recent.iter().rev().take(20).cloned().collect();
        let mut s = json!({
            "active": !live.is_empty() || file_recent,
            "status": status,
            "lobes": lobes,
            "lobe_names": API_LOBES,
            "model": model,
            "effort": "",
            "subagents": apps.into_iter().take(12).map(|x| x.1).collect::<Vec<_>>(),
            "jobs": [],
            "sessions": [],
            "calls": calls,
            "quota": i.quota,
            "quota_ts": i.quota_ts,
            "balance": i.balance,
            "warning": i.warning,
            "proxy": format!("/proxy/{}/v1", self.id),
            "ready": true
        });
        // Live plan limits for subscription agents, whatever the brain.
        if self.kind == "codex" {
            let home = crate::cli_agents::codex_home(&self.cfg());
            let q = crate::cli_agents::codex_quota(&home);
            if !q.is_empty() {
                s["quota"] = json!(q);
            }
            if let Some(e) = crate::cli_agents::codex_account(&home).filter(|e| !e.is_empty()) {
                s["account"] = json!(e);
            }
        }
        if self.kind == "antigravity" {
            let f = crate::util::home()
                .join(".gemini")
                .join("google_accounts.json");
            if let Some(a) = std::fs::read_to_string(f)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("active").and_then(|x| x.as_str()).map(String::from))
            {
                s["account"] = json!(a);
            }
        }
        if self.kind == "ollama" {
            s["loaded"] = json!(i.loaded);
            if !s.b("active", false) && !i.loaded.is_empty() {
                s["status"] = json!(format!(
                    "loaded: {}",
                    i.loaded
                        .iter()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        s
    }
}
