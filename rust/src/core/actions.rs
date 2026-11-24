//! HMI action dispatcher (`act`) and cross-brain session handoff into OpenAI/Ollama terminals.
use super::regulation::disp;
use super::Supervisor;
use crate::brains::Brain;
use crate::config::EFFORTS;
use crate::util::{now, truthy, J};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl Supervisor {
    // ---------- actions from the HMI ----------
    pub fn act(&self, d: &Value) -> String {
        let a = d.s("a");
        let b = d.os("brain").and_then(|id| self.brain(&id));

        if (a == "pause" || a == "resume") && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            let paused = a == "pause";
            self.state.set(
                &b.id,
                json!({"paused": paused, "paused_by": if paused { json!("user") } else { Value::Null }}),
            );
            self.diary.add("You", &format!("{a}d {}", b.name()), "info");
        } else if a == "ack_alarm" {
            // Hide an alarm for `hours` (default 8) without touching its cause.
            let id = d.s("id");
            if id.is_empty() {
                return "missing id".into();
            }
            let hours = d.get("hours").and_then(|v| v.as_f64()).unwrap_or(8.0);
            let mut acked = self.cfg.key("acked_alarms");
            if !acked.is_object() {
                acked = json!({});
            }
            let tnow = crate::util::now();
            if let Some(m) = acked.as_object_mut() {
                m.retain(|_, v| v.as_f64().map(|u| u > tnow).unwrap_or(false));
                m.insert(id, json!(tnow + hours * 3600.0));
            }
            self.cfg.set_key("acked_alarms", acked);
            self.cfg.save();
            return "ok".into();
        } else if a == "pause_all" {
            for x in self.brain_list() {
                self.state
                    .set(&x.id, json!({"paused": true, "paused_by": "user"}));
            }
            self.diary.add("You", "paused every brain", "warning");
        } else if a == "resume_all" {
            for x in self.brain_list() {
                self.state
                    .set(&x.id, json!({"paused": false, "paused_by": null}));
            }
            self.diary.add("You", "resumed every brain", "info");
        } else if a == "priority" {
            let known: HashSet<String> = self.brain_list().iter().map(|b| b.id.clone()).collect();
            let mut order: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for v in d.arr("order") {
                if let Some(id) = v.as_str() {
                    if known.contains(id) && seen.insert(id.to_string()) {
                        order.push(id.to_string());
                    }
                }
            }
            {
                let mut c = self.cfg.data.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(o) = c.as_object_mut() {
                    o.insert("priority".into(), json!(order));
                    if let Some(r) = d.get("ranks").and_then(|v| v.as_object()) {
                        let clean: Map<String, Value> = r
                            .iter()
                            .filter(|(k, v)| known.contains(k.as_str()) && v.is_i64())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        o.insert("ranks".into(), Value::Object(clean));
                    }
                }
            }
            self.cfg.save();
            let text = if order.is_empty() {
                "prioridad: automática".to_string()
            } else {
                format!("prioridad: {}", order.join(" → "))
            };
            self.diary.add("You", &text, "info");
        } else if a == "take_command" {
            let Some(id) = d.os("brain") else {
                return "unknown brain".into();
            };
            if self.brain(&id).is_none() {
                return "unknown brain".into();
            }
            let mut order = self.configured_priority();
            if order.is_empty() {
                order = self.priority_list();
            }
            order.retain(|x| x != &id);
            order.insert(0, id.clone());
            {
                let mut c = self.cfg.data.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(o) = c.as_object_mut() {
                    o.insert("priority".into(), json!(order));
                    // Alone at the top: a rank below every other level.
                    if let Some(r) = o.get_mut("ranks").and_then(|v| v.as_object_mut()) {
                        let low = r
                            .iter()
                            .filter(|(k, _)| *k != &id)
                            .filter_map(|(_, v)| v.as_i64())
                            .min()
                            .unwrap_or(1);
                        r.insert(id.clone(), json!(low - 1));
                    }
                }
            }
            self.cfg.save();
            let text = format!("mando: {}", order.join(" → "));
            self.diary.add("You", &text, "info");
        } else if a == "set" && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            let field = d.s("field");
            let mut val = d.get("value").cloned().unwrap_or(Value::Null);

            if field == "readonly" {
                let on = truthy(Some(&val));
                self.set_readonly(&b.id, on);
                self.diary
                    .add("You", &format!("{}: readonly = {}", b.name(), on), "info");
            } else {
                if field == "model" {
                    let in_ladder = val
                        .as_str()
                        .map(|v| b.models().iter().any(|m| m == v))
                        .unwrap_or(false);
                    let empty =
                        val.is_null() || val.as_str().map(|s| s.is_empty()).unwrap_or(false);
                    if !in_ladder && !empty && !truthy(d.get("custom")) {
                        return "unknown model for this brain".into();
                    }
                }
                if field == "effort" {
                    let v = val.as_str().unwrap_or("");
                    let empty =
                        val.is_null() || val.as_str().map(|s| s.is_empty()).unwrap_or(false);
                    if !empty && !EFFORTS.contains(&v) {
                        return "unknown effort".into();
                    }
                }
                if field == "thinking" || field == "max_output" {
                    let empty =
                        val.is_null() || val.as_str().map(|s| s.is_empty()).unwrap_or(false);
                    val = if empty {
                        Value::Null
                    } else {
                        match val
                            .as_i64()
                            .or_else(|| val.as_str().and_then(|s| s.parse::<i64>().ok()))
                            .or_else(|| val.as_f64().map(|f| f as i64))
                        {
                            Some(i) => json!(i),
                            None => Value::Null,
                        }
                    };
                }
                if !truthy(d.get("confirm")) && self.raises_spend(b, &field, &val) {
                    return "confirm".into();
                }
                let stored = if truthy(Some(&val)) {
                    val.clone()
                } else {
                    Value::Null
                };
                let mut kw = Map::new();
                kw.insert(field.clone(), stored);
                self.state.set(&b.id, Value::Object(kw));
                self.diary.add(
                    "You",
                    &format!("{}: {} = {}", b.name(), field, disp(&val)),
                    "info",
                );
            }
        } else if a == "budget" && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            self.cfg.with_brain(&b.id, |bc| {
                if !bc.get("budget").map(|v| v.is_object()).unwrap_or(false) {
                    bc["budget"] = json!({});
                }
                if let Some(bud) = bc.get_mut("budget").and_then(|v| v.as_object_mut()) {
                    for k in ["tokens_day", "usd_day", "quota_pct"] {
                        if let Some(v) = d.get(k) {
                            let empty = v.is_null()
                                || v.as_str().map(|s| s.is_empty()).unwrap_or(false)
                                || v.as_f64() == Some(0.0);
                            let nv = if empty {
                                Value::Null
                            } else if k == "usd_day" {
                                v.as_f64().map(|f| json!(f)).unwrap_or(Value::Null)
                            } else {
                                v.as_i64()
                                    .map(|i| json!(i))
                                    .or_else(|| v.as_f64().map(|f| json!(f as i64)))
                                    .unwrap_or(Value::Null)
                            };
                            bud.insert(k.to_string(), nv);
                        }
                    }
                }
            });
            self.cfg.save();
            self.diary
                .add("You", &format!("{}: budget updated", b.name()), "info");
        } else if a == "subagent_policy" && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            let typ = d.os("type").unwrap_or_else(|| "*".into());
            let mut pol_now = json!({});
            self.cfg.with_brain(&b.id, |bc| {
                if !bc.get("subagents").map(|v| v.is_object()).unwrap_or(false) {
                    bc["subagents"] = json!({});
                }
                if let Some(subs) = bc.get_mut("subagents").and_then(|v| v.as_object_mut()) {
                    let mut remove = false;
                    {
                        let entry = subs.entry(typ.clone()).or_insert_with(|| json!({}));
                        if !entry.is_object() {
                            *entry = json!({});
                        }
                        if let Some(pol) = entry.as_object_mut() {
                            for k in ["model", "max_parallel", "allow"] {
                                if let Some(v) = d.get(k) {
                                    let empty = v.is_null()
                                        || v.as_str().map(|s| s.is_empty()).unwrap_or(false);
                                    if empty {
                                        pol.remove(k);
                                    } else {
                                        let nv = match k {
                                            "max_parallel" => json!(v
                                                .as_i64()
                                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                                                .unwrap_or(0)),
                                            "allow" => json!(truthy(Some(v))),
                                            _ => v.clone(),
                                        };
                                        pol.insert(k.to_string(), nv);
                                    }
                                }
                            }
                            pol_now = Value::Object(pol.clone());
                            remove = pol.is_empty();
                        }
                    }
                    if remove {
                        subs.remove(&typ);
                    }
                }
            });
            self.cfg.save();
            let who = if d.s("by") == "pilot" { "Pilot" } else { "You" };
            let text = if pol_now.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                "default".to_string()
            } else {
                pol_now.to_string()
            };
            self.diary.add(
                who,
                &format!("{}: subagents '{}' → {}", b.name(), typ, text),
                "info",
            );
        } else if a == "pilot"
            && b.is_some()
            && matches!(d.s("mode").as_str(), "off" | "advise" | "act")
        {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            let mode = d.s("mode");
            self.cfg.with_brain(&b.id, |bc| {
                bc["pilot"] = Value::String(mode.clone());
            });
            self.cfg.save();
            self.diary
                .add("You", &format!("{}: pilot {}", b.name(), mode), "info");
        } else if a == "apply_rec" && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            return self.advisor.apply(self, &b.id, &d.s("rec"));
        } else if a == "mode" && d.s("mode") == "max" && !truthy(d.get("confirm")) {
            return "confirm".into();
        } else if a == "mode" && matches!(d.s("mode").as_str(), "eco" | "normal" | "max") {
            let mode = d.s("mode");
            {
                let mut c = self.cfg.data.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(o) = c.as_object_mut() {
                    o.insert("mode".into(), Value::String(mode.clone()));
                }
            }
            self.cfg.save();
            for x in self.brain_list() {
                let lad = x.models();
                let model = match mode.as_str() {
                    "eco" => lad.last().cloned(),
                    _ => lad.first().cloned(),
                };
                let effort = match mode.as_str() {
                    "eco" => "low",
                    "max" => "high",
                    _ => "medium",
                };
                self.state
                    .set(&x.id, json!({"model": model, "effort": effort}));
            }
            self.diary.add(
                "You",
                &format!("mode {} for every brain", mode.to_uppercase()),
                "info",
            );
        } else if a == "rules" {
            {
                let mut c = self.cfg.data.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(o) = c.as_object_mut() {
                    if !o.get("rules").map(|v| v.is_object()).unwrap_or(false) {
                        o.insert("rules".into(), json!({}));
                    }
                    if let Some(r) = o.get_mut("rules").and_then(|v| v.as_object_mut()) {
                        for k in ["downgrade_at", "pause_at"] {
                            if let Some(v) = d.get(k) {
                                if let Some(n) = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
                                {
                                    r.insert(k.to_string(), json!(n.clamp(1, 200)));
                                }
                            }
                        }
                        if let Some(v) = d.get("auto_resume") {
                            r.insert("auto_resume".into(), json!(truthy(Some(v))));
                        }
                    }
                }
            }
            self.cfg.save();
        } else if a == "organize" {
            // Applied on the next tick, even without automatic mode.
            self.organize_async(false);
        } else if matches!(
            a.as_str(),
            "svc_start" | "svc_stop" | "svc_pause" | "svc_resume"
        ) {
            let sid = d.s("service");
            let s = self.services.items().into_iter().find(|x| x.s("id") == sid);
            let Some(s) = s else {
                return "unknown service".into();
            };
            let r = match a.as_str() {
                "svc_start" => {
                    self.services.set_manual_off(&sid, false);
                    self.services.start(&s)
                }
                "svc_stop" => {
                    self.services.set_manual_off(&sid, true);
                    self.services.stop(&s)
                }
                _ => self.services.set_pause(&s, a == "svc_pause", "manual"),
            };
            let name = s.os("name").unwrap_or_else(|| sid.clone());
            self.diary.add("You", &format!("{}: {}", name, r), "info");
            return r;
        } else if a == "continue" && b.is_some() {
            let Some(from) = b.as_ref() else {
                return "unknown action".into();
            };
            if let Some(tid) = d.os("target") {
                let Some(target) = self.brain(&tid) else {
                    return "unknown target".into();
                };
                if target.kind == "claude_code" {
                    let dir = target.config_dir();
                    let (msg, _) = self.failover.continue_session(
                        self,
                        from,
                        &d.s("session"),
                        Some(&dir),
                        true,
                    );
                    return msg;
                }
                if target.kind == "openai" || target.kind == "ollama" {
                    return self.continue_in_deepseek(from, &d.s("session"), &target);
                }
                return "target not supported".into();
            }
            let target = d.os("account");
            let (msg, _) = self.failover.continue_session(
                self,
                from,
                &d.s("session"),
                target.as_deref(),
                true,
            );
            return msg;
        } else if a == "switch_account" && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            match d.os("account") {
                Some(target) => self.failover.switch(self, b, &target, "manual", now()),
                None => {
                    self.state.set(
                        &b.id,
                        json!({"account": null, "account_since": null, "account_until": null, "account_reason": null}),
                    );
                    self.diary.add(
                        "You",
                        &format!("{}: back to the main account", b.name()),
                        "info",
                    );
                }
            }
        } else if a == "abort" && b.is_some() {
            let Some(b) = b.as_ref() else {
                return "unknown action".into();
            };
            let sid = d.s("session");
            let msg = match b.abort(&sid) {
                Ok(m) => m,
                Err(e) => e,
            };
            self.diary.add(
                "You",
                &format!("{}: abort {} → {}", b.name(), sid, msg),
                "warning",
            );
            return msg;
        } else if a == "kill" {
            let pid = d
                .get("pid")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(0);
            let heavy = self.machine.heavy();
            let found = heavy
                .iter()
                .any(|h| h.get("pid").and_then(|v| v.as_u64()) == Some(pid.max(0) as u64));
            if !found {
                return "only heavy render processes can be stopped from here".into();
            }
            self.machine.kill(pid.max(0) as u32);
            self.diary
                .add("You", &format!("stopped process {pid}"), "warning");
        } else if a == "lang" && matches!(d.s("lang").as_str(), "es" | "en") {
            let lang = d.s("lang");
            {
                let mut c = self.cfg.data.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(o) = c.as_object_mut() {
                    o.insert("lang".into(), Value::String(lang));
                }
            }
            self.cfg.save();
        } else if a == "config" {
            let errs = self
                .cfg
                .replace(d.get("config").cloned().unwrap_or(json!({})));
            if !errs.is_empty() {
                return errs.join("; ");
            }
            self.rebuild();
            let n = self.brain_list().len();
            self.diary
                .add("You", &format!("configuration saved: {} brains", n), "info");
        } else {
            return "unknown action".into();
        }

        self.write_gate(None);
        "ok".into()
    }

    /// Continue a Claude session in an OpenAI/Ollama brain: handoff + visible terminal.
    fn continue_in_deepseek(
        &self,
        from: &Arc<Brain>,
        session_id: &str,
        target: &Arc<Brain>,
    ) -> String {
        let sessions = from.my_sessions();
        let s = sessions
            .into_iter()
            .find(|s| s.id == session_id || s.id.starts_with(session_id));
        let Some(s) = s else {
            return "session not found".into();
        };

        let lang = self
            .cfg
            .key("lang")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "en".to_string());
        let account = self
            .state
            .get(&from.id)
            .os("account")
            .unwrap_or_else(|| from.config_dir());
        let (_files, _info) = crate::handoff::write_handoff(
            &s.path,
            &from.name(),
            &account,
            &format!("relevo de mando a {}", target.name()),
            &lang,
            None,
        );

        let cwd = if !s.cwd.is_empty() && Path::new(&s.cwd).is_dir() {
            PathBuf::from(&s.cwd)
        } else {
            crate::util::home()
        };

        let raw = self.cfg.key("deepseek_tui");
        let argv: Vec<String> = match raw.as_array() {
            Some(a) if !a.is_empty() => a
                .iter()
                .filter_map(|v| v.as_str().map(crate::util::expand_s))
                .collect(),
            _ => {
                if cfg!(windows) {
                    [
                        "powershell",
                        "-NoExit",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        "%APPDATA%\\npm\\dsh.ps1",
                        "tui",
                    ]
                    .iter()
                    .map(|x| crate::util::expand_s(x))
                    .collect()
                } else {
                    ["dsh", "tui"]
                        .iter()
                        .map(|x| crate::util::expand_s(x))
                        .collect()
                }
            }
        };
        if argv.is_empty() {
            return "deepseek_tui is empty".into();
        }

        let spawned = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.arg("/c").arg("start").arg("MetaBrain").args(&argv);
            c.current_dir(&cwd);
            c.spawn()
        } else {
            let mut c = std::process::Command::new("x-terminal-emulator");
            c.arg("-e").args(&argv);
            c.current_dir(&cwd);
            c.spawn()
        };
        if let Err(e) = spawned {
            return format!("handoff escrito; abre una terminal tú mismo ({e})");
        }

        let short: String = s.id.chars().take(8).collect();
        self.diary.add(
            "You",
            &format!(
                "{}: sesión {short} continúa en {}",
                from.name(),
                target.name()
            ),
            "info",
        );
        self.write_gate(None);
        "ok".into()
    }
}
