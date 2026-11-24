//! Task lifecycle operations: start, follow-up message, retry, stop, approve, discard, and diff.

use super::helpers::{
    brain_kind, brain_parallel_limit, can_edit, can_run_task_on, checked_git, clip, find_brain_cfg,
    follow_up_prompt, git_cmd, mk_event, new_id, put, terminate_tree, turn_busy,
};
use super::TaskManager;
use crate::util::{now, write_json, J};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl TaskManager {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        self: &Arc<Self>,
        brain_cfg: &Value,
        model: &str,
        effort: &str,
        prompt: &str,
        cwd: &str,
        mode: &str,
        worktree: bool,
    ) -> Result<Value, String> {
        self.start_with(
            brain_cfg,
            model,
            effort,
            prompt,
            cwd,
            mode,
            worktree,
            Value::Null,
        )
    }

    /// `start` plus extra meta fields (routing id, level, candidate, escalation count…),
    /// saved before the task can run.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with(
        self: &Arc<Self>,
        brain_cfg: &Value,
        model: &str,
        effort: &str,
        prompt: &str,
        cwd: &str,
        mode: &str,
        worktree: bool,
        extra: Value,
    ) -> Result<Value, String> {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("El prompt está vacío.".to_string());
        }
        let base = if cwd.trim().is_empty() {
            crate::util::home()
        } else {
            crate::extensions::sanitize_project_dir(cwd)?
        };
        if !base.is_dir() {
            return Err("The selected project folder does not exist.".into());
        }

        let id = new_id();
        let kind = brain_kind(brain_cfg);
        let (model_request, effort_request) = (model.trim(), effort.trim());
        let (model, effort) = if kind == "codex" {
            crate::cli_agents::codex_turn_settings(
                brain_cfg,
                &prompt,
                model_request,
                effort_request,
            )
        } else {
            (model.to_string(), effort.to_string())
        };
        if !can_run_task_on(&kind, mode) {
            return Err("Este cerebro no admite el modo solicitado.".into());
        }
        let title: String = prompt
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let brain_id = brain_cfg
            .os("id")
            .or_else(|| brain_cfg.os("name"))
            .unwrap_or_default();
        let brain_name = brain_cfg.os("name").unwrap_or_else(|| brain_id.clone());
        let cupo = brain_parallel_limit(brain_cfg);
        let ts = now();
        let mode_s = if mode == "plan" { "plan" } else { "build" };

        let mut meta = json!({
            "id": id,
            "turn_id": id,
            "brain": brain_id,
            "brain_name": brain_name,
            "kind": kind.clone(),
            "title": title,
            "prompt": prompt,
            "cwd": base.to_string_lossy().to_string(),
            "worktree": worktree,
            "wt_path": "",
            "branch": "",
            "mode": mode_s,
            "model": model,
            "effort": effort,
            "status": "queued",
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
                m.insert(k.clone(), v.clone());
            }
        }
        if kind == "codex" {
            let routed = extra.get("route_level").is_some();
            meta["model_request"] = json!(if routed { "" } else { model_request });
            meta["effort_request"] = json!(if routed { "" } else { effort_request });
        }

        fs::create_dir_all(&self.root)
            .map_err(|e| format!("No se pudo guardar la conversación: {e}"))?;
        write_json(&self.meta_path(&id), &meta)
            .map_err(|e| format!("No se pudo guardar la conversación: {e}"))?;
        self.emit(&id, mk_event("status", json!({ "status": "queued" })));

        let me = Arc::clone(self);
        let cfg = brain_cfg.clone();
        let id2 = id.clone();
        let prompt2 = prompt.clone();
        let model2 = model;
        let effort2 = effort;
        let mode2 = mode_s.to_string();
        let brain_key = brain_id.clone();
        let brain_name2 = brain_name.clone();
        std::thread::spawn(move || {
            if let Some(_slot) = me.acquire(&brain_key, cupo, &id2, &brain_name2) {
                me.clone().run(
                    id2.clone(),
                    cfg,
                    kind,
                    prompt2,
                    base,
                    model2,
                    effort2,
                    mode2,
                    worktree,
                );
            }
            me.finish_interruption(&id2, &id2);
        });

        Ok(meta)
    }

    pub fn message(
        self: &Arc<Self>,
        id: &str,
        text: &str,
        files: &[String],
    ) -> Result<String, String> {
        self.start_message(id, text, files, None, None)
    }

    /// A follow-up that continues the conversation on another brain (the user picked it).
    pub fn message_with(
        self: &Arc<Self>,
        id: &str,
        text: &str,
        files: &[String],
        brain: &str,
    ) -> Result<String, String> {
        self.start_message(id, text, files, None, Some((brain, "", "")))
    }

    /// A follow-up that switches brain and/or specific model (can change model on the same brain too).
    pub fn message_with_model(
        self: &Arc<Self>,
        id: &str,
        text: &str,
        files: &[String],
        brain: &str,
        model: &str,
    ) -> Result<String, String> {
        self.start_message(id, text, files, None, Some((brain, model, "")))
    }

    pub(crate) fn message_routed(
        self: &Arc<Self>,
        id: &str,
        text: &str,
        files: &[String],
        pick: &Value,
    ) -> Result<String, String> {
        self.start_message(
            id,
            text,
            files,
            None,
            Some((&pick.s("brain"), &pick.s("model"), &pick.s("effort"))),
        )
    }

    pub fn retry(self: &Arc<Self>, id: &str, brain: &str) -> Result<String, String> {
        self.start_message(id, "", &[], Some(brain), None)
    }

    fn start_message(
        self: &Arc<Self>,
        id: &str,
        text: &str,
        files: &[String],
        retry_brain: Option<&str>,
        switch_brain: Option<(&str, &str, &str)>,
    ) -> Result<String, String> {
        let retry = retry_brain.is_some();
        // Prepare attachments BEFORE acquiring `turn_state` so slow PDF/file extraction
        // never blocks other conversations, stop(), approve(), or discard().
        let (prepared_files, attach_warnings) = if files.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            crate::docs::prepare_attachments(files)
        };
        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        let mut meta = self.get(id).ok_or_else(|| "Task not found.".to_string())?;
        if turn_busy(&meta.s("status")) {
            return Err("Task is still running.".to_string());
        }
        if retry && meta.s("status") != "failed" {
            return Err("Only the last failed turn can be retried.".into());
        }
        let req_switch = retry_brain.map(|brain| (brain, "", "")).or(switch_brain);
        let cur_brain = meta.s("brain");
        let cur_model = meta.s("model");
        let cur_effort = meta.s("effort");
        let switch = req_switch.filter(|(brain, model, effort)| {
            (!brain.is_empty() && *brain != cur_brain && *brain != "auto")
                || (!model.is_empty() && *model != cur_model)
                || (!effort.is_empty() && *effort != cur_effort)
        });
        let mut switch_notice: Option<String> = None;
        if let Some((brain, model, effort)) = switch {
            let target_brain = if brain.is_empty() || brain == "auto" {
                cur_brain.as_str()
            } else {
                brain
            };
            let brain_changed = target_brain != cur_brain;
            if brain_changed {
                let cfg = find_brain_cfg(target_brain);
                if cfg.is_null() {
                    return Err("The selected brain is no longer available.".into());
                }
                let kind = brain_kind(&cfg);
                if !can_run_task_on(&kind, &meta.s("mode")) {
                    return Err("This brain does not support the conversation's mode.".into());
                }
                let new_name = cfg.os("name").unwrap_or_else(|| target_brain.into());
                for (key, value) in [
                    ("brain", cfg.os("id").unwrap_or_else(|| target_brain.into())),
                    ("brain_name", new_name.clone()),
                    ("kind", kind),
                    ("model", model.to_owned()),
                    ("effort", effort.to_owned()),
                    ("model_request", model.to_owned()),
                    ("effort_request", effort.to_owned()),
                    ("session_id", String::new()),
                ] {
                    put(&mut meta, key, json!(value));
                }
                switch_notice = Some(if model.is_empty() {
                    format!("Switched brain → {new_name}")
                } else {
                    format!("Switched brain → {new_name} · {model}")
                });
            } else {
                // Same brain, explicit model/effort switch.
                if !model.is_empty() {
                    put(&mut meta, "model", json!(model));
                    put(&mut meta, "model_request", json!(model));
                }
                if !effort.is_empty() {
                    put(&mut meta, "effort", json!(effort));
                    put(&mut meta, "effort_request", json!(effort));
                }
                if !model.is_empty() {
                    switch_notice = Some(format!(
                        "Switched model → {} · {model}",
                        meta.s("brain_name")
                    ));
                } else if !effort.is_empty() {
                    switch_notice = Some(format!(
                        "Switched effort → {} · {effort}",
                        meta.s("brain_name")
                    ));
                }
            }
        }
        let original = meta.s("prompt");
        let events = if retry {
            self.events_since(id, 0).0
        } else {
            Vec::new()
        };
        let previous = events
            .iter()
            .rposition(|event| event.s("type") == "text" && event.s("text").starts_with("Tú: "));
        let stored = previous.map(|i| events[i].s("text"));
        let text = if retry {
            stored
                .as_deref()
                .and_then(|value| value.strip_prefix("Tú: "))
                .unwrap_or(&original)
        } else {
            text
        };
        let user_text = text.trim();
        if user_text.is_empty() && files.is_empty() {
            return Err("Message is empty.".to_string());
        }
        let text =
            crate::docs::attach_to_prompt(user_text, &prepared_files, meta.s("kind") == "openai");
        let mut event = json!({"text": format!("Tú: {text}")});
        if !files.is_empty() {
            event["user_message"] = json!({"text": user_text, "files": files});
        }
        let turn_id = new_id();
        put(&mut meta, "turn_id", json!(turn_id));
        put(&mut meta, "error", json!(""));
        let cli_prompt = || match (retry, previous) {
            (true, None) => original.clone(),
            (true, Some(index)) => follow_up_prompt(&original, &events[..index], &text),
            (false, _) => follow_up_prompt(&original, &self.events_since(id, 0).0, &text),
        };
        if meta.s("kind") == "openai" {
            let brain = meta.s("brain");
            let name = meta.s("brain_name");
            let cfg = find_brain_cfg(&brain);
            if cfg.is_null() {
                return Err("The selected brain is no longer available.".into());
            }
            put(&mut meta, "status", json!("queued"));
            put(&mut meta, "updated", json!(now()));
            write_json(&self.meta_path(id), &meta)
                .map_err(|e| format!("Could not start turn: {e}"))?;
            for warn in &attach_warnings {
                self.emit(id, mk_event("info", json!({ "text": warn })));
            }
            if let Some(notice) = switch_notice {
                self.emit(id, mk_event("info", json!({ "text": notice })));
            }
            if !retry {
                self.emit(id, mk_event("text", event));
            }
            self.emit(id, mk_event("status", json!({ "status": "queued" })));
            let me = Arc::clone(self);
            let (id2, model, effort, prompt) = (
                id.to_string(),
                meta.s("model"),
                meta.s("effort"),
                meta.s("prompt"),
            );
            std::thread::spawn(move || {
                if let Some(_slot) = me.acquire(&brain, brain_parallel_limit(&cfg), &id2, &name) {
                    me.run_openai(&id2, &cfg, &model, &effort, &prompt);
                    if retry {
                        me.post_run(&id2, &turn_id);
                    }
                }
                me.finish_interruption(&id2, &turn_id);
            });
            return Ok("Message sent.".to_string());
        }
        if let Some(agent) = crate::cli_agents::Agent::from_kind(&meta.s("kind")) {
            // Codex / Gemini (Antigravity): a new run in the same folder that carries the
            // conversation so far in a compact form (the files on disk hold the rest).
            let work = task_work_dir(&meta)?;
            if !work.is_dir() {
                return Err(format!("La carpeta no existe: {}", work.display()));
            }
            let prompt = cli_prompt();
            let mut m = meta.clone();
            let brain = meta.s("brain");
            let name = meta.s("brain_name");
            let cfg = find_brain_cfg(&brain);
            if cfg.is_null() {
                return Err("The selected brain is no longer available.".into());
            }
            let cupo = brain_parallel_limit(&cfg);
            let (model, effort) = if agent == crate::cli_agents::Agent::Codex {
                let legacy_setting = |key: &str| {
                    if meta.get("route_level").is_some() {
                        ""
                    } else {
                        meta[key].as_str().unwrap_or("")
                    }
                };
                let model_request = meta["model_request"]
                    .as_str()
                    .unwrap_or_else(|| legacy_setting("model"));
                let effort_request = meta["effort_request"]
                    .as_str()
                    .unwrap_or_else(|| legacy_setting("effort"));
                let (run_model, run_effort) = switch.map_or(
                    (model_request, effort_request),
                    |(_, sw_model, sw_effort)| {
                        (
                            if sw_model.is_empty() {
                                model_request
                            } else {
                                sw_model
                            },
                            if sw_effort.is_empty() {
                                effort_request
                            } else {
                                sw_effort
                            },
                        )
                    },
                );
                let settings =
                    crate::cli_agents::codex_turn_settings(&cfg, &text, run_model, run_effort);
                put(&mut m, "model_request", json!(model_request));
                put(&mut m, "effort_request", json!(effort_request));
                put(&mut m, "model", json!(settings.0));
                put(&mut m, "effort", json!(settings.1));
                if meta.get("cached_tokens").is_none() && meta.f("tokens").unwrap_or(0.0) > 0.0 {
                    put(&mut m, "cache_breakdown_partial", json!(true));
                }
                settings
            } else {
                (meta.s("model"), meta.s("effort"))
            };
            put(&mut m, "status", json!("queued"));
            put(&mut m, "error", json!(""));
            put(&mut m, "updated", json!(now()));
            write_json(&self.meta_path(id), &m)
                .map_err(|e| format!("No se pudo iniciar el turno: {e}"))?;
            for warn in &attach_warnings {
                self.emit(id, mk_event("info", json!({ "text": warn })));
            }
            if let Some(notice) = switch_notice {
                self.emit(id, mk_event("info", json!({ "text": notice })));
            }
            if !retry {
                self.emit(id, mk_event("text", event));
            }
            self.emit(id, mk_event("status", json!({ "status": "queued" })));
            let me = Arc::clone(self);
            let (id2, mode) = (id.to_string(), meta.s("mode"));
            std::thread::spawn(move || {
                if let Some(_slot) = me.acquire(&brain, cupo, &id2, &name) {
                    me.run_cli(&id2, agent, &prompt, &work, &model, &effort, &mode);
                    if retry {
                        me.post_run(&id2, &turn_id);
                    }
                }
                me.finish_interruption(&id2, &turn_id);
            });
            return Ok("Mensaje enviado.".to_string());
        }
        if meta.s("kind") != "claude_code" {
            return Err("Este cerebro no admite continuar la conversación".to_string());
        }
        let session = meta.os("session_id").filter(|s| !retry && !s.is_empty());
        let prompt = if session.is_some() {
            text.clone()
        } else {
            cli_prompt()
        };

        let work = task_work_dir(&meta)?;
        if !work.is_dir() {
            return Err(format!("La carpeta no existe: {}", work.display()));
        }
        let cfg = find_brain_cfg(&meta.s("brain"));
        if cfg.is_null() {
            return Err("The selected brain is no longer available.".into());
        }
        let cdir = cfg.os("config_dir");
        let model = meta.s("model");
        let effort = meta.s("effort");
        let mode = meta.s("mode");

        let brain = meta.s("brain");
        let name = meta.s("brain_name");
        let cupo = brain_parallel_limit(&cfg);
        let mut m = meta;
        put(&mut m, "status", json!("queued"));
        put(&mut m, "error", json!(""));
        put(&mut m, "updated", json!(now()));
        write_json(&self.meta_path(id), &m)
            .map_err(|e| format!("No se pudo iniciar el turno: {e}"))?;
        for warn in &attach_warnings {
            self.emit(id, mk_event("info", json!({ "text": warn })));
        }
        if let Some(notice) = switch_notice {
            self.emit(id, mk_event("info", json!({ "text": notice })));
        }
        if !retry {
            self.emit(id, mk_event("text", event));
        }
        self.emit(id, mk_event("status", json!({ "status": "queued" })));

        let me = Arc::clone(self);
        let id2 = id.to_string();
        std::thread::spawn(move || {
            if let Some(_slot) = me.acquire(&brain, cupo, &id2, &name) {
                me.run_claude(&id2, &prompt, &work, &model, &effort, &mode, cdir, session);
                if retry {
                    me.post_run(&id2, &turn_id);
                }
            }
            me.finish_interruption(&id2, &turn_id);
        });

        Ok("Mensaje enviado.".to_string())
    }

    pub fn stop(&self, id: &str) -> Result<String, String> {
        let pid_to_kill: Option<u32> = {
            let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
            let mut m = self
                .get(id)
                .ok_or_else(|| "Tarea no encontrada.".to_string())?;
            let status = m.s("status");
            if !turn_busy(&status) || status == "stopping" {
                return Ok("El turno ya terminó o se está interrumpiendo.".into());
            }
            let queued = {
                let mut sched = match self.sched.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                if let Some(pos) = sched.waiting.iter().position(|w| w.id == id) {
                    sched.waiting.remove(pos);
                    true
                } else {
                    false
                }
            };
            if let Some(rid) = m.os("route_id") {
                crate::learn::discard_pending("", Some(&rid));
            }
            put(&mut m, "status", json!("stopping"));
            put(&mut m, "error", json!("Stopped by user"));
            put(&mut m, "updated", json!(now()));
            self.save(&m);
            self.emit(id, mk_event("status", json!({ "status": "stopping" })));
            if queued {
                self.cv.notify_all();
            }
            let procs = match self.procs.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            // Terminate the entire process tree BEFORE killing the parent handle so
            // parent-child PID links remain intact for `taskkill /T /F`.
            if let Some(c) = procs.get(id) {
                Some(c.id())
            } else {
                m["pid"]
                    .as_u64()
                    .and_then(|p| u32::try_from(p).ok())
                    .filter(|p| *p > 0)
            }
        };
        if let Some(pid) = pid_to_kill {
            std::thread::spawn(move || terminate_tree(pid));
        }
        Ok("Stopping…".into())
    }

    pub(super) fn interrupted(&self, id: &str) -> bool {
        self.get(id)
            .is_some_and(|m| matches!(m.s("status").as_str(), "stopping" | "stopped"))
    }

    pub(super) fn finish_interruption(&self, id: &str, turn_id: &str) {
        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut m) = self.get(id) else { return };
        if m.s("status") != "stopping" || m.s("turn_id") != turn_id {
            return;
        }
        put(&mut m, "status", json!("stopped"));
        put(&mut m, "pid", json!(0));
        put(&mut m, "updated", json!(now()));
        self.save(&m);
        self.emit(id, mk_event("status", json!({ "status": "stopped" })));
    }

    pub fn approve(&self, id: &str) -> Result<String, String> {
        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = self
            .get(id)
            .ok_or_else(|| "Tarea no encontrada.".to_string())?;
        if m.s("status") != "review" {
            return Err("Solo se pueden aprobar tareas en revisión.".into());
        }
        let branch = m.s("branch");
        if !branch.is_empty() {
            let work = task_work_dir(&m)?;
            let changes = checked_git(&work, &["status", "--porcelain", "--untracked-files=all"])?;
            if !changes.is_empty() {
                return Err(format!(
                    "Revisa los cambios en {} y guárdalos en un commit antes de aprobar.",
                    work.display()
                ));
            }
        } else if m.b("worktree", false) && can_edit(&m.s("kind")) {
            return Err("La tarea no tiene el worktree solicitado.".into());
        }
        put(&mut m, "status", json!("done"));
        put(&mut m, "updated", json!(now()));
        write_json(&self.meta_path(id), &m)
            .map_err(|e| format!("No se pudo registrar la aprobación: {e}"))?;
        self.emit(id, mk_event("status", json!({ "status": "done" })));
        if !branch.is_empty() {
            Ok(format!("Aprobada. Integración pendiente. Desde la rama de destino ejecuta git merge {branch}"))
        } else {
            Ok("Aprobada.".into())
        }
    }

    pub fn discard(&self, id: &str) -> Result<String, String> {
        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = self
            .get(id)
            .ok_or_else(|| "Tarea no encontrada.".to_string())?;
        if turn_busy(&m.s("status")) {
            return Err("La tarea sigue en ejecución.".into());
        }
        let cwd = PathBuf::from(m.s("cwd"));
        let wt = m.s("wt_path");
        let branch = m.s("branch");
        let owner = m.os("worktree_owner").unwrap_or_else(|| id.to_string());
        if wt.is_empty() || branch != format!("metabrain/{owner}") || !m.s("parent_run").is_empty()
        {
            return Err("La tarea no tiene una copia aislada identificable.".into());
        }
        let listed = checked_git(&cwd, &["worktree", "list", "--porcelain", "-z"])?;
        let expected = format!("branch refs/heads/{branch}");
        let worktree = Path::new(&wt);
        let registered = listed.split("\0\0").any(|entry| {
            entry
                .split('\0')
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
                .is_some_and(|path| {
                    Path::new(path) == worktree
                        || fs::canonicalize(path)
                            .ok()
                            .zip(fs::canonicalize(worktree).ok())
                            .is_some_and(|(a, b)| a == b)
                })
                && entry.split('\0').any(|line| line == expected)
        });
        if registered {
            checked_git(&cwd, &["worktree", "remove", "--force", &wt])?;
        } else if worktree.exists() {
            return Err("Git no reconoce la carpeta como la copia aislada de esta tarea.".into());
        }
        let branch_ref = format!("refs/heads/{branch}");
        if !checked_git(&cwd, &["for-each-ref", "--format=%(refname)", &branch_ref])?
            .trim()
            .is_empty()
        {
            checked_git(&cwd, &["branch", "-D", "--", &branch])?;
        }
        put(&mut m, "wt_path", json!(""));
        put(&mut m, "branch", json!(""));
        put(&mut m, "wt_base", json!(""));
        put(&mut m, "status", json!("done"));
        put(&mut m, "updated", json!(now()));
        write_json(&self.meta_path(id), &m)
            .map_err(|e| format!("No se pudo guardar el estado: {e}"))?;
        self.emit(id, mk_event("status", json!({ "status": "descartada" })));
        Ok("Descartada.".to_string())
    }

    pub fn diff(&self, id: &str) -> String {
        let Some(m) = self.get(id) else {
            return "Tarea no encontrada.".into();
        };
        let result = (|| {
            let dir = PathBuf::from(m.s("cwd"));
            let branch = m.s("branch");
            let worktree = PathBuf::from(m.s("wt_path"));
            let saved_base = m.s("wt_base");
            let (work, base) = if branch.is_empty() {
                (dir.as_path(), "HEAD".to_string())
            } else if worktree.is_dir() {
                let base = if saved_base.is_empty() {
                    checked_git(&dir, &["merge-base", "HEAD", &branch])?
                        .trim()
                        .to_string()
                } else {
                    saved_base
                };
                (worktree.as_path(), base)
            } else {
                return checked_git(&dir, &["diff", &format!("HEAD...{branch}"), "--"]);
            };
            let mut diff = if branch.is_empty()
                && git_cmd(work, &["rev-parse", "--verify", "HEAD"])
                    .is_some_and(|out| !out.status.success())
            {
                let mut staged = checked_git(work, &["diff", "--cached", "--"])?;
                staged.push_str(&checked_git(work, &["diff", "--"])?);
                staged
            } else {
                checked_git(work, &["diff", &base, "--"])?
            };
            let untracked =
                checked_git(work, &["ls-files", "--others", "--exclude-standard", "-z"])?;
            for path in untracked.split_terminator('\0') {
                if diff.chars().count() >= 200_000 {
                    break;
                }
                let path = format!("./{path}");
                if fs::symlink_metadata(work.join(&path)).is_ok_and(|meta| meta.len() > 1_000_000) {
                    diff.push_str(&format!(
                        "Archivo nuevo: {path} (vista previa omitida por tamaño)\n"
                    ));
                    continue;
                }
                let output = git_cmd(work, &["diff", "--no-index", "--", "/dev/null", &path])
                    .ok_or_else(|| "No se pudo ejecutar Git.".to_string())?;
                if !matches!(output.status.code(), Some(0 | 1)) {
                    return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
                }
                if output.stdout.is_empty() {
                    diff.push_str(&format!("Archivo nuevo: {path}\n"));
                } else {
                    diff.push_str(&String::from_utf8_lossy(&output.stdout));
                }
            }
            Ok(diff)
        })();
        match result {
            Ok(diff) => clip(&diff, 200_000),
            Err(error) => format!("No se pudieron obtener los cambios. {error}"),
        }
    }
}

pub(crate) fn task_work_dir(meta: &Value) -> Result<PathBuf, String> {
    let worktree = meta.s("wt_path");
    let branch = meta.s("branch");
    if !worktree.is_empty() {
        let path = PathBuf::from(worktree);
        if branch.is_empty()
            || !path.is_dir()
            || checked_git(&path, &["symbolic-ref", "--short", "HEAD"])
                .map_or(true, |head| head.trim() != branch)
        {
            return Err(format!("No se encontró el worktree de la rama {branch}."));
        }
        return Ok(path);
    }
    if meta.b("worktree", false) || !branch.is_empty() {
        return Err("La tarea no tiene el worktree solicitado. Inicia una tarea nueva.".into());
    }
    Ok(PathBuf::from(meta.s("cwd")))
}
