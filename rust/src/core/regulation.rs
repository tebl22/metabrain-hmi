//! Budget regulation, effective brain controls, gate.json writer, snapshot builder, and alarms.
use super::{AlarmEntry, EffectiveControls, Supervisor};
use crate::brains::Brain;
use crate::config::{templates, EFFORTS};
use crate::util::{model_family, now, truthy, write_json, J};
use serde_json::{json, Map, Value};

impl Supervisor {
    // ---------- read-only brains ----------
    pub fn is_readonly(&self, bid: &str) -> bool {
        // Persisted in state.json so it survives restarts.
        self.state
            .get(bid)
            .get("readonly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn set_readonly(&self, bid: &str, on: bool) {
        self.state.set(bid, json!({"readonly": on}));
    }

    // ---------- effective controls ----------
    pub fn effective_controls(&self, b: &Brain, summary: Option<&Value>) -> EffectiveControls {
        let st = self.state.get(&b.id);
        let ladder = b.models();
        let mut model = st.os("model").or_else(|| ladder.first().cloned());
        let mut effort = st.os("effort");
        let (ratio, reasons) = self.budget_ratio(b, summary);
        let rules = self.cfg.key("rules");
        let downgrade_at = rules.f("downgrade_at").unwrap_or(80.0);
        let down = ratio.map(|r| r * 100.0 >= downgrade_at).unwrap_or(false);
        if down {
            if let Some(m) = model.clone() {
                if let Some(ix) = ladder.iter().position(|x| *x == m) {
                    if ix + 1 < ladder.len() {
                        model = Some(ladder[ix + 1].clone());
                    }
                }
            }
            if let Some(e) = effort.clone() {
                if let Some(ix) = EFFORTS.iter().position(|x| *x == e) {
                    if ix > 0 {
                        effort = Some(EFFORTS[ix - 1].to_string());
                    }
                }
            }
        }
        EffectiveControls {
            model,
            effort,
            thinking: st.get("thinking").cloned().unwrap_or(Value::Null),
            max_output: st.get("max_output").cloned().unwrap_or(Value::Null),
            paused: st.b("paused", false),
            paused_by: st.get("paused_by").cloned().unwrap_or(Value::Null),
            downgraded: down,
            ratio,
            reasons,
            readonly: self.is_readonly(&b.id),
        }
    }

    pub fn effective(&self, b: &Brain, summary: Option<&Value>) -> Value {
        serde_json::to_value(self.effective_controls(b, summary)).unwrap_or_else(|_| json!({}))
    }

    pub fn budget_ratio(&self, b: &Brain, summary: Option<&Value>) -> (Option<f64>, Vec<String>) {
        let fresh;
        let sm: &Value = match summary {
            Some(v) => v,
            None => {
                fresh = self.ledger.summary(None);
                &fresh
            }
        };
        let bud = b
            .cfg()
            .get("budget")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let s = sm
            .get(&b.id)
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let sf = sm
            .get(format!("{}:file", b.id))
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let today = num_i64(s.get("today")) + num_i64(sf.get("today"));
        let usd = num_f64(s.get("usd_today")) + num_f64(sf.get("usd_today"));

        let mut parts: Vec<(f64, String)> = Vec::new();
        if let Some(v) = bud.get("tokens_day").filter(|v| truthy(Some(v))) {
            if let Some(lim) = v.as_f64().filter(|x| *x != 0.0) {
                parts.push((
                    today as f64 / lim,
                    format!("{} / {} tokens today", comma(today), limit_text(v)),
                ));
            }
        }
        if let Some(v) = bud.get("usd_day").filter(|v| truthy(Some(v))) {
            if let Some(lim) = v.as_f64().filter(|x| *x != 0.0) {
                parts.push((usd / lim, format!("${:.2} / ${:.2} today", usd, lim)));
            }
        }
        if let Some(v) = bud.get("quota_pct").filter(|v| truthy(Some(v))) {
            if let Some(qp) = self.active_quota_pct(b) {
                if let Some(lim) = v.as_f64().filter(|x| *x != 0.0) {
                    parts.push((qp / lim, format!("quota {:.0}% / {}%", qp, num_raw(v))));
                }
            }
        }
        if parts.is_empty() {
            return (None, Vec::new());
        }
        let r = parts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let mut sorted = parts.clone();
        sorted.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.cmp(&a.1))
        });
        (Some(r), sorted.into_iter().map(|p| p.1).collect())
    }

    /// Full quota payload of the active failover account (if Claude failover switched away from primary).
    pub(super) fn active_failover_quota(&self, b: &Brain) -> Option<Value> {
        if b.kind != "claude_code" {
            return None;
        }
        let acct = self.state.get(&b.id).os("account")?;
        if acct.trim().is_empty() {
            return None;
        }
        Some(self.failover.quota_for(b, &acct, false))
    }

    /// Highest active quota percentage (`session` / `weekly` / `quota`) respecting Claude failover.
    pub(super) fn active_quota_pct(&self, b: &Brain) -> Option<f64> {
        if let Some(q) = self.active_failover_quota(b) {
            let max_pct = q
                .arr("limits")
                .into_iter()
                .filter(|l| matches!(l.s("group").as_str(), "session" | "weekly" | "quota"))
                .filter_map(|l| l.f("pct"))
                .filter(|p| p.is_finite())
                .reduce(f64::max);
            if max_pct.is_some() {
                return max_pct;
            }
        }
        b.quota_pct()
    }

    // ---------- regulation (runs every ~20 s) ----------
    pub fn regulate(&self) {
        let rules = self.cfg.key("rules");
        let pause_at = rules.f("pause_at").unwrap_or(100.0);
        let downgrade_at = rules.f("downgrade_at").unwrap_or(80.0);
        let auto_resume = rules.b("auto_resume", true);
        let summ = self.ledger.summary(None);
        let brains = self.brain_list();

        for b in &brains {
            let st = self.state.get(&b.id);
            let (ratio, reasons) = self.budget_ratio(b, Some(&summ));
            let Some(ratio) = ratio else {
                // No budget data (quota unreadable, no limits): never leave an automatic pause hanging.
                if st.b("paused", false) && st.s("paused_by") == "auto" {
                    self.state.set(
                        &b.id,
                        json!({"paused": false, "paused_by": null, "downgraded": false}),
                    );
                    self.diary.add(
                        "MetaBrain",
                        &format!("Resumed {}: no budget data to justify the pause", b.name()),
                        "info",
                    );
                }
                continue;
            };
            let over = ratio * 100.0 >= pause_at;
            let down = ratio * 100.0 >= downgrade_at;
            let paused = st.b("paused", false);

            if over && !paused {
                self.state
                    .set(&b.id, json!({"paused": true, "paused_by": "auto"}));
                let reason = reasons.first().cloned().unwrap_or_default();
                self.diary.add(
                    "MetaBrain",
                    &format!("Paused {}: {}", b.name(), reason),
                    "alarm",
                );
            } else if !over && paused && st.s("paused_by") == "auto" && auto_resume {
                self.state
                    .set(&b.id, json!({"paused": false, "paused_by": null}));
                self.diary.add(
                    "MetaBrain",
                    &format!("Resumed {}: budget available again", b.name()),
                    "info",
                );
            }

            if down != st.b("downgraded", false) {
                self.state.set(&b.id, json!({"downgraded": down}));
                if down && !over {
                    let reason = reasons.first().cloned().unwrap_or_default();
                    self.diary.add(
                        "MetaBrain",
                        &format!("{} stepped down to save budget ({})", b.name(), reason),
                        "warning",
                    );
                }
            }

            b.apply(&self.effective(b, Some(&summ)));
        }

        let t = now();
        self.failover.tick(self, t);
        self.succession_tick_with(t, Some(&summ));

        let snap_brains: Vec<Value> = brains
            .iter()
            .filter(|b| b.kind == "claude_code")
            .map(|b| {
                json!({
                    "id": b.id,
                    "subagents": b.snapshot(t).get("subagents").cloned().unwrap_or(Value::Null)
                })
            })
            .collect();
        self.advisor.tick_with(self, &snap_brains, Some(&summ), t);

        // Command handover: remember who had the command and announce changes.
        let chain = self.chain_with(t, Some(&summ));
        // Everyone on the commanding level; a brain that drops out hands its sessions over.
        let ms = Self::masters_of(&chain);
        if !ms.is_empty() {
            let cur = ms.join("+");
            let prev = self
                .last_master
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(prev_set) = prev.filter(|p| p != &cur) {
                let lang = self
                    .cfg
                    .key("lang")
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "en".to_string());
                // Summaries go to an available API worker (never one without balance).
                let summarizer = chain
                    .iter()
                    .filter(|e| e.s("kind") == "openai" && e.b("available", false))
                    .find_map(|e| self.brain(&e.s("id")))
                    .map(|x| x.cfg());
                let mut jobs: Vec<(std::path::PathBuf, String, String)> = Vec::new();
                for prev_id in prev_set.split('+').filter(|p| !ms.iter().any(|m| m == p)) {
                    let Some(pb) = self.brain(prev_id) else {
                        continue;
                    };
                    if pb.kind != "claude_code" {
                        continue;
                    }
                    let account = self
                        .state
                        .get(prev_id)
                        .os("account")
                        .unwrap_or_else(|| pb.config_dir());
                    let pname = pb.name();
                    for s in pb.my_sessions() {
                        if t - s.c.last < 900.0 {
                            jobs.push((s.path, pname.clone(), account.clone()));
                        }
                    }
                }
                if !jobs.is_empty() {
                    let reason = format!("relevo de mando a {cur}");
                    std::thread::spawn(move || {
                        for (path, pname, account) in jobs {
                            let _ = crate::handoff::write_handoff(
                                &path,
                                &pname,
                                &account,
                                &reason,
                                &lang,
                                summarizer.as_ref(),
                            );
                        }
                    });
                }
            }
            let mut guard = self.last_master.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(cur);
        } else {
            let mut guard = self.last_master.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }

        self.write_gate_with(&chain, Some(&summ));
        self.services.watchdog();
    }

    pub fn write_gate(&self, summ: Option<&Value>) {
        let ts = now();
        let chain = self.chain_with(ts, summ);
        self.write_gate_with(&chain, summ);
    }

    fn write_gate_with(&self, chain: &[Value], summ: Option<&Value>) {
        // A tiny file the Claude Code hook reads on every tool call (fast path, no HTTP).
        let ts = now();
        let master = Self::lead_of(chain).map(Value::from).unwrap_or(Value::Null);
        let masters = json!(Self::masters_of(chain));

        let mut brains = Map::new();
        for b in self.brain_list() {
            let e = self.effective_controls(&b, summ);
            let st = self.state.get(&b.id);
            let cfg = b.cfg();
            let reason = e.reasons.first().cloned().unwrap_or_default();
            brains.insert(
                b.id.clone(),
                json!({
                    "paused": e.paused,
                    "by": e.paused_by,
                    "reason": reason,
                    "model": e.model,
                    "effort": e.effort,
                    "kind": b.kind,
                    "config_dir": cfg.get("config_dir").cloned().unwrap_or(Value::Null),
                    "match": cfg.get("match").filter(|v| v.is_object()).cloned().unwrap_or(json!({})),
                    "account": st.get("account").cloned().unwrap_or(Value::Null),
                    "readonly": e.readonly,
                }),
            );
        }
        let gate = json!({"ts": ts, "master": master, "masters": masters, "brains": brains});
        let _ = write_json(&self.gate_path, &gate);
    }

    // ---------- the one picture the HMI draws ----------
    pub fn snapshot(&self) -> Value {
        let ts = now();
        let summ = self.ledger.summary(Some(ts));
        let mut out: Vec<Value> = Vec::new();

        for b in self.brain_list() {
            let snap = b.snapshot(ts);
            let cfg = b.cfg();
            let s = summ
                .get(&b.id)
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or(json!({}));
            let sf = summ
                .get(format!("{}:file", b.id))
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or(json!({}));
            let eff = self.effective(&b, Some(&summ));

            let mut st = self.state.get(&b.id);
            if let Some(o) = st.as_object_mut() {
                o.insert("readonly".into(), json!(self.is_readonly(&b.id)));
            }

            let usage = json!({
                "today": num_i64(s.get("today")) + num_i64(sf.get("today")),
                "h5": num_i64(s.get("h5")),
                "d7": num_i64(s.get("d7")) + num_i64(sf.get("d7")),
                "usd_today": num_f64(s.get("usd_today")) + num_f64(sf.get("usd_today")),
                "usd_d7": num_f64(s.get("usd_d7")) + num_f64(sf.get("usd_d7")),
                "cache_today": num_i64(s.get("cache_today")) + num_i64(sf.get("cache_today")),
                "models": merge_models(s.get("models"), sf.get("models")),
                "hourly": s.get("hourly").cloned().unwrap_or_else(|| json!(vec![0; 24])),
                "daily": add_series(s.get("daily"), sf.get("daily")),
                "daily_usd": add_series(s.get("daily_usd"), sf.get("daily_usd")),
            });

            let model_now = snap
                .get("model")
                .and_then(|v| v.as_str())
                .filter(|x| !x.is_empty())
                .map(String::from)
                .unwrap_or_else(|| eff.s("model"));
            let family = model_family(&model_now);

            let relay = if b.kind == "claude_code" {
                json!({
                    "forecast": self.failover.forecast_for(&b.id).unwrap_or(Value::Null),
                    "failover": cfg.get("failover").cloned().unwrap_or(json!([])),
                    "account": self.state.get(&b.id).get("account").cloned().unwrap_or(Value::Null),
                })
            } else {
                Value::Null
            };

            let pilot = self
                .advisor
                .out_for(&b.id)
                .filter(|v| truthy(Some(v)))
                .unwrap_or_else(|| json!({"mode": self.advisor.mode(&b), "metrics": null, "score": null, "recs": []}));

            let mut e = Map::new();
            e.insert("id".into(), json!(b.id));
            e.insert(
                "name".into(),
                json!(cfg
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(b.id.as_str())),
            );
            e.insert("role".into(), cfg.get("role").cloned().unwrap_or(json!("")));
            e.insert("kind".into(), json!(b.kind));
            e.insert("controls".into(), json!(b.controls()));
            e.insert("models".into(), json!(b.models()));
            e.insert("efforts".into(), json!(EFFORTS));
            e.insert("state".into(), st);
            e.insert("effective".into(), eff);
            e.insert(
                "budget".into(),
                cfg.get("budget")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or(json!({})),
            );
            e.insert("usage".into(), usage);
            e.insert("family".into(), json!(family));
            e.insert("relay".into(), relay);
            e.insert("pilot".into(), pilot);
            e.insert(
                "subagent_policy".into(),
                cfg.get("subagents")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or(json!({})),
            );
            if let Some(so) = snap.as_object() {
                for (k, v) in so {
                    e.insert(k.clone(), v.clone());
                }
            }
            // If Claude failover switched to an alternate account, reflect its live quota/account in the snapshot.
            if let Some(q) = self.active_failover_quota(&b) {
                if let Some(limits) = q.get("limits").filter(|v| v.is_array()) {
                    e.insert("quota".into(), limits.clone());
                }
                if let Some(qts) = q.get("ts") {
                    e.insert("quota_ts".into(), qts.clone());
                }
                if let Some(acct) = q.os("account") {
                    e.insert("account".into(), json!(acct));
                }
                if let Some(plan) = q.os("plan") {
                    e.insert("plan".into(), json!(plan));
                }
                if let Some(err) = q.os("error") {
                    e.insert("quota_warning".into(), json!(err));
                } else {
                    e.remove("quota_warning");
                }
            }
            out.push(Value::Object(e));
        }

        let services = self.services.snapshot();
        let svc_arr = services.as_array().cloned().unwrap_or_default();
        let mut alarms = self.alarms(&out, &svc_arr);
        // Stable ids (keyed on structured entity+code when present so dynamic minutes don't break ack),
        // acknowledged alarms hidden until they expire, native notification for new alarms.
        let acked = self.cfg.key("acked_alarms");
        let mut active_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for a in alarms.iter_mut() {
            let stable_key = match (
                a.os("code"),
                a.os("brain"),
                a.os("service"),
                a.os("session"),
            ) {
                (Some(code), Some(bid), _, Some(sid)) => format!("{code}:{bid}:{sid}"),
                (Some(code), Some(bid), _, None) => format!("{code}:{bid}:{}", a.s("level")),
                (Some(code), None, Some(svc), _) => format!("{code}:{svc}"),
                _ => a.s("text"),
            };
            let id = sha1_smol::Sha1::from(stable_key).digest().to_string()[..10].to_string();
            active_ids.insert(id.clone());
            a["id"] = json!(id);
        }
        let tnow = crate::util::now();
        alarms.retain(|a| {
            acked
                .get(a.s("id"))
                .and_then(|v| v.as_f64())
                .map(|until| until < tnow)
                .unwrap_or(true)
        });
        let to_notify: Vec<(String, String)> = {
            let mut seen = self.notified.lock().unwrap_or_else(|e| e.into_inner());
            seen.retain(|id| active_ids.contains(id));
            let is_en = self.cfg.key("lang").as_str() != Some("es");
            let mut pending = Vec::new();
            for a in &alarms {
                if a.s("level") == "alarm" && seen.insert(a.s("id")) {
                    let text = if is_en {
                        a.s("text")
                    } else {
                        crate::l10n::es(&a.s("text"))
                    };
                    // "Claude 1 (Work): cuota semanal al 92 %" -> title "Claude 1 (Work)", body "Cuota semanal al 92 %".
                    let (title, body) = match text.split_once(": ") {
                        Some((t, b)) if !t.is_empty() && !b.is_empty() => {
                            let mut c = b.chars();
                            let body = match c.next() {
                                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                                None => b.to_string(),
                            };
                            (t.to_string(), body)
                        }
                        _ => (
                            if is_en { "Alarm" } else { "Alarma" }.to_string(),
                            text.clone(),
                        ),
                    };
                    pending.push((title, body));
                }
            }
            pending
        };
        for (title, body) in to_notify {
            crate::insights::notify(&title, &body);
        }
        let has_alarm = alarms.iter().any(|a| a.s("level") == "alarm");
        let has_warning = alarms.iter().any(|a| a.s("level") == "warning");
        let overall = if has_alarm {
            "ALARM"
        } else if has_warning {
            "WARNING"
        } else {
            "NORMAL"
        };
        let tnames: Vec<String> = templates()
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();

        let chain = self.chain_with(ts, Some(&summ));
        let master = Self::lead_of(&chain)
            .map(Value::from)
            .unwrap_or(Value::Null);
        let masters = json!(Self::masters_of(&chain));

        let mut snap = json!({
            "now": ts,
            "mode": self.cfg.key("mode"),
            "lang": self.cfg.key("lang"),
            "brains": out,
            "services": services,
            "machine": {
                "cpu": self.machine.cpu(),
                "ram": self.machine.ram(),
                "gpu": self.machine.gpu().map(|g| g.to_json()).unwrap_or(Value::Null),
                "heavy": self.machine.heavy(),
            },
            "rules": self.cfg.key("rules"),
            "alarms": alarms,
            "diary": self.diary.last(60),
            "overall": overall,
            "lan": {"enabled": self.cfg.key("lan").get("enabled").cloned().unwrap_or(json!(false)),
                     "port": self.cfg.key("port")},
            "templates": tnames,
            "chain": chain,
            "master": master,
            "masters": masters,
            "version": env!("CARGO_PKG_VERSION"),
        });
        if self.cfg.key("lang").as_str() == Some("es") {
            crate::l10n::snapshot_es(&mut snap);
        }
        snap
    }

    pub fn alarms(&self, brains: &[Value], services: &[Value]) -> Vec<Value> {
        let mut entries: Vec<AlarmEntry> = Vec::new();
        let brain_alarm = |level: &str, code: &str, bid: &str, text: String| AlarmEntry {
            level: level.into(),
            code: Some(code.into()),
            brain: Some(bid.into()),
            session: None,
            service: None,
            text,
        };

        for b in brains {
            let name = b.s("name");
            let bid = b.s("id");
            for l in b.arr("quota") {
                let Some(pct) = l.f("pct") else { continue };
                let grp = l.s("group");
                let msg = if grp == "quota" || grp.is_empty() {
                    format!("{name}: quota {pct:.0}%")
                } else {
                    format!("{name}: {grp} quota {pct:.0}%")
                };
                if pct >= 90.0 {
                    entries.push(brain_alarm("alarm", "quota", &bid, msg));
                } else if pct >= 75.0 {
                    entries.push(brain_alarm("warning", "quota", &bid, msg));
                }
            }
            let st = b.get("state").cloned().unwrap_or(json!({}));
            let eff = b.get("effective").cloned().unwrap_or(json!({}));
            if st.b("paused", false) {
                let level = if st.s("paused_by") != "auto" {
                    "warning"
                } else {
                    "alarm"
                };
                let mut text = format!("{name} paused");
                if let Some(r) = eff.arr("reasons").first().and_then(|v| v.as_str()) {
                    text = format!("{text} ({r})");
                }
                entries.push(brain_alarm(level, "paused", &bid, text));
            } else if eff.b("downgraded", false) {
                let r = eff
                    .arr("reasons")
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                entries.push(brain_alarm(
                    "warning",
                    "downgraded",
                    &bid,
                    format!("{} saving budget: {}", name, r),
                ));
            }
            if let Some(bal) = b.get("balance").filter(|v| v.is_object()) {
                if let Some(amount) = bal.f("amount") {
                    if amount < 1.0 {
                        let level = if amount < 0.3 { "alarm" } else { "warning" };
                        entries.push(brain_alarm(
                            level,
                            "balance",
                            &bid,
                            format!("{} balance {:.2} {}", name, amount, bal.s("currency")),
                        ));
                    }
                }
            }
            if let Some(w) = b.os("warning") {
                let code = if w.contains("signed in") {
                    "signin"
                } else {
                    "warning"
                };
                entries.push(brain_alarm(
                    "warning",
                    code,
                    &bid,
                    format!("{}: {}", name, w),
                ));
            }
            if eff.b("readonly", false) {
                entries.push(brain_alarm(
                    "info",
                    "readonly",
                    &bid,
                    format!("{}: solo lectura", name),
                ));
            }
            for s in b.arr("stuck") {
                let session = s.s("session");
                let reason = s.s("reason");
                entries.push(AlarmEntry {
                    level: "warning".into(),
                    code: Some("stuck".into()),
                    brain: Some(bid.clone()),
                    session: Some(session.clone()),
                    service: None,
                    text: format!("{}: agente atascado ({})", name, reason),
                });
                let reason_kind = reason
                    .split_whitespace()
                    .next()
                    .unwrap_or(&reason)
                    .to_string();
                let fresh = {
                    let mut seen = self.stuck_seen.lock().unwrap_or_else(|e| e.into_inner());
                    if seen.len() > 256 {
                        seen.clear();
                    }
                    seen.insert((session.clone(), reason_kind))
                };
                if fresh {
                    self.diary.add(
                        "MetaBrain",
                        &format!("{}: agente atascado ({})", name, reason),
                        "warning",
                    );
                }
            }
        }
        for s in services {
            let name = s.s("name");
            let running = s.b("running", false);
            let autorestart = s.b("autorestart", false);
            let paused = s.b("paused", false);
            if !running && autorestart {
                entries.push(AlarmEntry {
                    level: "warning".into(),
                    code: Some("service_down".into()),
                    brain: None,
                    session: None,
                    service: Some(name.clone()),
                    text: format!("{} is down", name),
                });
            } else if running {
                if let Some(hb) = s.f("heartbeat_ts") {
                    let t = now();
                    if t - hb > 900.0 && !paused {
                        entries.push(AlarmEntry {
                            level: "warning".into(),
                            code: Some("no_heartbeat".into()),
                            brain: None,
                            session: None,
                            service: Some(name.clone()),
                            text: format!(
                                "{}: no heartbeat for {} min",
                                name,
                                ((t - hb) / 60.0) as i64
                            ),
                        });
                    }
                }
            }
        }
        if let Some(g) = self.machine.gpu() {
            if g.temp >= 85 {
                entries.push(AlarmEntry {
                    level: "alarm".into(),
                    code: None,
                    brain: None,
                    session: None,
                    service: None,
                    text: format!("GPU {} °C", g.temp),
                });
            }
        }
        if let Some(ram) = self.machine.ram() {
            if ram >= 92.0 {
                entries.push(AlarmEntry {
                    level: "warning".into(),
                    code: None,
                    brain: None,
                    session: None,
                    service: None,
                    text: format!("RAM {}%", ram),
                });
            }
        }
        entries
            .into_iter()
            .filter_map(|a| serde_json::to_value(a).ok())
            .collect()
    }

    /// True when a manual change moves a brain to a more expensive setting.
    pub(super) fn raises_spend(&self, b: &Brain, field: &str, val: &Value) -> bool {
        let cur = self.effective_controls(b, None);
        match field {
            "model" if truthy(Some(val)) => {
                let lad = b.models();
                let v = val.as_str().unwrap_or("");
                let cur_model = cur.model.as_deref().unwrap_or("");
                match (
                    lad.iter().position(|x| x == v),
                    lad.iter().position(|x| x == cur_model),
                ) {
                    (Some(i), Some(j)) => i < j,
                    _ => false,
                }
            }
            "effort" => {
                let v = val.as_str().unwrap_or("");
                match EFFORTS.iter().position(|e| *e == v) {
                    Some(i) => {
                        let cure = cur.effort.as_deref().unwrap_or("");
                        match EFFORTS.iter().position(|e| *e == cure) {
                            Some(j) => i > j,
                            None => false,
                        }
                    }
                    None => false,
                }
            }
            "thinking" | "max_output" if truthy(Some(val)) => {
                let cur_val = if field == "thinking" {
                    &cur.thinking
                } else {
                    &cur.max_output
                };
                match cur_val.as_f64() {
                    Some(c) => val.as_f64().map(|x| x > c).unwrap_or(false),
                    None => false,
                }
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------- numeric & formatting helpers

pub(super) fn num_i64(v: Option<&Value>) -> i64 {
    v.and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

pub(super) fn num_f64(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn add_num(a: &Value, b: &Value) -> Value {
    match (a.as_i64(), b.as_i64()) {
        (Some(x), Some(y)) => json!(x + y),
        _ => json!(num_f64(Some(a)) + num_f64(Some(b))),
    }
}

/// Python `_addl`: element-wise sum, truncated to the shortest list.
pub(super) fn add_series(a: Option<&Value>, b: Option<&Value>) -> Vec<Value> {
    let av = a.and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let bv = b.and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if av.is_empty() {
        return bv;
    }
    if bv.is_empty() {
        return av;
    }
    av.iter()
        .zip(bv.iter())
        .map(|(x, y)| add_num(x, y))
        .collect()
}

fn merge_models(a: Option<&Value>, b: Option<&Value>) -> Value {
    let mut m = Map::new();
    for src in [a, b] {
        if let Some(o) = src.and_then(|v| v.as_object()) {
            for (k, v) in o {
                match m.get(k) {
                    Some(prev) => {
                        m.insert(k.clone(), add_num(prev, v));
                    }
                    None => {
                        m.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    Value::Object(m)
}

pub(super) fn comma(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

fn limit_text(v: &Value) -> String {
    if let Some(i) = v.as_i64() {
        return comma(i);
    }
    if let Some(f) = v.as_f64() {
        if f.fract() == 0.0 {
            return comma(f as i64);
        }
        return f.to_string();
    }
    v.to_string()
}

fn num_raw(v: &Value) -> String {
    if let Some(i) = v.as_i64() {
        i.to_string()
    } else if let Some(f) = v.as_f64() {
        f.to_string()
    } else {
        v.to_string()
    }
}

/// Python-style `str(value)` for the diary.
pub(super) fn disp(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        other => other.to_string(),
    }
}
