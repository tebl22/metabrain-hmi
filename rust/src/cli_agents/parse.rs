//! Typed NDJSON stream parsers for Codex, Gemini CLI, and Antigravity (`agy`).

use super::{Agent, Event};
use serde::de::IgnoredAny;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorField {
    Text(String),
    Obj { message: Option<String> },
    Other(#[allow(dead_code)] IgnoredAny),
}

impl ErrorField {
    fn message(&self) -> Option<&str> {
        match self {
            ErrorField::Text(s) => Some(s.as_str()),
            ErrorField::Obj { message } => message.as_deref(),
            ErrorField::Other(_) => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TextOrList {
    Text(String),
    List(Vec<Value>),
    Other(#[allow(dead_code)] IgnoredAny),
}

impl TextOrList {
    fn into_text(self) -> String {
        match self {
            TextOrList::Text(s) => s,
            TextOrList::List(a) => a
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            TextOrList::Other(_) => String::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SummaryItem {
    Text(String),
    Obj { text: Option<String> },
    Other(#[allow(dead_code)] IgnoredAny),
}

impl SummaryItem {
    fn as_str(&self) -> Option<&str> {
        match self {
            SummaryItem::Text(s) => Some(s.as_str()),
            SummaryItem::Obj { text } => text.as_deref(),
            SummaryItem::Other(_) => None,
        }
    }
}

// ---------------------------------------------------------------- Codex DTOs

#[derive(Deserialize, Default)]
struct CodexEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    item: Option<CodexItem>,
    #[serde(default)]
    usage: Option<CodexUsage>,
    #[serde(default)]
    error: Option<ErrorField>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, Default)]
struct CodexItem {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Value,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    summary: Option<Vec<SummaryItem>>,
    #[serde(default)]
    command: Option<TextOrList>,
    #[serde(default)]
    changes: Value,
    #[serde(default)]
    server: Value,
    #[serde(default)]
    tool: Value,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    query: Value,
    #[serde(default)]
    prompt: Value,
    #[serde(default)]
    items: Value,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    aggregated_output: Option<String>,
}

#[derive(Deserialize, Default)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
}

// ---------------------------------------------------------------- Gemini DTOs

#[derive(Deserialize, Default)]
struct GeminiEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tool_id: Value,
    #[serde(default)]
    parameters: Value,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<ErrorField>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    stats: Option<GeminiStats>,
}

#[derive(Deserialize, Default)]
struct GeminiStats {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cached: u64,
}

// ---------------------------------------------------------------- Antigravity DTOs

#[derive(Deserialize, Default)]
struct AntigravityEnvelope {
    #[serde(default)]
    event: String,
    #[serde(default)]
    step_update: Option<AntigravityStep>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Default)]
struct AntigravityStep {
    #[serde(default)]
    text_delta: Option<String>,
    #[serde(default)]
    thinking_delta: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    thought: Option<String>,
    #[serde(default)]
    step_type: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    step_index: Option<Value>,
    #[serde(default)]
    tool_info: Option<Value>,
    #[serde(default)]
    tool_summary: Option<String>,
    #[serde(default)]
    tool_result_is_error: Option<bool>,
    #[serde(default)]
    tool_result: Option<Value>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct AntigravityResult {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<AntigravityUsage>,
}

#[derive(Deserialize, Default)]
struct AntigravityUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    thinking_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
}

fn log_unknown_event(agent: Agent, kind: &str) {
    let kind = kind.trim();
    if kind.is_empty() {
        return;
    }
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let set = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let key = format!("{}:{kind}", agent.bin_name());
    if let Ok(mut g) = set.lock() {
        if g.len() < 128 && g.insert(key) {
            crate::util::log(&format!(
                "cli_agents: evento desconocido de {} ({kind})",
                agent.bin_name()
            ));
        }
    }
}

fn parse_codex(line: &str) -> Option<Event> {
    let ev: CodexEvent = serde_json::from_str(line).ok()?;
    match ev.kind.as_str() {
        "item.started" | "item.completed" => {
            let item = ev.item?;
            let completed = ev.kind == "item.completed";
            match item.kind.as_str() {
                "agent_message" if completed => {
                    let text = item.text.unwrap_or_default();
                    if text.is_empty() {
                        None
                    } else {
                        Some(Event::Text(text))
                    }
                }
                "reasoning" if completed => {
                    let text = match item.text.filter(|t| !t.is_empty()) {
                        Some(t) => t,
                        None => item
                            .summary
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(SummaryItem::as_str)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default(),
                    };
                    if text.trim().is_empty() {
                        None
                    } else {
                        Some(Event::Thinking(text))
                    }
                }
                "command_execution"
                | "file_change"
                | "mcp_tool_call"
                | "web_search"
                | "collab_agent_tool_call"
                | "todo_list" => {
                    let input = match item.kind.as_str() {
                        "command_execution" => {
                            json!({"command": item.command.map(TextOrList::into_text).unwrap_or_default()})
                        }
                        "file_change" => json!({"changes": item.changes}),
                        "mcp_tool_call" => {
                            json!({"server": item.server, "tool": item.tool, "arguments": item.arguments})
                        }
                        "web_search" => json!({"pattern": item.query}),
                        "collab_agent_tool_call" => {
                            json!({"tool": item.tool, "description": item.prompt})
                        }
                        _ => json!({"items": item.items}),
                    };
                    let mut tool = json!({"name": item.kind, "input": input, "tool_id": item.id});
                    if completed {
                        let failed = item.status.as_deref() == Some("failed")
                            || item.exit_code.is_some_and(|code| code != 0);
                        tool["error"] = json!(failed);
                        tool["text"] = json!(item
                            .aggregated_output
                            .as_deref()
                            .unwrap_or_default()
                            .chars()
                            .take(4000)
                            .collect::<String>());
                        Some(Event::ToolFinished(tool))
                    } else {
                        Some(Event::ToolStarted(tool))
                    }
                }
                "agent_message" | "reasoning" => None,
                other => {
                    log_unknown_event(Agent::Codex, &format!("{}:{other}", ev.kind));
                    None
                }
            }
        }
        "turn.completed" => {
            let u = ev.usage.unwrap_or_default();
            Some(Event::Usage {
                input: u.input_tokens,
                output: u.output_tokens,
                cached: u.cached_input_tokens,
            })
        }
        "turn.failed" => {
            let msg = ev
                .error
                .as_ref()
                .and_then(ErrorField::message)
                .unwrap_or("La tarea falló.")
                .to_string();
            Some(Event::Done {
                ok: false,
                error: Some(msg),
            })
        }
        "error" => {
            let msg = ev
                .message
                .unwrap_or_else(|| "Error del agente.".to_string());
            Some(Event::Done {
                ok: false,
                error: Some(msg),
            })
        }
        "thread.started" | "turn.started" | "response.created" | "item.updated" => None,
        other => {
            log_unknown_event(Agent::Codex, other);
            None
        }
    }
}

fn parse_gemini(line: &str) -> Option<Event> {
    let ev: GeminiEvent = serde_json::from_str(line).ok()?;
    match ev.kind.as_str() {
        "message" => {
            let role = ev.role.as_deref().unwrap_or("assistant");
            if role != "assistant" && role != "model" {
                return None;
            }
            let content = ev.content.unwrap_or_default();
            if content.is_empty() {
                None
            } else {
                Some(Event::Text(content))
            }
        }
        "tool_use" => {
            let name = ev
                .tool_name
                .or(ev.name)
                .unwrap_or_else(|| "herramienta".to_string());
            if ev.tool_id.is_null() {
                Some(Event::Tool(name))
            } else {
                Some(Event::ToolStarted(json!({
                    "name": name,
                    "tool_id": ev.tool_id,
                    "input": ev.parameters
                })))
            }
        }
        "tool_result" => {
            let text = ev
                .output
                .as_deref()
                .or_else(|| ev.error.as_ref().and_then(ErrorField::message))
                .unwrap_or_default();
            Some(Event::ToolFinished(json!({
                "name": ev.tool_name.as_deref().unwrap_or("Herramienta"),
                "tool_id": ev.tool_id,
                "error": ev.status.as_deref() == Some("error"),
                "text": text.chars().take(4000).collect::<String>()
            })))
        }
        "result" => {
            let status = ev.status.as_deref().unwrap_or("");
            let err = ev
                .error
                .as_ref()
                .and_then(ErrorField::message)
                .map(str::to_string);
            if status == "error" || status == "failed" || err.is_some() {
                let msg = err.unwrap_or_else(|| "La tarea falló.".to_string());
                return Some(Event::Done {
                    ok: false,
                    error: Some(msg),
                });
            }
            if let Some(stats) = ev.stats {
                let cached = stats
                    .cached_input_tokens
                    .max(stats.cached_tokens)
                    .max(stats.cached);
                return Some(Event::Usage {
                    input: stats.input_tokens,
                    output: stats.output_tokens,
                    cached,
                });
            }
            None
        }
        "error" => {
            let msg = ev
                .message
                .or_else(|| {
                    ev.error
                        .as_ref()
                        .and_then(ErrorField::message)
                        .map(String::from)
                })
                .unwrap_or_else(|| "Error del agente.".to_string());
            Some(Event::Done {
                ok: false,
                error: Some(msg),
            })
        }
        "init" => None,
        other => {
            log_unknown_event(Agent::Gemini, other);
            None
        }
    }
}

fn antigravity_tool_input(name: &str, p: &AntigravityStep) -> Value {
    let mut input = match p.tool_info.as_ref() {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(Value::String(s)) if !s.trim().is_empty() => {
            if matches!(name, "run_command" | "Bash" | "shell" | "command_execution") {
                json!({"command": s.trim()})
            } else {
                json!({"description": s.trim()})
            }
        }
        _ => json!({}),
    };
    if let Some(summary) = p
        .tool_summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(obj) = input.as_object_mut() {
            obj.entry("description".to_string())
                .or_insert_with(|| json!(summary));
        }
    }
    input
}

fn parse_antigravity(line: &str) -> Option<Event> {
    let env: AntigravityEnvelope = serde_json::from_str(line).ok()?;
    match env.event.as_str() {
        "step_update" => {
            // agy 1.5 nests the data under the event's own name; older builds used "payload" or flat fields.
            let p: AntigravityStep = if let Some(step) = env.step_update {
                step
            } else if let Some(payload) = env.payload {
                serde_json::from_value(payload).unwrap_or_default()
            } else {
                serde_json::from_value(Value::Object(env.rest)).unwrap_or_default()
            };
            if let Some(t) = p.text_delta.as_deref().filter(|t| !t.is_empty()) {
                return Some(Event::Text(t.to_string()));
            }
            if let Some(t) = p
                .thinking_delta
                .as_deref()
                .or(p.thinking.as_deref())
                .or(p.thought.as_deref())
                .filter(|t| !t.is_empty())
            {
                return Some(Event::Thinking(t.to_string()));
            }
            if p.step_type.as_deref() == Some("tool") {
                let name = p.tool_name.as_deref().unwrap_or("herramienta");
                let state = p.state.as_deref().unwrap_or("");
                let step_id = p
                    .step_index
                    .as_ref()
                    .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|u| u as i64)))
                    .map(|i| format!("agy_{i}"));
                let input = antigravity_tool_input(name, &p);
                if matches!(state, "DONE" | "ERROR" | "FAILED" | "CANCELLED") {
                    let is_err = p.tool_result_is_error.unwrap_or(false)
                        || matches!(state, "ERROR" | "FAILED" | "CANCELLED");
                    let raw_res = p
                        .tool_result
                        .as_ref()
                        .map(|r| match r {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        })
                        .filter(|s| !s.is_empty())
                        .or_else(|| p.error_message.clone().or_else(|| p.error.clone()))
                        .unwrap_or_default();
                    let mut tool = json!({
                        "name": name,
                        "input": input,
                        "error": is_err,
                        "text": raw_res.chars().take(4000).collect::<String>(),
                    });
                    if let Some(tid) = step_id {
                        tool["tool_id"] = json!(tid);
                    }
                    return Some(Event::ToolFinished(tool));
                }
                if matches!(state, "ACTIVE" | "RUNNING" | "PENDING" | "") {
                    let mut tool = json!({"name": name, "input": input});
                    if let Some(tid) = step_id {
                        tool["tool_id"] = json!(tid);
                    }
                    return Some(Event::ToolStarted(tool));
                }
            }
            None
        }
        "result" => {
            let src = env
                .result
                .filter(|v| v.is_object())
                .or(env.payload)
                .unwrap_or(Value::Object(env.rest));
            let p: AntigravityResult = serde_json::from_value(src).unwrap_or_default();
            if p.status.as_deref() != Some("SUCCESS") {
                let msg = p
                    .error
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "La tarea no terminó.".to_string());
                return Some(Event::Done {
                    ok: false,
                    error: Some(msg),
                });
            }
            let u = p.usage.unwrap_or_default();
            Some(Event::Usage {
                input: u.input_tokens,
                output: u.output_tokens + u.thinking_tokens,
                cached: u.cache_read_tokens,
            })
        }
        "init" | "ready" | "session_started" | "turn_started" | "ping" | "heartbeat" | "status" => {
            None
        }
        other => {
            log_unknown_event(Agent::Antigravity, other);
            None
        }
    }
}

pub type StreamParseResult = (String, Vec<Event>, Option<(bool, Option<String>)>);

/// Replay a multi-line NDJSON stream from an agent and return `(assembled_text, events, done)`.
pub fn parse_stream(agent: Agent, ndjson: &str) -> StreamParseResult {
    let mut text = String::new();
    let mut events = Vec::new();
    let mut done = None;
    let mut collect = |ev: Event| events.push(ev);
    for line in ndjson.lines() {
        absorb(agent, line, &mut collect, &mut text, &mut done);
    }
    (text, events, done)
}

/// Decode one stdout line into an `Event`, tolerating unknown/incomplete data.
pub fn parse_line(agent: Agent, line: &str) -> Option<Event> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match agent {
        Agent::Codex => parse_codex(line),
        Agent::Gemini => parse_gemini(line),
        Agent::Antigravity => parse_antigravity(line),
    }
}

/// Heuristics for "you hit the plan/usage limit".
pub fn is_limit_error(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("usage limit")
        || t.contains("rate limit")
        || t.contains("ratelimit")
        || t.contains("429")
        || t.contains("quota")
        || t.contains("too many requests")
}

pub(super) fn absorb(
    agent: Agent,
    line: &str,
    on_event: &mut dyn FnMut(Event),
    text: &mut String,
    done: &mut Option<(bool, Option<String>)>,
) {
    if let Some(ev) = parse_line(agent, line) {
        match &ev {
            Event::Text(t) => {
                if !t.is_empty() {
                    // Gemini streams deltas (pieces of one answer); Codex sends whole messages.
                    if agent == Agent::Codex && !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Event::Done { ok, error } => {
                *done = Some((*ok, error.clone()));
            }
            _ => {}
        }
        on_event(ev);
    }
}
