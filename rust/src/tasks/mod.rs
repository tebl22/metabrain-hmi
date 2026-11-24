//! Background task runner: launches Claude Code, CLI agents, or OpenAI-compatible brains
//! from the panel, streams their output as JSONL events and keeps the meta.
//! Event log: data_dir()/tasks/<id>.events.jsonl — one JSON object per line.

use crate::util::{data_dir, log, now, read_json, write_json, J};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

mod helpers;
mod lifecycle;
mod runners;
#[cfg(test)]
mod tests;

pub(crate) use helpers::{brain_parallel_limit_pub, can_run_task_on, mk_event_pub};
pub use helpers::{cached_system, can_edit, events_from_line, turn_busy};
use helpers::{mk_event, new_id, put};
pub(crate) use lifecycle::task_work_dir;

/// Called when a routed task fails its check: start the next attempt, return its id.
pub type Escalator = dyn Fn(&Value, &str) -> Option<String> + Send + Sync;

/// Escalations allowed per original task (cheap -> mid -> strong).
pub const MAX_ESCALATIONS: u64 = 2;

/// Typed queue slot state for one brain.
#[derive(Clone, Debug, Serialize)]
pub struct BrainQueueSlot {
    pub running: usize,
    pub limit: usize,
}

/// Typed queued task entry.
#[derive(Clone, Debug, Serialize)]
pub struct WaitingEntry {
    pub id: String,
    pub brain: String,
    pub position: usize,
}

/// Typed scheduler snapshot returned by `TaskManager::queue_info`.
#[derive(Clone, Debug, Serialize)]
pub struct QueueSummary {
    pub global_limit: usize,
    pub running_total: usize,
    pub per_brain: HashMap<String, BrainQueueSlot>,
    pub waiting: Vec<WaitingEntry>,
}

pub struct TaskManager {
    pub(super) root: PathBuf,
    pub(super) procs: Mutex<HashMap<String, Child>>,
    pub(super) sched: Mutex<Sched>,
    pub(super) cv: Condvar,
    pub(super) escalator: Mutex<Option<Arc<Escalator>>>,
    /// Config snapshot for the verifier (`verify` overrides).
    pub(super) verify_cfg: Mutex<Value>,
    pub(super) turn_state: Mutex<()>,
}

pub(super) struct WaitItem {
    pub(super) id: String,
    pub(super) brain: String,
    pub(super) cupo: usize,
    #[allow(dead_code)]
    pub(super) name: String,
}

pub(super) struct Sched {
    pub(super) limit: usize,
    pub(super) running_total: usize,
    pub(super) per_brain_running: HashMap<String, usize>,
    pub(super) brain_limits: HashMap<String, usize>,
    pub(super) waiting: Vec<WaitItem>,
}

pub(super) struct RunGuard {
    tm: Arc<TaskManager>,
    brain: String,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.tm.release(&self.brain);
    }
}

impl TaskManager {
    pub fn new() -> TaskManager {
        let root = data_dir().join("tasks");
        let _ = fs::create_dir_all(&root);
        let limit = std::env::var("MB_MAX_PARALLEL")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(8);
        let tm = TaskManager {
            root,
            procs: Mutex::new(HashMap::new()),
            sched: Mutex::new(Sched {
                limit,
                running_total: 0,
                per_brain_running: HashMap::new(),
                brain_limits: HashMap::new(),
                waiting: Vec::new(),
            }),
            cv: Condvar::new(),
            escalator: Mutex::new(None),
            verify_cfg: Mutex::new(Value::Null),
            turn_state: Mutex::new(()),
        };
        tm.recover();
        tm
    }

    /// Tasks left as queued/running by a previous process can never finish.
    fn recover(&self) {
        let rd = match fs::read_dir(&self.root) {
            Ok(r) => r,
            Err(_) => return,
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let p = e.path();
            let mut m = match read_json(&p) {
                Some(v) => v,
                None => continue,
            };
            let st = m.s("status");
            if !turn_busy(&st) {
                continue;
            }
            let mut id = m.s("id");
            if id.is_empty() {
                id = name.trim_end_matches(".json").to_string();
                put(&mut m, "id", json!(id));
            }
            let status = if st == "stopping" {
                "stopped"
            } else {
                "failed"
            };
            put(&mut m, "status", json!(status));
            if status == "failed" {
                put(
                    &mut m,
                    "error",
                    json!("MetaBrain se reinició durante la tarea"),
                );
            }
            put(&mut m, "pid", json!(0));
            put(&mut m, "updated", json!(now()));
            if let Err(e) = write_json(&p, &m) {
                log(&format!(
                    "tarea {id}: no se pudo guardar la recuperación: {e}"
                ));
                continue;
            }
            if !id.is_empty() {
                if status == "failed" {
                    self.emit(
                        &id,
                        mk_event(
                            "error",
                            json!({ "text": "MetaBrain se reinició durante la tarea" }),
                        ),
                    );
                }
                self.emit(&id, mk_event("status", json!({ "status": status })));
            }
        }
    }

    pub fn set_global_limit(&self, n: usize) {
        let n = n.max(1);
        {
            let mut sched = match self.sched.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            sched.limit = n;
        }
        self.cv.notify_all();
    }

    /// Wait until this task can run: it must be the first queued task whose
    /// brain has room and the global limit must allow another run.
    pub(super) fn acquire(
        self: &Arc<Self>,
        brain: &str,
        cupo: usize,
        id: &str,
        name: &str,
    ) -> Option<RunGuard> {
        let cupo = cupo.max(1);
        let mut sched = match self.sched.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if self
            .get(id)
            .is_some_and(|m| matches!(m.s("status").as_str(), "stopped" | "stopping"))
        {
            return None;
        }
        sched.brain_limits.insert(brain.to_string(), cupo);
        if !sched.waiting.iter().any(|w| w.id == id) {
            sched.waiting.push(WaitItem {
                id: id.to_string(),
                brain: brain.to_string(),
                cupo,
                name: name.to_string(),
            });
        }
        let mut last_pos = 0usize;
        let mut last_reason = String::new();
        loop {
            // removed from the queue => stopped while waiting
            let my_idx = sched.waiting.iter().position(|w| w.id == id)?;

            // first queued task whose brain has a free slot
            let mut cand: Option<usize> = None;
            for (i, w) in sched.waiting.iter().enumerate() {
                let used = *sched.per_brain_running.get(&w.brain).unwrap_or(&0);
                if used < w.cupo {
                    cand = Some(i);
                    break;
                }
            }
            let can_global = sched.running_total < sched.limit;

            if cand == Some(my_idx) && can_global {
                sched.waiting.remove(my_idx);
                *sched
                    .per_brain_running
                    .entry(brain.to_string())
                    .or_insert(0) += 1;
                sched.running_total += 1;
                drop(sched);
                self.cv.notify_all();
                return Some(RunGuard {
                    tm: Arc::clone(self),
                    brain: brain.to_string(),
                });
            }

            let position = my_idx + 1;
            let reason = if sched.running_total >= sched.limit {
                "tope global".to_string()
            } else {
                format!("cupo de {}", name)
            };
            if position != last_pos || reason != last_reason {
                last_pos = position;
                last_reason = reason.clone();
                drop(sched);
                self.emit(
                    id,
                    mk_event(
                        "status",
                        json!({ "status": "queued", "position": position, "waiting_for": reason }),
                    ),
                );
                sched = match self.sched.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                continue;
            }

            let res = self.cv.wait_timeout(sched, Duration::from_secs(1));
            let (g, _) = match res {
                Ok(x) => x,
                Err(p) => p.into_inner(),
            };
            sched = g;
        }
    }

    fn release(&self, brain: &str) {
        {
            let mut sched = match self.sched.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(n) = sched.per_brain_running.get_mut(brain) {
                if *n > 0 {
                    *n -= 1;
                }
            }
            if sched.running_total > 0 {
                sched.running_total -= 1;
            }
        }
        self.cv.notify_all();
    }

    pub fn queue_summary(&self) -> QueueSummary {
        let sched = match self.sched.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut per_brain = HashMap::new();
        let mut brains: Vec<String> = Vec::new();
        for b in sched.per_brain_running.keys() {
            if !brains.contains(b) {
                brains.push(b.clone());
            }
        }
        for w in &sched.waiting {
            if !brains.contains(&w.brain) {
                brains.push(w.brain.clone());
            }
        }
        for b in sched.brain_limits.keys() {
            if !brains.contains(b) {
                brains.push(b.clone());
            }
        }
        for b in brains {
            let running = *sched.per_brain_running.get(&b).unwrap_or(&0);
            let limit = *sched.brain_limits.get(&b).unwrap_or(&1);
            per_brain.insert(b, BrainQueueSlot { running, limit });
        }
        let waiting: Vec<WaitingEntry> = sched
            .waiting
            .iter()
            .enumerate()
            .map(|(i, w)| WaitingEntry {
                id: w.id.clone(),
                brain: w.brain.clone(),
                position: i + 1,
            })
            .collect();
        QueueSummary {
            global_limit: sched.limit,
            running_total: sched.running_total,
            per_brain,
            waiting,
        }
    }

    pub fn queue_info(&self) -> Value {
        serde_json::to_value(self.queue_summary()).unwrap_or_else(|_| json!({}))
    }

    pub fn list_with_queue(&self, limit: usize) -> Value {
        json!({
            "tasks": self.list(limit),
            "queue": self.queue_info(),
        })
    }

    pub(super) fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub(super) fn events_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.events.jsonl"))
    }

    pub(super) fn save(&self, meta: &Value) {
        let id = meta.s("id");
        if id.is_empty() {
            return;
        }
        let result =
            fs::create_dir_all(&self.root).and_then(|()| write_json(&self.meta_path(&id), meta));
        if let Err(e) = result {
            log(&format!("tarea {id}: no se pudo guardar el estado: {e}"));
        }
    }

    pub(super) fn emit(&self, id: &str, ev: Value) {
        if id.is_empty() || id.contains('/') || id.contains('\\') {
            return;
        }
        let result = (|| -> std::io::Result<()> {
            let mut line = serde_json::to_vec(&ev)?;
            line.push(b'\n');
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.events_path(id))?
                .write_all(&line)
        })();
        if let Err(e) = result {
            log(&format!(
                "tarea {id}: no se pudo guardar el evento {}: {e}",
                ev.s("type")
            ));
        }
    }

    pub(super) fn fail(&self, id: &str, msg: &str) {
        self.emit(id, mk_event("error", json!({ "text": msg })));
        let mut m = self.get(id).unwrap_or(Value::Null);
        let route_id = m.os("route_id");
        let project = std::path::Path::new(&m.s("cwd"))
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        crate::learn::discard_pending(&project, route_id.as_deref());
        // Provider limits / overload / auth errors take the brain out of command for a while.
        crate::succession::note_error(&m.s("brain"), msg, now());
        put(&mut m, "status", json!("failed"));
        put(&mut m, "error", json!(msg));
        put(&mut m, "pid", json!(0));
        put(&mut m, "updated", json!(now()));
        self.save(&m);
        self.emit(id, mk_event("status", json!({ "status": "failed" })));
        log(&format!("tarea {id}: fallo: {msg}"));
    }

    pub fn list(&self, limit: usize) -> Value {
        let mut items: Vec<Value> = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.ends_with(".json") {
                    continue;
                }
                if let Some(v) = read_json(&e.path()) {
                    if v.get("id").is_some() {
                        items.push(v);
                    }
                }
            }
        }
        items.sort_by(|a, b| {
            let x = b.f("updated").unwrap_or(0.0);
            let y = a.f("updated").unwrap_or(0.0);
            x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
        });
        if limit > 0 && items.len() > limit {
            items.truncate(limit);
        }
        Value::Array(items)
    }

    /// Seconds since the quietest running task of `brain` last emitted anything, when that is
    /// at least `min_secs` (a stalled task); None when every running task is alive.
    pub fn stalled_secs(&self, brain: &str, min_secs: u64) -> Option<u64> {
        if min_secs == 0 {
            return None;
        }
        let running: Vec<String> = {
            let procs = self.procs.lock().unwrap_or_else(|e| e.into_inner());
            procs.keys().cloned().collect()
        };
        let now_t = std::time::SystemTime::now();
        let mut worst: Option<u64> = None;
        for id in running {
            let Some(m) = self.get(&id) else { continue };
            if m.s("brain") != brain || m.s("status") != "running" {
                continue;
            }
            let idle = fs::metadata(self.events_path(&id))
                .and_then(|md| md.modified())
                .ok()
                .and_then(|mt| now_t.duration_since(mt).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if idle >= min_secs {
                worst = Some(worst.map_or(idle, |w: u64| w.max(idle)));
            }
        }
        worst
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
            return None;
        }
        read_json(&self.meta_path(id))
    }

    pub fn running(&self, id: &str) -> bool {
        let procs = match self.procs.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        procs.contains_key(id)
    }

    pub(crate) fn update_task_meta(&self, meta: &Value) {
        self.save(meta);
    }

    pub(crate) fn emit_event(&self, id: &str, ev: Value) {
        self.emit(id, ev);
    }

    /// Create a parent orchestrator task record for the Parallel Engine without spawning a single worker.
    pub(crate) fn create_orchestrator_task(
        &self,
        prompt: &str,
        cwd: &str,
        mode: &str,
        worktree: bool,
        brain_name: &str,
        extra: Value,
    ) -> Result<Value, String> {
        let id = new_id();
        let ts = now();
        let title: String = prompt
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let mut meta = json!({
            "id": id,
            "turn_id": id,
            "brain": "parallel",
            "brain_name": brain_name,
            "kind": "parallel",
            "parallel_run": id,
            "title": title,
            "prompt": prompt,
            "cwd": cwd,
            "worktree": worktree,
            "wt_path": "",
            "branch": "",
            "mode": mode,
            "model": "motor-paralelo",
            "effort": "",
            "status": "planning",
            "session_id": "",
            "tokens": 0,
            "usd": 0.0,
            "created": ts,
            "updated": ts,
            "pid": 0,
            "error": ""
        });
        if let (Some(m), Some(x)) = (meta.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                if !v.is_null() {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        fs::create_dir_all(&self.root)
            .map_err(|e| format!("No se pudo guardar la conversación: {e}"))?;
        write_json(&self.meta_path(&id), &meta)
            .map_err(|e| format!("No se pudo guardar la conversación: {e}"))?;
        self.emit(&id, mk_event("status", json!({ "status": "planning" })));
        Ok(meta)
    }

    /// Collect all assistant text emitted by a task (excluding user "Tú: " lines).
    pub(crate) fn extract_assistant_text(&self, id: &str) -> String {
        let (evs, _) = self.events_since(id, 0);
        let mut out = String::new();
        let mut streaming = false;
        for ev in evs {
            if ev.s("type") == "text" {
                let t = ev.s("text");
                let delta = ev.b("delta", false);
                if !delta && (t.starts_with("Tú: ") || t.trim().is_empty()) {
                    streaming = false;
                    continue;
                }
                if (!delta || !streaming) && !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(if delta { &t } else { t.trim() });
                streaming = delta;
            }
        }
        out.trim().to_owned()
    }

    pub fn set_escalator(&self, f: Arc<Escalator>) {
        *self.escalator.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    pub fn set_verify_cfg(&self, cfg: Value) {
        *self.verify_cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// Events written after `offset` bytes; returns them plus the new offset.
    pub fn events_since(&self, id: &str, offset: u64) -> (Vec<Value>, u64) {
        let p = self.events_path(id);
        let mut f = match fs::File::open(&p) {
            Ok(f) => f,
            Err(_) => return (Vec::new(), offset),
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = if offset > len { 0 } else { offset };
        if f.seek(SeekFrom::Start(start)).is_err() {
            return (Vec::new(), start);
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return (Vec::new(), start);
        }
        let consumed = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => 0,
        };
        let text = String::from_utf8_lossy(&buf[..consumed]);
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                out.push(v);
            }
        }
        (out, start + consumed as u64)
    }
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl TaskManager {
    pub(super) fn test_run<F: FnOnce()>(
        self: &Arc<Self>,
        brain: &str,
        cupo: usize,
        name: &str,
        id: &str,
        f: F,
    ) {
        let _g = match self.acquire(brain, cupo, id, name) {
            Some(g) => g,
            None => return,
        };
        f();
    }
}
