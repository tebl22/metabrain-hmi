//! Task execution runners: Claude Code, consumer CLI agents (Codex/Antigravity/Gemini),
//! OpenAI-compatible APIs, post-run verification, and API conversation history compaction.

use super::helpers::{
    cached_system, checked_git, clip, events_from_line, find_brain_cfg, mk_event, new_id, put,
    resolve_claude, terminate_tree,
};
use super::lifecycle::task_work_dir;
use super::{TaskManager, MAX_ESCALATIONS};
use crate::util::{expand_s, hidden, log, now, secret, write_json, J};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

impl TaskManager {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run(
        self: Arc<Self>,
        id: String,
        cfg: Value,
        kind: String,
        prompt: String,
        base: PathBuf,
        model: String,
        effort: String,
        mode: String,
        worktree: bool,
    ) {
        let me = Arc::clone(&self);
        let id2 = id.clone();
        me.clone()
            .run_inner(id, cfg, kind, prompt, base, model, effort, mode, worktree);
        me.post_run(&id2, &id2);
    }

    /// A routed or verification-flagged "build" task that finished: run the project's check.
    /// Pass => verified success for the router. Fail => verified failure, the task is marked failed
    /// and (for top-level routed tasks, up to MAX_ESCALATIONS times) escalated to the next model.
    pub(super) fn post_run(self: &Arc<Self>, id: &str, turn_id: &str) {
        let turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut m) = self.get(id) else { return };
        if m.s("turn_id") != turn_id {
            return;
        }
        let is_parallel_child = !m.s("parent_run").is_empty();
        if !is_parallel_child
            && m.s("status") == "failed"
            && !m.s("route_cand").is_empty()
            && m["escalations"].as_u64().unwrap_or(0) == 0
            && crate::succession::classify_error(&m.s("error")).is_some()
            && !self
                .events_since(id, 0)
                .0
                .iter()
                .any(|e| matches!(e.s("type").as_str(), "tool_use" | "text"))
        {
            let callback = self
                .escalator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let err_s = m.s("error");
            let meta_snap = m.clone();
            drop(turn);
            if let Some(next) = callback.and_then(|f| f(&meta_snap, &err_s)) {
                let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
                let mut cur = self.get(id).unwrap_or(meta_snap);
                put(&mut cur, "escalated_to", json!(next));
                self.save(&cur);
                self.emit(
                    id,
                    mk_event("status", json!({"status":"escalated","to":next})),
                );
            }
            return;
        }
        let should_verify = m.b("verify_on_finish", false) || !m.s("route_cand").is_empty();
        if m.s("status") != "review" || m.s("mode") != "build" || !should_verify {
            return;
        }
        let dir = {
            let w = m.s("wt_path");
            if w.is_empty() {
                m.s("cwd")
            } else {
                w
            }
        };
        let logical_home = {
            let p = m.s("cwd_project");
            if p.is_empty() {
                m.s("cwd")
            } else {
                p
            }
        };
        let project_name = Path::new(&logical_home)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|s| !s.is_empty());
        let vcfg = self
            .verify_cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(check) =
            crate::verify::detect_for_project(Path::new(&dir), project_name.as_deref(), &vcfg)
        else {
            return;
        };

        put(&mut m, "status", json!("verifying"));
        put(&mut m, "updated", json!(now()));
        self.save(&m);
        self.emit(id, mk_event("status", json!({ "status": "verifying" })));
        self.emit(
            id,
            mk_event("verify", json!({ "phase": "start", "cmd": check.label })),
        );

        drop(turn);
        let result = crate::verify::run_until(&check, 900, || self.interrupted(id));
        let turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        if self.interrupted(id) {
            return;
        }
        let Some(res) = result else {
            // The check could not start (tool missing): not the model's fault.
            self.emit(
                id,
                mk_event("verify", json!({ "phase": "skipped", "cmd": check.label, "text": "No se pudo ejecutar la verificación en este equipo." })),
            );
            let mut m = self.get(id).unwrap_or(Value::Null);
            put(&mut m, "status", json!("review"));
            self.save(&m);
            self.emit(id, mk_event("status", json!({ "status": "review" })));
            return;
        };
        self.emit(
            id,
            mk_event("verify", json!({ "phase": "done", "ok": res.ok, "cmd": check.label, "secs": res.secs, "text": crate::verify::tail(&res.tail, 1500) })),
        );

        let route_id = m.s("route_id");
        if !is_parallel_child && !route_id.is_empty() {
            let router = crate::router::Router::new(None);
            let project = project_name.clone().unwrap_or_else(|| "general".into());
            crate::learn::outcome_with_tokens(
                &router,
                Some(&project),
                Some(route_id.as_str()),
                res.ok,
                crate::learn::W_VERIFIED,
                if res.ok {
                    "verificación automática ok"
                } else {
                    "verificación automática falló"
                },
                m.f("last_turn_tokens").unwrap_or(0.0) as i64,
            );
        }

        let mut m = self.get(id).unwrap_or(Value::Null);
        put(&mut m, "verified", json!(res.ok));
        put(&mut m, "verify_cmd", json!(check.label));
        put(&mut m, "updated", json!(now()));
        if res.ok {
            put(&mut m, "status", json!("review"));
            self.save(&m);
            self.emit(id, mk_event("status", json!({ "status": "review" })));
            return;
        }
        put(&mut m, "status", json!("failed"));
        put(
            &mut m,
            "error",
            json!(format!("No pasó la verificación ({})", check.label)),
        );
        self.save(&m);
        self.emit(id, mk_event("status", json!({ "status": "failed" })));

        // Parallel branch failures are managed by ParallelEngine; do not spawn detached escalations
        // that would race on the shared worktree.
        if is_parallel_child {
            return;
        }

        let n = m.get("escalations").and_then(|v| v.as_u64()).unwrap_or(0);
        if n >= MAX_ESCALATIONS {
            self.emit(
                id,
                mk_event(
                    "error",
                    json!({ "text": "Se alcanzó el máximo de escalados; revísala tú." }),
                ),
            );
            return;
        }
        let esc = self
            .escalator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let meta_snap = m.clone();
        drop(turn);
        if let Some(f) = esc {
            if let Some(next) = f(&meta_snap, &res.tail) {
                let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
                let mut m = self.get(id).unwrap_or(Value::Null);
                put(&mut m, "escalated_to", json!(next));
                self.save(&m);
                self.emit(
                    id,
                    mk_event("status", json!({ "status": "escalated", "to": next })),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_inner(
        self: Arc<Self>,
        id: String,
        cfg: Value,
        kind: String,
        prompt: String,
        base: PathBuf,
        model: String,
        effort: String,
        mode: String,
        worktree: bool,
    ) {
        if self.interrupted(&id) {
            return;
        }
        if kind == "openai" {
            self.run_openai(&id, &cfg, &model, &effort, &prompt);
            return;
        }

        let work = if let Some(meta) = self
            .get(&id)
            .filter(|m| !m.s("wt_path").is_empty() || !m.s("branch").is_empty())
        {
            match task_work_dir(&meta) {
                Ok(path) => path,
                Err(e) => {
                    self.fail(&id, &e);
                    return;
                }
            }
        } else if worktree {
            let br = format!("metabrain/{id}");
            let result = (|| {
                let base_commit = checked_git(&base, &["rev-parse", "--verify", "HEAD"])?;
                // Ensure `.metabrain/` is excluded in the target repo's local `.git/info/exclude`
                // so worktrees never pollute `git status` or `TaskManager::diff()`.
                if let Ok(excl_rel) =
                    checked_git(&base, &["rev-parse", "--git-path", "info/exclude"])
                {
                    let excl_p = {
                        let p = PathBuf::from(excl_rel.trim());
                        if p.is_absolute() {
                            p
                        } else {
                            base.join(p)
                        }
                    };
                    if let Some(parent) = excl_p.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let existing = fs::read_to_string(&excl_p).unwrap_or_default();
                    if !existing
                        .lines()
                        .any(|l| l.trim() == ".metabrain/" || l.trim() == ".metabrain")
                    {
                        if let Ok(mut f) = fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&excl_p)
                        {
                            let prefix = if existing.is_empty() || existing.ends_with('\n') {
                                ""
                            } else {
                                "\n"
                            };
                            let _ = writeln!(f, "{prefix}.metabrain/");
                        }
                    }
                }
                let folder = base.join(".metabrain").join("wt");
                fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
                let wt = std::path::absolute(&folder)
                    .map_err(|e| e.to_string())?
                    .join(&id);
                checked_git(
                    &base,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        &br,
                        &wt.to_string_lossy(),
                        base_commit.trim(),
                    ],
                )?;
                Ok::<_, String>((wt, base_commit))
            })();
            let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
            let stopped = self.interrupted(&id);
            let wt = match result {
                Ok((wt, base_commit)) => {
                    let wt_s = wt.to_string_lossy();
                    let Some(mut m) = self.get(&id) else { return };
                    put(&mut m, "wt_path", json!(wt_s));
                    put(&mut m, "branch", json!(br));
                    put(&mut m, "wt_base", json!(base_commit.trim()));
                    put(&mut m, "updated", json!(now()));
                    if let Err(e) = write_json(&self.meta_path(&id), &m) {
                        log(&format!(
                            "tarea {id}: worktree sin registrar en {wt_s}: {e}"
                        ));
                        if !stopped {
                            self.fail(
                                &id,
                                &format!("No se pudo registrar el worktree en {wt_s}: {e}"),
                            );
                        }
                        return;
                    }
                    wt
                }
                Err(e) => {
                    if !stopped {
                        self.fail(&id, &format!("No se pudo crear el worktree: {e}"));
                    }
                    return;
                }
            };
            if stopped {
                return;
            }
            wt
        } else {
            base
        };

        if let Some(agent) = crate::cli_agents::Agent::from_kind(&kind) {
            self.run_cli(&id, agent, &prompt, &work, &model, &effort, &mode);
            return;
        }

        let cdir = cfg.os("config_dir");
        self.run_claude(&id, &prompt, &work, &model, &effort, &mode, cdir, None);
    }

    /// Run a consumer-account CLI agent (Codex / Gemini CLI / Antigravity) and publish its events.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_cli(
        &self,
        id: &str,
        agent: crate::cli_agents::Agent,
        prompt: &str,
        work: &Path,
        model: &str,
        effort: &str,
        mode: &str,
    ) {
        use crate::cli_agents::Event;
        let codex = agent == crate::cli_agents::Agent::Codex;
        let turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        let mut meta = self.get(id).unwrap_or(Value::Null);
        if matches!(meta.s("status").as_str(), "stopping" | "stopped") {
            return;
        }
        put(&mut meta, "status", json!("running"));
        put(&mut meta, "updated", json!(now()));
        self.save(&meta);
        self.emit(id, mk_event("status", json!({ "status": "running" })));
        let brain_for_guard = meta.s("brain");
        drop(turn);
        let _active_guard = super::ActiveTaskGuard::enter(self, id, &brain_for_guard);

        // "build" may edit files; "plan" only reads.
        let cli_mode = if mode == "plan" { "read" } else { "write" };
        let allow_unsafe = crate::util::read_json(&crate::util::data_dir().join("metabrain.json"))
            .and_then(|v| v.get("allow_unsafe_exec").and_then(|b| b.as_bool()))
            .unwrap_or(false);
        if cli_mode == "write" && allow_unsafe {
            let permissions = match agent {
                crate::cli_agents::Agent::Codex if cfg!(windows) => {
                    Some("Codex is running with unsafe full filesystem access (allow_unsafe_exec enabled).")
                }
                crate::cli_agents::Agent::Antigravity => {
                    Some("Antigravity is running without permission prompts (allow_unsafe_exec enabled).")
                }
                _ => None,
            };
            if let Some(text) = permissions {
                self.emit(id, mk_event("info", json!({ "text": text })));
            }
        }
        let mut pending = String::new();
        let mut pending_thinking = String::new();
        let mut last_text_flush = Instant::now();
        let mut last_think_flush = Instant::now();
        let mut started_tools: HashMap<String, bool> = HashMap::new();
        // Follow-up turns add to what the conversation already spent.
        let turn_id = meta.s("turn_id");
        let mut tokens = meta.f("tokens").unwrap_or(0.0) as u64;
        let base_tokens = tokens;
        let mut cached_tokens = meta["cached_tokens"].as_u64();
        let mut last_save = Instant::now();
        let mut on_ev = |ev: Event| match ev {
            Event::Text(t) if codex => {
                if !pending_thinking.trim().is_empty() {
                    self.emit(id, mk_event("thinking", json!({ "text": clip(&std::mem::take(&mut pending_thinking), 2000), "delta": true })));
                }
                self.emit(id, mk_event("text", json!({ "text": t })));
            }
            Event::Text(t) => {
                if !pending_thinking.trim().is_empty() {
                    self.emit(id, mk_event("thinking", json!({ "text": clip(&std::mem::take(&mut pending_thinking), 2000), "delta": true })));
                }
                pending.push_str(&t);
                if pending.contains('\n')
                    || pending.len() >= 80
                    || last_text_flush.elapsed() >= Duration::from_millis(200)
                {
                    last_text_flush = Instant::now();
                    self.emit(
                        id,
                        mk_event(
                            "text",
                            json!({ "text": std::mem::take(&mut pending), "delta": true }),
                        ),
                    );
                }
            }
            Event::Thinking(t) => {
                if !pending.is_empty() {
                    self.emit(
                        id,
                        mk_event(
                            "text",
                            json!({ "text": std::mem::take(&mut pending), "delta": true }),
                        ),
                    );
                }
                if codex {
                    self.emit(id, mk_event("thinking", json!({ "text": clip(&t, 2000) })));
                } else {
                    pending_thinking.push_str(&t);
                    if pending_thinking.contains('\n')
                        || pending_thinking.len() >= 120
                        || last_think_flush.elapsed() >= Duration::from_millis(300)
                    {
                        last_think_flush = Instant::now();
                        self.emit(id, mk_event("thinking", json!({ "text": clip(&std::mem::take(&mut pending_thinking), 2000), "delta": true })));
                    }
                }
            }
            Event::Tool(name) => {
                if !pending_thinking.trim().is_empty() {
                    self.emit(id, mk_event("thinking", json!({ "text": clip(&std::mem::take(&mut pending_thinking), 2000), "delta": true })));
                }
                if !pending.is_empty() {
                    self.emit(
                        id,
                        mk_event(
                            "text",
                            json!({ "text": std::mem::take(&mut pending), "delta": true }),
                        ),
                    );
                }
                self.emit(
                    id,
                    mk_event("tool_use", json!({ "name": name, "input": {} })),
                );
            }
            Event::ToolStarted(mut tool) => {
                if !pending_thinking.trim().is_empty() {
                    self.emit(id, mk_event("thinking", json!({ "text": clip(&std::mem::take(&mut pending_thinking), 2000), "delta": true })));
                }
                if !pending.is_empty() {
                    self.emit(
                        id,
                        mk_event(
                            "text",
                            json!({ "text": std::mem::take(&mut pending), "delta": true }),
                        ),
                    );
                }
                let tid = tool.s("tool_id");
                let has_input = tool
                    .get("input")
                    .and_then(|v| v.as_object())
                    .is_some_and(|o| !o.is_empty());
                if !tid.is_empty() {
                    if let Some(&prev_has_input) = started_tools.get(&tid) {
                        if prev_has_input || !has_input {
                            return;
                        }
                    }
                    started_tools.insert(tid, has_input);
                }
                put(&mut tool, "turn_id", json!(turn_id));
                self.emit(id, mk_event("tool_use", tool));
            }
            Event::ToolFinished(mut tool) => {
                if !pending_thinking.trim().is_empty() {
                    self.emit(id, mk_event("thinking", json!({ "text": clip(&std::mem::take(&mut pending_thinking), 2000), "delta": true })));
                }
                if !pending.is_empty() {
                    self.emit(
                        id,
                        mk_event(
                            "text",
                            json!({ "text": std::mem::take(&mut pending), "delta": true }),
                        ),
                    );
                }
                put(&mut tool, "turn_id", json!(turn_id));
                self.emit(id, mk_event("tool_result", tool));
            }
            Event::Usage {
                input,
                output,
                cached,
            } => {
                tokens += input + output;
                if codex {
                    cached_tokens = Some(cached_tokens.unwrap_or(0) + cached.min(input));
                }
                self.emit(
                    id,
                    mk_event(
                        "usage",
                        json!({ "tokens": tokens, "cached_tokens": cached_tokens, "usd": 0.0 }),
                    ),
                );
                if last_save.elapsed() >= Duration::from_secs(2) {
                    last_save = Instant::now();
                    let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
                    let mut m = self.get(id).unwrap_or(Value::Null);
                    put(&mut m, "tokens", json!(tokens));
                    put(&mut m, "last_turn_tokens", json!(tokens - base_tokens));
                    if let Some(cached) = cached_tokens {
                        put(&mut m, "cached_tokens", json!(cached));
                    }
                    put(&mut m, "updated", json!(now()));
                    self.save(&m);
                }
            }
            Event::Done { .. } => {}
        };
        let mut on_pid = |pid: u32| {
            let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
            let mut m = self.get(id).unwrap_or(Value::Null);
            if m.s("status") != "running" {
                drop(_turn);
                terminate_tree(pid);
                return false;
            }
            put(&mut m, "pid", json!(pid));
            self.save(&m);
            true
        };
        let bcfg = find_brain_cfg(&self.get(id).map(|m| m.s("brain")).unwrap_or_default());
        let env = if codex {
            vec![(
                "CODEX_HOME".to_string(),
                crate::cli_agents::codex_home(&bcfg).display().to_string(),
            )]
        } else {
            vec![]
        };
        let res = crate::cli_agents::run_blocking_until(
            agent,
            prompt,
            &work.to_string_lossy(),
            model,
            effort,
            cli_mode,
            &env,
            3600,
            &mut on_ev,
            &mut on_pid,
            &mut || self.interrupted(id),
        );
        if !pending_thinking.trim().is_empty() {
            self.emit(
                id,
                mk_event(
                    "thinking",
                    json!({ "text": clip(&pending_thinking, 2000), "delta": true }),
                ),
            );
        }
        if !pending.is_empty() {
            self.emit(
                id,
                mk_event("text", json!({ "text": pending, "delta": true })),
            );
        }
        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        if self.interrupted(id) {
            return;
        }
        match res {
            Ok(_) => {
                let mut m = self.get(id).unwrap_or(Value::Null);
                put(&mut m, "tokens", json!(tokens));
                put(&mut m, "last_turn_tokens", json!(tokens - base_tokens));
                if let Some(cached) = cached_tokens {
                    put(&mut m, "cached_tokens", json!(cached));
                }
                put(&mut m, "status", json!("review"));
                put(&mut m, "pid", json!(0));
                put(&mut m, "updated", json!(now()));
                self.save(&m);
                crate::succession::clear_error(&m.s("brain"));
                self.emit(id, mk_event("status", json!({ "status": "review" })));
            }
            Err(e) => {
                let msg = e
                    .strip_prefix("LIMITE: ")
                    .map(|r| format!("Límite de uso alcanzado: {r}"))
                    .unwrap_or(e);
                let label = self
                    .get(id)
                    .map(|m| {
                        let name = m.os("brain_name").unwrap_or_else(|| m.s("brain"));
                        let model = m.s("model");
                        if model.is_empty() {
                            name
                        } else {
                            format!("{name} ({model})")
                        }
                    })
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| agent.display_name().to_string());
                let full = if msg.starts_with(&label) {
                    msg
                } else {
                    format!("{label}: {msg}")
                };
                self.fail(id, &full);
            }
        }
    }

    /// Spawn claude in `work`, stream its stdout as events and finalise the meta.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_claude(
        &self,
        id: &str,
        prompt: &str,
        work: &Path,
        model: &str,
        effort: &str,
        mode: &str,
        config_dir: Option<String>,
        resume: Option<String>,
    ) {
        let (brain_for_guard, turn_id, base_tokens, base_usd) = {
            let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
            if self.interrupted(id) {
                return;
            }
            let meta = self.get(id).unwrap_or(Value::Null);
            (
                meta.s("brain"),
                meta.s("turn_id"),
                meta.f("tokens").unwrap_or(0.0),
                meta.f("usd").unwrap_or(0.0),
            )
        };
        let _active_guard = super::ActiveTaskGuard::enter(self, id, &brain_for_guard);

        let exe = match resolve_claude() {
            Some(e) => e,
            None => {
                self.fail(id, "No se encontró el ejecutable claude. Define MB_CLAUDE.");
                return;
            }
        };
        if !crate::connect::is_safe_cmd_path(&exe) {
            self.fail(
                id,
                "La ruta del ejecutable claude contiene caracteres no permitidos.",
            );
            return;
        }
        let exe_s = exe.to_string_lossy().to_string();

        // Sanitize model and resume session ID before passing to `cmd /d /c` or `claude`.
        let clean_model = match crate::cli_agents::sanitize_model(model) {
            Ok(m) => m,
            Err(e) => {
                self.fail(id, &e);
                return;
            }
        };
        let clean_resume = if let Some(s) = resume.as_deref() {
            let trimmed = s.trim();
            if trimmed.is_empty()
                || trimmed.len() > 128
                || !trimmed
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                self.fail(id, "Invalid Claude session ID.");
                return;
            }
            Some(trimmed.to_string())
        } else {
            None
        };

        let mut cmd = if exe_s.to_lowercase().ends_with(".cmd") {
            let mut c = Command::new("cmd");
            c.arg("/d").arg("/c").arg(&exe_s);
            c
        } else {
            Command::new(&exe)
        };

        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--permission-mode")
            .arg(if mode == "plan" {
                "plan"
            } else {
                "acceptEdits"
            });
        if !clean_model.is_empty() {
            cmd.arg("--model").arg(&clean_model);
        }
        if let Some(s) = clean_resume.as_deref() {
            cmd.arg("--resume").arg(s);
        }
        if let Some(cd) = config_dir
            .as_deref()
            .map(expand_s)
            .filter(|s| !s.is_empty())
        {
            cmd.env("CLAUDE_CONFIG_DIR", cd);
        }
        match effort {
            "low" => {
                cmd.env("MAX_THINKING_TOKENS", "4000");
            }
            "medium" => {
                cmd.env("MAX_THINKING_TOKENS", "12000");
            }
            "high" => {
                cmd.env("MAX_THINKING_TOKENS", "32000");
            }
            _ => {}
        }
        cmd.current_dir(work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hidden(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.fail(id, &format!("No se pudo lanzar claude: {e}"));
                return;
            }
        };
        let job = crate::util::attach_kill_job(&child).ok();

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pid = child.id();

        {
            let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
            if self.interrupted(id) {
                let _ = child.kill();
                drop(job);
                return;
            }
            let mut meta = self.get(id).unwrap_or(Value::Null);
            put(&mut meta, "pid", json!(pid));
            put(&mut meta, "status", json!("running"));
            put(&mut meta, "error", json!(""));
            put(&mut meta, "updated", json!(now()));
            self.save(&meta);
            self.emit(id, mk_event("status", json!({ "status": "running" })));
            log(&format!("tarea {id}: claude lanzado (pid {pid})"));
            let mut procs = match self.procs.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            procs.insert(id.to_string(), child);
        }

        // Attach stderr reader BEFORE writing stdin so large prompts (>4 KB) cannot deadlock OS pipes.
        let err_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let h_err = stderr.map(|se| {
            let buf = Arc::clone(&err_lines);
            std::thread::spawn(move || {
                let reader = BufReader::new(se);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            let mut v = match buf.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            v.push(l);
                            if v.len() > 20 {
                                v.remove(0);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        });

        // Write prompt asynchronously so stdin can never block stdout draining.
        if let Some(mut sin) = stdin {
            let payload = prompt.to_string();
            std::thread::spawn(move || {
                let _ = sin.write_all(payload.as_bytes());
                let _ = sin.write_all(b"\n");
                let _ = sin.flush();
            });
        }

        let mut run_tokens: i64 = 0;
        let mut run_usd: f64 = 0.0;
        let mut session = String::new();
        let mut last_write = Instant::now();
        let mut result_err: Option<String> = None;

        if let Some(out) = stdout {
            let reader = BufReader::new(out);
            for line in reader.lines() {
                if self.interrupted(id) {
                    break;
                }
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let (evs, sess, useg) = events_from_line(&line);
                for mut e in evs {
                    if e.s("type") == "error" && result_err.is_none() {
                        let t = e.s("text");
                        if !t.trim().is_empty() {
                            result_err = Some(t);
                        }
                    }
                    if e["tool_id"].is_string() {
                        put(&mut e, "turn_id", json!(turn_id));
                    }
                    self.emit(id, e);
                }
                if let Some(s) = sess {
                    session = s;
                    let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(mut m) = self.get(id) {
                        put(&mut m, "session_id", json!(session));
                        self.save(&m);
                    }
                }
                if let Some((t, c, is_err)) = useg {
                    if is_err && result_err.is_none() {
                        result_err = Some("Claude reported an execution error.".to_string());
                    }
                    run_tokens += t;
                    run_usd += c;
                    self.emit(
                        id,
                        mk_event("usage", json!({ "tokens": base_tokens as i64 + run_tokens, "usd": base_usd + run_usd })),
                    );
                }
                if last_write.elapsed() >= Duration::from_secs(2) {
                    last_write = Instant::now();
                    let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
                    let mut m = self.get(id).unwrap_or(Value::Null);
                    if !m.is_null() {
                        put(&mut m, "tokens", json!(base_tokens + run_tokens as f64));
                        put(&mut m, "usd", json!(base_usd + run_usd));
                        if !session.is_empty() {
                            put(&mut m, "session_id", json!(session));
                        }
                        put(&mut m, "updated", json!(now()));
                        self.save(&m);
                    }
                }
            }
        }

        // wait for the child to exit (stop() may kill it from another thread)
        let mut code: Option<i32> = None;
        loop {
            let mut procs = match self.procs.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let mut finished = false;
            if let Some(c) = procs.get_mut(id) {
                match c.try_wait() {
                    Ok(Some(st)) => {
                        code = st.code();
                        finished = true;
                    }
                    Ok(None) => {}
                    Err(_) => finished = true,
                }
            } else {
                finished = true;
            }
            if finished {
                procs.remove(id);
                drop(procs);
                break;
            }
            drop(procs);
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(job);
        if let Some(h) = h_err {
            let wait_end = Instant::now() + Duration::from_millis(500);
            while !h.is_finished() && Instant::now() < wait_end {
                std::thread::sleep(Duration::from_millis(15));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }

        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        if self.interrupted(id) {
            return;
        }
        let ok = code == Some(0) && result_err.is_none();
        let err_tail = {
            let v = match err_lines.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            v.join("\n")
        };

        let mut fin = self.get(id).unwrap_or(Value::Null);
        if fin.is_null() {
            fin = json!({ "id": id });
        }
        if !session.is_empty() {
            put(&mut fin, "session_id", json!(session));
        }
        put(&mut fin, "tokens", json!(base_tokens + run_tokens as f64));
        put(&mut fin, "last_turn_tokens", json!(run_tokens));
        put(&mut fin, "usd", json!(base_usd + run_usd));
        put(&mut fin, "pid", json!(0));
        put(&mut fin, "updated", json!(now()));

        if ok {
            put(&mut fin, "status", json!("review"));
            put(&mut fin, "error", json!(""));
            self.save(&fin);
            crate::succession::clear_error(&fin.s("brain"));
            self.emit(id, mk_event("status", json!({ "status": "review" })));
        } else {
            self.save(&fin);
            let msg = result_err
                .filter(|s| !s.trim().is_empty())
                .or_else(|| (!err_tail.trim().is_empty()).then(|| clip(&err_tail, 2000)))
                .unwrap_or_else(|| format!("Claude exited with status {code:?}"));
            self.fail(id, &msg);
        }
        log(&format!("tarea {id}: claude terminó (código {code:?})"));
    }

    pub(super) fn run_openai(
        &self,
        id: &str,
        cfg: &Value,
        model: &str,
        effort: &str,
        prompt: &str,
    ) {
        let brain_for_guard = self.get(id).map(|m| m.s("brain")).unwrap_or_default();
        let _active_guard = super::ActiveTaskGuard::enter(self, id, &brain_for_guard);
        self.api_turn(id, cfg, model, effort, prompt, |url, headers, body| {
            let url = url.to_owned();
            let headers = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>();
            let body = body.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let headers = headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect::<Vec<_>>();
                let _ = tx.send(crate::util::http_json(&url, &headers, Some(&body), 300));
            });
            loop {
                if self.interrupted(id) {
                    return Err("Interrumpida".into());
                }
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(result) => return result,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("Se perdió la respuesta de la API.".into())
                    }
                }
            }
        });
    }

    pub(super) fn api_turn(
        &self,
        id: &str,
        cfg: &Value,
        model: &str,
        effort: &str,
        prompt: &str,
        request: impl FnOnce(&str, &[(&str, &str)], &Value) -> Result<Value, String>,
    ) {
        let base_url = {
            let b = cfg.s("base_url");
            if b.is_empty() {
                "https://api.deepseek.com/v1".to_string()
            } else {
                b
            }
        };
        let model = {
            let m = model.trim();
            if m.is_empty() {
                let ladder = cfg.arr("ladder");
                ladder
                    .iter()
                    .filter_map(|v| v.as_str())
                    .next_back()
                    .unwrap_or("")
                    .to_string()
            } else {
                m.to_string()
            }
        };
        let key_env = cfg.s("key_env");
        let key = secret(&key_env);
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let _start = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.get(id).is_some_and(|m| m.s("status") == "queued") {
            return;
        }
        let key = match key {
            Some(k) => k,
            None => {
                self.fail(id, "falta la clave");
                return;
            }
        };

        let body = {
            let Some(mut meta) = self.get(id) else { return };
            let body = match self.openai_body(id, &meta.s("cwd"), &model, effort, &base_url, prompt)
            {
                Ok(body) => body,
                Err(e) => {
                    self.fail(
                        id,
                        &format!("No se pudo guardar o leer el prefijo de la conversación: {e}"),
                    );
                    return;
                }
            };
            put(&mut meta, "status", json!("running"));
            put(&mut meta, "pid", json!(0));
            put(&mut meta, "updated", json!(now()));
            let attempt = new_id();
            put(&mut meta, "api_attempt", json!(attempt));
            self.save(&meta);
            self.emit(id, mk_event("status", json!({ "status": "running" })));
            (body, attempt)
        };
        drop(_start);
        let (body, attempt) = body;

        let auth = format!("Bearer {key}");
        let headers: [(&str, &str); 2] = [
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
        ];
        let response = request(&url, &headers, &body);
        let _turn = self.turn_state.lock().unwrap_or_else(|e| e.into_inner());
        if !self
            .get(id)
            .is_some_and(|m| m.s("status") == "running" && m.s("api_attempt") == attempt)
        {
            return;
        }
        match response {
            Ok(res) => {
                let text = res
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.is_empty() {
                    self.fail(id, "La API devolvió una respuesta vacía.");
                    return;
                }
                let u = res.get("usage").cloned().unwrap_or(Value::Null);
                let tokens = u.get("total_tokens").and_then(|t| t.as_i64()).unwrap_or(0);
                // Input served from the provider's prompt cache (DeepSeek / OpenAI-style fields).
                let cached = u
                    .get("prompt_cache_hit_tokens")
                    .and_then(|t| t.as_i64())
                    .or_else(|| {
                        u.get("prompt_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|t| t.as_i64())
                    })
                    .unwrap_or(0);
                self.emit(id, mk_event("text", json!({ "text": text })));
                let input = u["prompt_tokens"].as_i64().unwrap_or(0);
                let output = u["completion_tokens"].as_i64().unwrap_or(0);
                let fresh = (input - cached).max(0);
                let rates = cfg["prices"]
                    .get(model.as_str())
                    .filter(|rates| rates.is_object())
                    .unwrap_or(&cfg["pricing"][&model]);
                let usd = rates["input"]
                    .as_f64()
                    .zip(rates["output"].as_f64())
                    .map(|(i, o)| {
                        (fresh as f64 * i
                            + cached as f64 * rates["cached_input"].as_f64().unwrap_or(i)
                            + output as f64 * o)
                            / 1_000_000.0
                    });
                self.emit(id, mk_event("usage", json!({ "tokens": tokens, "input_new": fresh, "cached": cached, "output": output, "usd": usd })));
                if let Some(mut mm) = self.get(id) {
                    let c0 = mm.f("cached").unwrap_or(0.0);
                    put(&mut mm, "cached", json!(c0 + cached as f64));
                    self.save(&mm);
                }

                let mut m = self.get(id).unwrap_or(Value::Null);
                let bt = m.f("tokens").unwrap_or(0.0);
                let bu = m.f("usd").unwrap_or(0.0);
                put(&mut m, "tokens", json!(bt + tokens as f64));
                put(&mut m, "last_turn_tokens", json!(tokens));
                put(&mut m, "usd", json!(bu + usd.unwrap_or(0.0)));
                put(&mut m, "status", json!("review"));
                put(&mut m, "updated", json!(now()));
                self.save(&m);
                crate::succession::clear_error(&m.s("brain"));
                self.emit(id, mk_event("status", json!({ "status": "review" })));
            }
            Err(e) => {
                let provider = cfg.os("name").unwrap_or_else(|| "Proveedor API".into());
                self.fail(id, &format!("{provider} ({model}): {e}"));
            }
        }
    }

    pub(super) fn openai_body(
        &self,
        id: &str,
        cwd: &str,
        model: &str,
        effort: &str,
        base_url: &str,
        prompt: &str,
    ) -> std::io::Result<Value> {
        // Keep the snapshot alongside the task without adding it to the task's public metadata.
        let path = self.root.join(format!("{id}.system"));
        let system = match fs::read_to_string(&path) {
            Ok(stored) => serde_json::from_str::<String>(&stored).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let system = cached_system(cwd);
                write_json(&path, &json!(system))?;
                system
            }
            Err(e) => return Err(e),
        };
        let mut messages = vec![json!({ "role": "system", "content": system })];
        messages.extend(self.history(id, prompt));
        let mut body = json!({ "model": model, "stream": false, "messages": messages });
        if base_url.to_lowercase().contains("deepseek") {
            body["thinking"] =
                json!({ "type": if effort == "high" { "enabled" } else { "disabled" } });
        }
        Ok(body)
    }

    /// Original request, earlier exchanges and the user message awaiting an answer.
    pub(super) fn history(&self, id: &str, prompt: &str) -> Vec<Value> {
        const HISTORY_LIMIT: usize = 24_000;
        const RECENT_TURNS: usize = 4;
        let (evs, _) = self.events_since(id, 0);
        let mut out = vec![json!({"role": "user", "content": prompt})];
        for ev in evs {
            if ev.s("type") != "text" {
                continue;
            }
            let t = ev.s("text");
            match t.strip_prefix("Tú: ") {
                Some(u) => out.push(json!({"role": "user", "content": u})),
                None => {
                    if out.last().is_some_and(|m| m.s("role") == "assistant") {
                        if let Some(last) = out.last_mut() {
                            let prev = last.s("content");
                            if ev.b("delta", false) {
                                last["content"] = json!(format!("{prev}{t}"));
                            } else {
                                last["content"] = json!(format!("{prev}\n{t}"));
                            }
                        }
                    } else {
                        out.push(json!({"role": "assistant", "content": t}));
                    }
                }
            }
        }

        // Exclude the pending request when counting and measuring prior turns.
        let prior = &out[..out.len() - 1];
        let prior_chars: usize = prior
            .iter()
            .map(|m| m["content"].as_str().unwrap_or_default().chars().count())
            .sum();
        if prior_chars > HISTORY_LIMIT {
            let keep_from = prior
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, m)| m["role"] == "user")
                .nth(RECENT_TURNS - 1)
                .map_or(0, |(i, _)| i);
            if keep_from > 0 {
                let old = &out[..keep_from];
                let mut summary = format!(
                    "Resumen de mensajes anteriores ({} mensajes, extractos):",
                    old.len()
                );
                // Keep the original objective and a few excerpts nearest the retained turns.
                for message in old
                    .iter()
                    .take(1)
                    .chain(old.iter().skip(old.len().saturating_sub(6).max(1)))
                {
                    let role = if message["role"] == "user" {
                        "Usuario"
                    } else {
                        "Asistente"
                    };
                    let content = message["content"].as_str().unwrap_or_default().trim();
                    let (excerpt, suffix) = match content.char_indices().nth(200) {
                        Some((end, _)) => (&content[..end], "…"),
                        None => (content, ""),
                    };
                    summary.push_str(&format!("\n- {role}: {excerpt}{suffix}"));
                }
                let next_is_user = out.get(keep_from).is_some_and(|m| m["role"] == "user");
                let mut replacement = vec![json!({"role": "user", "content": summary})];
                if next_is_user {
                    replacement.push(json!({"role": "assistant", "content": "Understood, continuing with that context."}));
                }
                out.splice(..keep_from, replacement);
            }
            let end = out.len() - 1;
            let mut excess = out[..end]
                .iter()
                .map(|m| m["content"].as_str().unwrap_or_default().chars().count())
                .sum::<usize>()
                .saturating_sub(HISTORY_LIMIT);
            for message in &mut out[..end] {
                if excess == 0 {
                    break;
                }
                let content = message["content"].as_str().unwrap_or_default();
                let size = content.chars().count();
                let trim = excess.min(size.saturating_sub(200));
                if trim > 0 {
                    let clipped = clip(content, size - trim - 1);
                    message["content"] = json!(format!("{clipped}…"));
                    excess -= trim;
                }
            }
            if excess > 0 {
                let mut drop_count = 0;
                for message in &out[..end] {
                    if excess == 0 {
                        break;
                    }
                    excess = excess.saturating_sub(
                        message["content"]
                            .as_str()
                            .unwrap_or_default()
                            .chars()
                            .count(),
                    );
                    drop_count += 1;
                }
                out.drain(..drop_count);
                // Ensure the compacted conversation still starts with a `"user"` turn
                // (required by strict OpenAI/DeepSeek reasoning endpoints).
                while out.len() > 1 && out[0]["role"] == "assistant" {
                    out.remove(0);
                }
            }
        }
        out
    }
}
