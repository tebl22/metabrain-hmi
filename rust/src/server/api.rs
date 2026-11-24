//! REST/SSE API endpoint dispatcher for MetaBrain HMI (`/api/*`).
use super::http::{qparam, qr_svg, read_body, req_header, send_json, send_raw, TaskEvents};
use super::lan_ip;
use super::security::{redact_remote_secrets, safe_name, safe_upload_dir, UPLOAD_NONCE};
use crate::core::Supervisor;
use crate::util::{now, truthy, J};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tiny_http::{Header, Request, Response, StatusCode};

fn truthy_v(v: Option<&Value>) -> bool {
    truthy(v)
}

fn attachment_paths(body: &Value) -> Vec<String> {
    body.arr("files")
        .into_iter()
        .filter_map(|file| {
            file.as_str()
                .filter(|path| !path.trim().is_empty())
                .map(String::from)
        })
        .collect()
}

pub(super) fn dispatch(
    sup: &Arc<Supervisor>,
    mut req: Request,
    method: &str,
    path: &str,
    local: bool,
    lan_listening: bool,
) {
    if path == "/api/show" && method == "POST" {
        crate::gui::request_show();
        send_json(req, 200, &json!({"ok": true, "app": "MetaBrain"}), &[]);
        return;
    }
    if path == "/api/health" && method == "GET" && local {
        send_json(req, 200, &json!({"pid": std::process::id()}), &[]);
        return;
    }
    if path == "/api/state" {
        send_json(req, 200, &sup.snapshot(), &[]);
        return;
    }

    if path == "/api/act" && method == "POST" {
        let Some(d) = read_body(&mut req) else {
            send_json(req, 400, &json!({"error": "bad JSON"}), &[]);
            return;
        };
        let a = d.get("a").and_then(|v| v.as_str()).unwrap_or("");
        if (a == "config" || a == "kill") && !local {
            send_json(req, 403, &json!({"error": "only from this computer"}), &[]);
            return;
        }
        let result = sup.act(&d);
        send_json(req, 200, &json!({"result": result}), &[]);
        return;
    }

    if path == "/api/config" {
        handle_config(sup, req, method, local);
        return;
    }

    if path == "/api/router" {
        let q = req
            .url()
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();
        let project = q
            .split('&')
            .find_map(|kv| kv.strip_prefix("project="))
            .map(|v| v.replace("%20", " "));
        send_json(
            req,
            200,
            &sup.router_table(project.as_deref().filter(|p| !p.is_empty())),
            &[],
        );
        return;
    }

    if path == "/api/route" && method == "POST" {
        let d = read_body(&mut req).unwrap_or_else(|| json!({}));
        let task = d.get("task").and_then(|v| v.as_str()).unwrap_or("");
        if task.trim().is_empty() {
            send_json(req, 400, &json!({"error": "task is required"}), &[]);
            return;
        }
        let project = d
            .get("project")
            .and_then(|v| v.as_str())
            .unwrap_or("general");
        let level = d
            .get("level")
            .and_then(|v| v.as_u64())
            .map(|l| l.clamp(1, 5) as u8);
        send_json(req, 200, &sup.route_task(project, task, level), &[]);
        return;
    }

    if path == "/api/outcome" && method == "POST" {
        handle_outcome(sup, req);
        return;
    }

    // ---------- parallel engine ----------
    if path == "/api/parallel" && method == "GET" {
        send_json(req, 200, &sup.parallel.list(100), &[]);
        return;
    }
    if let Some(rest) = path.strip_prefix("/api/parallel/") {
        let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
        match (method, action) {
            ("GET", "") => match sup.parallel.get(id) {
                Some(r) => send_json(req, 200, &r, &[]),
                None => send_json(req, 404, &json!({"error": "not found"}), &[]),
            },
            ("POST", "stop") => match sup.parallel.stop(sup, id) {
                Ok(msg) => send_json(req, 200, &json!({"result": msg}), &[]),
                Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
            },
            _ => send_json(req, 404, &json!({"error": "not found"}), &[]),
        }
        return;
    }

    // ---------- tasks ----------
    if path == "/api/tasks" && method == "GET" {
        send_json(req, 200, &sup.tasks.list(200), &[]);
        return;
    }
    if path == "/api/tasks/queue" && method == "GET" {
        send_json(req, 200, &sup.tasks.queue_info(), &[]);
        return;
    }
    if path == "/api/tasks" && method == "POST" {
        handle_tasks_create(sup, req);
        return;
    }
    if let Some(rest) = path.strip_prefix("/api/tasks/") {
        handle_task_action(sup, req, method, rest);
        return;
    }

    // ---------- attachments: raw body, ?name=&cwd= ----------
    if path == "/api/upload" && method == "POST" {
        handle_upload(req, local);
        return;
    }

    // ---------- usage, memory, first-run diagnosis ----------
    if path == "/api/usage" {
        let days = qparam(req.url(), "days")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(7)
            .clamp(1, 90);
        let brains = sup.cfg.brains();
        send_json(
            req,
            200,
            &crate::insights::top_questions(&brains, days, 30),
            &[],
        );
        return;
    }
    if path == "/api/memory" {
        let cfg = sup.cfg.get();
        let mem = crate::memory::Memory::from_cfg(&cfg);
        send_json(
            req,
            200,
            &json!({"locks": mem.active_locks(), "recent": mem.recent(48.0, 80)}),
            &[],
        );
        return;
    }
    if path == "/api/savings" || path == "/api/savings.csv" {
        let days = qparam(req.url(), "days")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(14)
            .clamp(1, 120);
        let rep = crate::savings::report(&sup.cfg.brains(), days);
        if path.ends_with(".csv") {
            send_raw(
                req,
                200,
                crate::savings::csv(&rep).into_bytes(),
                "text/csv; charset=utf-8",
                &[],
            );
        } else {
            send_json(req, 200, &rep, &[]);
        }
        return;
    }
    if path == "/api/approvals" {
        let (_, wait_s) = crate::approvals::settings(&sup.cfg.get());
        send_json(req, 200, &crate::approvals::list(30, wait_s), &[]);
        return;
    }
    if let Some(id) = path.strip_prefix("/api/approvals/") {
        handle_approval_decide(sup, req, method, local, id);
        return;
    }
    if path == "/api/succession" {
        handle_succession(sup, req, method);
        return;
    }
    if path == "/api/doctor" {
        send_json(req, 200, &crate::insights::doctor(&sup.cfg.get()), &[]);
        return;
    }

    // ---------- extensions: MCP servers and skills per brain ----------
    if path == "/api/extensions" {
        handle_extensions(sup, req, method);
        return;
    }

    if path == "/api/learn" {
        let q = req
            .url()
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();
        let project = q
            .split('&')
            .find_map(|kv| kv.strip_prefix("project="))
            .map(|v| v.replace("%20", " "));
        send_json(
            req,
            200,
            &sup.learn_view(project.as_deref().filter(|p| !p.is_empty())),
            &[],
        );
        return;
    }

    if path == "/api/relay" {
        let q = req
            .url()
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();
        let key = q
            .split('&')
            .find_map(|kv| kv.strip_prefix("key="))
            .unwrap_or("")
            .to_string();
        match crate::relay::read(&key) {
            Some(md) => send_json(req, 200, &json!({"key": key, "markdown": md}), &[]),
            None => send_json(req, 404, &json!({"error": "not found"}), &[]),
        }
        return;
    }

    if path == "/api/timeline" {
        handle_timeline(sup, req);
        return;
    }

    if path == "/api/history.csv" {
        let csv = sup.ledger.csv();
        send_raw(
            req,
            200,
            csv.into_bytes(),
            "text/csv",
            &[(
                "Content-Disposition",
                "attachment; filename=metabrain-usage.csv".to_string(),
            )],
        );
        return;
    }

    if path == "/api/lan" {
        let port = sup.cfg.key("port").as_u64().unwrap_or(0);
        let pin = if local {
            json!(sup.cfg.key("lan").s("pin"))
        } else {
            Value::Null
        };
        send_json(
            req,
            200,
            &json!({
                "enabled": sup.cfg.key("lan").b("enabled", false),
                "listening": lan_listening,
                "host": if lan_listening { "0.0.0.0" } else { "127.0.0.1" },
                "tls": false,
                "url": format!("http://{}:{}/", lan_ip(), port),
                "pin": pin
            }),
            &[],
        );
        return;
    }

    if path == "/api/qr.svg" {
        let port = sup.cfg.key("port").as_u64().unwrap_or(0);
        match qr_svg(&format!("http://{}:{}/", lan_ip(), port)) {
            Ok(svg) => send_raw(req, 200, svg.into_bytes(), "image/svg+xml", &[]),
            Err(e) => send_json(req, 500, &json!({"error": e}), &[]),
        }
        return;
    }

    if path == "/api/hooks/install" && method == "POST" {
        handle_hooks_install(sup, req, local);
        return;
    }

    send_json(req, 404, &json!({"error": "not found"}), &[]);
}

fn handle_config(sup: &Arc<Supervisor>, mut req: Request, method: &str, local: bool) {
    if method == "POST" {
        if !local {
            send_json(req, 403, &json!({"error": "only from this computer"}), &[]);
            return;
        }
        let Some(body) = read_body(&mut req) else {
            send_json(req, 400, &json!({"error": "bad JSON"}), &[]);
            return;
        };
        let result = sup.act(&json!({"a": "config", "config": body}));
        send_json(req, 200, &json!({"result": result}), &[]);
        return;
    }
    let mut data = match sup.cfg.data.lock() {
        Ok(g) => g.clone(),
        Err(e) => e.into_inner().clone(),
    };
    if !local {
        if !data.get("lan").map(|v| v.is_object()).unwrap_or(false) {
            data["lan"] = json!({});
        }
        data["lan"]["pin"] = json!("******");
        redact_remote_secrets(&mut data);
    }
    send_json(
        req,
        200,
        &json!({"config": data, "templates": crate::config::templates()}),
        &[],
    );
}

fn handle_outcome(sup: &Arc<Supervisor>, mut req: Request) {
    let d = read_body(&mut req).unwrap_or_else(|| json!({}));
    let project = d
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or("general");
    let level = d
        .get("level")
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .clamp(1, 5) as u8;
    let cand = d.get("candidate").and_then(|v| v.as_str()).unwrap_or("");
    let ok = d.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
    let tokens = d.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    if cand.is_empty() {
        // No candidate: resolve the pending attempt (by id, or the latest of the project).
        let id = d.get("id").and_then(|v| v.as_str());
        let p = d.get("project").and_then(|v| v.as_str());
        let why = d
            .get("why")
            .and_then(|v| v.as_str())
            .unwrap_or("marcado en el panel");
        send_json(
            req,
            200,
            &crate::learn::outcome(&sup.router, p, id, ok, why),
            &[],
        );
        return;
    }
    send_json(
        req,
        200,
        &sup.router.record(project, level, cand, ok, tokens),
        &[],
    );
}

fn handle_tasks_create(sup: &Arc<Supervisor>, mut req: Request) {
    let d = read_body(&mut req).unwrap_or_else(|| json!({}));
    let prompt = d.s("prompt");
    let cwd = d.s("cwd");
    let attachments = attachment_paths(&d);
    let mut brain_id = d.s("brain");
    if brain_id == "parallel" || truthy_v(d.get("parallel")) {
        match sup.parallel.start(
            sup,
            &prompt,
            &cwd,
            &d.s("mode"),
            truthy_v(d.get("worktree")),
            &attachments,
        ) {
            Ok(m) => send_json(req, 200, &m, &[]),
            Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
        }
        return;
    }
    let mut model = d.s("model");
    let mut effort = d.s("effort");
    let mut routed = Value::Null;
    let mut exclude = d.arr("exclude");
    if brain_id.is_empty() || brain_id == "auto" {
        let project = std::path::Path::new(&cwd)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "general".into());
        // Images go to brains that can see them: text-only API brains (DeepSeek) step aside.
        if attachments.iter().any(|f| crate::docs::is_image(f)) {
            for b in sup.cfg.get().arr("brains") {
                if b.s("kind") == "openai" {
                    exclude.push(json!(b.s("id")));
                }
            }
        }
        let v = sup.route_task_in(&project, &prompt, &cwd, &d.s("mode"), &exclude);
        brain_id = v["pick"].s("brain");
        model = v["pick"].s("model");
        effort = v["pick"].s("effort");
        routed = v;
    }
    let Some(bcfg) = sup.cfg.brain(&brain_id) else {
        send_json(
            req,
            400,
            &json!({"error": "Ningún cerebro disponible para esta tarea."}),
            &[],
        );
        return;
    };
    if d.s("mode") == "build" && !crate::tasks::can_edit(&bcfg.s("kind")) {
        send_json(
            req,
            400,
            &json!({"error": "Construir requiere un agente con acceso a los archivos."}),
            &[],
        );
        return;
    }
    let api_brain = bcfg.s("kind") == "openai";
    let (files, notes) = crate::docs::prepare_attachments(&attachments);
    let full = crate::docs::attach_to_prompt(&prompt, &files, api_brain);
    let mut extra = json!({});
    if !routed.is_null() {
        extra = json!({
            "route_id": routed.s("id"),
            "route_level": routed["level"],
            "route_cand": routed["pick"].s("candidate"),
            "base_prompt": full.clone(),
            "escalations": 0,
            "route_excluded": exclude
        });
    }
    if !attachments.is_empty() {
        extra["user_message"] = json!({"text": prompt.trim(), "files": attachments});
    }
    match sup.tasks.start_with(
        &bcfg,
        &model,
        &effort,
        &full,
        &cwd,
        &d.s("mode"),
        truthy_v(d.get("worktree")),
        extra,
    ) {
        Ok(mut m) => {
            m["routed"] = routed;
            m["notes"] = json!(notes);
            sup.diary.add(
                "Tú",
                &format!(
                    "Tarea para {}: {}",
                    bcfg.s("name"),
                    prompt.chars().take(60).collect::<String>()
                ),
                "info",
            );
            send_json(req, 200, &m, &[]);
        }
        Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
    }
}

fn handle_task_action(sup: &Arc<Supervisor>, mut req: Request, method: &str, rest: &str) {
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    let id = id.to_string();
    match (method, action) {
        ("GET", "events") => {
            let start = qparam(req.url(), "from")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0u64);
            let reader = TaskEvents {
                tasks: sup.tasks.clone(),
                id,
                offset: start,
                buf: vec![],
                pos: 0,
                idle: std::time::Instant::now(),
                ended: None,
            };
            let headers = vec![
                Header::from_bytes(
                    &b"Content-Type"[..],
                    &b"text/event-stream; charset=utf-8"[..],
                )
                .ok(),
                Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).ok(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            let resp = Response::new(StatusCode(200), headers, reader, None, None);
            let _ = req.respond(resp);
        }
        ("GET", "diff") => send_raw(
            req,
            200,
            sup.tasks.diff(&id).into_bytes(),
            "text/plain; charset=utf-8",
            &[],
        ),
        ("GET", "") => match sup.tasks.get(&id) {
            Some(m) => send_json(req, 200, &m, &[]),
            None => send_json(req, 404, &json!({"error": "not found"}), &[]),
        },
        ("POST", "message") => {
            let d = read_body(&mut req).unwrap_or_else(|| json!({}));
            let target = d.s("brain");
            let model = d.s("model");
            let message = d.s("text");
            let files = attachment_paths(&d);
            let r = match sup.tasks.get(&id) {
                Some(m) if m.s("kind") == "parallel" => {
                    sup.parallel.continue_run(sup, &id, &message, &files)
                }
                Some(m)
                    if model.is_empty()
                        && (target == "auto"
                            || (target.is_empty() && !m.s("route_cand").is_empty())) =>
                {
                    let pick = sup.route_follow_up(
                        &m,
                        &message,
                        files.iter().any(|f| crate::docs::is_image(f)),
                    );
                    if pick.s("brain").is_empty() {
                        Err("Ningún cerebro disponible para este turno.".into())
                    } else {
                        sup.tasks.message_routed(&id, &message, &files, &pick)
                    }
                }
                _ if (target.is_empty() || target == "current") && model.is_empty() => {
                    sup.tasks.message(&id, &message, &files)
                }
                _ => {
                    let brain_arg = if target == "current" || target == "auto" {
                        ""
                    } else {
                        target.as_str()
                    };
                    sup.tasks
                        .message_with_model(&id, &message, &files, brain_arg, &model)
                }
            };
            match r {
                Ok(msg) => send_json(req, 200, &json!({"result": msg}), &[]),
                Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
            }
        }
        ("POST", "retry") => {
            let d = read_body(&mut req).unwrap_or_else(|| json!({}));
            match sup.tasks.retry(&id, &d.s("brain")) {
                Ok(msg) => send_json(req, 200, &json!({"result": msg}), &[]),
                Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
            }
        }
        ("POST", a @ ("stop" | "approve" | "discard")) => {
            let r = match a {
                "stop" => {
                    if sup
                        .tasks
                        .get(&id)
                        .is_some_and(|m| m.s("kind") == "parallel")
                    {
                        sup.parallel.stop(sup, &id)
                    } else {
                        sup.tasks.stop(&id)
                    }
                }
                "approve" => sup.tasks.approve(&id),
                _ => sup.tasks.discard(&id),
            };
            match r {
                Ok(msg) => send_json(req, 200, &json!({"result": msg}), &[]),
                Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
            }
        }
        _ => send_json(req, 404, &json!({"error": "not found"}), &[]),
    }
}

fn handle_upload(mut req: Request, local: bool) {
    if !local {
        send_json(req, 403, &json!({"error": "only from this computer"}), &[]);
        return;
    }
    const MAX: usize = 30 * 1024 * 1024;
    let name = safe_name(&qparam(req.url(), "name").unwrap_or_default());
    let cwd = qparam(req.url(), "cwd").unwrap_or_default();
    let n = req_header(&req, "Content-Length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    if n == 0 || n > MAX {
        send_json(
            req,
            413,
            &json!({"error": "El archivo debe pesar entre 1 byte y 30 MB."}),
            &[],
        );
        return;
    }
    let mut raw = Vec::with_capacity(n);
    if req
        .as_reader()
        .take(n as u64)
        .read_to_end(&mut raw)
        .is_err()
    {
        send_json(
            req,
            400,
            &json!({"error": "No se pudo leer el archivo."}),
            &[],
        );
        return;
    }
    let base = safe_upload_dir(&cwd);
    let _ = std::fs::create_dir_all(&base);
    let nonce = UPLOAD_NONCE.fetch_add(1, Ordering::Relaxed);
    let file = base.join(format!(
        "{}_{:x}_{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        nonce,
        name
    ));
    match std::fs::write(&file, &raw) {
        Ok(_) => send_json(
            req,
            200,
            &json!({
                "path": file.to_string_lossy(),
                "name": name,
                "size": raw.len(),
                "image": crate::docs::is_image(&name)
            }),
            &[],
        ),
        Err(e) => send_json(
            req,
            500,
            &json!({"error": format!("No se pudo guardar: {e}")}),
            &[],
        ),
    }
}

fn handle_approval_decide(
    sup: &Arc<Supervisor>,
    mut req: Request,
    method: &str,
    local: bool,
    id: &str,
) {
    if method != "POST" {
        send_json(req, 405, &json!({"error": "POST"}), &[]);
        return;
    }
    let d = read_body(&mut req).unwrap_or_else(|| json!({}));
    let allow = d.s("decision") == "allow";
    let by = if local { "panel" } else { "celular" };
    let ok = crate::approvals::decide(id, allow, by);
    if ok {
        let rec = crate::approvals::get(id).unwrap_or(Value::Null);
        sup.diary.add(
            "Tú",
            &format!(
                "{} {}: {} {}",
                if allow { "permitiste a" } else { "denegaste a" },
                rec.s("brain_name"),
                rec.s("tool"),
                rec.s("summary").chars().take(60).collect::<String>()
            ),
            "info",
        );
    }
    send_json(
        req,
        if ok { 200 } else { 409 },
        &json!({
            "ok": ok,
            "error": if ok { "" } else { "Ya se respondió en la terminal o expiró." }
        }),
        &[],
    );
}

fn handle_succession(sup: &Arc<Supervisor>, mut req: Request, method: &str) {
    if method == "POST" {
        let d = read_body(&mut req).unwrap_or_else(|| json!({}));
        let mut cur = sup.cfg.key("succession");
        if !cur.is_object() {
            cur = json!({});
        }
        for (k, lo, hi) in [
            ("threshold_pct", 50.0, 100.0),
            ("stall_minutes", 0.0, 120.0),
            ("handoff_tokens", 0.0, 2_000_000.0),
        ] {
            if let Some(v) = d.get(k).and_then(|v| v.as_f64()) {
                cur[k] = json!(v.clamp(lo, hi));
            }
        }
        sup.cfg.set_key("succession", cur);
        sup.cfg.save();
    }
    let c = crate::succession::Chain::from_cfg(&sup.cfg.get());
    send_json(
        req,
        200,
        &json!({"params": c.params(), "chain": sup.chain(now())}),
        &[],
    );
}

fn handle_extensions(sup: &Arc<Supervisor>, mut req: Request, method: &str) {
    if method == "GET" {
        let project = qparam(req.url(), "project").filter(|p| !p.trim().is_empty());
        if let Some(ref p) = project {
            if let Err(e) = crate::extensions::sanitize_project_dir(p) {
                send_json(req, 400, &json!({"error": e}), &[]);
                return;
            }
        }
        let out: Vec<Value> = sup
            .cfg
            .brains()
            .iter()
            .map(|b| {
                let mut v = crate::extensions::list(b, project.as_deref());
                v["brain"] = json!(b.s("id"));
                v["name"] = json!(b.s("name"));
                v["kind"] = json!(b.s("kind"));
                v
            })
            .collect();
        send_json(req, 200, &json!(out), &[]);
        return;
    }
    if method == "POST" {
        let d = read_body(&mut req).unwrap_or_else(|| json!({}));
        let Some(b) = sup.cfg.brain(&d.s("brain")) else {
            send_json(req, 400, &json!({"error": "Cerebro desconocido."}), &[]);
            return;
        };
        let project = d.os("project").filter(|p| !p.is_empty());
        let scope = {
            let s = d.s("scope");
            if s.is_empty() {
                "user".to_string()
            } else {
                s
            }
        };
        let r: Result<String, String> = match d.s("op").as_str() {
            "add_mcp" => {
                if let Some(text) = d.os("json") {
                    crate::extensions::parse_mcp_json(&text).and_then(|specs| {
                        let mut msgs = vec![];
                        for sp in specs {
                            msgs.push(crate::extensions::add_mcp(
                                &b,
                                &scope,
                                project.as_deref(),
                                &sp,
                            )?);
                        }
                        Ok(msgs.join(" "))
                    })
                } else {
                    crate::extensions::add_mcp(&b, &scope, project.as_deref(), &d.o("spec"))
                }
            }
            "remove_mcp" => {
                crate::extensions::remove_mcp(&b, &scope, project.as_deref(), &d.s("name"))
            }
            "add_skill" => crate::extensions::add_skill(
                &b,
                &scope,
                project.as_deref(),
                &d.s("name"),
                &d.s("content"),
            ),
            "remove_skill" => {
                crate::extensions::remove_skill(&b, &scope, project.as_deref(), &d.s("name"))
            }
            "copy" => match sup.cfg.brain(&d.s("to")) {
                Some(to) => crate::extensions::copy(
                    &b,
                    &to,
                    &d.s("kind"),
                    &d.s("name"),
                    &scope,
                    project.as_deref(),
                ),
                None => Err("Cerebro destino desconocido.".into()),
            },
            _ => Err("Operación desconocida.".into()),
        };
        match r {
            Ok(m) => {
                sup.diary.add("Tú", &m, "info");
                send_json(req, 200, &json!({"result": m}), &[]);
            }
            Err(e) => send_json(req, 400, &json!({"error": e}), &[]),
        }
        return;
    }
    send_json(req, 404, &json!({"error": "not found"}), &[]);
}

fn handle_timeline(sup: &Arc<Supervisor>, req: Request) {
    // Gantt data for one brain: /api/timeline?brain=<id>&window=<seconds, default 1800>
    let q = req
        .url()
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
    let mut bid = String::new();
    let mut window = 1800.0_f64;
    for kv in q.split('&') {
        match kv.split_once('=') {
            Some(("brain", v)) => bid = v.replace("%20", " "),
            Some(("window", v)) => {
                window = v
                    .parse::<f64>()
                    .unwrap_or(1800.0)
                    .clamp(60.0, 86400.0 * 7.0)
            }
            _ => {}
        }
    }
    match sup.brain(&bid) {
        Some(b) => {
            let sessions = b.my_sessions();
            send_json(
                req,
                200,
                &crate::activity::timeline(&sessions, crate::util::now(), window),
                &[],
            );
        }
        None => send_json(req, 404, &json!({"error": "unknown brain"}), &[]),
    }
}

fn handle_hooks_install(sup: &Arc<Supervisor>, mut req: Request, local: bool) {
    if !local {
        send_json(req, 403, &json!({"error": "only from this computer"}), &[]);
        return;
    }
    let body = read_body(&mut req).unwrap_or_else(|| json!({}));
    let dry = body.b("dry_run", false);
    let bin = if cfg!(windows) {
        "metabrain-hook.exe"
    } else {
        "metabrain-hook"
    };
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(bin)))
        .unwrap_or_else(|| std::path::PathBuf::from(bin));
    let cfg = sup.cfg.get();
    let lines = crate::hooks::install(&cfg, exe.to_string_lossy().as_ref(), dry);
    send_json(
        req,
        200,
        &json!({"ok": true, "dry_run": dry, "result": lines}),
        &[],
    );
}
