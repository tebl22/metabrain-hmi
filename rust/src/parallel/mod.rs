//! Parallel Orchestration Engine (Motor Paralelo) for MetaBrain.
//!
//! Combines four research-backed mechanisms into one lightweight, resilient Rust engine:
//! - **LLMCompiler DAG**: decomposes a task into a validated acyclic dependency graph (`deps`),
//!   launching every ready branch concurrently as soon as its dependencies finish.
//! - **Nexa Parallel-to-Sequential**: runs parallel exploration/analysis across available Tier 2
//!   (Gemini) and Tier 3 (DeepSeek) brains first, reserving Tier 1 (Claude/ChatGPT) for final
//!   arbitration, synthesis, or verified implementation.
//! - **FATE + RouteBalance Assignment**: scores candidate brains per node using tier affinity,
//!   live parallel queue load (`queue_info`), cross-provider wave diversity, quota headroom, and
//!   project cache locality (`cwd`).
//! - **AOrchestra Scoped Context**: passes only compact, budgeted summaries of upstream dependency
//!   outputs into downstream nodes instead of raw transcripts.

mod planner;
mod scheduler;
#[cfg(test)]
mod tests;

pub use planner::{
    available_brains, heuristic_plan, parse_plan_json, pick_director, topological_waves,
    validate_and_finalize_dag,
};
pub use scheduler::{assign_brain, build_branch_prompt};

use crate::core::Supervisor;
use crate::docs::is_image as is_image_path;
use crate::tasks::{can_edit, mk_event_pub, turn_busy};
use crate::util::{data_dir, log, now, read_json, write_json, J};
use planner::plan_task;
use scheduler::{
    compact_thread_context, compose_final_answer, pick_model_and_effort, recent_brains_for_cwd,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(super) const MAX_BRANCHES: usize = 5;
pub(super) const SUMMARY_BUDGET: usize = 2800;
pub(super) const SYNTH_DEP_BUDGET: usize = 2400;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanNode {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default = "default_tier")]
    pub tier: u8,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default)]
    pub files: Vec<String>,
}

fn default_tier() -> u8 {
    2
}
fn default_mode() -> String {
    "plan".into()
}
fn default_role() -> String {
    "analyze".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanSpec {
    pub strategy: String, // "dag" | "fanout" | "direct"
    pub nodes: Vec<PlanNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchState {
    pub id: String,
    pub title: String,
    pub role: String,
    pub tier: u8,
    pub mode: String,
    pub deps: Vec<String>,
    pub files: Vec<String>,
    pub prompt: String,
    pub worktree: bool,
    pub task_id: String,
    pub brain: String,
    pub brain_name: String,
    pub model: String,
    pub status: String, // "pending" | "running" | "done" | "failed" | "stopped" | "skipped"
    pub summary: String,
    pub tokens: i64,
    pub usd: f64,
    pub error: String,
    pub wave: usize,
    pub started_at: f64,
    pub finished_at: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParallelRun {
    pub id: String,
    pub prompt: String,
    pub cwd: String,
    pub mode: String,
    pub worktree: bool,
    pub strategy: String,
    pub director: String,
    pub director_name: String,
    pub status: String, // "planning" | "running" | "synthesizing" | "review" | "done" | "failed" | "stopped"
    pub waves_total: usize,
    pub branches: Vec<BranchState>,
    pub total_tokens: i64,
    pub total_usd: f64,
    pub created: f64,
    pub updated: f64,
    pub error: String,
}

pub(super) fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn inherit_worktree(parent: &mut Value, child: &Value) {
    if child.s("wt_path").is_empty() || child.s("branch").is_empty() {
        return;
    }
    for key in ["wt_path", "branch", "wt_base"] {
        parent[key] = child[key].clone();
    }
    if parent.s("worktree_owner").is_empty() {
        parent["worktree_owner"] = json!(child.s("id"));
    }
}

pub struct ParallelEngine {
    root: PathBuf,
    runs: Mutex<HashMap<String, ParallelRun>>,
    io_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    stopped: Mutex<HashSet<String>>,
    transitions: Mutex<()>,
}

impl ParallelEngine {
    pub fn new() -> ParallelEngine {
        let root = data_dir().join("parallel");
        let _ = fs::create_dir_all(&root);
        let engine = ParallelEngine {
            root,
            runs: Mutex::new(HashMap::new()),
            io_locks: Mutex::new(HashMap::new()),
            stopped: Mutex::new(HashSet::new()),
            transitions: Mutex::new(()),
        };
        engine.recover();
        engine
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        crate::util::valid_id(id).then(|| self.root.join(format!("{id}.json")))
    }

    fn run_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut map = self.io_locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn save(&self, run: &ParallelRun) {
        let Some(path) = self.path(&run.id) else {
            log(&format!(
                "parallel: refusing to save invalid run id '{}'",
                run.id
            ));
            return;
        };
        let io_lock = self.run_lock(&run.id);
        let _io_guard = io_lock.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut g = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            g.insert(run.id.clone(), run.clone());
        }
        if let Err(e) = fs::create_dir_all(&self.root) {
            log(&format!(
                "parallel: failed creating {}: {e}",
                self.root.display()
            ));
            return;
        }
        match serde_json::to_value(run) {
            Ok(v) => {
                if let Err(e) = write_json(&path, &v) {
                    log(&format!("parallel: failed writing {}: {e}", path.display()));
                }
            }
            Err(e) => log(&format!("parallel: failed serializing run {}: {e}", run.id)),
        }
    }

    /// Recover runs that were still active when MetaBrain restarted.
    fn recover(&self) {
        let Ok(rd) = fs::read_dir(&self.root) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let Some(v) = read_json(&e.path()) else {
                continue;
            };
            let Ok(mut run) = serde_json::from_value::<ParallelRun>(v) else {
                continue;
            };
            if !crate::util::valid_id(&run.id) {
                continue;
            }
            if matches!(run.status.as_str(), "planning" | "running" | "synthesizing") {
                run.status = "failed".into();
                run.error = "MetaBrain restarted during parallel execution".into();
                run.updated = now();
                for b in &mut run.branches {
                    if matches!(b.status.as_str(), "pending" | "running") {
                        b.status = "failed".into();
                    }
                }
                self.save(&run);
            } else {
                let mut g = self.runs.lock().unwrap_or_else(|e| e.into_inner());
                g.insert(run.id.clone(), run);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        let path = self.path(id)?;
        {
            let g = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(r) = g.get(id) {
                return serde_json::to_value(r).ok();
            }
        }
        read_json(&path)
    }

    pub fn list(&self, limit: usize) -> Value {
        let mut items: Vec<ParallelRun> = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.ends_with(".json") {
                    continue;
                }
                if let Some(v) = read_json(&e.path()) {
                    if let Ok(r) = serde_json::from_value::<ParallelRun>(v) {
                        items.push(r);
                    }
                }
            }
        }
        items.sort_by(|a, b| {
            b.updated
                .partial_cmp(&a.updated)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if limit > 0 && items.len() > limit {
            items.truncate(limit);
        }
        serde_json::to_value(items).unwrap_or_else(|_| json!([]))
    }

    /// Fast in-memory snapshot of recent parallel runs (zero disk I/O for UI polling).
    pub fn list_cached(&self, limit: usize) -> Vec<ParallelRun> {
        let mut items: Vec<ParallelRun> = self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        items.sort_by(|a, b| {
            b.updated
                .partial_cmp(&a.updated)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if limit > 0 && items.len() > limit {
            items.truncate(limit);
        }
        items
    }

    pub fn is_stopped(&self, id: &str) -> bool {
        self.stopped.lock().map(|s| s.contains(id)).unwrap_or(false)
    }

    fn is_current(&self, run: &ParallelRun) -> bool {
        !self.is_stopped(&run.id)
            && self
                .runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&run.id)
                .is_some_and(|current| current.created == run.created)
    }

    /// Stop an active parallel run and every active child task belonging to it.
    pub fn stop(&self, sup: &Supervisor, id: &str) -> Result<String, String> {
        let _transition = self.transitions.lock().unwrap_or_else(|e| e.into_inner());
        let mut parent = sup
            .tasks
            .get(id)
            .ok_or_else(|| "Tarea paralela no encontrada.".to_string())?;
        if let Ok(mut s) = self.stopped.lock() {
            s.insert(id.to_string());
        }
        let run = self
            .get(id)
            .and_then(|v| serde_json::from_value::<ParallelRun>(v).ok());
        if let Some(mut r) = run {
            for b in &mut r.branches {
                if !b.task_id.is_empty() && matches!(b.status.as_str(), "running" | "pending") {
                    let _ = sup.tasks.stop(&b.task_id);
                    b.status = "stopped".into();
                    b.finished_at = now();
                } else if b.status == "pending" {
                    b.status = "stopped".into();
                }
                if b.mode == "build" {
                    if let Some(child) = sup.tasks.get(&b.task_id) {
                        inherit_worktree(&mut parent, &child);
                    }
                }
            }
            r.status = "stopped".into();
            r.error = "Detenida por ti".into();
            r.updated = now();
            self.save(&r);
        }
        parent["status"] = json!("stopped");
        parent["error"] = json!("Detenida por ti");
        parent["updated"] = json!(now());
        sup.tasks.update_task_meta(&parent);
        sup.tasks
            .emit_event(id, mk_event_pub("status", json!({ "status": "stopped" })));
        Ok("Ejecución paralela detenida.".into())
    }

    /// Start a new parallel orchestration run. Returns the parent task metadata immediately;
    /// planning, parallel wave execution, and synthesis proceed asynchronously.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        self: &Arc<Self>,
        sup: &Arc<Supervisor>,
        prompt: &str,
        cwd: &str,
        mode: &str,
        worktree: bool,
        attachments: &[String],
    ) -> Result<Value, String> {
        let prompt_clean = prompt.trim().to_string();
        if prompt_clean.is_empty() {
            return Err("El prompt está vacío.".into());
        }
        let base_dir = if cwd.trim().is_empty() {
            crate::util::home()
        } else {
            PathBuf::from(crate::util::expand_s(cwd))
        };
        if !base_dir.is_dir() {
            return Err(format!("La carpeta no existe: {}", base_dir.display()));
        }

        let mode_s = if mode == "plan" { "plan" } else { "build" };
        let has_image = attachments.iter().any(|f| is_image_path(f));
        let chain = sup.chain(now());
        let cfg = sup.cfg.get();
        let avail = available_brains(&cfg, &chain, has_image);
        if avail.is_empty() {
            return Err("Ningún cerebro disponible para el motor paralelo.".into());
        }
        if mode_s == "build" && !avail.iter().any(|b| can_edit(&b.s("kind"))) {
            return Err(
                "Construir requiere al menos un agente disponible con acceso a los archivos."
                    .into(),
            );
        }

        let (dir_id, dir_name) = pick_director(&avail, &chain, mode_s);
        let extra = json!({
            "parallel_mode": true,
            "director": dir_id,
            "director_name": dir_name,
            "user_message": if attachments.is_empty() {
                Value::Null
            } else {
                json!({"text": prompt_clean, "files": attachments})
            }
        });

        let parent = sup.tasks.create_orchestrator_task(
            &prompt_clean,
            &base_dir.to_string_lossy(),
            mode_s,
            worktree,
            &format!("Motor paralelo ({} cerebros)", avail.len()),
            extra,
        )?;
        let run_id = parent.s("id");
        let ts = now();
        let run = ParallelRun {
            id: run_id.clone(),
            prompt: prompt_clean.clone(),
            cwd: base_dir.to_string_lossy().to_string(),
            mode: mode_s.to_string(),
            worktree,
            strategy: "planning".into(),
            director: dir_id.clone(),
            director_name: dir_name.clone(),
            status: "planning".into(),
            waves_total: 0,
            branches: vec![],
            total_tokens: 0,
            total_usd: 0.0,
            created: ts,
            updated: ts,
            error: String::new(),
        };
        self.save(&run);
        sup.diary.add(
            "MetaBrain",
            &format!(
                "Motor paralelo ({} cerebros, dirige {}): {}",
                avail.len(),
                dir_name,
                clip(&prompt_clean, 60)
            ),
            "info",
        );

        let engine = Arc::clone(self);
        let sup_c = Arc::clone(sup);
        let atts = attachments.to_vec();
        std::thread::spawn(move || {
            engine.execute_run(sup_c, run, atts, None);
        });

        Ok(parent)
    }

    /// Continue an existing parallel conversation with a new user message, launching a fresh
    /// parallel wave carrying the compact history of the thread.
    pub fn continue_run(
        self: &Arc<Self>,
        sup: &Arc<Supervisor>,
        id: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<String, String> {
        let user_text = text.trim();
        if user_text.is_empty() && attachments.is_empty() {
            return Err("El mensaje está vacío.".into());
        }
        let _transition = self.transitions.lock().unwrap_or_else(|e| e.into_inner());
        let mut parent = sup
            .tasks
            .get(id)
            .ok_or_else(|| "Tarea no encontrada.".to_string())?;
        if turn_busy(&parent.s("status")) {
            return Err("La ejecución paralela sigue en curso.".into());
        }
        if let Some(previous) = self.runs.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            for branch in &previous.branches {
                if let Some(child) = sup.tasks.get(&branch.task_id) {
                    if turn_busy(&child.s("status")) {
                        return Err("Todavía hay ramas en ejecución o deteniéndose.".into());
                    }
                    if branch.mode == "build" {
                        inherit_worktree(&mut parent, &child);
                    }
                }
            }
        }

        let mode_s = if parent.s("mode") == "plan" {
            "plan"
        } else {
            "build"
        };
        let cwd = if parent.s("wt_path").is_empty() && parent.s("branch").is_empty() {
            parent.s("cwd")
        } else {
            crate::tasks::task_work_dir(&parent)?
                .to_string_lossy()
                .into_owned()
        };
        let has_image = attachments.iter().any(|f| is_image_path(f));
        let chain = sup.chain(now());
        let cfg = sup.cfg.get();
        let avail = available_brains(&cfg, &chain, has_image);
        if !avail
            .iter()
            .any(|b| mode_s == "plan" || can_edit(&b.s("kind")))
        {
            return Err("Ningún cerebro compatible disponible para continuar.".into());
        }
        if let Ok(mut s) = self.stopped.lock() {
            s.remove(id);
        }

        let prev_events = sup.tasks.events_since(id, 0).0;
        let history_ctx = compact_thread_context(&parent.s("prompt"), &prev_events);

        let mut user_ev = json!({"text": format!("Tú: {user_text}")});
        if !attachments.is_empty() {
            user_ev["user_message"] = json!({"text": user_text, "files": attachments});
        }
        sup.tasks.emit_event(id, mk_event_pub("text", user_ev));
        parent["status"] = json!("planning");
        parent["error"] = json!("");
        parent["updated"] = json!(now());
        sup.tasks.update_task_meta(&parent);
        sup.tasks
            .emit_event(id, mk_event_pub("status", json!({ "status": "planning" })));

        let (dir_id, dir_name) = pick_director(&avail, &chain, mode_s);
        let ts = now();
        let run = ParallelRun {
            id: id.to_string(),
            prompt: user_text.to_string(),
            cwd,
            mode: mode_s.to_string(),
            worktree: parent.b("worktree", false),
            strategy: "planning".into(),
            director: dir_id,
            director_name: dir_name,
            status: "planning".into(),
            waves_total: 0,
            branches: vec![],
            total_tokens: parent.f("tokens").unwrap_or(0.0) as i64,
            total_usd: parent.f("usd").unwrap_or(0.0),
            created: ts,
            updated: ts,
            error: String::new(),
        };
        self.save(&run);

        let engine = Arc::clone(self);
        let sup_c = Arc::clone(sup);
        let atts = attachments.to_vec();
        std::thread::spawn(move || {
            engine.execute_run(sup_c, run, atts, Some(history_ctx));
        });

        Ok("Nueva ola paralela iniciada.".into())
    }

    fn execute_run(
        self: Arc<Self>,
        sup: Arc<Supervisor>,
        mut run: ParallelRun,
        attachments: Vec<String>,
        history_ctx: Option<String>,
    ) {
        let run_id = run.id.clone();
        let cfg = sup.cfg.get();
        let chain = sup.chain(now());
        let has_image = attachments.iter().any(|f| is_image_path(f));
        let avail = available_brains(&cfg, &chain, has_image);

        // 1. Plan DAG (LLMCompiler planner -> heuristic Nexa fallback).
        let base_prompt = match &history_ctx {
            Some(ctx) if !ctx.is_empty() => format!("{ctx}\n\nNuevo pedido:\n{}", run.prompt),
            _ => run.prompt.clone(),
        };
        let (raw_plan, planner_usage) = plan_task(&cfg, &chain, &avail, &base_prompt, &run.mode);
        let (mut planner_tokens, mut planner_usd) = (0, 0.0);
        if let Some(usage) = planner_usage {
            planner_tokens = usage.inp + usage.out + usage.cache_read + usage.cache_write;
            planner_usd = usage.usd;
            sup.ledger.add(usage, true);
        }
        let plan = validate_and_finalize_dag(raw_plan, &base_prompt, &run.mode, avail.len());

        let transition = self.transitions.lock().unwrap_or_else(|e| e.into_inner());
        if !self.is_current(&run) {
            return;
        }
        let parent_meta = sup.tasks.get(&run_id).unwrap_or(Value::Null);

        let wave_map = topological_waves(&plan.nodes).unwrap_or_default();
        let waves_total = wave_map.values().copied().max().map(|w| w + 1).unwrap_or(1);

        run.strategy = plan.strategy.clone();
        run.waves_total = waves_total;
        run.status = "running".into();
        run.updated = now();
        run.branches = plan
            .nodes
            .iter()
            .map(|n| {
                let wave = *wave_map.get(&n.id).unwrap_or(&0);
                let wt = n.mode == "build" && run.worktree;
                BranchState {
                    id: n.id.clone(),
                    title: n.title.clone(),
                    role: n.role.clone(),
                    tier: n.tier,
                    mode: n.mode.clone(),
                    deps: n.deps.clone(),
                    files: n.files.clone(),
                    prompt: n.prompt.clone(),
                    worktree: wt,
                    task_id: String::new(),
                    brain: String::new(),
                    brain_name: String::new(),
                    model: String::new(),
                    status: "pending".into(),
                    summary: String::new(),
                    tokens: 0,
                    usd: 0.0,
                    error: String::new(),
                    wave,
                    started_at: 0.0,
                    finished_at: 0.0,
                }
            })
            .collect();
        self.save(&run);

        if let Some(mut parent) = sup.tasks.get(&run_id) {
            parent["status"] = json!("running");
            parent["strategy"] = json!(run.strategy);
            parent["branches_total"] = json!(run.branches.len());
            parent["updated"] = json!(now());
            sup.tasks.update_task_meta(&parent);
        }
        sup.tasks.emit_event(
            &run_id,
            mk_event_pub(
                "parallel_plan",
                json!({
                    "strategy": run.strategy,
                    "director": run.director_name,
                    "waves": run.waves_total,
                    "branches": run.branches.len()
                }),
            ),
        );
        sup.tasks.emit_event(
            &run_id,
            mk_event_pub("status", json!({ "status": "running" })),
        );
        drop(transition);

        // Prepare attachments once (shared across branches).
        let (prepared_files, _) = crate::docs::prepare_attachments(&attachments);
        let recent_cwd_brains = recent_brains_for_cwd(&sup, &run.cwd);
        let base_tokens = run.total_tokens;
        let base_usd = run.total_usd;

        // 2. LLMCompiler Task Fetching Loop: launch ready nodes in parallel as deps finish.
        loop {
            let transition = self.transitions.lock().unwrap_or_else(|e| e.into_inner());
            if !self.is_current(&run) {
                return;
            }

            let mut changed = false;

            // 2a. Poll running branches.
            for b in &mut run.branches {
                if b.status != "running" || b.task_id.is_empty() {
                    continue;
                }
                let Some(meta) = sup.tasks.get(&b.task_id) else {
                    continue;
                };
                let toks = meta.f("tokens").unwrap_or(0.0) as i64;
                let usd = meta.f("usd").unwrap_or(0.0);
                if toks != b.tokens || (usd - b.usd).abs() > 1e-9 {
                    b.tokens = toks;
                    b.usd = usd;
                    changed = true;
                }
                let st = meta.s("status");
                match st.as_str() {
                    "review" | "done" => {
                        b.status = "done".into();
                        b.finished_at = now();
                        b.summary = clip(
                            &sup.tasks.extract_assistant_text(&b.task_id),
                            SUMMARY_BUDGET,
                        );
                        if b.summary.is_empty() {
                            b.summary = "(La rama terminó sin texto de respuesta.)".into();
                        }
                        changed = true;
                        sup.tasks.emit_event(
                            &run_id,
                            mk_event_pub(
                                "parallel_branch",
                                json!({"branch": b.id, "title": b.title, "brain": b.brain_name, "status": "done", "tokens": b.tokens}),
                            ),
                        );
                    }
                    "failed" => {
                        b.status = "failed".into();
                        b.finished_at = now();
                        b.error = meta.s("error");
                        changed = true;
                        sup.tasks.emit_event(
                            &run_id,
                            mk_event_pub(
                                "parallel_branch",
                                json!({"branch": b.id, "title": b.title, "brain": b.brain_name, "status": "failed", "error": b.error}),
                            ),
                        );
                    }
                    "stopped" => {
                        b.status = "stopped".into();
                        b.finished_at = now();
                        changed = true;
                    }
                    _ => {}
                }
            }

            // 2b. Identify ready branches and skip branches with failed prerequisites.
            let done_map: HashMap<String, (String, String, String, String)> = run
                .branches
                .iter()
                .map(|b| {
                    (
                        b.id.clone(),
                        (
                            b.status.clone(),
                            b.title.clone(),
                            b.brain_name.clone(),
                            b.summary.clone(),
                        ),
                    )
                })
                .collect();

            let mut ready_indices: Vec<usize> = Vec::new();
            for (i, b) in run.branches.iter_mut().enumerate() {
                if b.status != "pending" {
                    continue;
                }
                if let Some(dep) = b.deps.iter().find(|dep| {
                    done_map.get(*dep).is_none_or(|(status, _, _, _)| {
                        matches!(status.as_str(), "failed" | "stopped" | "skipped")
                    })
                }) {
                    b.status = "skipped".into();
                    b.error = format!("No se completó la dependencia {dep}.");
                    b.finished_at = now();
                    changed = true;
                    sup.tasks.emit_event(
                        &run_id,
                        mk_event_pub("parallel_branch", json!({
                            "branch": b.id, "title": b.title, "status": "skipped", "error": b.error,
                        })),
                    );
                } else if b.deps.iter().all(|dep| done_map[dep].0 == "done") {
                    ready_indices.push(i);
                }
            }

            if !ready_indices.is_empty() {
                let live_chain = sup.chain(now());
                let live_cfg = sup.cfg.get();
                let live_avail = available_brains(&live_cfg, &live_chain, has_image);
                let queue = sup.tasks.queue_info();

                // Track brains already active in this run so siblings spread across providers.
                let mut active_wave_brains: HashMap<String, usize> = HashMap::new();
                for b in &run.branches {
                    if b.status == "running" && !b.brain.is_empty() {
                        *active_wave_brains.entry(b.brain.clone()).or_insert(0) += 1;
                    }
                }

                for idx in ready_indices {
                    let b_snapshot = run.branches[idx].clone();
                    let is_sink = matches!(b_snapshot.role.as_str(), "synthesize" | "implement");

                    // Update parent status to synthesizing when the final synthesis wave launches.
                    if is_sink && run.status != "synthesizing" {
                        run.status = "synthesizing".into();
                        if let Some(mut parent) = sup.tasks.get(&run_id) {
                            parent["status"] = json!("synthesizing");
                            parent["updated"] = json!(now());
                            sup.tasks.update_task_meta(&parent);
                        }
                        sup.tasks.emit_event(
                            &run_id,
                            mk_event_pub("status", json!({ "status": "synthesizing" })),
                        );
                    }

                    // Build AOrchestra scoped prompt with only upstream dependency summaries.
                    let dep_summaries: Vec<(String, String, String)> = b_snapshot
                        .deps
                        .iter()
                        .filter_map(|dep_id| {
                            let (st, title, brain_name, summary) = done_map.get(dep_id)?;
                            if st == "done" && !summary.is_empty() {
                                Some((title.clone(), brain_name.clone(), summary.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    let picked = assign_brain(
                        &b_snapshot,
                        &live_avail,
                        &live_chain,
                        &queue,
                        &active_wave_brains,
                        &recent_cwd_brains,
                        &run.director,
                    );

                    let Some(bcfg) = picked else {
                        run.branches[idx].status = "failed".into();
                        run.branches[idx].error =
                            "Ningún cerebro compatible disponible para esta rama.".into();
                        run.branches[idx].finished_at = now();
                        changed = true;
                        continue;
                    };

                    let bid = bcfg.s("id");
                    let bname = bcfg.os("name").unwrap_or_else(|| bid.clone());
                    let (model, effort) =
                        pick_model_and_effort(&bcfg, b_snapshot.tier, &b_snapshot.role);
                    let api_brain = bcfg.s("kind") == "openai";
                    let scoped_prompt = build_branch_prompt(
                        &run.prompt,
                        &b_snapshot,
                        &dep_summaries,
                        &prepared_files,
                        api_brain,
                    );

                    let logical_cwd = parent_meta
                        .os("cwd_project")
                        .unwrap_or_else(|| parent_meta.s("cwd"));
                    let mut child_extra = json!({
                        "parent_run": run_id,
                        "branch_id": b_snapshot.id,
                        "branch_role": b_snapshot.role,
                        "branch_tier": b_snapshot.tier,
                        "verify_on_finish": b_snapshot.mode == "build",
                        "cwd_project": logical_cwd,
                    });
                    if b_snapshot.mode == "build" && !parent_meta.s("wt_path").is_empty() {
                        for key in ["wt_path", "branch", "wt_base"] {
                            child_extra[key] = parent_meta[key].clone();
                        }
                    }

                    match sup.tasks.start_with(
                        &bcfg,
                        &model,
                        &effort,
                        &scoped_prompt,
                        &run.cwd,
                        &b_snapshot.mode,
                        b_snapshot.worktree,
                        child_extra,
                    ) {
                        Ok(child_meta) => {
                            let child_id = child_meta.s("id");
                            *active_wave_brains.entry(bid.clone()).or_insert(0) += 1;
                            let br = &mut run.branches[idx];
                            br.task_id = child_id;
                            br.brain = bid;
                            br.brain_name = bname.clone();
                            br.model = child_meta.s("model");
                            br.status = "running".into();
                            br.started_at = now();
                            changed = true;
                            sup.tasks.emit_event(
                                &run_id,
                                mk_event_pub(
                                    "parallel_branch",
                                    json!({"branch": br.id, "title": br.title, "brain": bname, "status": "running", "task_id": br.task_id}),
                                ),
                            );
                        }
                        Err(e) => {
                            let br = &mut run.branches[idx];
                            br.status = "failed".into();
                            br.error = e;
                            br.finished_at = now();
                            changed = true;
                        }
                    }
                }
            }

            // Aggregate usage.
            let run_toks = planner_tokens + run.branches.iter().map(|b| b.tokens).sum::<i64>();
            let run_usd = planner_usd + run.branches.iter().map(|b| b.usd).sum::<f64>();
            run.total_tokens = base_tokens + run_toks;
            run.total_usd = base_usd + run_usd;

            if changed {
                run.updated = now();
                self.save(&run);
                if let Some(mut parent) = sup.tasks.get(&run_id) {
                    parent["tokens"] = json!(run.total_tokens);
                    parent["last_turn_tokens"] = json!(run_toks);
                    parent["usd"] = json!(run.total_usd);
                    parent["updated"] = json!(now());
                    sup.tasks.update_task_meta(&parent);
                }
            }

            let any_active = run
                .branches
                .iter()
                .any(|b| matches!(b.status.as_str(), "pending" | "running"));
            if !any_active {
                break;
            }
            drop(transition);
            std::thread::sleep(Duration::from_millis(250));
        }

        let _transition = self.transitions.lock().unwrap_or_else(|e| e.into_inner());
        if !self.is_current(&run) {
            return;
        }

        // 3. Finalize: publish synthesized answer and inherit worktree/branch from final build node if present.
        let any_ok = run.branches.iter().any(|b| b.status == "done");
        let all_ok = run.branches.iter().all(|b| b.status == "done") && any_ok;
        let final_answer = run
            .branches
            .iter()
            .find(|b| {
                b.status == "done"
                    && (run.branches.len() == 1 || b.mode == "build" || b.role == "synthesize")
            })
            .map(|b| sup.tasks.extract_assistant_text(&b.task_id))
            .filter(|answer| !answer.is_empty())
            .unwrap_or_else(|| compose_final_answer(&run.branches));
        let final_build_meta = run
            .branches
            .iter()
            .rev()
            .find(|b| b.mode == "build" && !b.task_id.is_empty())
            .and_then(|b| sup.tasks.get(&b.task_id));

        run.status = if all_ok {
            "review".into()
        } else {
            "failed".into()
        };
        if !all_ok {
            run.error = if any_ok {
                "Resultado parcial: quedaron ramas sin completar."
            } else {
                "Ninguna rama paralela se completó."
            }
            .into();
        }
        run.updated = now();
        self.save(&run);

        if !final_answer.is_empty() {
            sup.tasks.emit_event(
                &run_id,
                mk_event_pub("text", json!({ "text": final_answer })),
            );
        }
        if !all_ok {
            sup.tasks
                .emit_event(&run_id, mk_event_pub("error", json!({ "text": run.error })));
        }

        if let Some(mut parent) = sup.tasks.get(&run_id) {
            parent["status"] = json!(run.status);
            parent["error"] = json!(run.error);
            parent["tokens"] = json!(run.total_tokens);
            parent["last_turn_tokens"] = json!(run.total_tokens - base_tokens);
            parent["usd"] = json!(run.total_usd);
            parent["updated"] = json!(now());
            if let Some(bm) = final_build_meta {
                inherit_worktree(&mut parent, &bm);
                for k in ["verified", "verify_cmd"] {
                    if let Some(v) = bm.get(k).filter(|v| !v.is_null() && v.as_str() != Some("")) {
                        parent[k] = v.clone();
                    }
                }
            }
            sup.tasks.update_task_meta(&parent);
        }
        sup.tasks.emit_event(
            &run_id,
            mk_event_pub("status", json!({ "status": run.status })),
        );
        log(&format!(
            "paralelo {run_id}: terminó ({}, {} ramas, {} tokens)",
            run.status,
            run.branches.len(),
            run.total_tokens
        ));
    }
}

impl Default for ParallelEngine {
    fn default() -> Self {
        Self::new()
    }
}
