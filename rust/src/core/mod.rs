//! The supervisor: perceives every brain, integrates one state, regulates by rules, executes HMI actions.
//! Port of metabrain/core.py plus read-only brains, stuck-agent alarms and abort.
use crate::advisor::Advisor;
use crate::brains::Brain;
use crate::config::Config;
use crate::failover::Failover;
use crate::quota::Predictor;
use crate::services::{Machine, Services};
use crate::store::{Diary, Ledger, State};
use crate::util::{data_dir, log, now, secret, J};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

mod actions;
mod chain;
mod regulation;
#[cfg(test)]
mod tests;

/// Typed command-chain entry serialized into snapshots, routing, and the HMI map.
#[derive(Clone, Debug, Serialize)]
pub struct ChainEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub available: bool,
    pub reason: String,
    pub pct: Option<f64>,
    pub eta_min: Option<f64>,
    pub master: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<bool>,
    pub rank: u32,
}

/// Typed effective runtime controls computed for one brain (model, effort, budget ratio, pause).
#[derive(Clone, Debug, Serialize)]
pub struct EffectiveControls {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub thinking: Value,
    pub max_output: Value,
    pub paused: bool,
    pub paused_by: Value,
    pub downgraded: bool,
    pub ratio: Option<f64>,
    pub reasons: Vec<String>,
    pub readonly: bool,
}

/// Typed alarm/warning entry produced by `Supervisor::alarms`.
#[derive(Clone, Debug, Serialize)]
pub struct AlarmEntry {
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub text: String,
}

type TaskLedgerSeen = HashMap<String, (i64, i64, f64)>;

pub struct Supervisor {
    pub cfg: Arc<Config>,
    pub state: Arc<State>,
    pub ledger: Arc<Ledger>,
    pub diary: Arc<Diary>,
    pub machine: Arc<Machine>,
    pub services: Arc<Services>,
    pub predictor: Arc<Predictor>,
    pub advisor: Advisor,
    pub failover: Failover,
    /// Task router: which brain/model/effort per task level, learned per project.
    pub router: crate::router::Router,
    pub brains: RwLock<Vec<Arc<Brain>>>,
    pub gate_path: PathBuf,
    pub(super) stuck_seen: Mutex<HashSet<(String, String)>>,
    /// Alarm ids already shown as a native notification.
    pub(super) notified: Mutex<HashSet<String>>,
    /// Tasks launched from the HMI.
    pub tasks: Arc<crate::tasks::TaskManager>,
    /// Parallel orchestration engine (DAG + multi-brain waves).
    pub parallel: Arc<crate::parallel::ParallelEngine>,
    sent: Mutex<HashMap<String, f64>>,
    pub(super) last_master: Mutex<Option<String>>,
    /// Latest AI-generated work architecture plan (scoped per Supervisor instance).
    pub(super) model_plan: Arc<Mutex<Value>>,
    /// Guard preventing overlapping async organizer runs on this Supervisor.
    pub(super) organize_busy: Arc<AtomicBool>,
    /// Previous unavailability reason per brain, for comeback notifications.
    pub(super) comeback_prev: Mutex<HashMap<String, String>>,
    /// Last set of available brain IDs evaluated by `auto_architect`.
    pub(super) auto_arch_last: Mutex<String>,
    /// High-water mark of task `(tokens, cached, usd)` already recorded into the ledger.
    pub(super) task_ledger_seen: Mutex<Option<TaskLedgerSeen>>,
}

impl Supervisor {
    pub fn new(cfg: Arc<Config>) -> Arc<Supervisor> {
        let state = Arc::new(State::new(None));
        let ledger = Arc::new(Ledger::new(
            None,
            cfg.key("history_days").as_i64().unwrap_or(30),
        ));
        let diary = Arc::new(Diary::new(500));
        let machine = Arc::new(Machine::new());
        let services = Arc::new(Services::new(cfg.get(), machine.clone(), diary.clone()));
        let predictor = Arc::new(Predictor::new(None));
        let advisor = Advisor::new(None);
        let failover = Failover::new(predictor.clone());
        let gate_path = data_dir().join("gate.json");

        let sup = Arc::new(Supervisor {
            cfg,
            state,
            ledger,
            diary,
            machine,
            services,
            predictor,
            advisor,
            failover,
            router: crate::router::Router::new(None),
            brains: RwLock::new(Vec::new()),
            gate_path,
            stuck_seen: Mutex::new(HashSet::new()),
            notified: Mutex::new(HashSet::new()),
            tasks: Arc::new(crate::tasks::TaskManager::new()),
            parallel: Arc::new(crate::parallel::ParallelEngine::new()),
            sent: Mutex::new(HashMap::new()),
            last_master: Mutex::new(None),
            model_plan: Arc::new(Mutex::new(Value::Null)),
            organize_busy: Arc::new(AtomicBool::new(false)),
            comeback_prev: Mutex::new(HashMap::new()),
            auto_arch_last: Mutex::new(String::new()),
            task_ledger_seen: Mutex::new(None),
        });
        sup.rebuild();

        // Notifications: the diary calls back into the supervisor (weakly, to avoid a cycle).
        let weak = Arc::downgrade(&sup);
        let listener: crate::store::Listener = Box::new(move |e: &Value| {
            if let Some(s) = weak.upgrade() {
                s.notify(e);
            }
        });
        if let Ok(mut ls) = sup.diary.listeners.lock() {
            ls.push(Arc::new(listener));
        }
        sup
    }

    // ---------- setup ----------
    pub fn rebuild(&self) {
        let new = crate::brains::build(&self.cfg, &self.ledger, &self.diary);
        {
            let mut guard = self.brains.write().unwrap_or_else(|e| e.into_inner());
            *guard = new;
        }
        // Global cap for tasks running at once (config "max_parallel_total", default 8).
        let total = self
            .cfg
            .key("max_parallel_total")
            .as_u64()
            .unwrap_or(8)
            .max(1) as usize;
        self.tasks.set_global_limit(total);
        self.tasks.set_verify_cfg(self.cfg.get());
        self.services.update_cfg(self.cfg.get());
        self.write_gate(None);
    }

    pub fn brain(&self, bid: &str) -> Option<Arc<Brain>> {
        self.brain_list().into_iter().find(|b| b.id == bid)
    }

    pub(super) fn brain_list(&self) -> Vec<Arc<Brain>> {
        let g = self.brains.read().unwrap_or_else(|e| e.into_inner());
        (*g).clone()
    }

    // ---------- loops ----------
    pub fn start(self: &Arc<Self>) {
        // A routed task that fails its check goes to the next model, with the failure as context.
        let weak = Arc::downgrade(self);
        self.tasks.set_verify_cfg(self.cfg.get());
        self.tasks
            .set_escalator(Arc::new(move |meta: &Value, failure: &str| {
                let sup = weak.upgrade()?;
                sup.escalate_task(meta, failure)
            }));
        let s1 = self.clone();
        spawn_loop(move || s1.poll_fast(), 3.0, 0.0);
        let s2 = self.clone();
        spawn_loop(move || s2.poll_slow(), 60.0, 0.2);
        let s3 = self.clone();
        spawn_loop(move || s3.machine.poll(), 8.0, 0.0);
        let s4 = self.clone();
        spawn_loop(move || s4.regulate(), 20.0, 25.0);
        self.diary
            .add("MetaBrain", "awake: reading every brain", "info");
    }

    pub fn poll_fast(&self) {
        for b in self.brain_list() {
            b.poll();
        }
    }

    pub fn poll_slow(&self) {
        let brains = self.brain_list();
        std::thread::scope(|scope| {
            for b in &brains {
                let st = self.state.get(&b.id);
                let active_acct = (b.kind == "claude_code")
                    .then(|| st.os("account"))
                    .flatten();
                let failover = &self.failover;
                scope.spawn(move || {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        b.slow_poll();
                        if let Some(acct) = active_acct.as_deref() {
                            let _ = failover.quota_for(b, acct, true);
                        }
                    }))
                    .is_err()
                    {
                        log(&format!("slow_poll: panic en {}", b.id));
                    }
                });
            }
        });
    }

    // ---------- notifications ----------
    pub fn notify(&self, e: &Value) {
        let tg = self.cfg.key("telegram");
        if !tg.b("enabled", false) {
            return;
        }
        let level = e.s("level");
        if level != "alarm" && level != "warning" {
            return;
        }
        let text = e.s("text");
        let t = now();
        {
            let mut sent = self.sent.lock().unwrap_or_else(|x| x.into_inner());
            if let Some(last) = sent.get(&text) {
                if t - *last < 1800.0 {
                    return;
                }
            }
            sent.insert(text.clone(), t);
            if sent.len() > 200 {
                sent.retain(|_, v| t - *v < 1800.0);
            }
        }
        let Some(tok) = secret(&tg.s("token_env")) else {
            return;
        };
        let chat = tg.s("chat_id");
        if tok.is_empty() || chat.is_empty() {
            return;
        }
        let icon = if level == "alarm" { "🔴" } else { "🟠" };
        let msg = format!("{icon} MetaBrain · {text}");
        let url = format!("https://api.telegram.org/bot{tok}/sendMessage");
        std::thread::spawn(move || {
            let r = ureq::post(&url)
                .timeout(Duration::from_secs(10))
                .send_form(&[("chat_id", chat.as_str()), ("text", msg.as_str())]);
            if let Err(err) = r {
                log(&format!("telegram: {err}"));
            }
        });
    }

    // ---------- routing & learning ----------
    /// Route one task with the current chain of command (used by the API and the CLI).
    /// Route a task that will run in `cwd`: with an automatic check there, start cheaper.
    pub fn route_task_in(
        &self,
        project: &str,
        task: &str,
        cwd: &str,
        mode: &str,
        excluded: &[Value],
    ) -> Value {
        let cfg = crate::router::task_routing_config(self.cfg.get());
        let mut chain = self.chain(crate::util::now());
        chain.retain(|b| {
            !excluded.contains(&b["id"]) && crate::tasks::can_run_task_on(&b.s("kind"), mode)
        });
        let expanded_cwd = crate::util::expand_s(cwd);
        let verified = mode != "plan"
            && !expanded_cwd.is_empty()
            && crate::verify::detect(std::path::Path::new(&expanded_cwd), &cfg).is_some();
        let v = crate::learn::route_auto_in(&self.router, &cfg, &chain, project, task, verified);
        self.diary.add(
            "MetaBrain",
            &format!(
                "Enrutó \"{}\" (nivel {}) a {}",
                task.chars().take(50).collect::<String>(),
                v["level"],
                v["pick"]["candidate"].as_str().unwrap_or("?")
            ),
            "info",
        );
        v
    }

    pub(crate) fn route_follow_up(&self, meta: &Value, text: &str, has_image: bool) -> Value {
        let cfg = crate::router::task_routing_config(self.cfg.get());
        let mut chain = self.chain(crate::util::now());
        let excluded = meta.arr("route_excluded");
        chain.retain(|b| {
            crate::tasks::can_run_task_on(&b.s("kind"), &meta.s("mode"))
                && (!has_image || b.s("kind") != "openai")
                && !excluded.contains(&b["id"])
        });
        let (level, why) = crate::router::classify(text);
        let current = meta.s("brain");
        if !why.starts_with("palabra clave:")
            && chain
                .iter()
                .any(|b| b.s("id") == current && b.b("available", false))
        {
            return json!({"brain": current});
        }
        let project = meta.os("cwd_project").unwrap_or_else(|| meta.s("cwd"));
        let project = std::path::Path::new(&project)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("general");
        self.router.route(&cfg, &chain, project, text, Some(level))["pick"].clone()
    }

    /// Start the next attempt of a task whose check failed. Returns the new task id.
    pub fn escalate_task(&self, meta: &Value, failure: &str) -> Option<String> {
        let cfg = crate::router::task_routing_config(self.cfg.get());
        let mut chain = self.chain(crate::util::now());
        let provider_failure = meta.s("verify_cmd").is_empty();
        let mut excluded = meta.arr("route_excluded");
        if provider_failure && !meta.s("brain").is_empty() && !excluded.contains(&meta["brain"]) {
            excluded.push(meta["brain"].clone());
        }
        chain.retain(|b| {
            (!provider_failure || b["id"] != meta["brain"])
                && !excluded.contains(&b["id"])
                && crate::tasks::can_run_task_on(&b.s("kind"), &meta.s("mode"))
        });
        let cwd = {
            let c = meta.s("cwd_project");
            if c.is_empty() {
                meta.s("cwd")
            } else {
                c
            }
        };
        let project = std::path::Path::new(&cwd)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "general".into());
        let base = {
            let b = meta.s("base_prompt");
            if b.is_empty() {
                meta.s("prompt")
            } else {
                b
            }
        };
        let level = meta
            .get("route_level")
            .and_then(|v| v.as_u64())
            .unwrap_or(3) as u8;
        let v = if provider_failure {
            crate::learn::route_auto_in(&self.router, &cfg, &chain, &project, &base, false)
        } else {
            crate::learn::escalate_after(
                &self.router,
                &cfg,
                &chain,
                &project,
                &base,
                level,
                &meta.s("route_cand"),
            )
        };
        let pick = v.get("pick").cloned().unwrap_or(Value::Null);
        if v.is_null() || pick.s("brain").is_empty() {
            self.diary.add(
                "MetaBrain",
                &format!(
                    "\"{}\": no hay un modelo superior disponible para reintentar",
                    meta.s("title")
                ),
                "warning",
            );
            return None;
        }
        let bcfg = self.cfg.brain(&pick.s("brain"))?;
        let check = meta.s("verify_cmd");
        let prompt = if provider_failure {
            base.clone()
        } else {
            format!(
                "{base}\n\n---\nUn intento anterior ({}) terminó, pero la verificación `{check}` falló. Revisa el estado actual de los archivos, corrige la causa y deja la verificación en verde. Salida de la verificación:\n```\n{}\n```",
                meta.s("route_cand"),
                crate::verify::tail(failure, 3000)
            )
        };
        // Continue where the previous attempt left the files (its worktree, if it had one).
        if !meta.s("wt_path").is_empty() && crate::tasks::task_work_dir(meta).is_err() {
            return None;
        }
        let wt_owner = meta.os("worktree_owner").unwrap_or_else(|| meta.s("id"));
        let mut extra = json!({
            "route_id": v.s("id"), "route_level": v["level"], "route_cand": pick.s("candidate"),
            "base_prompt": base, "escalations": meta.get("escalations").and_then(|x| x.as_u64()).unwrap_or(0) + 1,
            "escalated_from": meta.s("id"), "cwd_project": cwd,
            "route_excluded": excluded,
            "worktree_owner": wt_owner,
        });
        for key in ["wt_path", "branch", "wt_base"] {
            extra[key] = meta[key].clone();
        }
        let dir = meta.s("cwd");
        match self.tasks.start_with(
            &bcfg,
            &pick.s("model"),
            &pick.s("effort"),
            &prompt,
            &dir,
            &meta.s("mode"),
            meta.b("worktree", false),
            extra,
        ) {
            Ok(m) => {
                let text = if provider_failure {
                    format!(
                        "\"{}\" falló por error del proveedor; reintenta con {} en {}",
                        meta.s("title"),
                        pick.s("model"),
                        bcfg.s("name")
                    )
                } else {
                    format!(
                        "\"{}\" no pasó {check}; reintenta con {} en {}",
                        meta.s("title"),
                        pick.s("model"),
                        bcfg.s("name")
                    )
                };
                self.diary.add("MetaBrain", &text, "warning");
                crate::insights::notify("MetaBrain", &text);
                m.os("id")
            }
            Err(e) => {
                self.diary.add(
                    "MetaBrain",
                    &format!("no se pudo reintentar: {e}"),
                    "warning",
                );
                None
            }
        }
    }

    pub fn route_task(&self, project: &str, task: &str, level: Option<u8>) -> Value {
        let cfg = crate::router::task_routing_config(self.cfg.get());
        let mut chain = self.chain(crate::util::now());
        chain.retain(|b| crate::tasks::can_run_task_on(&b.s("kind"), "plan"));
        let v = self.router.route(&cfg, &chain, project, task, level);
        self.diary.add(
            "MetaBrain",
            &format!(
                "Enrutó \"{}\" (nivel {}) a {}",
                task.chars().take(50).collect::<String>(),
                v["level"],
                v["pick"]["candidate"].as_str().unwrap_or("?")
            ),
            "info",
        );
        v
    }

    /// Shadow-mode report and continuous handoffs, for the HMI.
    pub fn learn_view(&self, project: Option<&str>) -> Value {
        let cfg = self
            .cfg
            .data
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut v = crate::learn::shadow_stats(project, 2000);
        if let Some(m) = v.as_object_mut() {
            m.insert(
                "shadow_on".into(),
                json!(cfg
                    .get("router")
                    .and_then(|r| r.get("shadow"))
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true)),
            );
            m.insert(
                "llm_classify".into(),
                json!(cfg
                    .get("router")
                    .and_then(|r| r.get("llm_classify"))
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true)),
            );
            m.insert("relays".into(), crate::relay::latest(20));
        }
        v
    }

    pub fn router_table(&self, project: Option<&str>) -> Value {
        let cfg = crate::router::task_routing_config(self.cfg.get());
        self.router.table(&cfg, project)
    }
}

fn spawn_loop<F: Fn() + Send + 'static>(f: F, every: f64, delay: f64) {
    std::thread::spawn(move || {
        if delay > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(delay));
        }
        loop {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(&f)).is_err() {
                log("supervisor loop: panic");
            }
            std::thread::sleep(Duration::from_secs_f64(every));
        }
    });
}
