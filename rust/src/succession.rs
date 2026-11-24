//! Succession: when the brain in command must hand over, and to whom.
//!
//! The order is the user's command chain (`priority`, edited on Home). This module adds the
//! reasons to step down besides quota: provider errors (rate limit, overload, auth), a task
//! that stopped answering, and a session grown so long that every turn re-reads a huge context.
//! Pure decision functions live here (tested); `core` feeds them live data every tick.
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// Errors older than this no longer keep a brain out of command.
pub const ERROR_TTL_S: f64 = 15.0 * 60.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Chain {
    pub order: Vec<String>,
    pub threshold_pct: f64,
    pub stall_minutes: u64,
    pub handoff_tokens: u64,
}

impl Chain {
    /// Read `"succession": {...}` (with defaults) and the user's `priority` order.
    /// With no order configured: claude_code brains first, then the rest, in config order.
    pub fn from_cfg(v: &Value) -> Chain {
        let s = v.get("succession").cloned().unwrap_or(Value::Null);
        let num_pos = |k: &str, d: f64| {
            s.get(k)
                .and_then(|x| x.as_f64())
                .filter(|x| x.is_finite() && *x > 0.0)
                .unwrap_or(d)
        };
        let num_nn = |k: &str, d: f64| {
            s.get(k)
                .and_then(|x| x.as_f64())
                .filter(|x| x.is_finite() && *x >= 0.0)
                .unwrap_or(d)
        };
        let mut order: Vec<String> = s
            .get("order")
            .or_else(|| v.get("priority"))
            .and_then(|o| o.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if order.is_empty() {
            let brains = v
                .get("brains")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default();
            let id = |b: &Value| {
                b.get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let is_cc = |b: &Value| b.get("kind").and_then(|x| x.as_str()) == Some("claude_code");
            order.extend(brains.iter().filter(|b| is_cc(b)).map(id));
            order.extend(brains.iter().filter(|b| !is_cc(b)).map(id));
            order.retain(|x| !x.is_empty());
        }
        let mut seen = HashSet::new();
        order.retain(|x| seen.insert(x.clone()));
        Chain {
            order,
            threshold_pct: num_pos("threshold_pct", 90.0).min(100.0),
            stall_minutes: num_nn("stall_minutes", 6.0) as u64,
            handoff_tokens: num_nn("handoff_tokens", 120_000.0) as u64,
        }
    }

    pub fn params(&self) -> Value {
        json!({"threshold_pct": self.threshold_pct, "stall_minutes": self.stall_minutes, "handoff_tokens": self.handoff_tokens})
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reason {
    Quota(f64),
    Error(String),
    Stalled(u64),
    Size(u64),
}

impl Reason {
    pub fn label(&self, lang: &str) -> String {
        let en = lang == "en";
        match self {
            Reason::Quota(p) => {
                if en {
                    format!("quota at {p:.0} %")
                } else {
                    format!("cuota al {p:.0} %")
                }
            }
            Reason::Error(e) => format!("error: {e}"),
            Reason::Stalled(m) => {
                if en {
                    format!("no answer for {m} min")
                } else {
                    format!("sin respuesta hace {m} min")
                }
            }
            Reason::Size(t) => {
                let k = (*t as f64 / 1000.0).round() as u64;
                if en {
                    format!("very long session ({k}k tokens)")
                } else {
                    format!("sesión muy larga ({k}k tokens)")
                }
            }
        }
    }
}

/// Short Spanish label for errors that mean "this brain cannot work right now".
pub fn classify_error(text: &str) -> Option<String> {
    let t = text.to_lowercase();
    let has = |xs: &[&str]| xs.iter().any(|x| t.contains(x));
    if has(&[
        "usage limit",
        "rate limit",
        "rate_limit",
        "ratelimit",
        "429",
        "too many requests",
        "quota",
        "límite de uso",
    ]) {
        Some("límite de uso".into())
    } else if has(&[
        "overloaded",
        "529",
        "503",
        "service unavailable",
        "sobrecarga",
    ]) {
        Some("servicio sobrecargado".into())
    } else if has(&[
        "401",
        "unauthorized",
        "invalid api key",
        "authentication",
        "not signed in",
        "sin sesión",
        "oauth token has expired",
    ]) {
        Some("sin autorización".into())
    } else {
        None
    }
}

/// The next available brain after `current` in the chain, wrapping around; never `current`.
pub fn next_in_chain(
    chain: &Chain,
    current: &str,
    available: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let n = chain.order.len();
    if n == 0 {
        return None;
    }
    let start = chain
        .order
        .iter()
        .position(|x| x == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    (0..n)
        .map(|k| &chain.order[(start + k) % n])
        .find(|id| id.as_str() != current && available(id))
        .cloned()
}

/// Why `current` should hand over, if it should. Priority: Error > Quota > Stalled (only while
/// busy) > Size.
pub fn decide(
    chain: &Chain,
    _current: &str,
    quota_pct: Option<f64>,
    last_error: Option<&str>,
    idle_secs: u64,
    busy: bool,
    est_tokens: u64,
) -> Option<Reason> {
    if let Some(e) = last_error.and_then(classify_error) {
        return Some(Reason::Error(e));
    }
    if let Some(p) = quota_pct.filter(|p| *p >= chain.threshold_pct) {
        return Some(Reason::Quota(p));
    }
    if busy && chain.stall_minutes > 0 && idle_secs >= chain.stall_minutes * 60 {
        return Some(Reason::Stalled(idle_secs / 60));
    }
    if chain.handoff_tokens > 0 && est_tokens >= chain.handoff_tokens {
        return Some(Reason::Size(est_tokens));
    }
    None
}

/// Rough token count of a transcript from its size on disk.
pub fn estimate_tokens(transcript_bytes: u64) -> u64 {
    (transcript_bytes as f64 / 3.6) as u64
}

// ---------------------------------------------------------------- live state

#[derive(Default)]
struct Live {
    /// brain id -> (ts, label)
    errors: HashMap<String, (f64, String)>,
    /// sessions already handed over for size (so we warn once)
    sized: HashSet<String>,
    master: Option<String>,
}

fn live() -> &'static Mutex<Live> {
    static L: OnceLock<Mutex<Live>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Live::default()))
}

/// Remember a provider error seen for a brain (from a task, a hook or a proxy call).
pub fn note_error(brain: &str, text: &str, ts: f64) {
    if brain.is_empty() {
        return;
    }
    if let Some(label) = classify_error(text) {
        let mut l = live().lock().unwrap_or_else(|e| e.into_inner());
        l.errors.insert(brain.to_string(), (ts, label));
    }
}

/// A recent blocking error for this brain, if any.
pub fn recent_error(brain: &str, now: f64) -> Option<String> {
    let l = live().lock().unwrap_or_else(|e| e.into_inner());
    l.errors
        .get(brain)
        .filter(|(ts, _)| now - ts < ERROR_TTL_S)
        .map(|(_, e)| e.clone())
}

pub fn clear_error(brain: &str) {
    let mut l = live().lock().unwrap_or_else(|e| e.into_inner());
    l.errors.remove(brain);
}

/// Record who is in command; returns the previous master when it changed.
pub fn swap_master(new: Option<&str>) -> Option<Option<String>> {
    let mut l = live().lock().unwrap_or_else(|e| e.into_inner());
    let new = new.map(String::from);
    if l.master == new {
        return None;
    }
    Some(std::mem::replace(&mut l.master, new))
}

/// True the first time a session crosses the size threshold.
pub fn mark_sized(session: &str) -> bool {
    let mut l = live().lock().unwrap_or_else(|e| e.into_inner());
    if l.sized.len() > 5000 {
        l.sized.clear();
    }
    l.sized.insert(session.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(order: &[&str]) -> Chain {
        Chain {
            order: order.iter().map(|s| s.to_string()).collect(),
            threshold_pct: 90.0,
            stall_minutes: 6,
            handoff_tokens: 120_000,
        }
    }

    #[test]
    fn from_cfg_reads_params_and_defaults() {
        let c = Chain::from_cfg(
            &json!({"succession": {"threshold_pct": 85, "stall_minutes": 4}, "priority": ["B", "A"]}),
        );
        assert_eq!(c.order, vec!["B", "A"]);
        assert_eq!(c.threshold_pct, 85.0);
        assert_eq!(c.stall_minutes, 4);
        assert_eq!(c.handoff_tokens, 120_000);
        let d = Chain::from_cfg(
            &json!({"brains": [{"id": "DS", "kind": "openai"}, {"id": "A", "kind": "claude_code"}, {"id": "A", "kind": "claude_code"}]}),
        );
        assert_eq!(d.order, vec!["A", "DS"]);
        assert_eq!(d.threshold_pct, 90.0);
    }

    #[test]
    fn classifies_errors() {
        assert_eq!(
            classify_error("Claude AI usage limit reached|1732"),
            Some("límite de uso".into())
        );
        assert_eq!(
            classify_error("HTTP 429 Too Many Requests"),
            Some("límite de uso".into())
        );
        assert_eq!(
            classify_error("API Error: 529 Overloaded"),
            Some("servicio sobrecargado".into())
        );
        assert_eq!(
            classify_error("401 Unauthorized"),
            Some("sin autorización".into())
        );
        assert_eq!(classify_error("file not found"), None);
    }

    #[test]
    fn next_wraps_and_skips_unavailable_and_current() {
        let c = chain(&["A", "B", "G", "DS"]);
        let all = |_: &str| true;
        assert_eq!(next_in_chain(&c, "A", &all), Some("B".into()));
        assert_eq!(next_in_chain(&c, "DS", &all), Some("A".into()));
        let no_b = |x: &str| x != "B";
        assert_eq!(next_in_chain(&c, "A", &no_b), Some("G".into()));
        let only_a = |x: &str| x == "A";
        assert_eq!(next_in_chain(&c, "A", &only_a), None);
        assert_eq!(next_in_chain(&c, "X", &all), Some("A".into()));
        assert_eq!(next_in_chain(&chain(&[]), "A", &all), None);
    }

    #[test]
    fn decide_priority_order() {
        let c = chain(&["A", "B"]);
        assert_eq!(
            decide(&c, "A", Some(95.0), Some("429"), 999, true, 999_999),
            Some(Reason::Error("límite de uso".into()))
        );
        assert_eq!(
            decide(&c, "A", Some(95.0), None, 999, true, 999_999),
            Some(Reason::Quota(95.0))
        );
        assert_eq!(
            decide(&c, "A", Some(50.0), None, 420, true, 999_999),
            Some(Reason::Stalled(7))
        );
        assert_eq!(
            decide(&c, "A", Some(50.0), None, 420, false, 130_000),
            Some(Reason::Size(130_000))
        );
        assert_eq!(
            decide(&c, "A", Some(50.0), Some("ok"), 10, true, 1000),
            None
        );
    }

    #[test]
    fn labels_and_estimate() {
        assert_eq!(Reason::Quota(92.4).label("es"), "cuota al 92 %");
        assert_eq!(Reason::Stalled(7).label("es"), "sin respuesta hace 7 min");
        assert_eq!(
            Reason::Size(130_400).label("es"),
            "sesión muy larga (130k tokens)"
        );
        assert_eq!(
            Reason::Error("límite de uso".into()).label("es"),
            "error: límite de uso"
        );
        assert_eq!(estimate_tokens(360_000), 100_000);
    }

    #[test]
    fn live_errors_expire_and_master_swaps() {
        note_error("zz", "429 rate limit", 1000.0);
        assert_eq!(recent_error("zz", 1100.0), Some("límite de uso".into()));
        assert_eq!(recent_error("zz", 1000.0 + ERROR_TTL_S + 1.0), None);
        note_error("zz", "harmless", 2000.0);
        clear_error("zz");
        assert_eq!(recent_error("zz", 2001.0), None);
        assert!(mark_sized("s-1"));
        assert!(!mark_sized("s-1"));
    }
}
