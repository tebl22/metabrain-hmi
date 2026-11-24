//! Smart pilot: per-brain and per-subagent-type metrics, a 0-100 score against the brain's own
//! history, and recommendations the HMI can apply with one click (port of metabrain/advisor.py).
//!
//! Pilot modes per brain (config "pilot"): off | advise (default) | act.
//! "act" only applies changes that LOWER spending; anything that raises it always needs the user.
use crate::brains::Brain;
use crate::config::EFFORTS;
use crate::core::Supervisor;
use crate::util::{data_dir, read_json, truthy, write_json, J};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

const EWMA: f64 = 0.1;
const WINDOW: f64 = 600.0; // tokens/min over the last 10 min
const APPLY_EVERY: f64 = 3600.0; // the pilot re-applies the same recommendation at most once an hour
const SUB_CHEAP_TOKENS: f64 = 30000.0; // a subagent type that stays small does not need the top model
const SAVE_EVERY: f64 = 300.0;

/// A poisoned lock is not a reason to lose the whole panel: reuse the inner value.
fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Learned baseline per brain: {"tpm": f64, "cache": f64|null}.
struct Agg {
    n: i64,
    tok: i64,
    models: Vec<String>,
}

pub struct Advisor {
    path: PathBuf,
    learned: Mutex<Value>,
    /// brain id -> {"mode","metrics","score","recs"}
    pub out: Mutex<HashMap<String, Value>>,
    applied: Mutex<HashMap<(String, String), f64>>,
    saved: Mutex<f64>,
}

impl Advisor {
    pub fn new(path: Option<PathBuf>) -> Advisor {
        let path = path.unwrap_or_else(|| data_dir().join("advisor.json"));
        let learned = match read_json(&path) {
            Some(v @ Value::Object(_)) => v,
            _ => json!({}),
        };
        Advisor {
            path,
            learned: Mutex::new(learned),
            out: Mutex::new(HashMap::new()),
            applied: Mutex::new(HashMap::new()),
            saved: Mutex::new(0.0),
        }
    }

    pub fn mode(&self, b: &Brain) -> String {
        mode_of(&b.cfg())
    }

    // ---------------------------------------------------------------- metrics
    fn metrics(
        &self,
        sup: &Supervisor,
        b: &Brain,
        usages: &[crate::store::Usage],
        summ: Option<&Value>,
        now: f64,
    ) -> Value {
        let ids = [b.id.clone(), format!("{}:file", b.id)];
        let mut recent_tok: i64 = 0;
        let mut hour_tok: i64 = 0;
        let mut read: i64 = 0;
        let mut fresh: i64 = 0;
        let mut usd_h = 0.0;
        for u in usages {
            if u.ts < now - 3600.0 || !ids.contains(&u.brain) {
                continue;
            }
            let t = u.total();
            hour_tok += t;
            usd_h += u.usd;
            read += u.cache_read;
            // For Claude Code & OpenCode, `u.inp` is already uncached input.
            // For proxy / task-ledger records (`openai`, `codex`, `antigravity`, `gemini_cli`),
            // `u.inp` includes `cache_read`; subtract it so 100% cache hit reports 100%.
            let uncached_inp = if matches!(b.kind.as_str(), "claude_code" | "opencode") {
                u.inp
            } else {
                (u.inp - u.cache_read).max(0)
            };
            fresh += uncached_inp + u.cache_write;
            if u.ts >= now - WINDOW {
                recent_tok += t;
            }
        }
        let tpm = recent_tok as f64 / (WINDOW / 60.0);
        let cache = if read + fresh > 0 {
            Some(100.0 * read as f64 / (read + fresh) as f64)
        } else {
            None
        };
        let fc = guard(&sup.failover.forecasts).get(&b.id).cloned();
        let (ratio, _reasons) = sup.budget_ratio(b, summ);
        let h = guard(&self.learned)
            .get(b.id.as_str())
            .cloned()
            .unwrap_or_else(|| json!({}));
        json!({
            "tpm": tpm.round() as i64,
            "cache_pct": cache.map(|c| (c * 10.0).round() / 10.0),
            "usd_h": (usd_h * 10000.0).round() / 10000.0,
            "tok_h": hour_tok,
            "eta_min": fc.as_ref().and_then(|f| f.get("eta_min")).cloned().unwrap_or(Value::Null),
            "risk": fc.as_ref().and_then(|f| f.get("risk")).cloned().unwrap_or(Value::Null),
            "budget": ratio,
            "tpm_usual": h.f("tpm").unwrap_or(0.0).round() as i64,
            "cache_usual": h.get("cache").cloned().unwrap_or(Value::Null),
        })
    }

    fn learn_update(&self, bid: &str, tpm: f64, cache: Option<f64>) {
        if tpm <= 0.0 {
            return;
        }
        let mut hist = guard(&self.learned);
        if !hist.is_object() {
            *hist = json!({});
        }
        let map = match hist.as_object_mut() {
            Some(m) => m,
            None => return,
        };
        let entry = map
            .entry(bid.to_string())
            .or_insert_with(|| json!({"tpm": tpm, "cache": cache}));
        if !entry.is_object() {
            *entry = json!({"tpm": tpm});
        }
        let e = match entry.as_object_mut() {
            Some(e) => e,
            None => return,
        };
        let cur = e.get("tpm").and_then(Value::as_f64).unwrap_or(tpm);
        e.insert("tpm".to_string(), json!(cur + EWMA * (tpm - cur)));
        if let Some(c) = cache {
            let cur = e.get("cache").and_then(Value::as_f64);
            let v = match cur {
                Some(x) => x + EWMA * (c - x),
                None => c,
            };
            e.insert("cache".to_string(), json!(v));
        }
    }

    // ---------------------------------------------------------------- recommendations
    fn recommend(&self, b: &Brain, m: &Value, eff: &Value, subs: Option<&Value>) -> Vec<Value> {
        let cfg = b.cfg();
        let ladder = b.models();
        let sub_list: Vec<Value> = subs.and_then(|v| v.as_array()).cloned().unwrap_or_default();
        recommendations(
            &b.id,
            &b.kind,
            &ladder,
            eff.os("model").as_deref(),
            eff.os("effort").as_deref(),
            m,
            &sub_list,
            &cfg.o("subagents"),
        )
    }

    // ---------------------------------------------------------------- per tick
    pub fn tick(&self, sup: &Supervisor, snap_brains: &[Value], now: f64) {
        let summ = sup.ledger.summary(Some(now));
        self.tick_with(sup, snap_brains, Some(&summ), now);
    }

    pub fn tick_with(
        &self,
        sup: &Supervisor,
        snap_brains: &[Value],
        summ: Option<&Value>,
        now: f64,
    ) {
        let mut snaps: HashMap<String, Value> = HashMap::new();
        for x in snap_brains {
            if let Some(id) = x.os("id") {
                snaps.insert(id, x.clone());
            }
        }
        let brains: Vec<Arc<Brain>> = match sup.brains.read() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        };
        let has_active = brains.iter().any(|b| self.mode(b) != "off");
        let usages = if has_active {
            sup.ledger.all()
        } else {
            Vec::new()
        };
        for b in brains {
            let mode = self.mode(&b);
            if mode == "off" {
                guard(&self.out).insert(
                    b.id.clone(),
                    json!({"mode": "off", "metrics": null, "score": null, "recs": []}),
                );
                continue;
            }
            let m = self.metrics(sup, &b, &usages, summ, now);
            let eff = sup.effective(&b, summ);
            let subs = snaps.get(&b.id).and_then(|x| x.get("subagents")).cloned();
            let recs = self.recommend(&b, &m, &eff, subs.as_ref());
            self.learn_update(&b.id, m.f("tpm").unwrap_or(0.0), m.f("cache_pct"));
            if mode == "act" {
                let name = b.cfg().os("name").unwrap_or_else(|| b.id.clone());
                for r in &recs {
                    let action = r.get("action").cloned().unwrap_or(Value::Null);
                    if !action.is_object() || !r.b("saves", false) {
                        continue;
                    }
                    let key = (b.id.clone(), r.s("id"));
                    if now - guard(&self.applied).get(&key).copied().unwrap_or(0.0) <= APPLY_EVERY {
                        continue;
                    }
                    guard(&self.applied).insert(key, now);
                    let mut d = action;
                    if let Some(o) = d.as_object_mut() {
                        o.insert("by".to_string(), json!("pilot"));
                    }
                    let res = sup.act(&d);
                    sup.diary.add(
                        "Pilot",
                        &format!("{name}: {} ({res})", r.s("text")),
                        "warning",
                    );
                }
            }
            let score = score_of(&m);
            guard(&self.out).insert(
                b.id.clone(),
                json!({"mode": mode, "metrics": m, "score": score, "recs": recs}),
            );
        }
        let due = {
            let mut saved = guard(&self.saved);
            if now - *saved > SAVE_EVERY {
                *saved = now;
                true
            } else {
                false
            }
        };
        if due {
            let snapshot = guard(&self.learned).clone();
            // Same as the Python version: a write failure is not fatal for the pilot.
            let _ = write_json(&self.path, &snapshot);
        }
    }

    pub fn out_for(&self, bid: &str) -> Option<Value> {
        guard(&self.out).get(bid).cloned()
    }

    pub fn apply(&self, sup: &Supervisor, bid: &str, rec_id: &str) -> String {
        let recs = guard(&self.out)
            .get(bid)
            .and_then(|v| v.get("recs"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let Some(r) = recs.iter().find(|x| x.s("id") == rec_id) else {
            return "recommendation no longer valid".to_string();
        };
        let action = r.get("action").cloned().unwrap_or(Value::Null);
        if !action.is_object() {
            return "recommendation no longer valid".to_string();
        }
        let mut d = action;
        if let Some(o) = d.as_object_mut() {
            o.insert("confirm".to_string(), json!(true));
        }
        sup.act(&d)
    }
}

fn mode_of(cfg: &Value) -> String {
    match cfg.os("pilot").unwrap_or_default().as_str() {
        "off" | "act" => cfg.s("pilot"),
        _ => "advise".to_string(),
    }
}

fn score_of(m: &Value) -> i64 {
    let mut s = 100.0_f64;
    let tpm = m.f("tpm").unwrap_or(0.0);
    let usual = m.f("tpm_usual").unwrap_or(0.0);
    if usual != 0.0 && tpm > 2.0 * usual {
        s -= (10.0 * tpm / usual - 10.0).min(25.0);
    }
    if let Some(c) = m.f("cache_pct") {
        if m.f("tok_h").unwrap_or(0.0) > 20000.0 && c < 60.0 {
            s -= (60.0 - c) / 2.0;
        }
    }
    if let Some(b) = m.f("budget") {
        if b > 0.6 {
            s -= ((b - 0.6) * 90.0).min(35.0);
        }
    }
    if let Some(r) = m.f("risk") {
        s -= 30.0 * r.min(1.0);
    }
    s.round().clamp(0.0, 100.0) as i64
}

#[allow(clippy::too_many_arguments)]
fn recommendations(
    bid: &str,
    kind: &str,
    ladder: &[String],
    model: Option<&str>,
    effort: Option<&str>,
    m: &Value,
    subs: &[Value],
    pol: &Value,
) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    let model = model.unwrap_or("");
    let effort = effort.unwrap_or("");
    let cheaper = match ladder.iter().position(|x| x == model) {
        Some(ix) if ix + 1 < ladder.len() => Some(ladder[ix + 1].clone()),
        _ => None,
    };
    let risk = m.f("risk").unwrap_or(0.0);
    let budget = m.f("budget").unwrap_or(0.0);
    let pressure = risk >= 0.7 || budget >= 0.7;
    let tpm = m.f("tpm").unwrap_or(0.0);
    let tpm_usual = m.f("tpm_usual").unwrap_or(0.0);
    let burning = tpm_usual != 0.0 && tpm > 2.0 * tpm_usual;

    if pressure {
        if let Some(c) = &cheaper {
            out.push(json!({
                "id": format!("model:{c}"),
                "level": "warning",
                "text": format!("Budget/quota under pressure: switch to {c}"),
                "action": {"a": "set", "brain": bid, "field": "model", "value": c},
                "saves": true
            }));
        }
    }
    if let Some(ix) = EFFORTS.iter().position(|e| *e == effort) {
        if ix >= 2 && (pressure || burning) {
            let lower = EFFORTS[ix - 1];
            out.push(json!({
                "id": format!("effort:{lower}"),
                "level": "info",
                "text": format!("Burning faster than usual: effort {effort} → {lower}"),
                "action": {"a": "set", "brain": bid, "field": "effort", "value": lower},
                "saves": true
            }));
        }
    }
    if let Some(c) = m.f("cache_pct") {
        if m.f("tok_h").unwrap_or(0.0) > 50000.0 && c < 40.0 {
            out.push(json!({
                "id": "cache",
                "level": "info",
                "action": null,
                "saves": false,
                "text": format!("Low cache reuse ({c:.0}%): long sessions reuse context, restarts and model switches waste it")
            }));
        }
    }

    if kind == "claude_code" {
        let mut by: Vec<(String, Agg)> = vec![];
        for s in subs {
            let typ = s
                .os("type")
                .unwrap_or_else(|| "general-purpose".to_string());
            let ix = match by.iter().position(|(k, _)| *k == typ) {
                Some(ix) => ix,
                None => {
                    by.push((
                        typ.clone(),
                        Agg {
                            n: 0,
                            tok: 0,
                            models: vec![],
                        },
                    ));
                    by.len() - 1
                }
            };
            let a = &mut by[ix].1;
            a.n += 1;
            a.tok += s.get("tokens").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
            a.models.push(s.os("model").unwrap_or_default());
        }
        let small = ladder.last().cloned();
        for (typ, a) in &by {
            let avg = a.tok as f64 / a.n.max(1) as f64;
            let top = ladder
                .first()
                .map(|first| a.models.iter().any(|mm| mm == first))
                .unwrap_or(false);
            let cur_model = pol
                .get(typ.as_str())
                .and_then(|p| p.get("model"))
                .and_then(|v| v.as_str());
            if top && avg < SUB_CHEAP_TOKENS {
                if let Some(sm) = &small {
                    if cur_model != Some(sm.as_str()) {
                        out.push(json!({
                            "id": format!("sub:{typ}:{sm}"),
                            "level": "info",
                            "saves": true,
                            "text": format!(
                                "Subagents '{typ}' stay small (~{:.0}k tokens) on the top model: use {sm}",
                                avg / 1000.0
                            ),
                            "action": {"a": "subagent_policy", "brain": bid, "type": typ, "model": sm}
                        }));
                    }
                }
            }
        }
        let parallel = pol.get("*").and_then(|p| p.get("max_parallel"));
        if pressure && !truthy(parallel) {
            out.push(json!({
                "id": "sub:*:parallel",
                "level": "info",
                "saves": true,
                "text": "Limit subagents to 2 in parallel while the budget is tight",
                "action": {"a": "subagent_policy", "brain": bid, "type": "*", "max_parallel": 2}
            }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_advisor(tag: &str) -> Advisor {
        let p =
            std::env::temp_dir().join(format!("mb-advisor-{}-{}.json", std::process::id(), tag));
        let _ = std::fs::remove_file(&p);
        Advisor::new(Some(p))
    }

    #[test]
    fn mode_defaults_to_advise() {
        assert_eq!(mode_of(&json!({})), "advise");
        assert_eq!(mode_of(&json!({"pilot": ""})), "advise");
        assert_eq!(mode_of(&json!({"pilot": "whatever"})), "advise");
        assert_eq!(mode_of(&json!({"pilot": "off"})), "off");
        assert_eq!(mode_of(&json!({"pilot": "act"})), "act");
    }

    #[test]
    fn score_penalises_pressure_and_low_cache() {
        let calm = json!({"tpm": 100, "tpm_usual": 100, "cache_pct": 80.0, "tok_h": 60000, "budget": 0.2, "risk": 0.0});
        assert_eq!(score_of(&calm), 100);
        let hot = json!({"tpm": 500, "tpm_usual": 100, "cache_pct": 20.0, "tok_h": 60000, "budget": 0.9, "risk": 0.8});
        // 100 - 25 (tpm) - 20 (cache) - 27 (budget) - 24 (risk)
        assert_eq!(score_of(&hot), 4);
    }

    #[test]
    fn learn_moves_the_baseline_with_ewma() {
        let a = temp_advisor("learn");
        a.learn_update("b1", 100.0, Some(50.0));
        a.learn_update("b1", 200.0, Some(70.0));
        let hist = guard(&a.learned);
        let e = hist.get("b1").cloned().unwrap_or_default();
        assert!((e.f("tpm").unwrap_or(0.0) - 110.0).abs() < 1e-9);
        assert!((e.f("cache").unwrap_or(0.0) - 52.0).abs() < 1e-9);
    }

    #[test]
    fn pressure_recommends_cheaper_model_and_lower_effort() {
        let ladder = vec![
            "opus".to_string(),
            "sonnet".to_string(),
            "haiku".to_string(),
        ];
        let m = json!({"tpm": 100, "tpm_usual": 100, "cache_pct": 80.0, "tok_h": 1000, "budget": 0.8, "risk": 0.75});
        let recs = recommendations(
            "b1",
            "anthropic",
            &ladder,
            Some("opus"),
            Some("high"),
            &m,
            &[],
            &json!({}),
        );
        let ids: Vec<String> = recs.iter().map(|r| r.s("id")).collect();
        assert!(ids.contains(&"model:sonnet".to_string()), "{ids:?}");
        assert!(ids.contains(&"effort:medium".to_string()), "{ids:?}");
    }

    #[test]
    fn small_subagents_on_the_top_model_get_a_cheaper_one() {
        let ladder = vec!["opus".to_string(), "haiku".to_string()];
        let m = json!({"tpm": 10, "tpm_usual": 10, "cache_pct": 90.0, "tok_h": 1000, "budget": 0.1, "risk": 0.0});
        let subs = vec![
            json!({"type": "explore", "tokens": 5000, "model": "opus"}),
            json!({"type": "explore", "tokens": 7000, "model": "opus"}),
        ];
        let recs = recommendations(
            "b1",
            "claude_code",
            &ladder,
            Some("opus"),
            Some("medium"),
            &m,
            &subs,
            &json!({}),
        );
        let ids: Vec<String> = recs.iter().map(|r| r.s("id")).collect();
        assert!(ids.contains(&"sub:explore:haiku".to_string()), "{ids:?}");
        // No spending pressure here, so no parallel-limit recommendation.
        assert!(!ids.contains(&"sub:*:parallel".to_string()), "{ids:?}");
    }
}
