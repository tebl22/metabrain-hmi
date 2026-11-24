//! Automatic account relay for Claude Code brains (port of metabrain/failover.py).
//!
//! Each regulate tick feeds every account's session quota into the Predictor. When the
//! current account is about to run out, live sessions get a handoff and the brain is
//! pointed at the alternative with the most room. "continue" copies one session to
//! another account and resumes it in a terminal there.

use crate::brains::{scanner_for, Brain, Session};
use crate::core::Supervisor;
use crate::handoff::write_handoff;
use crate::quota::Predictor;
use crate::util::{expand, home, norm, now as unix_now, truthy, J};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const SWITCH_RISK: f64 = 1.0;
/// An alternative account must have at least 20 % of its session left.
pub const ALT_MAX_PCT: f64 = 80.0;
pub const LIVE_SECONDS: f64 = 600.0;
/// Switch accounts before the limit, while the session can still finish its thought.
pub const EARLY_SWITCH_PCT: f64 = 85.0;

// ---------------------------------------------------------------- helpers

/// (pct, reset_ts) of the 5 h session window from a Scanner.quota dict.
pub(crate) fn session_limit(quota: &Value) -> (Option<f64>, Option<f64>) {
    let limits = quota
        .get("limits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for l in limits {
        if l.s("group") == "session" {
            if let Some(pct) = l.f("pct") {
                return (Some(pct), l.f("reset"));
            }
        }
    }
    (None, None)
}

fn same(a: &str, b: &str) -> bool {
    let a = if a.trim().is_empty() { "~/.claude" } else { a };
    let b = if b.trim().is_empty() { "~/.claude" } else { b };
    norm(&expand(a)) == norm(&expand(b))
}

fn brain_name(b: &Brain) -> String {
    let cfg = b.cfg();
    cfg.os("name").unwrap_or_else(|| b.id.clone())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

fn set_config_env(c: &mut std::process::Command, target: &str) {
    if same(target, "~/.claude") {
        // The default account keeps ~/.claude.json outside the directory.
        c.env_remove("CLAUDE_CONFIG_DIR");
    } else {
        c.env("CLAUDE_CONFIG_DIR", target);
    }
}

// The supervisor owns the brain list. Accept the common wrappers so this module is
// independent of whether core.rs exposes a Vec, a Mutex or an RwLock.
trait BrainList {
    fn list(&self) -> Vec<Arc<Brain>>;
}

impl BrainList for Vec<Arc<Brain>> {
    fn list(&self) -> Vec<Arc<Brain>> {
        self.clone()
    }
}

impl BrainList for std::sync::RwLock<Vec<Arc<Brain>>> {
    fn list(&self) -> Vec<Arc<Brain>> {
        self.read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

impl BrainList for std::sync::Mutex<Vec<Arc<Brain>>> {
    fn list(&self) -> Vec<Arc<Brain>> {
        self.lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

impl BrainList for Arc<std::sync::RwLock<Vec<Arc<Brain>>>> {
    fn list(&self) -> Vec<Arc<Brain>> {
        self.read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

impl BrainList for Arc<std::sync::Mutex<Vec<Arc<Brain>>>> {
    fn list(&self) -> Vec<Arc<Brain>> {
        self.lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

impl BrainList for Arc<Vec<Arc<Brain>>> {
    fn list(&self) -> Vec<Arc<Brain>> {
        (**self).clone()
    }
}

// ---------------------------------------------------------------- Failover

pub struct Failover {
    /// brain id -> forecast of the account it is using now
    pub forecasts: Mutex<HashMap<String, Value>>,
    pred: Arc<Predictor>,
}

impl Failover {
    pub fn new(predictor: Arc<Predictor>) -> Failover {
        Failover {
            forecasts: Mutex::new(HashMap::new()),
            pred: predictor,
        }
    }

    // ---------- helpers ----------
    fn primary(&self, b: &Brain) -> String {
        let cfg = b.cfg();
        let d = cfg.os("config_dir").unwrap_or_else(|| "~/.claude".into());
        expand(&d).to_string_lossy().to_string()
    }

    pub fn current(&self, sup: &Supervisor, b: &Brain) -> String {
        sup.state
            .get(&b.id)
            .os("account")
            .unwrap_or_else(|| self.primary(b))
    }

    pub(crate) fn quota_for(&self, b: &Brain, d: &str, refresh: bool) -> Value {
        let days = b.config.key("history_days").as_i64().unwrap_or(30);
        let ds = expand(d).to_string_lossy().to_string();
        let sc = scanner_for(&ds, days);
        let should = if refresh {
            let g = match sc.quota.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            g.f("ts").map(|t| unix_now() - t > 90.0).unwrap_or(true)
        } else {
            false
        };
        if should {
            sc.read_quota();
        }
        let g = match sc.quota.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        (*g).clone()
    }

    fn best_alternative(&self, b: &Brain, exclude: &str) -> Option<(String, f64)> {
        let cfg = b.cfg();
        let mut best: Option<(String, f64)> = None;
        for x in cfg.arr("failover") {
            let Some(xs) = x.as_str() else { continue };
            let d = expand(xs);
            let ds = d.to_string_lossy().to_string();
            if same(&ds, exclude) || !d.is_dir() {
                continue;
            }
            let (pct, _) = session_limit(&self.quota_for(b, &ds, true));
            let Some(pct) = pct else { continue };
            if pct >= ALT_MAX_PCT {
                continue;
            }
            if best.as_ref().map(|(_, p)| pct < *p).unwrap_or(true) {
                best = Some((ds, pct));
            }
        }
        best
    }

    fn set_forecast(&self, bid: &str, fc: &Value) {
        let mut g = match self.forecasts.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.insert(bid.to_string(), fc.clone());
    }

    pub fn forecast_for(&self, bid: &str) -> Option<Value> {
        let g = match self.forecasts.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.get(bid).cloned()
    }

    fn live_sessions(&self, b: &Brain, now: f64) -> Vec<Session> {
        b.my_sessions()
            .into_iter()
            .filter(|s| now - s.c.last < LIVE_SECONDS)
            .collect()
    }

    fn summarizer(&self, sup: &Supervisor) -> Option<Value> {
        let sid = sup
            .cfg
            .key("handoff")
            .get("summarizer")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        sid.and_then(|id| sup.cfg.brain(&id))
    }

    // ---------- the regulate() step ----------
    pub fn tick(&self, sup: &Supervisor, now: f64) {
        let now = if now == 0.0 { unix_now() } else { now };
        let brains = sup.brains.list();
        for b in brains {
            if b.kind != "claude_code" {
                continue;
            }
            let cfg = b.cfg();
            let prim = self.primary(&b);
            let cur = self.current(sup, &b);

            let mut seen: HashSet<String> = HashSet::new();
            for d in [prim.clone(), cur.clone()] {
                let key = norm(Path::new(&d));
                if !seen.insert(key.clone()) {
                    continue;
                }
                let (pct, reset) = session_limit(&self.quota_for(&b, &d, false));
                self.pred.add(&key, pct, reset, Some(now));
            }

            let ckey = norm(Path::new(&cur));
            let fc = self.pred.forecast(&ckey, Some(now));
            if let Some(ref f) = fc {
                self.set_forecast(&b.id, f);
            }

            if !truthy(cfg.get("failover")) {
                continue;
            }

            if same(&cur, &prim) {
                let risk = fc.as_ref().and_then(|f| f.f("risk")).unwrap_or(0.0);
                // Switch early, while there is still room to close the current idea (default 85 %).
                let early_at = sup
                    .cfg
                    .key("failover_pct")
                    .as_f64()
                    .unwrap_or(EARLY_SWITCH_PCT);
                let (cur_pct, _) = session_limit(&self.quota_for(&b, &cur, false));
                let early = cur_pct.map(|p| p >= early_at).unwrap_or(false);
                if risk >= SWITCH_RISK || early {
                    let why = if early && risk < SWITCH_RISK {
                        format!(
                            "{:.0} % usado (cambio anticipado al {:.0} %)",
                            cur_pct.unwrap_or(0.0),
                            early_at
                        )
                    } else {
                        fc.as_ref().map(|f| f.s("why")).unwrap_or_default()
                    };
                    match self.best_alternative(&b, &cur) {
                        Some((alt, _)) => {
                            self.switch(sup, &b, &alt, &format!("quota: {why}"), now);
                        }
                        None => {
                            static NO_ALT_LOG: std::sync::OnceLock<Mutex<HashMap<String, f64>>> =
                                std::sync::OnceLock::new();
                            let mut log_map = NO_ALT_LOG
                                .get_or_init(|| Mutex::new(HashMap::new()))
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let last = log_map.get(&b.id).copied().unwrap_or(0.0);
                            if now - last >= 600.0 {
                                log_map.insert(b.id.clone(), now);
                                sup.diary.add(
                                    "MetaBrain",
                                    &format!(
                                        "{}: quota almost out and no other account has room",
                                        brain_name(&b)
                                    ),
                                    "alarm",
                                );
                            }
                        }
                    }
                }
            } else {
                let pq = self.quota_for(&b, &prim, false);
                let (ppct, _preset) = session_limit(&pq);
                let pweekly = pq
                    .arr("limits")
                    .into_iter()
                    .find(|l| l.s("group") == "weekly")
                    .and_then(|l| l.f("pct"));
                let early_at = sup
                    .cfg
                    .key("failover_pct")
                    .as_f64()
                    .unwrap_or(EARLY_SWITCH_PCT);
                let back =
                    ppct.is_some_and(|p| p < 50.0) && pweekly.map(|w| w < early_at).unwrap_or(true);
                if back {
                    sup.state.set(
                        &b.id,
                        json!({"account": null, "account_since": null, "account_until": null, "account_reason": null}),
                    );
                    sup.diary.add(
                        "MetaBrain",
                        &format!("{}: back to the main account (quota reset)", brain_name(&b)),
                        "info",
                    );
                } else {
                    // Cascade to another healthy alternative if the current alternate is also running out.
                    let (cur_pct, _) = session_limit(&self.quota_for(&b, &cur, false));
                    let risk = fc.as_ref().and_then(|f| f.f("risk")).unwrap_or(0.0);
                    if risk >= SWITCH_RISK || cur_pct.is_some_and(|p| p >= early_at) {
                        if let Some((alt, _)) = self.best_alternative(&b, &cur) {
                            self.switch(sup, &b, &alt, "quota: cascade to next account", now);
                        }
                    }
                }
            }
        }
    }

    /// Hand off live sessions and point the brain at another account.
    pub fn switch(&self, sup: &Supervisor, b: &Brain, target: &str, reason: &str, now: f64) {
        let now = if now == 0.0 { unix_now() } else { now };
        let cur = self.current(sup, b);
        let (_pct, reset) = session_limit(&self.quota_for(b, &cur, false));
        let name = brain_name(b);
        let lang = sup
            .cfg
            .key("lang")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "en".to_string());
        let summarizer = self.summarizer(sup);

        let live = self.live_sessions(b, now);
        let n_live = live.len();
        if !live.is_empty() {
            let paths: Vec<PathBuf> = live.into_iter().map(|s| s.path).collect();
            let name_bg = name.clone();
            let cur_bg = cur.clone();
            let reason_bg = reason.to_string();
            std::thread::spawn(move || {
                for p in paths {
                    let _ = write_handoff(
                        &p,
                        &name_bg,
                        &cur_bg,
                        &reason_bg,
                        &lang,
                        summarizer.as_ref(),
                    );
                }
            });
        }

        sup.state.set(
            &b.id,
            json!({"account": target, "account_since": now, "account_until": reset, "account_reason": reason}),
        );
        self.pred.note_handoff(&norm(Path::new(&cur)));
        sup.diary.add(
            "MetaBrain",
            &format!(
                "{name} moved to account {target} ({reason}); handing off {n_live} live session(s)"
            ),
            "alarm",
        );
    }

    // ---------- "Continue in account X" ----------
    /// Copy one session to another account and resume it there. -> (message, command).
    pub fn continue_session(
        &self,
        sup: &Supervisor,
        b: &Brain,
        session_id: &str,
        target: Option<&str>,
        launch: bool,
    ) -> (String, Option<Vec<String>>) {
        let cur = self.current(sup, b);
        let target = match target {
            Some(t) if !t.is_empty() => expand(t).to_string_lossy().to_string(),
            _ => match self.best_alternative(b, &cur) {
                Some((d, _)) => d,
                None => return ("no other account with room".to_string(), None),
            },
        };

        let session_id = session_id.trim();
        if session_id.is_empty() {
            return ("session not found".to_string(), None);
        }

        let days = b.config.key("history_days").as_i64().unwrap_or(30);
        let sc = scanner_for(&cur, days);
        if !sc.ready() {
            sc.scan(0.0);
        }
        let sessions = sc.sessions();
        let s = sessions
            .into_iter()
            .find(|s| s.id == session_id || s.id.starts_with(session_id));
        let Some(s) = s else {
            return ("session not found".to_string(), None);
        };
        if !is_safe_session_id(&s.id) || !is_safe_project_segment(&s.project) {
            return ("invalid session identifier".to_string(), None);
        }

        let dst_dir = Path::new(&target).join("projects").join(&s.project);
        if fs::create_dir_all(&dst_dir).is_err() {
            return ("could not copy the session".to_string(), None);
        }
        let fname = s
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        if fs::copy(&s.path, dst_dir.join(fname)).is_err() {
            return ("could not copy the session".to_string(), None);
        }
        let subdir = s.path.with_extension("");
        if subdir.is_dir() {
            let subname = subdir
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            let _ = copy_dir_all(&subdir, &dst_dir.join(subname));
        }

        let cwd = if !s.cwd.is_empty() && Path::new(&s.cwd).is_dir() {
            PathBuf::from(&s.cwd)
        } else {
            home()
        };
        let cmd_vec: Vec<String> = vec!["claude".to_string(), "--resume".to_string(), s.id.clone()];

        if launch {
            let spawned = if cfg!(windows) {
                let joined = cmd_vec.join(" ");
                let mut c = std::process::Command::new("cmd");
                c.arg("/c")
                    .arg("start")
                    .arg("MetaBrain")
                    .arg("cmd")
                    .arg("/k")
                    .arg(&joined);
                c.current_dir(&cwd);
                set_config_env(&mut c, &target);
                c.spawn()
            } else {
                let mut c = std::process::Command::new("x-terminal-emulator");
                c.arg("-e").args(&cmd_vec);
                c.current_dir(&cwd);
                set_config_env(&mut c, &target);
                c.spawn()
            };
            if let Err(e) = spawned {
                return (
                    format!("copied; open a terminal yourself ({e})"),
                    Some(cmd_vec),
                );
            }
        }

        let short: String = s.id.chars().take(8).collect();
        sup.diary.add(
            "You",
            &format!("{}: session {short} continued in {target}", brain_name(b)),
            "info",
        );
        ("ok".to_string(), Some(cmd_vec))
    }
}

// ---------------------------------------------------------------- tests

fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.contains("..")
}

fn is_safe_project_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && !seg.contains("..")
        && !seg.contains(['/', '\\', ':', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_limit_finds_the_session_window() {
        let q = json!({"limits": [
            {"group": "weekly", "pct": 10.0},
            {"group": "session", "pct": 42.5, "reset": 1234.0},
            {"group": "session", "pct": 99.0, "reset": 9999.0}
        ]});
        assert_eq!(session_limit(&q), (Some(42.5), Some(1234.0)));
    }

    #[test]
    fn session_limit_is_empty_without_a_session() {
        let q = json!({"limits": [
            {"group": "weekly", "pct": 10.0},
            {"group": "session", "pct": null}
        ]});
        assert_eq!(session_limit(&q), (None, None));
        assert_eq!(session_limit(&json!({})), (None, None));
    }

    #[test]
    fn same_expands_and_normalises() {
        assert!(same("~/.claude", "~/.claude"));
        assert!(same("", "~/.claude"));
        assert!(!same("~/.claude", "~/.claude-alt"));
    }

    #[test]
    fn copy_tree_keeps_files_and_subfolders() {
        let base = std::env::temp_dir().join(format!(
            "mb_failover_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = fs::create_dir_all(src.join("sub"));
        let _ = fs::write(src.join("a.txt"), b"a");
        let _ = fs::write(src.join("sub").join("b.txt"), b"b");

        assert!(copy_dir_all(&src, &dst).is_ok());
        assert_eq!(
            fs::read_to_string(dst.join("a.txt")).unwrap_or_default(),
            "a"
        );
        assert_eq!(
            fs::read_to_string(dst.join("sub").join("b.txt")).unwrap_or_default(),
            "b"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn session_id_and_project_validators_block_injection_and_traversal() {
        assert!(is_safe_session_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_safe_session_id("ses_01.abc"));
        assert!(!is_safe_session_id(""));
        assert!(!is_safe_session_id("abc&calc"));
        assert!(!is_safe_session_id("abc | whoami"));
        assert!(!is_safe_session_id("../evil"));

        assert!(is_safe_project_segment(
            "C--Users-esteb-proyectos-metabrain"
        ));
        assert!(!is_safe_project_segment(""));
        assert!(!is_safe_project_segment(".."));
        assert!(!is_safe_project_segment("../outside"));
        assert!(!is_safe_project_segment("a\\b"));
    }
}
