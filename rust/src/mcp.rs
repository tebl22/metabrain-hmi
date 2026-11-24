//! MCP server (stdio, JSON-RPC 2.0) that bridges MCP clients (Claude Desktop,
//! Claude Code) to an already running MetaBrain over its local HTTP API.
//!
//! Nothing here panics on external data: every network/JSON problem becomes a
//! tool result with `isError: true`. stdout is reserved for the protocol;
//! anything diagnostic goes to stderr.

use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::time::Duration;

use crate::util::J;

const DEFAULT_PORT: u16 = 8790;
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Exact text required when the HMI is not running.
const NO_SERVER: &str = "MetaBrain no esta abierto. Abrelo y vuelve a intentar.";
const API_TIMEOUT: Duration = Duration::from_secs(20);
const ASK_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------- HTTP client

/// Why a call to the local API failed.
#[derive(Debug)]
enum HttpError {
    /// Nothing is listening on the configured port.
    Offline,
    /// Connected but the exchange broke / the response was malformed.
    Io(String),
}

fn http_text(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), HttpError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .timeout_write(timeout)
        .build();
    let url = format!("http://127.0.0.1:{port}{path}");
    let req = agent
        .request(method, &url)
        .set("Accept", "application/json");
    let result = match body {
        Some(b) => req.set("Content-Type", "application/json").send_string(b),
        None => req.call(),
    };
    match result {
        Ok(resp) => {
            let status = resp.status();
            let mut buf = Vec::new();
            if let Err(e) = resp.into_reader().take(8_000_000).read_to_end(&mut buf) {
                return Err(HttpError::Io(e.to_string()));
            }
            Ok((status, String::from_utf8_lossy(&buf).to_string()))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut buf = Vec::new();
            if let Err(e) = resp.into_reader().take(8_000_000).read_to_end(&mut buf) {
                return Err(HttpError::Io(e.to_string()));
            }
            Ok((code, String::from_utf8_lossy(&buf).to_string()))
        }
        Err(ureq::Error::Transport(t)) => {
            if matches!(
                t.kind(),
                ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns
            ) {
                Err(HttpError::Offline)
            } else {
                Err(HttpError::Io(t.to_string()))
            }
        }
    }
}

/// GET/POST JSON against the local API; `Err` is already a user-facing message.
fn api_json(port: u16, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let text = body.map(|b| b.to_string());
    let (code, raw) = match http_text(port, method, path, text.as_deref(), API_TIMEOUT) {
        Ok(x) => x,
        Err(HttpError::Offline) => return Err(NO_SERVER.to_string()),
        Err(HttpError::Io(e)) => return Err(format!("Error hablando con MetaBrain: {e}")),
    };
    let v: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "raw": raw }))
    };
    if code >= 400 {
        let msg = v
            .get("error")
            .and_then(error_text)
            .unwrap_or_else(|| format!("MetaBrain respondio HTTP {code}: {}", truncate(&raw, 300)));
        return Err(msg);
    }
    Ok(v)
}

// ------------------------------------------------------------ config / entry

/// Same port resolution the server uses (`data/metabrain.json` -> `port`).
fn config_port() -> u16 {
    crate::util::read_json(&crate::util::data_dir().join("metabrain.json"))
        .and_then(|c| crate::config::valid_port(&c))
        .unwrap_or(DEFAULT_PORT)
}

/// Run the stdio MCP loop until stdin closes. Returns the process exit code.
pub fn run() -> i32 {
    let port = config_port();
    {
        let mut err = std::io::stderr();
        let _ = writeln!(err, "metabrain mcp: puente a http://127.0.0.1:{port}");
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(v) => handle_message(&v, port),
            Err(e) => Some(err_response(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            )),
        };
        if let Some(r) = resp {
            let text = match serde_json::to_string(&r) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if writeln!(stdout, "{text}").is_err() {
                break;
            }
            let _ = stdout.flush();
        }
    }
    0
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One JSON-RPC message in, one response out (`None` for notifications).
fn handle_message(v: &Value, port: u16) -> Option<Value> {
    let id = v.get("id").cloned();
    let method = match v.get("method").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => return id.map(|id| err_response(id, -32600, "Invalid Request")),
    };
    match method.as_str() {
        "initialize" => {
            let id = id?;
            let pv = v
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|p| p.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            Some(ok_response(
                id,
                json!({
                    "protocolVersion": pv,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "metabrain", "version": env!("CARGO_PKG_VERSION")}
                }),
            ))
        }
        "notifications/initialized" => None,
        "ping" => id.map(|id| ok_response(id, json!({}))),
        "tools/list" => id.map(|id| ok_response(id, json!({"tools": tools()}))),
        "tools/call" => {
            let id = id?;
            let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if name.is_empty() {
                return Some(err_response(
                    id,
                    -32602,
                    "Invalid params: falta el nombre de la herramienta",
                ));
            }
            Some(ok_response(id, call_tool(&name, &args, port)))
        }
        other => id.map(|id| err_response(id, -32601, &format!("Method not found: {other}"))),
    }
}

// ------------------------------------------------------------------ the tools

fn tools() -> Value {
    json!([
        {
            "name": "brains",
            "description": "Lista los cerebros de MetaBrain (Claude Code, DeepSeek, Gemini...) con su id, nombre, tipo, estado, % de cuota usado y tareas corriendo sobre su cupo.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "delegate",
            "description": "Manda una tarea a MetaBrain y devuelve su id. Si no indicas cerebro, MetaBrain elige el mejor automáticamente con su enrutador.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "La tarea o instrucción completa para el cerebro."},
                    "brain": {"type": "string", "description": "Id del cerebro a usar. Si se omite, se elige automáticamente."},
                    "cwd": {"type": "string", "description": "Carpeta del proyecto donde debe trabajar la tarea."},
                    "worktree": {"type": "boolean", "description": "Trabajar en una copia aislada del proyecto (worktree). Por defecto true."},
                    "level": {"type": "integer", "description": "Nivel de esfuerzo de 1 (rápido) a 5 (a fondo).", "minimum": 1, "maximum": 5},
                    "mode": {"type": "string", "description": "Modo de trabajo: build (construir) o plan (solo planear).", "enum": ["build", "plan"]}
                },
                "required": ["prompt"]
            }
        },
        {
            "name": "task_status",
            "description": "Estado de una tarea: en qué cerebro va, tokens gastados y los últimos 20 eventos resumidos.",
            "inputSchema": {
                "type": "object",
                "properties": {"task_id": {"type": "string", "description": "Id de la tarea."}},
                "required": ["task_id"]
            }
        },
        {
            "name": "task_result",
            "description": "Texto final de una tarea y su diff (si trabajó en un worktree), truncado a 20000 caracteres.",
            "inputSchema": {
                "type": "object",
                "properties": {"task_id": {"type": "string", "description": "Id de la tarea."}},
                "required": ["task_id"]
            }
        },
        {
            "name": "task_stop",
            "description": "Detiene una tarea en curso. Confirma antes con el usuario: la tarea no se puede reanudar.",
            "inputSchema": {
                "type": "object",
                "properties": {"task_id": {"type": "string", "description": "Id de la tarea."}},
                "required": ["task_id"]
            }
        },
        {
            "name": "task_approve",
            "description": "Aprueba los cambios de una tarea que espera revisión. Confirma con el usuario y muéstrale antes el diff: la aprobación no se puede deshacer.",
            "inputSchema": {
                "type": "object",
                "properties": {"task_id": {"type": "string", "description": "Id de la tarea."}},
                "required": ["task_id"]
            }
        },
        {
            "name": "task_discard",
            "description": "Descarta y borra los cambios de una tarea. Confirma explícitamente con el usuario antes de llamarla: la acción no se puede deshacer.",
            "inputSchema": {
                "type": "object",
                "properties": {"task_id": {"type": "string", "description": "Id de la tarea."}},
                "required": ["task_id"]
            }
        },
        {
            "name": "queue",
            "description": "Cupos y fila de espera de MetaBrain: qué tareas están corriendo y cuáles esperan turno.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "ask",
            "description": "Pregunta corta y síncrona a un cerebro de API (DeepSeek/Gemini). Espera como máximo 120 segundos y devuelve la respuesta.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "La pregunta, breve y concreta."},
                    "brain": {"type": "string", "description": "Id del cerebro de API a consultar. Si se omite, se usa el primero disponible."}
                },
                "required": ["prompt"]
            }
        }
    ])
}

fn tool_result(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": false})
}

fn tool_error(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": true})
}

fn call_tool(name: &str, args: &Value, port: u16) -> Value {
    match name {
        "brains" => tool_brains(port),
        "delegate" => tool_delegate(args, port),
        "task_status" => tool_task_status(args, port),
        "task_result" => tool_task_result(args, port),
        "task_stop" => tool_task_action(args, port, "stop"),
        "task_approve" => tool_task_action(args, port, "approve"),
        "task_discard" => tool_task_action(args, port, "discard"),
        "queue" => tool_queue(port),
        "ask" => tool_ask(args, port),
        _ => tool_error(format!("Herramienta desconocida: {name}")),
    }
}

fn tool_brains(port: u16) -> Value {
    let cfg = match api_json(port, "GET", "/api/config", None) {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let state = api_json(port, "GET", "/api/state", None).unwrap_or_else(|_| json!({}));
    let tasks = api_json(port, "GET", "/api/tasks", None).unwrap_or_else(|_| json!([]));
    let brains = cfg
        .get("config")
        .and_then(|c| c.get("brains"))
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    if brains.is_empty() {
        return tool_result("No hay cerebros configurados en MetaBrain.");
    }
    let list = task_array(&tasks);
    let mut out = String::from("Cerebros:\n");
    for b in &brains {
        let id = b.s("id");
        let name = {
            let n = b.s("name");
            if n.is_empty() {
                id.clone()
            } else {
                n
            }
        };
        let kind = b.s("kind");
        let st = brain_state(&state, &id);
        let estado = st
            .and_then(|s| pick_str(s, &["status", "state", "estado"]))
            .unwrap_or_else(|| "listo".into());
        let quota = st
            .and_then(pct)
            .map(|q| format!("{:.0}%", q))
            .unwrap_or_else(|| "?".into());
        let running = list
            .iter()
            .filter(|t| t.s("brain") == id && is_running(t))
            .count();
        let cupo = b
            .get("max_parallel")
            .and_then(|v| v.as_u64())
            .or_else(|| b.get("capacity").and_then(|v| v.as_u64()))
            .or_else(|| b.get("max_tasks").and_then(|v| v.as_u64()))
            .or_else(|| st.and_then(|s| s.get("cupo").and_then(|v| v.as_u64())))
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        out.push_str(&format!(
            "- {id} — {name} ({kind}) — estado: {estado} — cuota: {quota} — tareas: {running}/{cupo}\n"
        ));
    }
    tool_result(out)
}

fn tool_delegate(args: &Value, port: u16) -> Value {
    let Some(prompt) = arg_str(args, "prompt") else {
        return tool_error("Falta el campo 'prompt'.");
    };
    let cwd = arg_str(args, "cwd").unwrap_or_default();
    let project = if cwd.is_empty() {
        "general".to_string()
    } else {
        basename(&cwd)
    };
    let mut brain = arg_str(args, "brain").unwrap_or_default();
    let worktree = args
        .get("worktree")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let mode = arg_str(args, "mode").unwrap_or_default();
    let level = args
        .get("level")
        .and_then(|v| v.as_u64())
        .map(|l| l.clamp(1, 5));
    let mut model = String::new();
    let mut effort = String::new();

    if brain.is_empty() || brain.eq_ignore_ascii_case("auto") {
        let body = json!({"project": project, "task": prompt, "level": level});
        let routed = match api_json(port, "POST", "/api/route", Some(&body)) {
            Ok(v) => v,
            Err(e) => return tool_error(e),
        };
        let pick = routed.get("pick").cloned().unwrap_or_else(|| json!({}));
        brain = pick.s("brain");
        model = pick.s("model");
        effort = pick.s("effort");
        if brain.is_empty() {
            return tool_error("No hay ningún cerebro disponible para esta tarea.");
        }
    }

    let mut body = json!({"prompt": prompt, "brain": brain, "cwd": cwd, "worktree": worktree});
    if !mode.is_empty() {
        set(&mut body, "mode", json!(mode));
    }
    if !model.is_empty() {
        set(&mut body, "model", json!(model));
    }
    if !effort.is_empty() {
        set(&mut body, "effort", json!(effort));
    }

    match api_json(port, "POST", "/api/tasks", Some(&body)) {
        Ok(v) => {
            let id = pick_str(&v, &["id", "task_id"]).unwrap_or_else(|| "?".into());
            let b = pick_str(&v, &["brain"]).unwrap_or_else(|| brain.clone());
            let status = pick_str(&v, &["status", "state"]).unwrap_or_else(|| "queued".into());
            let position = pick_num(&v, &["position", "pos", "queue_pos"])
                .map(|n| format!("{}", n as i64))
                .unwrap_or_else(|| "0".into());
            tool_result(format!(
                "Tarea creada.\ntask_id: {id}\nbrain: {b}\nstatus: {status}\nposition: {position}"
            ))
        }
        Err(e) => tool_error(e),
    }
}

fn tool_task_status(args: &Value, port: u16) -> Value {
    let Some(id) = task_arg(args) else {
        return tool_error("Falta 'task_id'.");
    };
    let t = match api_json(port, "GET", &format!("/api/tasks/{id}/summary"), None)
        .or_else(|_| api_json(port, "GET", &format!("/api/tasks/{id}"), None))
    {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let estado = pick_str(&t, &["status", "state"]).unwrap_or_else(|| "desconocido".into());
    let brain = pick_str(&t, &["brain", "brain_id"]).unwrap_or_else(|| "?".into());
    let tokens = pick_num(&t, &["tokens", "total_tokens"])
        .map(|n| format!("{}", n as i64))
        .unwrap_or_else(|| "?".into());
    let mut out = format!("Tarea {id}\nEstado: {estado}\nCerebro: {brain}\nTokens: {tokens}\n");
    match t.get("events").and_then(|v| v.as_array()) {
        Some(evs) if !evs.is_empty() => {
            let start = evs.len().saturating_sub(20);
            out.push_str("\nÚltimos eventos:\n");
            for e in &evs[start..] {
                out.push_str(&format!("- {}\n", summarize_event(e)));
            }
        }
        _ => out.push_str("\n(No hay eventos en el registro de la tarea.)\n"),
    }
    tool_result(out)
}

fn tool_task_result(args: &Value, port: u16) -> Value {
    let Some(id) = task_arg(args) else {
        return tool_error("Falta 'task_id'.");
    };
    let t = match api_json(port, "GET", &format!("/api/tasks/{id}/summary"), None)
        .or_else(|_| api_json(port, "GET", &format!("/api/tasks/{id}"), None))
    {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let estado = pick_str(&t, &["status", "state"]).unwrap_or_else(|| "desconocido".into());
    let mut out = format!("Tarea {id} — estado: {estado}\n");
    match pick_str(&t, &["result", "output", "final", "text", "answer"]) {
        Some(s) if !s.trim().is_empty() => {
            out.push_str("\n==== RESULTADO ====\n");
            out.push_str(s.trim());
            out.push('\n');
        }
        _ => out.push_str("\n(La tarea todavía no tiene texto final.)\n"),
    }
    // The diff endpoint answers text/plain; ignore it when empty or failing.
    if let Ok((code, raw)) = http_text(
        port,
        "GET",
        &format!("/api/tasks/{id}/diff"),
        None,
        API_TIMEOUT,
    ) {
        if code < 400 && !raw.trim().is_empty() {
            out.push_str("\n==== DIFF ====\n");
            out.push_str(&truncate(&raw, 20000));
            out.push('\n');
        }
    }
    tool_result(out)
}

fn tool_task_action(args: &Value, port: u16, action: &str) -> Value {
    let Some(id) = task_arg(args) else {
        return tool_error("Falta 'task_id'.");
    };
    let path = format!("/api/tasks/{id}/{action}");
    match api_json(port, "POST", &path, None) {
        Ok(v) => {
            let msg = pick_str(&v, &["result", "message"])
                .or_else(|| v.get("result").map(|r| one_line(&r.to_string(), 200)))
                .unwrap_or_else(|| format!("{action} enviado."));
            tool_result(format!("Tarea {id}: {msg}"))
        }
        Err(e) => tool_error(e),
    }
}

fn tool_queue(port: u16) -> Value {
    let v = match api_json(port, "GET", "/api/tasks", None) {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let mut out = String::new();
    if let Ok(qi) = api_json(port, "GET", "/api/tasks/queue", None) {
        out.push_str("Cupos y fila:\n");
        out.push_str(&pretty(&qi));
        out.push('\n');
    }
    let tasks = task_array(&v);
    let running: Vec<&Value> = tasks.iter().filter(|t| is_running(t)).collect();
    let queued: Vec<&Value> = tasks.iter().filter(|t| is_queued(t)).collect();
    out.push_str(&format!(
        "\nCorriendo: {}\nEn fila: {}\n",
        running.len(),
        queued.len()
    ));
    if !running.is_empty() {
        out.push_str("\nAhora:\n");
        for t in &running {
            out.push_str(&format!(
                "- {} [{}] {}\n",
                task_id(t),
                t.s("brain"),
                one_line(&t.s("prompt"), 80)
            ));
        }
    }
    if !queued.is_empty() {
        out.push_str("\nEn espera:\n");
        for (i, t) in queued.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} [{}] {}\n",
                i + 1,
                task_id(t),
                t.s("brain"),
                one_line(&t.s("prompt"), 80)
            ));
        }
    }
    if out.trim().is_empty() {
        out = "Sin tareas en MetaBrain.".into();
    }
    tool_result(out)
}

fn tool_ask(args: &Value, port: u16) -> Value {
    let Some(prompt) = arg_str(args, "prompt") else {
        return tool_error("Falta el campo 'prompt'.");
    };
    let want = arg_str(args, "brain").unwrap_or_default();
    let cfg = match api_json(port, "GET", "/api/config", None) {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let brains = cfg
        .get("config")
        .and_then(|c| c.get("brains"))
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    let is_proxy_api = |b: &Value| {
        b.b("enabled", true) && matches!(b.s("kind").as_str(), "openai" | "ollama" | "anthropic")
    };
    let chosen = if want.is_empty() {
        brains.iter().find(|b| is_proxy_api(b)).cloned()
    } else {
        brains
            .iter()
            .find(|b| b.s("id").eq_ignore_ascii_case(&want))
            .cloned()
    };
    let Some(b) = chosen else {
        return tool_error(
            "No hay ningún cerebro de API (DeepSeek/OpenAI/Anthropic/Ollama) para responder. Configura uno o indica 'brain'.",
        );
    };
    let kind = b.s("kind");
    if !matches!(kind.as_str(), "openai" | "ollama" | "anthropic") {
        return tool_error(
            "Ese cerebro es un agente CLI y no admite preguntas síncronas por proxy. Usa 'delegate' o elige un cerebro de API.",
        );
    }
    let id = b.s("id");
    let model = {
        let m = b.s("model");
        if !m.is_empty() {
            m
        } else {
            let ladder = b.arr("ladder");
            let models = b.arr("models");
            ladder
                .first()
                .and_then(|v| v.as_str())
                .or_else(|| models.first().and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string()
        }
    };
    let mut body = json!({
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "max_tokens": 1024
    });
    if !model.is_empty() {
        set(&mut body, "model", json!(model));
    }
    let base_url = b.s("base_url");
    let base_trim = base_url.trim().trim_end_matches('/');
    let endpoint = if kind == "anthropic" {
        if base_trim.ends_with("/v1") {
            "/messages"
        } else {
            "/v1/messages"
        }
    } else if base_trim.ends_with("/v1") || base_trim.ends_with("/openai") {
        "/chat/completions"
    } else {
        "/v1/chat/completions"
    };
    let path = format!("/proxy/{}{}", url_escape(&id), endpoint);
    let (code, raw) = match http_text(port, "POST", &path, Some(&body.to_string()), ASK_TIMEOUT) {
        Ok(x) => x,
        Err(HttpError::Offline) => return tool_error(NO_SERVER),
        Err(HttpError::Io(e)) => return tool_error(format!("Error hablando con MetaBrain: {e}")),
    };
    let v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({ "raw": raw }));
    if code >= 400 {
        let msg = v
            .get("error")
            .and_then(error_text)
            .unwrap_or_else(|| format!("HTTP {code}"));
        return tool_error(format!("El cerebro {id} respondió con error: {msg}"));
    }
    match answer_text(&v) {
        Some(t) => tool_result(t),
        None => tool_error(format!(
            "El cerebro {id} no devolvió texto. Respuesta: {}",
            one_line(&v.to_string(), 500)
        )),
    }
}

// --------------------------------------------------------------- JSON helpers

/// Insert a key only when the value really is an object (never panics).
fn set(obj: &mut Value, key: &str, val: Value) {
    if let Some(m) = obj.as_object_mut() {
        m.insert(key.to_string(), val);
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn task_arg(args: &Value) -> Option<String> {
    let id = arg_str(args, "task_id").or_else(|| arg_str(args, "id"))?;
    crate::util::valid_id(&id).then_some(id)
}

fn pick_str(v: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(x) = v.get(*k) {
            if let Some(s) = x.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
            if let Some(n) = x.as_f64() {
                return Some(format!("{}", n as i64));
            }
        }
    }
    None
}

fn pick_num(v: &Value, keys: &[&str]) -> Option<f64> {
    for k in keys {
        if let Some(x) = v.get(*k).and_then(|x| x.as_f64()) {
            return Some(x);
        }
    }
    None
}

fn error_text(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    pick_str(v, &["message", "msg", "detail", "error"])
}

fn answer_text(v: &Value) -> Option<String> {
    let openai = |key: &str| -> Option<String> {
        v.get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get(key))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
    };
    if let Some(t) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        return Some(t.to_string());
    }
    if let Some(t) = openai("text") {
        return Some(t);
    }
    // Gemini native shape.
    if let Some(t) = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(|c| c.as_str())
    {
        return Some(t.to_string());
    }
    // Anthropic Messages shape: {"content": [{"type": "text", "text": "..."}]}
    if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
        let joined: String = arr
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    pick_str(v, &["content", "text", "output", "answer"])
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out
}

/// Character-bounded truncation with a visible warning.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n\n[aviso: contenido truncado a {max} caracteres]")
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn basename(p: &str) -> String {
    std::path::Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "general".into())
}

fn url_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn task_array(v: &Value) -> Vec<Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    v.get("tasks")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default()
}

fn task_id(t: &Value) -> String {
    pick_str(t, &["id", "task_id"]).unwrap_or_else(|| "?".into())
}

fn status_of(t: &Value) -> String {
    t.s("status").to_lowercase()
}

fn is_running(t: &Value) -> bool {
    matches!(
        status_of(t).as_str(),
        "running" | "starting" | "working" | "streaming"
    )
}

fn is_queued(t: &Value) -> bool {
    matches!(
        status_of(t).as_str(),
        "queued" | "pending" | "waiting" | "paused" | "new"
    )
}

/// Per-brain snapshot entry, supporting both array (`/api/state` `brains: [{id, ...}]`) and map shapes.
fn brain_state<'a>(state: &'a Value, id: &str) -> Option<&'a Value> {
    if let Some(arr) = state.get("brains").and_then(|b| b.as_array()) {
        if let Some(found) = arr.iter().find(|item| item.s("id") == id) {
            return Some(found);
        }
    }
    state
        .get("brains")
        .and_then(|b| b.get(id))
        .or_else(|| state.get(id))
        .or_else(|| state.get("state").and_then(|s| s.get(id)))
        .filter(|v| v.is_object())
}

/// Quota percentage, accepting a plain number, a `{used, limit}` object, or `/api/state`'s `[{group, pct}]` array.
fn pct(v: &Value) -> Option<f64> {
    for k in ["quota_pct", "used_pct", "pct", "usage_pct"] {
        if let Some(x) = v.get(k).and_then(|x| x.as_f64()) {
            return Some(x);
        }
    }
    let q = v.get("quota")?;
    if let Some(x) = q.as_f64() {
        return Some(x);
    }
    if let Some(arr) = q.as_array() {
        return arr
            .iter()
            .filter_map(|item| item.get("pct").and_then(|x| x.as_f64()))
            .filter(|x| x.is_finite())
            .reduce(f64::max);
    }
    let used = q.get("used").and_then(|x| x.as_f64());
    let limit = q
        .get("limit")
        .and_then(|x| x.as_f64())
        .or_else(|| q.get("max").and_then(|x| x.as_f64()));
    match (used, limit) {
        (Some(u), Some(l)) if l > 0.0 => Some(u / l * 100.0),
        _ => None,
    }
}

fn summarize_event(e: &Value) -> String {
    if let Some(s) = e.as_str() {
        return one_line(s, 200);
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = e.get("ts").or_else(|| e.get("t")).or_else(|| e.get("time")) {
        parts.push(short(t));
    }
    if let Some(k) = pick_str(e, &["kind", "type", "event"]) {
        parts.push(k);
    }
    if let Some(m) = pick_str(e, &["text", "msg", "message", "line", "content", "output"]) {
        parts.push(one_line(&m, 200));
    }
    if parts.is_empty() {
        one_line(&e.to_string(), 200)
    } else {
        parts.join(" · ")
    }
}

fn short(v: &Value) -> String {
    match v {
        Value::String(s) => one_line(s, 60),
        Value::Number(n) => n.to_string(),
        other => one_line(&other.to_string(), 60),
    }
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn free_port() -> u16 {
        // Bind and immediately drop: the OS hands us a port nobody listens on.
        std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .unwrap_or(1)
    }

    #[test]
    fn parses_initialize_and_echoes_protocol() {
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "1"}}
        });
        let resp = handle_message(&req, 1).expect("respuesta");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "metabrain");
        assert_eq!(
            resp["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(resp["result"]["capabilities"].get("tools").is_some());

        let req2 = json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {}});
        let resp2 = handle_message(&req2, 1).expect("respuesta");
        assert_eq!(resp2["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn notifications_get_no_response() {
        let n = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&n, 1).is_none());
        let ping = json!({"jsonrpc": "2.0", "id": 7, "method": "ping"});
        let resp = handle_message(&ping, 1).expect("respuesta");
        assert_eq!(resp["id"], 7);
        assert_eq!(resp["result"], json!({}));
    }

    #[test]
    fn tools_list_has_nine_tools() {
        let req = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"});
        let resp = handle_message(&req, 1).expect("respuesta");
        let tools = resp["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(tools.len(), 9);
        let names: Vec<String> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        for n in [
            "brains",
            "delegate",
            "task_status",
            "task_result",
            "task_stop",
            "task_approve",
            "task_discard",
            "queue",
            "ask",
        ] {
            assert!(names.contains(&n.to_string()), "falta la herramienta {n}");
        }
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let req = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "no_existe", "arguments": {}}
        });
        let resp = handle_message(&req, 1).expect("respuesta");
        assert_eq!(resp["result"]["isError"], true);
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let req = json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"});
        let resp = handle_message(&req, 1).expect("respuesta");
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn offline_port_returns_is_error() {
        let port = free_port();
        let req = json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {"name": "brains", "arguments": {}}
        });
        let resp = handle_message(&req, port).expect("respuesta");
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("MetaBrain no esta abierto"), "{text}");
    }

    #[test]
    fn http_text_handles_ok_not_found_and_chunked() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let req = String::from_utf8_lossy(&buf).to_string();
                let response = if req.starts_with("GET /ok") {
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\nConnection: close\r\n\r\n{\"a\":1}"
                } else if req.starts_with("GET /missing") {
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"error\":\"no\"}"
                } else {
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n{\"a\":\r\n2\r\n1}\r\n0\r\n\r\n"
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        let (code, text) =
            http_text(port, "GET", "/ok", None, Duration::from_secs(5)).expect("200");
        assert_eq!(code, 200);
        assert_eq!(text, "{\"a\":1}");

        let (code, text) =
            http_text(port, "GET", "/missing", None, Duration::from_secs(5)).expect("404");
        assert_eq!(code, 404);
        assert!(text.contains("error"));

        let (code, text) =
            http_text(port, "GET", "/chunked", None, Duration::from_secs(5)).expect("chunked");
        assert_eq!(code, 200);
        assert_eq!(text, "{\"a\":1}");

        let _ = handle.join();
    }

    #[test]
    fn helpers_do_not_panic_on_odd_input() {
        assert_eq!(one_line("a\nb", 10), "a b");
        assert!(truncate("abcdef", 3).contains("truncado"));
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(basename(""), "general");
        assert_eq!(url_escape("a b"), "a%20b");
        assert_eq!(summarize_event(&json!("hola")), "hola");
        assert!(summarize_event(&json!({"kind": "x"})).contains('x'));
    }
}

// ---------------------------------------------------------- Claude Desktop
//
// Pega este bloque dentro de "mcpServers" en claude_desktop_config.json
// (sustituye <ruta del exe> por la ruta real de MetaBrain.exe; si prefieres
// una ventana de consola, usa metabrain-hook.exe):
//
//   "metabrain": {"command": "<ruta del exe>", "args": ["mcp"]}
//
