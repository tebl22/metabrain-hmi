//! Contract tests against captured real NDJSON transcripts for external CLIs
//! (`codex`, `antigravity`/`agy`, and `gemini`).

use metabrain::cli_agents::{is_limit_error, parse_stream, Agent, Event};

#[test]
fn codex_0_171_contract() {
    let raw = include_str!("fixtures/codex_0.171.ndjson");
    let (text, events, done) = parse_stream(Agent::Codex, raw);

    assert_eq!(text, "Todas las pruebas pasaron correctamente.");
    assert_eq!(done, None);

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Thinking(t) if t.contains("Reviso"))));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolStarted(v) if v["name"] == "command_execution" && v["input"]["command"] == "cargo test --lib"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolFinished(v) if v["name"] == "command_execution" && v["error"] == false && v["text"].as_str().unwrap_or("").contains("212 passed")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolFinished(v) if v["name"] == "file_change" && v["error"] == false
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Usage {
            input: 12400,
            output: 340,
            cached: 8000
        }
    )));
}

#[test]
fn antigravity_1_5_contract() {
    let raw = include_str!("fixtures/antigravity_1.5.ndjson");
    let (text, events, done) = parse_stream(Agent::Antigravity, raw);

    assert_eq!(text, "Compilación exitosa.");
    assert_eq!(done, None);

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Thinking(t) if t == "Analizando el proyecto")));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolStarted(v) if v["tool_id"] == "agy_2" && v["name"] == "run_command" && v["input"]["command"] == "cargo check"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolFinished(v) if v["tool_id"] == "agy_2" && v["error"] == false && v["text"] == "Finished dev profile"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Usage {
            input: 9549,
            output: 449,
            cached: 4000
        }
    )));
}

#[test]
fn gemini_0_70_contract() {
    let raw = include_str!("fixtures/gemini_0.70.ndjson");
    let (text, events, done) = parse_stream(Agent::Gemini, raw);

    assert_eq!(text, "El paquete es metabrain.");
    assert_eq!(done, None);

    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolStarted(v) if v["tool_id"] == "tu_1" && v["name"] == "read_file"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::ToolFinished(v) if v["tool_id"] == "tu_1" && v["error"] == false
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Usage {
            input: 4200,
            output: 85,
            cached: 1500
        }
    )));
}

#[test]
fn error_and_rate_limit_streams_fail_loudly() {
    let (_, _, done_codex) = parse_stream(
        Agent::Codex,
        "{\"type\":\"turn.failed\",\"error\":{\"message\":\"429 Too Many Requests: usage limit reached\"}}\n",
    );
    assert!(matches!(done_codex, Some((false, Some(ref m))) if is_limit_error(m)));

    let (_, _, done_agy) = parse_stream(
        Agent::Antigravity,
        "{\"event\":\"result\",\"result\":{\"status\":\"ERROR\",\"error\":\"Quota exceeded\"}}\n",
    );
    assert!(matches!(done_agy, Some((false, Some(ref m))) if is_limit_error(m)));

    let (_, _, done_gem) = parse_stream(
        Agent::Gemini,
        "{\"type\":\"error\",\"message\":\"rate limit exceeded\"}\n",
    );
    assert!(matches!(done_gem, Some((false, Some(ref m))) if is_limit_error(m)));
}
