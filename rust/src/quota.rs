//! Adaptive quota predictor (port of metabrain/quota.py).
//!
//! Learns how fast an account burns its quota window: an EWMA burn rate, a
//! per-hour-of-day profile and a variance-driven warning margin. Everything is
//! persisted to `quota_model.json`, so the model keeps learning across restarts.

use crate::util::{data_dir, local_hour, now as unix_now, read_json, write_json};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Weight of the newest burn-rate measurement.
pub const ALPHA: f64 = 0.35;
/// How fast the hour-of-day profile adapts.
pub const PROFILE_ALPHA: f64 = 0.15;
pub const MIN_LEAD: f64 = 6.0;
pub const MAX_LEAD: f64 = 40.0;

pub struct Predictor {
    pub path: PathBuf,
    m: Mutex<Value>,
}

impl Predictor {
    pub fn new(path: Option<PathBuf>) -> Predictor {
        let path = path.unwrap_or_else(|| data_dir().join("quota_model.json"));
        let m = match read_json(&path) {
            Some(v @ Value::Object(_)) => v,
            _ => json!({}),
        };
        Predictor {
            path,
            m: Mutex::new(m),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Value> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Feed one reading of the session (5 h) percentage.
    pub fn add(&self, key: &str, pct: Option<f64>, reset: Option<f64>, now: Option<f64>) {
        let Some(pct) = pct else { return };
        let now = now.unwrap_or_else(unix_now);
        let mut guard = self.lock();
        let persist;
        {
            let m = ensure_obj(&mut guard);
            let a = acct_mut(m, key);

            let last = last_of(a);
            let last_raw = a.get("last").cloned();
            if let Some(o) = a.as_object_mut() {
                o.insert("last".into(), json!([now, pct, reset]));
            }

            let Some((last_t, last_pct, _)) = last else {
                return;
            };
            let dt = (now - last_t) / 60.0;
            if dt < 0.5 {
                // Too close to the previous sample: keep the old reading.
                if let (Some(o), Some(raw)) = (a.as_object_mut(), last_raw) {
                    o.insert("last".into(), raw);
                }
                return;
            }
            if pct + 0.01 < last_pct {
                // The quota window was reset.
                if last_pct >= 99.0 {
                    push_exhausted(a, last_t);
                }
                return;
            }

            let r = ((pct - last_pct) / dt).max(0.0);
            let rate = f_of(a, "rate");
            let err = r - rate;
            set_f(a, "rate", rate + ALPHA * err);
            set_f(
                a,
                "var",
                (1.0 - ALPHA) * (f_of(a, "var") + ALPHA * err * err),
            );

            let h = local_hour(now);
            let p = profile_get(a, h);
            profile_set(a, h, p + PROFILE_ALPHA * (r - p));

            let samples = i_of(a, "samples") + 1;
            set_i(a, "samples", samples);
            persist = samples % 5 == 0;
        }
        let snapshot = if persist { Some(guard.clone()) } else { None };
        drop(guard);
        if let Some(snap) = snapshot {
            let _ = write_json(&self.path, &snap);
        }
    }

    /// -> JSON object with pct, rate, eta_min, lead_min, risk 0..1, why, reset, samples.
    pub fn forecast(&self, key: &str, now: Option<f64>) -> Option<Value> {
        let now = now.unwrap_or_else(unix_now);
        let guard = self.lock();
        let a = guard.get(key)?;
        let (_, pct, reset) = last_of(a)?;

        let rate = f_of(a, "rate");
        let h = local_hour(now);
        let expected = rate
            .max(0.6 * profile_get(a, h))
            .max(0.6 * profile_get(a, (h + 1) % 24));
        let sd = f_of(a, "var").max(0.0).sqrt();
        let mut lead = (8.0 + 30.0 * sd / expected.max(0.05)).clamp(MIN_LEAD, MAX_LEAD);
        let samples = i_of(a, "samples");
        if samples < 4 {
            // Not enough history yet: be careful.
            lead = lead.max(15.0);
        }
        let left = (100.0 - pct).max(0.0);
        let mut eta = if expected > 0.001 {
            Some(left / expected)
        } else {
            None
        };
        if let (Some(e), Some(r)) = (eta, reset) {
            // Python treats a reset timestamp of 0 as "unknown".
            if r != 0.0 && e > (r - now) / 60.0 {
                // The window resets before we would run out.
                eta = None;
            }
        }
        let risk = if pct >= 98.0 {
            1.0
        } else {
            eta.map(|e| (lead / e.max(0.1)).clamp(0.0, 1.0))
                .unwrap_or(0.0)
        };
        let why = match eta {
            None => format!("{pct:.0}% used, steady"),
            Some(e) => format!(
                "{pct:.0}% used, burning {expected:.2} pts/min: about {e:.0} min left (margin {lead:.0} min)"
            ),
        };
        Some(json!({
            "pct": pct,
            "rate": expected,
            "eta_min": eta,
            "lead_min": lead,
            "risk": risk,
            "why": why,
            "reset": reset,
            "samples": samples,
        }))
    }

    pub fn note_handoff(&self, key: &str) {
        let snapshot = {
            let mut guard = self.lock();
            {
                let m = ensure_obj(&mut guard);
                let a = acct_mut(m, key);
                let n = i_of(a, "handoffs") + 1;
                set_i(a, "handoffs", n);
            }
            guard.clone()
        };
        let _ = write_json(&self.path, &snapshot);
    }
}

// ---------------------------------------------------------------- helpers

fn acct_default() -> Value {
    let mut profile = Vec::with_capacity(24);
    for _ in 0..24 {
        profile.push(json!(0.0));
    }
    json!({
        "last": null,
        "rate": 0.0,
        "var": 0.0,
        "profile": profile,
        "samples": 0,
        "exhausted_at": [],
        "handoffs": 0,
    })
}

fn ensure_obj(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    match v {
        Value::Object(m) => m,
        _ => unreachable!("value was just made an object"),
    }
}

/// Python's `self.m.setdefault(key, {...})` (existing values are kept).
fn acct_mut<'a>(m: &'a mut Map<String, Value>, key: &str) -> &'a mut Value {
    let e = m.entry(key.to_string()).or_insert_with(acct_default);
    if !e.is_object() {
        *e = acct_default();
    }
    e
}

fn last_of(a: &Value) -> Option<(f64, f64, Option<f64>)> {
    let arr = a.get("last")?.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    let t = arr[0].as_f64()?;
    let p = arr[1].as_f64()?;
    let r = arr.get(2).and_then(|v| v.as_f64());
    Some((t, p, r))
}

fn f_of(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn i_of(v: &Value, k: &str) -> i64 {
    match v.get(k) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

fn set_f(a: &mut Value, k: &str, val: f64) {
    if let Some(o) = a.as_object_mut() {
        o.insert(k.to_string(), json!(val));
    }
}

fn set_i(a: &mut Value, k: &str, val: i64) {
    if let Some(o) = a.as_object_mut() {
        o.insert(k.to_string(), json!(val));
    }
}

fn profile_get(a: &Value, i: usize) -> f64 {
    a.get("profile")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.get(i))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

fn profile_set(a: &mut Value, i: usize, val: f64) {
    if let Some(o) = a.as_object_mut() {
        let p = o
            .entry("profile")
            .or_insert_with(|| Value::Array(vec![json!(0.0); 24]));
        if !p.is_array() {
            *p = Value::Array(vec![json!(0.0); 24]);
        }
        if let Some(arr) = p.as_array_mut() {
            while arr.len() < 24 {
                arr.push(json!(0.0));
            }
            if let Some(slot) = arr.get_mut(i) {
                *slot = json!(val);
            }
        }
    }
}

fn push_exhausted(a: &mut Value, ts: f64) {
    if let Some(o) = a.as_object_mut() {
        let e = o.entry("exhausted_at").or_insert_with(|| json!([]));
        if !e.is_array() {
            *e = json!([]);
        }
        if let Some(arr) = e.as_array_mut() {
            arr.push(json!(ts));
            while arr.len() > 20 {
                arr.remove(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Predictor, MAX_LEAD, MIN_LEAD};
    use crate::util::read_json;
    use serde_json::Value;
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!(
            "metabrain_quota_test_{tag}_{}_{nanos}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn first_reading_only_records_state() {
        let path = tmp_path("first");
        let p = Predictor::new(Some(path.clone()));
        p.add("acct", Some(10.0), None, Some(1_000.0));
        // Like Python: a forecast exists after the first reading, but there is no rate yet.
        let f = p
            .forecast("acct", Some(1_060.0))
            .expect("forecast after first reading");
        assert!(f["eta_min"].is_null());
        assert!(p.forecast("missing", Some(1_060.0)).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn burn_rate_and_forecast() {
        let path = tmp_path("burn");
        let p = Predictor::new(Some(path.clone()));
        p.add("acct", Some(10.0), Some(9_000.0), Some(1_000.0));
        p.add("acct", Some(12.0), Some(9_000.0), Some(1_060.0)); // 2 pts in 1 min
        let f = p.forecast("acct", Some(1_060.0)).expect("forecast");
        let rate = f.get("rate").and_then(Value::as_f64).unwrap_or(f64::NAN);
        assert!((rate - 0.7).abs() < 1e-9, "rate {rate}");
        let eta = f.get("eta_min").and_then(Value::as_f64).unwrap_or(f64::NAN);
        assert!((eta - 88.0 / 0.7).abs() < 1e-6, "eta {eta}");
        let lead = f.get("lead_min").and_then(Value::as_f64).unwrap_or(0.0);
        assert!((MIN_LEAD..=MAX_LEAD).contains(&lead), "lead {lead}");
        let risk = f.get("risk").and_then(Value::as_f64).unwrap_or(0.0);
        assert!(risk > 0.0 && risk < 1.0, "risk {risk}");
        assert_eq!(f.get("samples").and_then(Value::as_i64), Some(1));
        assert_eq!(f.get("reset").and_then(Value::as_f64), Some(9_000.0));
        let why = f.get("why").and_then(Value::as_str).unwrap_or("");
        assert!(why.contains("pts/min"), "why {why}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn window_reset_does_not_lower_rate() {
        let path = tmp_path("reset");
        let p = Predictor::new(Some(path.clone()));
        p.add("acct", Some(20.0), None, Some(0.0));
        p.add("acct", Some(21.0), None, Some(60.0)); // 1 pt/min -> rate 0.35
        p.add("acct", Some(2.0), None, Some(120.0)); // window reset
        p.note_handoff("acct"); // force a persist
        let f = p.forecast("acct", Some(120.0)).expect("forecast");
        assert_eq!(f.get("pct").and_then(Value::as_f64), Some(2.0));
        let rate = f.get("rate").and_then(Value::as_f64).unwrap_or(0.0);
        assert!((rate - 0.35).abs() < 1e-9, "rate {rate}");
        let saved = read_json(&path).expect("saved file");
        let samples = saved
            .get("acct")
            .and_then(|a| a.get("samples"))
            .and_then(Value::as_i64);
        assert_eq!(samples, Some(1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exhausted_marks_when_previous_pct_was_full() {
        let path = tmp_path("exhausted");
        let p = Predictor::new(Some(path.clone()));
        p.add("acct", Some(50.0), None, Some(0.0));
        p.add("acct", Some(99.5), None, Some(60.0));
        p.add("acct", Some(1.0), None, Some(120.0)); // reset after being full
        p.note_handoff("acct");
        let saved = read_json(&path).expect("saved");
        let ex = saved
            .get("acct")
            .and_then(|a| a.get("exhausted_at"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].as_f64(), Some(60.0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn handoff_increments_and_persists() {
        let path = tmp_path("handoff");
        let p = Predictor::new(Some(path.clone()));
        p.note_handoff("acct");
        p.note_handoff("acct");
        let saved = read_json(&path).expect("saved");
        let acct = saved.get("acct").expect("account");
        assert_eq!(acct.get("handoffs").and_then(Value::as_i64), Some(2));
        let profile_len = acct
            .get("profile")
            .and_then(Value::as_array)
            .map(|a| a.len());
        assert_eq!(profile_len, Some(24));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn flat_usage_has_no_eta() {
        let path = tmp_path("flat");
        let p = Predictor::new(Some(path.clone()));
        p.add("acct", Some(10.0), None, Some(0.0));
        p.add("acct", Some(10.0), None, Some(60.0));
        let f = p.forecast("acct", Some(60.0)).expect("forecast");
        assert!(f.get("eta_min").map(|v| v.is_null()).unwrap_or(false));
        assert_eq!(f.get("risk").and_then(Value::as_f64), Some(0.0));
        let why = f.get("why").and_then(Value::as_str).unwrap_or("");
        assert!(why.ends_with("steady"), "why {why}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn nearly_exhausted_is_high_risk() {
        let path = tmp_path("risk");
        let p = Predictor::new(Some(path.clone()));
        p.add("acct", Some(97.0), None, Some(0.0));
        p.add("acct", Some(98.5), None, Some(60.0));
        let f = p.forecast("acct", Some(60.0)).expect("forecast");
        assert_eq!(f.get("risk").and_then(Value::as_f64), Some(1.0));
        let _ = std::fs::remove_file(&path);
    }
}
