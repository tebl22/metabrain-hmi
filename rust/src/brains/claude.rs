//! Claude Code brain perception, session ownership, snapshot builder, and settings.json control.
use super::scanner::{matches, pretty, Scanner, Session};
use super::{tail, Brain};
use crate::store::Usage;
use crate::util::{expand, log, norm, now, read_json, write_json, J};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

impl Brain {
    pub(super) fn poll_claude(
        &self,
        scan: &Arc<Scanner>,
        version: &Mutex<i64>,
        seen_spawns: &Mutex<HashSet<String>>,
    ) {
        scan.scan(2.0);
        let v = scan.version() as i64;
        {
            let mut cur = version.lock().unwrap_or_else(|e| e.into_inner());
            if *cur == v {
                return;
            }
            *cur = v;
        }
        let ready = scan.ready();
        let t = now();
        let mut vol = HashMap::new();
        for s in self.my_sessions() {
            for (key, (ts, model, u)) in &s.usages {
                vol.insert(
                    key.clone(),
                    Usage {
                        ts: *ts,
                        brain: self.id.clone(),
                        model: Some(model.clone()),
                        inp: u[0],
                        out: u[1],
                        cache_read: u[2],
                        cache_write: u[3],
                        usd: 0.0,
                    },
                );
            }
            let mut seen = seen_spawns.lock().unwrap_or_else(|e| e.into_inner());
            for (tid, sp) in &s.spawns {
                if seen.insert(tid.clone()) {
                    if let Some(ts) = sp.ts {
                        if ready && t - ts < 60.0 {
                            let d: String = sp.desc.chars().take(60).collect();
                            self.say(&format!("launched a subagent: {d}"), "info");
                        }
                    }
                }
            }
        }
        self.ledger.set_volatile(&self.id, vol);
    }

    pub(super) fn slow_poll_claude(&self, scan: &Arc<Scanner>) {
        let ts = scan.quota.lock().unwrap_or_else(|e| e.into_inner()).f("ts");
        if ts.map(|t| now() - t > 90.0).unwrap_or(true) {
            scan.read_quota();
        }
    }

    /// Brains on the same config dir, in config order (for session assignment).
    fn sibling_cfgs(&self) -> Vec<Value> {
        let me = norm(&expand(
            &self
                .cfg()
                .os("config_dir")
                .unwrap_or_else(|| "~/.claude".into()),
        ));
        self.config
            .brains()
            .into_iter()
            .filter(|b| b.b("enabled", true) && b.s("kind") == "claude_code")
            .filter(|b| {
                norm(&expand(
                    &b.os("config_dir").unwrap_or_else(|| "~/.claude".into()),
                )) == me
            })
            .collect()
    }

    /// Decide which brain of this account a session belongs to.
    pub fn owns_view(&self, sibs: &[Value], view: &Value) -> bool {
        for b in sibs {
            let m = b.o("match");
            if !m.as_object().unwrap().is_empty() && matches(&m, view) {
                return b.s("id") == self.id;
            }
        }
        sibs.iter()
            .find(|b| b.o("match").as_object().unwrap().is_empty())
            .map(|b| b.s("id") == self.id)
            .unwrap_or(false)
    }

    pub fn my_sessions(&self) -> Vec<Session> {
        let Some(scan) = self.scanner() else {
            return vec![];
        };
        let sibs = self.sibling_cfgs();
        scan.sessions()
            .into_iter()
            .filter(|s| self.owns_view(&sibs, &s.match_view()))
            .collect()
    }

    pub(super) fn snapshot_claude(&self, scan: &Arc<Scanner>, now_ts: f64) -> Value {
        let sess = self.my_sessions();
        let cfg = self.cfg();
        let live: Vec<&Session> = sess.iter().filter(|s| now_ts - s.c.last < 120.0).collect();
        let pool: Vec<&Session> = if live.is_empty() {
            sess.iter().collect()
        } else {
            live.clone()
        };
        let top = pool.into_iter().max_by(|a, b| {
            a.c.last
                .partial_cmp(&b.c.last)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut lobes = Map::new();
        for (ix, k) in ["plan", "act", "delegate"].iter().enumerate() {
            let v = sess.iter().map(|s| s.lobes[ix]).fold(0.0, f64::max);
            lobes.insert(k.to_string(), json!(Self::lobe(v, now_ts)));
        }
        let delegates = cfg.o("delegates_to");
        let mut subs: Vec<Value> = vec![];
        let mut jobs: Vec<Value> = vec![];
        for s in &sess {
            if now_ts - s.c.last > 3.0 * 3600.0 {
                continue;
            }
            let mut linked: HashSet<Option<String>> = HashSet::new();
            for sb in s.subs.values() {
                linked.insert(sb.tool.clone());
                let alive = sb.alive(&s.path, now_ts);
                if alive || now_ts - sb.c.last < 600.0 {
                    let desc = if sb.desc.is_empty() {
                        sb.typ.clone()
                    } else {
                        sb.desc.clone()
                    };
                    subs.push(json!({
                        "id": sb.id,
                        "desc": desc,
                        "type": sb.typ,
                        "model": sb.c.model,
                        "tokens": sb.tok,
                        "alive": alive,
                        "status": sb.c.status,
                        "last": sb.c.last,
                        "session": s.id
                    }));
                }
            }
            for (tid, sp) in &s.spawns {
                if let Some(ts) = sp.ts {
                    if !linked.contains(&Some(tid.clone())) && now_ts - ts < 90.0 {
                        let model = if sp.model.is_empty() {
                            "inherited".to_string()
                        } else {
                            sp.model.clone()
                        };
                        subs.push(json!({
                            "id": tail(tid, 8),
                            "session": s.id,
                            "desc": sp.desc,
                            "type": sp.typ,
                            "model": model,
                            "tokens": 0,
                            "alive": true,
                            "status": "starting",
                            "last": ts
                        }));
                    }
                }
            }
            for (tid, j) in &s.jobs {
                let alive = j.end.is_none()
                    && j.ts.map(|t| now_ts - t < 5.0 * 3600.0).unwrap_or(false)
                    && now_ts - s.c.last < 1800.0;
                if alive || j.end.map(|e| now_ts - e < 900.0).unwrap_or(false) {
                    let target = delegates
                        .os(&format!("mcp__{}", j.server))
                        .unwrap_or_else(|| j.server.clone());
                    jobs.push(json!({
                        "id": tail(tid, 8),
                        "to": target,
                        "tool": j.tool,
                        "model": j.model,
                        "task": j.task,
                        "alive": alive,
                        "ts": j.ts
                    }));
                }
            }
        }
        subs.sort_by(|a, b| {
            let ka = (!a.b("alive", false), -a.f("last").unwrap_or(0.0));
            let kb = (!b.b("alive", false), -b.f("last").unwrap_or(0.0));
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        });
        subs.truncate(16);
        let mut sorted: Vec<&Session> = sess.iter().collect();
        sorted.sort_by(|a, b| {
            b.c.last
                .partial_cmp(&a.c.last)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let sessions: Vec<Value> = sorted
            .into_iter()
            .filter(|s| now_ts - s.c.last < 8.0 * 3600.0)
            .take(12)
            .map(|s| {
                let active = now_ts - s.c.last < 120.0;
                json!({
                    "id": s.id.chars().take(8).collect::<String>(),
                    "project": pretty(s),
                    "title": s.title,
                    "model": s.c.model,
                    "effort": s.effort,
                    "status": if active { s.c.status.clone() } else { String::new() },
                    "tokens": s.tokens(),
                    "cost": s.cost,
                    "last": s.c.last,
                    "active": active,
                    "subs_alive": s.subs.values().filter(|x| x.alive(&s.path, now_ts)).count()
                })
            })
            .collect();
        let q = scan.quota.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let status = match top {
            Some(t) if !live.is_empty() => t.c.status.clone(),
            Some(_) => "idle".into(),
            None => "no sessions".into(),
        };
        json!({
            "active": !live.is_empty(),
            "status": status,
            "lobes": lobes,
            "lobe_names": ["plan", "act", "delegate"],
            "model": top.map(|t| t.c.model.clone()).unwrap_or_default(),
            "effort": top.map(|t| t.effort.clone()).unwrap_or_default(),
            "project": top.map(pretty).unwrap_or_default(),
            "subagents": subs,
            "jobs": jobs,
            "sessions": sessions,
            "quota": q.get("limits").cloned().unwrap_or(json!([])),
            "quota_ts": q.get("ts").cloned().unwrap_or(Value::Null),
            "account": q.s("email"),
            "plan": q.s("plan"),
            "warning": q.s("warning"),
            "balance": null,
            "ready": scan.ready(),
            "stuck": sess.iter().flat_map(|s| crate::activity::stuck(s, now_ts)).collect::<Vec<Value>>(),
            "last": top.map(|t| json!(t.c.last)).unwrap_or(Value::Null)
        })
    }

    /// Optional for Claude Code: write model / thinking budget into this account's settings.json.
    /// Pause and budgets are enforced live by the PreToolUse hook, whatever this setting says.
    pub fn apply(&self, state: &Value) {
        if !self.is_claude() {
            return;
        }
        let cfg = self.cfg();
        if cfg.s("apply") != "settings" {
            return;
        }
        let p = expand(&cfg.os("config_dir").unwrap_or_else(|| "~/.claude".into()))
            .join("settings.json");
        let mut s = match read_json(&p) {
            Some(v @ Value::Object(_)) => v,
            Some(_) => return, // never overwrite something we cannot understand
            None if p.exists() => return,
            None => json!({}),
        };
        let mut changed = false;
        if let Some(m) = state.os("model") {
            if s.s("model") != m {
                s["model"] = json!(m);
                changed = true;
            }
        }
        if let Some(t) = state.get("thinking").filter(|v| !v.is_null()) {
            let val = (t.as_f64().unwrap_or(0.0) as i64).to_string();
            if !s.get("env").map(|e| e.is_object()).unwrap_or(false) {
                s["env"] = json!({});
            }
            if s["env"].s("MAX_THINKING_TOKENS") != val {
                s["env"]["MAX_THINKING_TOKENS"] = json!(val);
                changed = true;
            }
        }
        if changed && write_json(&p, &s).is_ok() {
            log(&format!("{}: settings.json updated", self.id));
        }
    }
}
