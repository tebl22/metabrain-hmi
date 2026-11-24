//! Command chain, priority order, succession, auto-architecture, and CLI task ledger accounting.
use super::{ChainEntry, Supervisor};
use crate::brains::Brain;
use crate::util::{now, J};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

impl Supervisor {
    /// Priority entries that the user actually configured (valid, unique, existing brains).
    pub(super) fn configured_priority(&self) -> Vec<String> {
        let known: HashSet<String> = self.brain_list().iter().map(|b| b.id.clone()).collect();
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for v in self
            .cfg
            .key("priority")
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            if let Some(id) = v.as_str() {
                if known.contains(id) && seen.insert(id.to_string()) {
                    out.push(id.to_string());
                }
            }
        }
        out
    }

    /// Effective priority: user list, then claude_code brains in config order, then the rest.
    pub fn priority_list(&self) -> Vec<String> {
        let mut out = self.configured_priority();
        let mut seen: HashSet<String> = out.iter().cloned().collect();
        let brains = self.brain_list();
        for b in &brains {
            if b.kind == "claude_code" && seen.insert(b.id.clone()) {
                out.push(b.id.clone());
            }
        }
        for b in &brains {
            if seen.insert(b.id.clone()) {
                out.push(b.id.clone());
            }
        }
        out
    }

    /// Tell the user when a brain that was out (quota, budget, no balance) can work again:
    /// "your usage reset" or "DeepSeek recharged".
    fn notify_comebacks(&self, chain: &[Value]) {
        let mut g = self.comeback_prev.lock().unwrap_or_else(|e| e.into_inner());
        let spent = |r: &str| {
            r.starts_with("presupuesto")
                || r.starts_with("sin saldo")
                || r.starts_with("saldo bajo")
                || r.starts_with("cuota")
                || r.starts_with("se agota")
        };
        for e in chain {
            let (id, now_r) = (e.s("id"), e.s("reason"));
            if let Some(before) = g.get(&id) {
                if spent(before) && e.b("available", false) {
                    let name = e.s("name");
                    let (title, body) = if before.contains("saldo") {
                        (
                            format!("{name} recargado"),
                            format!("{name} ya tiene saldo y vuelve a trabajar."),
                        )
                    } else {
                        (
                            "Se restableció tu uso".to_string(),
                            format!("{name} está disponible de nuevo."),
                        )
                    };
                    self.diary.add("MetaBrain", &body, "info");
                    crate::insights::notify_with(&title, &body, "brains");
                }
            }
            g.insert(id, now_r);
        }
    }

    /// When "architect.auto" is on, re-organise the hierarchy each time the set of available
    /// brains changes (never on every tick, so a manual change sticks until something happens).
    fn auto_architect(&self, chain: &[Value]) {
        let cfg = self.cfg.get();
        self.apply_model_plan(chain);
        if !cfg.o("architect").b("auto", false) {
            return;
        }
        let avail: Vec<String> = chain
            .iter()
            .filter(|e| e.b("available", false))
            .map(|e| e.s("id"))
            .collect();
        let key = avail.join(",");
        let mut g = self
            .auto_arch_last
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *g == key {
            return;
        }
        let first = g.is_empty();
        *g = key;
        drop(g);
        let rec = crate::architect::recommend(&cfg, chain);
        let changed = self.apply_architecture(&rec, chain, !first);
        // Then ask a good model to organise it properly (in the background: it takes seconds).
        if changed && cfg.o("architect").b("model", true) {
            self.organize_async(!first);
        }
    }

    fn apply_architecture(&self, rec: &Value, chain: &[Value], notify: bool) -> bool {
        if !crate::architect::differs(rec, chain) {
            return false;
        }
        let _ = self.act(&crate::architect::apply_payload(rec));
        let who = if rec.b("by_model", false) {
            "MetaBrain (IA) reorganizó"
        } else {
            "Reorganicé"
        };
        self.diary.add(
            "MetaBrain",
            &format!("{who} el trabajo: {}", rec.s("summary")),
            "info",
        );
        if notify {
            crate::insights::notify_routine("MetaBrain se reorganizó", &rec.s("summary"), "home");
        }
        true
    }

    /// Ask the organizer model in the background; its plan is applied on the next tick.
    pub fn organize_async(&self, notify: bool) {
        if self.organize_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let cfg = self.cfg.get();
        let chain = self.chain(now());
        let busy = Arc::clone(&self.organize_busy);
        let model_plan = Arc::clone(&self.model_plan);
        std::thread::spawn(move || {
            // Released even if the organizer panics.
            struct Release(Arc<AtomicBool>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _release = Release(busy);
            if let Some(plan) = crate::architect::organize_with_model(&cfg, &chain) {
                if let Ok(mut g) = model_plan.lock() {
                    *g = json!({
                        "summary": plan.s("summary"),
                        "lines": plan.get("lines").cloned().unwrap_or(json!([])),
                        "levels": plan.get("levels").cloned(),
                        "roles": plan.get("roles").cloned(),
                        "ts": now(),
                        "pending": true,
                        "notify": notify,
                        "by_model": true
                    });
                }
            }
        });
    }

    /// Apply a freshly arrived organizer plan (once).
    fn apply_model_plan(&self, chain: &[Value]) {
        let plan = {
            let Ok(mut g) = self.model_plan.lock() else {
                return;
            };
            if !g.b("pending", false) {
                return;
            }
            g["pending"] = json!(false);
            g.clone()
        };
        self.apply_architecture(&plan, chain, plan.b("notify", false));
    }

    pub fn architecture(&self) -> Value {
        let chain = self.chain(now());
        let mut r = crate::architect::recommend(&self.cfg.get(), &chain);
        r["differs"] = json!(crate::architect::differs(&r, &chain));
        r["auto"] = json!(self.cfg.key("architect").b("auto", false));
        // The organizer model's latest plan replaces the heuristic one when it has run.
        if let Some(mp) = self
            .model_plan
            .lock()
            .ok()
            .map(|g| g.clone())
            .filter(|v| !v.is_null())
        {
            for k in ["summary", "lines", "levels", "roles"] {
                if let Some(x) = mp.get(k).filter(|x| !x.is_null()) {
                    r[k] = x.clone();
                }
            }
            r["by_model"] = json!(true);
            r["differs"] = json!(crate::architect::differs(&r, &chain));
        }
        r
    }

    /// Running or queued tasks per brain (to share work among co-masters) — in-memory, zero disk I/O.
    fn task_load(&self) -> HashMap<String, usize> {
        let qs = self.tasks.queue_summary();
        let mut m: HashMap<String, usize> = HashMap::new();
        for (bid, slot) in qs.per_brain {
            if slot.running > 0 {
                *m.entry(bid).or_insert(0) += slot.running;
            }
        }
        for w in qs.waiting {
            *m.entry(w.brain).or_insert(0) += 1;
        }
        m
    }

    /// Who takes the next job: the least busy brain of the commanding level.
    pub fn lead_of(chain: &[Value]) -> Option<String> {
        chain
            .iter()
            .find(|e| e.b("lead", false))
            .or_else(|| chain.iter().find(|e| e.b("master", false)))
            .map(|e| e.s("id"))
    }

    /// Everyone in command right now, e.g. "A+B+GM" (stable, for announcements).
    pub fn masters_of(chain: &[Value]) -> Vec<String> {
        chain
            .iter()
            .filter(|e| e.b("master", false))
            .map(|e| e.s("id"))
            .collect()
    }

    /// Typed command chain: one entry per brain in priority order.
    pub fn chain_entries(&self, ts: f64) -> Vec<ChainEntry> {
        let summ = self.ledger.summary(Some(ts));
        self.chain_entries_with(ts, Some(&summ))
    }

    pub(super) fn chain_entries_with(&self, ts: f64, summ: Option<&Value>) -> Vec<ChainEntry> {
        let succ = crate::succession::Chain::from_cfg(&self.cfg.get());
        let brains = self.brain_list();
        let mut by_id: HashMap<String, Arc<Brain>> = HashMap::new();
        for b in brains {
            by_id.insert(b.id.clone(), b);
        }

        let mut entries: Vec<ChainEntry> = Vec::new();
        for id in self.priority_list() {
            let Some(b) = by_id.get(&id) else { continue };
            let cfg = b.cfg();
            let st = self.state.get(&id);
            let paused = st.b("paused", false);
            let paused_by = st.s("paused_by");
            let (ratio, _) = self.budget_ratio(b, summ);
            // Quota applies to all subscription-backed brains (Claude Code, Codex/ChatGPT, Antigravity/Gemini),
            // taking the active failover account's max(session, weekly, quota) when Claude failover is active.
            let pct = self.active_quota_pct(b);
            let fc = if b.kind == "claude_code" {
                self.failover.forecast_for(&id)
            } else {
                None
            };
            let risk = fc.as_ref().and_then(|f| f.f("risk")).unwrap_or(0.0);
            let eta = fc.as_ref().and_then(|f| {
                f.f("eta_min")
                    .or_else(|| f.f("minutes"))
                    .or_else(|| f.f("eta"))
            });
            let balance = if matches!(b.kind.as_str(), "openai" | "anthropic" | "ollama") {
                b.snapshot(ts)
                    .get("balance")
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            let bal = balance.get("amount").and_then(|v| v.as_f64());

            let mut available = true;
            let mut reason = "disponible".to_string();
            if !cfg.b("enabled", true) {
                available = false;
                reason = "deshabilitado".into();
            } else if paused && paused_by == "user" {
                available = false;
                reason = "pausado".into();
            } else if ratio.map(|r| r >= 1.0).unwrap_or(false) {
                available = false;
                reason = "presupuesto agotado".into();
            } else if bal.map(|a| a <= 0.0).unwrap_or(false) {
                available = false;
                reason = "sin saldo".into();
            } else if let Some(e) = crate::succession::recent_error(&id, ts) {
                available = false;
                reason = format!("error: {e}");
            } else if let Some(idle) = self
                .tasks
                .stalled_secs(&id, succ.stall_minutes.saturating_mul(60))
            {
                available = false;
                reason = crate::succession::Reason::Stalled(idle / 60).label("es");
            } else if let Some(p) = pct.filter(|p| *p >= succ.threshold_pct) {
                available = false;
                reason = if b.kind == "claude_code" {
                    format!("cuota 5 h al {:.0} %", p)
                } else {
                    format!("cuota al {:.0} %", p)
                };
            } else if b.kind == "claude_code" && risk >= 1.0 {
                available = false;
                reason = match eta {
                    Some(m) => format!("se agota en ~{:.0} min", m),
                    None => {
                        let why = fc.as_ref().map(|f| f.s("why")).unwrap_or_default();
                        if why.is_empty() {
                            "se agota pronto".into()
                        } else {
                            why
                        }
                    }
                };
            }
            if available {
                if let Some(amount) = bal {
                    if amount < 0.3 {
                        available = false;
                        reason = "saldo bajo".into();
                    }
                }
            }

            let name = cfg.os("name").unwrap_or_else(|| id.clone());
            entries.push(ChainEntry {
                id,
                name,
                kind: b.kind.clone(),
                available,
                reason,
                pct,
                eta_min: eta,
                master: false,
                lead: None,
                rank: 0,
            });
        }

        // Hierarchy levels: brains on the same level share the command.
        let ranks = self.cfg.key("ranks");
        let mut level = 0u32;
        let mut prev_rank: Option<i64> = None;
        for e in entries.iter_mut() {
            let r = ranks.get(e.id.as_str()).and_then(|v| v.as_i64());
            let same = matches!((r, prev_rank), (Some(a), Some(b)) if a == b);
            if !same || level == 0 {
                level += 1;
            }
            prev_rank = r;
            e.rank = level;
        }
        let load = self.task_load();
        if let Some(ix) = entries.iter().position(|e| e.available) {
            let top = entries[ix].rank;
            let mut lead: Option<(usize, usize)> = None;
            for (i, e) in entries.iter_mut().enumerate() {
                if e.rank == top && e.available {
                    let n = load.get(&e.id).copied().unwrap_or(0);
                    if lead.map(|(_, m)| n < m).unwrap_or(true) {
                        lead = Some((i, n));
                    }
                    e.master = true;
                }
            }
            if let Some((i, _)) = lead {
                entries[i].lead = Some(true);
            }
        } else if let Some(first) = entries.first_mut() {
            first.master = true;
            first.lead = Some(true);
            first.reason = "ninguno disponible".into();
        }
        entries
    }

    /// The command chain as the HMI draws it: one entry per brain in priority order.
    pub fn chain(&self, ts: f64) -> Vec<Value> {
        let summ = self.ledger.summary(Some(ts));
        self.chain_with(ts, Some(&summ))
    }

    pub(super) fn chain_with(&self, ts: f64, summ: Option<&Value>) -> Vec<Value> {
        self.chain_entries_with(ts, summ)
            .into_iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect()
    }

    /// Tokens and USD spent by tasks on agents whose usage MetaBrain does not otherwise see (Codex,
    /// Gemini/Antigravity, direct API calls) go into the brain's ledger, so "hoy / 5 h / 7 días"
    /// and the Uso charts count them. Only the growth since the last look is added.
    fn ledger_task_usage(&self) {
        let path = crate::util::data_dir().join("task_ledger.json");
        let tasks = self.tasks.list(80).as_array().cloned().unwrap_or_default();
        let first_run = !path.exists();
        let mut to_add: Vec<crate::store::Usage> = Vec::new();
        let mut to_save: Option<Value> = None;

        {
            let Ok(mut g) = self.task_ledger_seen.lock() else {
                return;
            };
            let seen = g.get_or_insert_with(|| {
                crate::util::read_json(&path)
                    .and_then(|v| v.as_object().cloned())
                    .map(|m| {
                        m.into_iter()
                            .map(|(k, v)| {
                                (
                                    k,
                                    (
                                        v.get(0).and_then(|x| x.as_i64()).unwrap_or(0),
                                        v.get(1).and_then(|x| x.as_i64()).unwrap_or(0),
                                        v.get(2).and_then(|x| x.as_f64()).unwrap_or(0.0),
                                    ),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            });
            let mut changed = false;
            for m in &tasks {
                if !matches!(
                    m.s("kind").as_str(),
                    "codex" | "antigravity" | "gemini_cli" | "openai"
                ) {
                    continue;
                }
                let id = m.s("id");
                let tokens = m.get("tokens").and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;
                let cached = m
                    .f("cached_tokens")
                    .or_else(|| m.f("cached"))
                    .unwrap_or(0.0) as i64;
                let usd = m.f("usd").unwrap_or(0.0);
                let (t0, c0, u0) = seen.get(&id).copied().unwrap_or((0, 0, 0.0));
                if tokens <= t0 && usd <= u0 {
                    continue;
                }
                seen.insert(id, (tokens.max(t0), cached.max(c0), usd.max(u0)));
                changed = true;
                if first_run {
                    continue; // don't backfill history on the first start
                }
                let dt = (tokens - t0).max(0);
                let dc = (cached - c0).max(0).min(dt);
                let du = (usd - u0).max(0.0);
                let model = Some(m.s("model")).filter(|x| !x.is_empty());
                to_add.push(crate::store::Usage {
                    ts: now(),
                    brain: m.s("brain"),
                    model,
                    // Store total input delta in `inp` (matching proxy OpenAI convention) so
                    // `Usage::total()` includes cached tokens and `advisor.rs` computes cache hit cleanly.
                    inp: dt,
                    out: 0,
                    cache_read: dc,
                    cache_write: 0,
                    usd: du,
                });
            }
            if seen.len() > 300 {
                let keep: std::collections::HashSet<String> =
                    tasks.iter().map(|m| m.s("id")).collect();
                seen.retain(|k, _| keep.contains(k));
                changed = true;
            }
            if changed || first_run {
                let v: Map<String, Value> = seen
                    .iter()
                    .map(|(k, (a, b, u))| (k.clone(), json!([a, b, u])))
                    .collect();
                to_save = Some(Value::Object(v));
            }
        }
        for u in to_add {
            self.ledger.add(u, true);
        }
        if let Some(v) = to_save {
            let _ = crate::util::write_json(&path, &v);
        }
    }

    pub(super) fn succession_tick_with(&self, t: f64, summ: Option<&Value>) {
        self.ledger_task_usage();
        let chain = self.chain_with(t, summ);
        self.notify_comebacks(&chain);
        self.auto_architect(&chain);
        let chain = self.chain_with(t, summ);
        let ms = Self::masters_of(&chain);
        let master = if ms.is_empty() {
            None
        } else {
            Some(ms.join("+"))
        };
        if let Some(Some(prev)) = crate::succession::swap_master(master.as_deref()) {
            if master.is_some() {
                let name = |id: &str| {
                    chain
                        .iter()
                        .find(|e| e.s("id") == id)
                        .map(|e| e.s("name"))
                        .unwrap_or_else(|| id.to_string())
                };
                let who = ms.iter().map(|m| name(m)).collect::<Vec<_>>().join(" + ");
                let gone: Vec<String> = prev
                    .split('+')
                    .filter(|p| !ms.iter().any(|m| m == p))
                    .map(String::from)
                    .collect();
                let why = gone
                    .iter()
                    .map(|g| {
                        format!(
                            "{} ({})",
                            name(g),
                            chain
                                .iter()
                                .find(|e| &e.s("id") == g)
                                .map(|e| e.s("reason"))
                                .unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let verb = if ms.len() > 1 { "toman" } else { "toma" };
                let text = if why.is_empty() {
                    format!("{who} {verb} el mando")
                } else {
                    format!("{who} {verb} el mando: {why}")
                };
                self.diary.add("MetaBrain", &text, "warning");
                crate::insights::notify_routine("MetaBrain", &text, "home");
            }
        }

        let succ = crate::succession::Chain::from_cfg(&self.cfg.get());
        if succ.handoff_tokens == 0 {
            return;
        }
        let lang = self.cfg.key("lang").as_str().unwrap_or("es").to_string();
        for b in self.brain_list().iter().filter(|b| b.kind == "claude_code") {
            let Some(scan) = b.scanner() else { continue };
            for s in scan.sessions().into_iter().filter(|s| t - s.c.last < 900.0) {
                let bytes = std::fs::metadata(&s.path).map(|m| m.len()).unwrap_or(0);
                let est = crate::succession::estimate_tokens(bytes);
                if est < succ.handoff_tokens || !crate::succession::mark_sized(&s.id) {
                    continue;
                }
                let _ = crate::relay::update(&s.path, &s.cwd, &s.id, &b.id, &b.name(), &lang, true);
                let text = format!(
                    "{}: {} en {}. El traspaso ya está listo; abre una sesión nueva en ese proyecto para seguir más barato.",
                    b.name(),
                    crate::succession::Reason::Size(est).label(&lang),
                    s.project
                );
                self.diary.add("MetaBrain", &text, "warning");
                crate::insights::notify_routine("MetaBrain", &text, "home");
            }
        }
    }
}
