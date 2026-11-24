//! Observed usage and savings estimates from session relays and routing.
//!
//! Every Claude Code turn re-reads the whole conversation (mostly as cache reads), so a turn in a
//! long session costs far more than the same turn in a fresh one. This module reads the session
//! transcripts and reports, from the user's own data:
//!   * tokens per turn by day (the trend),
//!   * each session's turns, total and average per turn,
//!   * for every session that started from a MetaBrain relay, its cost per turn versus the session
//!     it replaced, and the tokens that difference saved.
use crate::router::OPUS_COST;
use crate::util::{expand_s, J};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Typed subset of a saved task metadata file (`tasks/<id>.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskRecord {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub brain: String,
    #[serde(default)]
    pub brain_name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, deserialize_with = "token_count")]
    pub tokens: u64,
    #[serde(default)]
    pub created: f64,
    #[serde(default)]
    pub updated: f64,
    #[serde(default)]
    pub route_cand: String,
}

fn token_count<'de, D: serde::Deserializer<'de>>(value: D) -> Result<u64, D::Error> {
    let value = Value::deserialize(value)?;
    Ok(value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| *n >= 0.0 && n.fract() == 0.0)
                .map(|n| n as u64)
        })
        .unwrap_or(0))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TierBreakdown {
    pub tier: String,
    pub tasks: u64,
    pub tokens: u64,
    pub saved_tokens: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RoutingSummary {
    pub baseline: String,
    pub tasks: u64,
    pub routed_tasks: u64,
    pub actual_tokens: u64,
    pub saved_tokens: u64,
    pub saved_pct: f64,
    pub by_tier: Vec<TierBreakdown>,
}

/// Relative cost weight in `[0.0, 1.0]` versus the flagship `opus:high` baseline (`OPUS_COST`).
pub fn model_effort_weight(
    kind: &str,
    model: &str,
    effort: &str,
    route_cand: &str,
) -> (f64, &'static str) {
    let mut cand_model = "";
    let mut cand_effort = "";
    if !route_cand.trim().is_empty() {
        let mut parts = route_cand.trim().split(':');
        let _fam = parts.next();
        cand_model = parts.next().unwrap_or("").trim();
        cand_effort = parts.next().unwrap_or("").trim();
    }
    let m_raw = if !model.trim().is_empty() {
        model.trim()
    } else {
        cand_model
    };
    let e_raw = if !effort.trim().is_empty() {
        effort.trim()
    } else {
        cand_effort
    };
    let m = m_raw.to_lowercase();
    let e = e_raw.to_lowercase();
    let k = kind.trim().to_lowercase();

    let (base_cost, tier) = if m.contains("flash")
        || m.contains("mini")
        || m.contains("lite")
        || m.contains("nano")
    {
        (1.0, "ligero")
    } else if m.contains("deepseek") || (k == "openai" && m.is_empty()) {
        (4.0, "economico")
    } else if m.contains("haiku") {
        (8.0, "rapido")
    } else if m.contains("sonnet") || m.contains("sol") || (k == "codex" && m.is_empty()) {
        (30.0, "estandar")
    } else if m.contains("opus") || m.contains("astra") || m.contains("neon") || m.contains("pro") {
        (OPUS_COST, "tope")
    } else if k == "antigravity" || k == "gemini_cli" {
        (30.0, "estandar")
    } else {
        (OPUS_COST, "tope")
    };

    let eff_mult = match e.as_str() {
        "none" | "minimal" | "low" => 0.75,
        "medium" => 0.90,
        _ => 1.0,
    };

    let weight = ((base_cost / OPUS_COST) * eff_mult).clamp(0.0, 1.0);
    (weight, tier)
}

/// Estimate flagship-equivalent cost saved by routing tasks to cheaper models/efforts
/// compared against an always-`opus:high` baseline.
pub fn routing_savings_in(tasks_dir: &Path, days: i64, now_ts: f64) -> RoutingSummary {
    let cutoff = now_ts - (days.max(1) as f64) * 86400.0;
    let mut summary = RoutingSummary {
        baseline: "opus:high".to_string(),
        ..Default::default()
    };
    let mut tiers: BTreeMap<&'static str, (u64, u64, u64)> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(tasks_dir) else {
        return summary;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<TaskRecord>(&raw) else {
            continue;
        };
        if rec.tokens == 0
            || rec.kind == "parallel"
            || matches!(
                rec.status.as_str(),
                "queued" | "failed" | "discarded" | "stopped" | "cancelled"
            )
        {
            continue;
        }
        let ts = rec.updated.max(rec.created);
        if ts > 0.0 && ts < cutoff {
            continue;
        }
        let (weight, tier) =
            model_effort_weight(&rec.kind, &rec.model, &rec.effort, &rec.route_cand);
        let saved = ((1.0 - weight).max(0.0) * rec.tokens as f64).round() as u64;
        summary.tasks += 1;
        if saved > 0 {
            summary.routed_tasks += 1;
        }
        summary.actual_tokens += rec.tokens;
        summary.saved_tokens += saved;
        let e = tiers.entry(tier).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += rec.tokens;
        e.2 += saved;
    }
    if summary.actual_tokens > 0 {
        summary.saved_pct =
            (summary.saved_tokens as f64 * 1000.0 / summary.actual_tokens as f64).round() / 10.0;
    }
    summary.by_tier = tiers
        .into_iter()
        .map(|(tier, (tasks, tokens, saved_tokens))| TierBreakdown {
            tier: tier.to_string(),
            tasks,
            tokens,
            saved_tokens,
        })
        .collect();
    summary
}

pub fn routing_savings(days: i64) -> RoutingSummary {
    routing_savings_in(
        &crate::util::data_dir().join("tasks"),
        days,
        crate::util::now(),
    )
}

/// Marker the relay puts at the start of a session it hands over (see relay::inject).
pub const RELAY_MARK: &str = "## Handoff · ";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub id: String,
    pub account: String,
    pub project: String,
    pub start: f64,
    pub end: f64,
    pub turns: u64,
    pub tokens: u64,
    pub relayed: bool,
    /// (local day "YYYY-MM-DD", turns, tokens) per day the session was active.
    pub days: Vec<(String, u64, u64)>,
}

impl Session {
    pub fn per_turn(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.tokens as f64 / self.turns as f64
        }
    }
}

fn ts_of(v: &Value) -> Option<f64> {
    let s = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis() as f64 / 1000.0)
}

fn day_of(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}

fn user_text(v: &Value) -> Option<String> {
    let c = v.get("message")?.get("content")?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    let arr = c.as_array()?;
    if arr.iter().any(|b| b.s("type") == "tool_result") {
        return None;
    }
    let t: Vec<String> = arr
        .iter()
        .filter(|b| b.s("type") == "text")
        .map(|b| b.s("text"))
        .collect();
    if t.is_empty() {
        None
    } else {
        Some(t.join("\n"))
    }
}

/// Read one transcript. A turn is one assistant API message (deduplicated by message id).
pub fn read_session(path: &Path, account: &str) -> Option<Session> {
    let f = std::fs::File::open(path).ok()?;
    let mut s = Session {
        id: path
            .file_stem()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        account: account.to_string(),
        ..Default::default()
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut days: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut first_user = true;
    for line in BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .take(500_000)
    {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if s.project.is_empty() {
            let cwd = v.s("cwd");
            if !cwd.is_empty() {
                s.project = cwd
                    .trim_end_matches(['/', '\\'])
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&cwd)
                    .to_string();
            }
        }
        let ts = ts_of(&v);
        if let Some(t) = ts {
            if s.start == 0.0 || t < s.start {
                s.start = t;
            }
            s.end = s.end.max(t);
        }
        match v.s("type").as_str() {
            "user" if first_user => {
                if let Some(t) = user_text(&v) {
                    first_user = false;
                    let trimmed = t.trim_start();
                    s.relayed = trimmed.starts_with(RELAY_MARK)
                        || trimmed.starts_with("## Traspaso · ")
                        || t.contains("Continúas el trabajo que dejó")
                        || t.contains("Continuing work from");
                }
            }
            "assistant" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                let Some(u) = m.get("usage") else { continue };
                let mid = m.s("id");
                if !mid.is_empty() && !seen.insert(mid) {
                    continue;
                }
                let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                let tok = g("input_tokens")
                    + g("output_tokens")
                    + g("cache_creation_input_tokens")
                    + g("cache_read_input_tokens");
                s.turns += 1;
                s.tokens += tok;
                if let Some(t) = ts {
                    let e = days.entry(day_of(t)).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += tok;
                }
            }
            _ => {}
        }
    }
    if s.turns == 0 {
        return None;
    }
    s.days = days.into_iter().map(|(d, (a, b))| (d, a, b)).collect();
    Some(s)
}

fn relayed_path() -> PathBuf {
    crate::util::data_dir()
        .join("relay")
        .join("relayed_sessions.txt")
}

/// Remember that a session started from a MetaBrain relay (called by the SessionStart hook).
pub fn mark_relayed(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    use std::io::Write;
    let p = relayed_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let _ = writeln!(f, "{session_id}");
    }
}

fn relayed_ids() -> HashSet<String> {
    std::fs::read_to_string(relayed_path())
        .map(|t| {
            t.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// All sessions of the claude_code brains touched in the last `days` days.
pub fn sessions(brains: &[Value], days: i64) -> Vec<Session> {
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs((days.max(1) as u64) * 86400);
    let mut out = Vec::new();
    let marked = relayed_ids();
    let mut seen_dirs: HashSet<String> = HashSet::new();
    for b in brains.iter().filter(|b| b.s("kind") == "claude_code") {
        let cd = b.os("config_dir").unwrap_or_else(|| "~/.claude".into());
        let dir_key = crate::util::norm(&PathBuf::from(expand_s(&cd)));
        if !seen_dirs.insert(dir_key) {
            continue;
        }
        let root = PathBuf::from(expand_s(&cd)).join("projects");
        let Ok(projects) = std::fs::read_dir(&root) else {
            continue;
        };
        for p in projects.flatten() {
            let Ok(files) = std::fs::read_dir(p.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().map(|x| x != "jsonl").unwrap_or(true) {
                    continue;
                }
                let fresh = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t >= cutoff)
                    .unwrap_or(false);
                if !fresh {
                    continue;
                }
                if let Some(mut s) = read_session(&path, &b.s("name")) {
                    s.relayed = s.relayed || marked.contains(&s.id);
                    out.push(s);
                }
            }
        }
    }
    out.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// For each relayed session: the previous session of the same project (any account), the cost per
/// turn of both, and tokens saved = (before - after) x turns after (never negative).
pub fn relay_pairs(all: &[Session]) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, s) in all.iter().enumerate() {
        if !s.relayed || s.project.is_empty() {
            continue;
        }
        let prev = all[..i]
            .iter()
            .rev()
            .find(|p| p.project == s.project && p.id != s.id && p.turns >= 5);
        let Some(p) = prev else { continue };
        let before = p.per_turn();
        let after = s.per_turn();
        let saved = ((before - after).max(0.0) * s.turns as f64).round() as u64;
        out.push(json!({
            "project": s.project, "when": day_of(s.start),
            "from": {"id": p.id, "account": p.account, "turns": p.turns, "per_turn": before.round()},
            "to": {"id": s.id, "account": s.account, "turns": s.turns, "per_turn": after.round()},
            "saved_tokens": saved,
            "ratio": if before > 0.0 { (after / before * 1000.0).round() / 1000.0 } else { 1.0 },
        }));
    }
    out
}

/// The report the HMI shows (cached 5 minutes: reading transcripts is not free).
pub fn report(brains: &[Value], days: i64) -> Value {
    static CACHE: Mutex<Option<(f64, String, Value)>> = Mutex::new(None);
    let key = format!(
        "{days}|{}|{}",
        crate::util::data_dir().display(),
        brains
            .iter()
            .map(|b| b.s("id"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let t = crate::util::now();
    if let Some((ts, k, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if k == key && t - ts < 300.0 {
            return v;
        }
    }
    let all = sessions(brains, days);
    let mut by_day: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for s in &all {
        for (d, n, tok) in &s.days {
            let e = by_day.entry(d.clone()).or_insert((0, 0));
            e.0 += n;
            e.1 += tok;
        }
    }
    let daily: Vec<Value> = by_day
        .iter()
        .map(|(d, (n, tok))| json!({"day": d, "turns": n, "tokens": tok, "per_turn": if *n > 0 { (*tok as f64 / *n as f64).round() } else { 0.0 }}))
        .collect();
    let pairs = relay_pairs(&all);
    let relay_saved: u64 = pairs
        .iter()
        .map(|p| p["saved_tokens"].as_u64().unwrap_or(0))
        .sum();
    let routing = routing_savings(days);
    let total: u64 = all.iter().map(|s| s.tokens).sum();
    let turns: u64 = all.iter().map(|s| s.turns).sum();
    let mut top: Vec<&Session> = all.iter().collect();
    top.sort_by_key(|s| std::cmp::Reverse(s.tokens));
    let heavy: Vec<Value> = top
        .iter()
        .take(12)
        .map(|s| json!({"id": s.id, "project": s.project, "account": s.account, "turns": s.turns, "tokens": s.tokens,
                        "per_turn": s.per_turn().round(), "share": if total > 0 { (s.tokens as f64 * 1000.0 / total as f64).round() / 10.0 } else { 0.0 },
                        "relayed": s.relayed, "start": s.start}))
        .collect();
    let saved_pct = if total + relay_saved > 0 {
        (relay_saved as f64 * 1000.0 / (total + relay_saved) as f64).round() / 10.0
    } else {
        0.0
    };
    let v = json!({
        "days": days, "sessions": all.len(), "turns": turns, "tokens": total,
        "per_turn": if turns > 0 { (total as f64 / turns as f64).round() } else { 0.0 },
        "daily": daily, "heavy": heavy, "relays": pairs,
        "saved_tokens": relay_saved,
        "relay_saved_tokens": relay_saved,
        "routing_saved_tokens": routing.saved_tokens,
        "routing": routing,
        "saved_pct": saved_pct,
    });
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((t, key, v.clone()));
    v
}

/// CSV of the daily series, relay pairs and routing tiers, for the README or a spreadsheet.
pub fn csv(rep: &Value) -> String {
    let mut out = String::from(
        "tipo,fecha,proyecto,turnos,tokens,tokens_por_turno,antes_por_turno,ahorro_estimado\n",
    );
    for d in rep.arr("daily") {
        out.push_str(&format!(
            "dia,{},,{},{},{},,\n",
            d.s("day"),
            d["turns"],
            d["tokens"],
            d["per_turn"]
        ));
    }
    for p in rep.arr("relays") {
        out.push_str(&format!(
            "relevo,{},{},{},,{},{},{}\n",
            p.s("when"),
            p.s("project").replace(',', " "),
            p["to"]["turns"],
            p["to"]["per_turn"],
            p["from"]["per_turn"],
            p["saved_tokens"]
        ));
    }
    if let Some(by_tier) = rep
        .get("routing")
        .and_then(|r| r.get("by_tier"))
        .and_then(|a| a.as_array())
    {
        for t in by_tier {
            out.push_str(&format!(
                "enrutamiento_equivalente_opus,,{},{},{},,,{}\n",
                t.s("tier"),
                t["tasks"],
                t["tokens"],
                t["saved_tokens"]
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    fn write_session(
        dir: &Path,
        name: &str,
        project: &str,
        start: &str,
        first: &str,
        per_turn: u64,
        turns: u64,
    ) -> PathBuf {
        let mut s = line(
            json!({"type": "user", "cwd": format!("/p/{project}"), "timestamp": start, "message": {"content": first}}),
        );
        for i in 0..turns {
            s += &line(
                json!({"type": "assistant", "timestamp": start, "message": {"id": format!("{name}-{i}"), "usage": {"input_tokens": 10, "output_tokens": 10, "cache_read_input_tokens": per_turn - 20}}}),
            );
            // streamed duplicate of the same API message: must not count twice
            s += &line(
                json!({"type": "assistant", "timestamp": start, "message": {"id": format!("{name}-{i}"), "usage": {"input_tokens": 10, "output_tokens": 10, "cache_read_input_tokens": per_turn - 20}}}),
            );
        }
        let p = dir.join(format!("{name}.jsonl"));
        std::fs::write(&p, s).unwrap();
        p
    }

    #[test]
    fn measures_turns_and_relay_savings() {
        let d = std::env::temp_dir().join(format!("mb_sav_{}", std::process::id()));
        let proj = d.join("projects").join("C--p-demo");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&proj).unwrap();
        write_session(
            &proj,
            "old",
            "demo",
            "2026-11-20T10:00:00Z",
            "arregla el panel",
            500_000,
            40,
        );
        write_session(
            &proj,
            "new",
            "demo",
            "2026-11-21T10:00:00Z",
            "[METABRAIN] Continúas el trabajo que dejó Claude 1",
            60_000,
            30,
        );

        let old = read_session(&proj.join("old.jsonl"), "A").unwrap();
        assert_eq!(old.turns, 40);
        assert_eq!(old.per_turn(), 500_000.0);
        assert!(!old.relayed);

        let brains = vec![
            json!({"id": "A", "name": "Claude 1", "kind": "claude_code", "config_dir": d.to_string_lossy()}),
        ];
        let all = sessions(&brains, 3650);
        assert_eq!(all.len(), 2);
        let pairs = relay_pairs(&all);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0]["saved_tokens"], json!((500_000u64 - 60_000) * 30));
        assert_eq!(pairs[0]["from"]["per_turn"], json!(500_000.0));

        let rep = report(&brains, 3650);
        assert_eq!(rep["turns"], json!(70));
        assert!(rep["saved_pct"].as_f64().unwrap() > 0.0);
        let c = csv(&rep);
        assert!(c.starts_with("tipo,fecha"));
        assert!(c.contains("relevo,"));
    }

    #[test]
    fn measures_routing_savings_against_opus_baseline() {
        let d = std::env::temp_dir().join(format!("mb_route_sav_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let now_ts = 1_795_500_000.0;
        let write_task = |name: &str, v: Value| {
            std::fs::write(d.join(format!("{name}.json")), v.to_string()).unwrap();
        };
        // Cheap DeepSeek flash task -> high savings
        write_task(
            "t1",
            json!({
                "id": "t1", "kind": "openai", "model": "deepseek-flash", "effort": "none",
                "status": "done", "tokens": 100_000, "created": now_ts - 3600.0
            }),
        );
        // Mid-tier sonnet medium -> substantial savings
        write_task(
            "t2",
            json!({
                "id": "t2", "kind": "claude_code", "model": "claude-sonnet-4", "effort": "medium",
                "status": "review", "tokens": 100_000.0, "created": now_ts - 1800.0
            }),
        );
        // Flagship opus high -> zero savings vs baseline
        write_task(
            "t3",
            json!({
                "id": "t3", "kind": "claude_code", "model": "claude-opus-4", "effort": "high",
                "status": "done", "tokens": 100_000, "created": now_ts - 600.0
            }),
        );
        // Old task outside window -> ignored
        write_task(
            "old",
            json!({
                "id": "old", "kind": "openai", "model": "deepseek-flash",
                "status": "done", "tokens": 500_000, "created": now_ts - 30.0 * 86400.0
            }),
        );

        let r = routing_savings_in(&d, 14, now_ts);
        let _ = std::fs::remove_dir_all(&d);

        assert_eq!(r.baseline, "opus:high");
        assert_eq!(r.tasks, 3);
        assert_eq!(r.routed_tasks, 2);
        assert_eq!(r.actual_tokens, 300_000);
        // t1 saves ~99_500, t2 saves 82_000, t3 saves 0 => ~181_500 (~60.5%)
        assert!(
            r.saved_tokens > 180_000 && r.saved_tokens < 183_000,
            "saved={}",
            r.saved_tokens
        );
        assert!(
            r.saved_pct > 60.0 && r.saved_pct < 61.5,
            "pct={}",
            r.saved_pct
        );
        assert!(r.by_tier.iter().any(|t| t.tier == "ligero" && t.tasks == 1));
        assert!(r
            .by_tier
            .iter()
            .any(|t| t.tier == "tope" && t.saved_tokens == 0));
    }
}
