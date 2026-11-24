use super::quota::{pick_codex_turn, read_codex_quota_at};
use super::*;

#[test]
fn antigravity_nested_events() {
    let t = r#"{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"listo"}}"#;
    assert_eq!(
        parse_line(Agent::Antigravity, t),
        Some(Event::Text("listo".into()))
    );
    let th = r#"{"event":"step_update","step_update":{"step_index":2,"state":"ACTIVE","step_type":"thinking","thinking_delta":"analizando"}}"#;
    assert_eq!(
        parse_line(Agent::Antigravity, th),
        Some(Event::Thinking("analizando".into()))
    );
    let start = r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":"cargo test"}}"#;
    assert!(matches!(
        parse_line(Agent::Antigravity, start),
        Some(Event::ToolStarted(v)) if v["tool_id"] == "agy_3" && v["name"] == "run_command" && v["input"]["command"] == "cargo test"
    ));
    let done = r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":"cargo test","tool_result":"ok"}}"#;
    assert!(matches!(
        parse_line(Agent::Antigravity, done),
        Some(Event::ToolFinished(v)) if v["tool_id"] == "agy_3" && v["error"] == false && v["text"] == "ok"
    ));
    let r = r#"{"event":"result","result":{"status":"SUCCESS","response":"listo\n","usage":{"input_tokens":9549,"output_tokens":226,"thinking_tokens":223,"cache_read_tokens":0}}}"#;
    assert_eq!(
        parse_line(Agent::Antigravity, r),
        Some(Event::Usage {
            input: 9549,
            output: 449,
            cached: 0
        })
    );
}

#[test]
fn codex_parse_line() {
    let ev = parse_line(
        Agent::Codex,
        r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Hola mundo"}}"#,
    );
    assert_eq!(ev, Some(Event::Text("Hola mundo".to_string())));

    let ev = parse_line(
        Agent::Codex,
        r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls -la","exit_code":0}}"#,
    );
    assert!(
        matches!(ev, Some(Event::ToolFinished(tool)) if tool["input"]["command"] == "ls -la" && tool["error"] == false)
    );

    let ev = parse_line(
        Agent::Codex,
        r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":20}}"#,
    );
    assert_eq!(
        ev,
        Some(Event::Usage {
            input: 100,
            output: 20,
            cached: 40
        })
    );

    let ev = parse_line(
        Agent::Codex,
        r#"{"type":"turn.failed","error":{"message":"boom"}}"#,
    );
    assert_eq!(
        ev,
        Some(Event::Done {
            ok: false,
            error: Some("boom".to_string())
        })
    );

    let ev = parse_line(Agent::Codex, r#"{"type":"error","message":"algo mal"}"#);
    assert_eq!(
        ev,
        Some(Event::Done {
            ok: false,
            error: Some("algo mal".to_string())
        })
    );

    assert_eq!(parse_line(Agent::Codex, "no json"), None);
    assert_eq!(parse_line(Agent::Codex, ""), None);
}

#[test]
fn gemini_parse_line() {
    let ev = parse_line(
        Agent::Gemini,
        r#"{"type":"message","role":"assistant","content":"Hola"}"#,
    );
    assert_eq!(ev, Some(Event::Text("Hola".to_string())));

    let ev = parse_line(
        Agent::Gemini,
        r#"{"type":"tool_use","tool_name":"read_file"}"#,
    );
    assert_eq!(ev, Some(Event::Tool("read_file".to_string())));

    let ev = parse_line(
        Agent::Gemini,
        r#"{"type":"result","status":"success","stats":{"input_tokens":10,"output_tokens":5,"cached_input_tokens":3}}"#,
    );
    assert_eq!(
        ev,
        Some(Event::Usage {
            input: 10,
            output: 5,
            cached: 3
        })
    );

    let ev = parse_line(Agent::Gemini, r#"{"type":"error","message":"rate limit"}"#);
    match ev {
        Some(Event::Done {
            ok: false,
            error: Some(m),
        }) => assert!(m.contains("rate limit")),
        _ => panic!("esperaba Done con error"),
    }

    assert_eq!(parse_line(Agent::Gemini, "nope"), None);
}

#[test]
fn build_cmd_args_and_explicit_env() {
    let c = argv(Agent::Codex, "C:\\proj", "gpt-5.5", "write");
    assert_eq!(c[0], "exec");
    assert!(c.contains(&"--json".to_string()));
    assert!(c.contains(&"--skip-git-repo-check".to_string()));
    let pos = c.iter().position(|a| a == "-C").unwrap();
    assert_eq!(c[pos + 1], "C:\\proj");
    let mp = c.iter().position(|a| a == "-m").unwrap();
    assert_eq!(c[mp + 1], "gpt-5.5");
    let sp = c.iter().position(|a| a == "--sandbox").unwrap();
    assert!(matches!(
        c[sp + 1].as_str(),
        "workspace-write" | "danger-full-access"
    ));
    assert_eq!(c.last().map(|s| s.as_str()), Some("-"));

    let c2 = argv(Agent::Codex, "/tmp", "", "read");
    assert!(!c2.contains(&"-m".to_string()));
    let sp = c2.iter().position(|a| a == "--sandbox").unwrap();
    assert_eq!(c2[sp + 1], "read-only");

    let g = argv(Agent::Gemini, "/tmp", "gemini-3-pro", "write");
    assert!(g.contains(&"--output-format".to_string()));
    assert!(g.contains(&"stream-json".to_string()));
    let ap = g.iter().position(|a| a == "--approval-mode").unwrap();
    assert_eq!(g[ap + 1], "auto_edit");
    let pp = g.iter().position(|a| a == "-p").unwrap();
    assert_eq!(g[pp + 1], "");

    let g2 = argv(Agent::Gemini, "/tmp", "", "read");
    let ap = g2.iter().position(|a| a == "--approval-mode").unwrap();
    assert_eq!(g2[ap + 1], "plan");
    assert!(!g2.contains(&"-m".to_string()));

    // Safe defaults never silently disable sandboxes or skip permissions.
    let c_safe = argv(Agent::Codex, "/tmp", "gpt-5.5", "write");
    let sp = c_safe.iter().position(|a| a == "--sandbox").unwrap();
    assert_eq!(c_safe[sp + 1], "workspace-write");
    let agy_safe = argv(Agent::Antigravity, "/tmp", "gemini-4-neon", "write");
    assert!(!agy_safe.contains(&"--dangerously-skip-permissions".to_string()));
    assert!(sanitize_model("--inject").is_err());
    assert!(sanitize_model("gpt&calc").is_err());
    assert!(sanitize_cwd("C:\\proj&calc", true).is_err());

    // build_cmd must carry args and explicit per-run env without any thread-local state.
    let env = vec![("CODEX_HOME".to_string(), "/custom/codex".to_string())];
    let cmd = build_cmd(Agent::Codex, "/tmp", "", "low", "read", &env);
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert!(args.contains(&"--json".to_string()));
    assert!(args.contains(&"--skip-git-repo-check".to_string()));
    assert!(args
        .windows(2)
        .any(|a| a == ["-c", "model_reasoning_effort=low"]));
    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().to_string(),
                v.map(|x| x.to_string_lossy().to_string()),
            )
        })
        .collect();
    assert!(envs.contains(&("CODEX_HOME".to_string(), Some("/custom/codex".to_string()))));
}

#[test]
fn logged_in_with_env_respects_explicit_codex_home() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("mb_codex_env_{}_{}", std::process::id(), nanos));
    std::fs::create_dir_all(&root).unwrap();
    let env = vec![("CODEX_HOME".to_string(), root.display().to_string())];
    assert!(!logged_in_with_env(Agent::Codex, &env));
    std::fs::write(root.join("auth.json"), "{}").unwrap();
    assert!(logged_in_with_env(Agent::Codex, &env));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn codex_turn_uses_available_models_and_supported_effort() {
    let catalog = serde_json::json!({"models": [
        {"slug": "gpt-6.2-sol", "visibility": "list", "default_reasoning_level": "low",
         "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}]},
        {"slug": "gpt-6.2-astra", "visibility": "list", "default_reasoning_level": "low",
         "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}]}
    ]});
    let pick = |task, model, effort| pick_codex_turn(task, model, effort, &catalog);
    assert_eq!(pick("Hola", "", ""), ("gpt-6.2-sol".into(), "low".into()));
    assert_eq!(
        pick("Ayúdame a arreglar este error", "", ""),
        ("gpt-6.2-sol".into(), "medium".into())
    );
    assert_eq!(
        pick("Audita la arquitectura", "auto", "auto"),
        ("gpt-6.2-astra".into(), "medium".into())
    );
    assert_eq!(
        pick("Revisión final para producción", "", ""),
        ("gpt-6.2-astra".into(), "high".into())
    );
    assert_eq!(
        pick("Hola", "gpt-6.2-astra", "high"),
        ("gpt-6.2-astra".into(), "high".into())
    );
    assert_eq!(
        pick("Hola", "gpt-6.2-sol", "high"),
        ("gpt-6.2-sol".into(), "low".into())
    );
    assert_eq!(
        pick_codex_turn("Hola", "", "", &Value::Null),
        ("".into(), "low".into())
    );
}

#[test]
fn detects_usage_limit() {
    assert!(is_limit_error("Error: usage limit reached"));
    assert!(is_limit_error("429 Too Many Requests"));
    assert!(is_limit_error("You have exceeded your quota"));
    assert!(is_limit_error("rate limit exceeded"));
    assert!(!is_limit_error("todo bien"));
}

#[test]
fn from_kind_maps() {
    assert_eq!(Agent::from_kind("codex"), Some(Agent::Codex));
    assert_eq!(Agent::from_kind("gemini_cli"), Some(Agent::Gemini));
    assert_eq!(Agent::from_kind("claude_code"), None);
}

#[test]
fn parse_cli_version_extracts_semver() {
    assert_eq!(
        parse_cli_version("codex-cli 0.171.0\n"),
        Some("0.171.0".into())
    );
    assert_eq!(parse_cli_version("0.70.0"), Some("0.70.0".into()));
    assert_eq!(
        parse_cli_version("agy v1.5.16-beta.1"),
        Some("1.5.16-beta.1".into())
    );
    assert_eq!(parse_cli_version("sin version"), None);
}

#[test]
fn codex_quota_skips_null_premium_snapshots_and_walks_older_days() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("mb_codex_q_{}_{}", std::process::id(), nanos));
    let prev_day = root.join("sessions").join("2026").join("11").join("22");
    let cur_day = root.join("sessions").join("2026").join("11").join("23");
    std::fs::create_dir_all(&prev_day).unwrap();
    std::fs::create_dir_all(&cur_day).unwrap();

    // Older day has valid codex limits (session 64%, weekly 31%).
    std::fs::write(
        prev_day.join("rollout-2026-11-22T23-00-00.jsonl"),
        r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":64.0,"resets_at":2000000000},"secondary":{"used_percent":31.0,"resets_at":2000500000}}}}"#,
    )
    .unwrap();
    // Newest day only has a "premium" snapshot with null primary/secondary (the bug!).
    std::fs::write(
        cur_day.join("rollout-2026-11-23T22-49-39.jsonl"),
        r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"premium","primary":null,"secondary":null}}}"#,
    )
    .unwrap();

    let limits = read_codex_quota_at(&root, 1_900_000_000.0).expect("must find older valid limits");
    assert_eq!(limits.len(), 2);
    assert_eq!(limits[0].group, "session");
    assert_eq!(limits[0].pct, Some(64.0));
    assert_eq!(limits[1].group, "weekly");
    assert_eq!(limits[1].pct, Some(31.0));

    // When session reset timestamp is in the past, session resets to 0% while weekly remains 31%.
    let after_reset = read_codex_quota_at(&root, 2_000_000_100.0).expect("after session reset");
    assert_eq!(after_reset[0].pct, Some(0.0));
    assert_eq!(after_reset[1].pct, Some(31.0));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn codex_quota_picks_latest_event_timestamp_across_parallel_rollouts() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("mb_codex_par_{}_{}", std::process::id(), nanos));
    let day = root.join("sessions").join("2026").join("11").join("23");
    std::fs::create_dir_all(&day).unwrap();

    // Session A started earlier (20-25-50) but finished later (01:34:24Z) at 100%.
    std::fs::write(
        day.join("rollout-2026-11-23T20-25-50.jsonl"),
        r#"{"timestamp":"2026-11-24T01:34:24.891Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":100.0,"resets_at":2000000000},"secondary":{"used_percent":31.0,"resets_at":2000500000}}}}"#,
    )
    .unwrap();
    // Session B started later (20-31-04) so its filename sorts newer, but ended earlier (01:32:53Z) at 96%.
    std::fs::write(
        day.join("rollout-2026-11-23T20-31-04.jsonl"),
        r#"{"timestamp":"2026-11-24T01:32:53.825Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":96.0,"resets_at":2000000000},"secondary":{"used_percent":31.0,"resets_at":2000500000}}}}"#,
    )
    .unwrap();

    let limits =
        read_codex_quota_at(&root, 1_900_000_000.0).expect("must pick newest event timestamp");
    assert_eq!(limits[0].group, "session");
    assert_eq!(limits[0].pct, Some(100.0));
    assert_eq!(limits[1].group, "weekly");
    assert_eq!(limits[1].pct, Some(31.0));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn antigravity_usage_converts_remaining_to_used_and_selects_pool() {
    let raw = "\
Gemini Models\tWeekly Limit Remaining\t73%\t2026-11-30T23:47:52Z
Gemini Models\tFive Hour Limit Remaining\t95%\t2026-11-24T09:47:52Z
Claude and GPT models\tWeekly Limit Remaining\t40%\t2026-12-01T04:50:51Z
Claude and GPT models\tFive Hour Limit Remaining\t80%\t2026-11-24T09:50:51Z
";
    // 2026-11-24T00:00:00Z = 1795478400
    let now_ts = 1_795_478_400.0;
    let gemini = parse_antigravity_usage(raw, "gemini-4-neon-high", now_ts);
    assert_eq!(gemini.len(), 2);
    assert_eq!(gemini[0].group, "session");
    assert_eq!(gemini[0].pct, Some(5.0)); // 100 - 95
    assert!(gemini[0].reset.is_some());
    assert_eq!(gemini[1].group, "weekly");
    assert_eq!(gemini[1].pct, Some(27.0)); // 100 - 73

    let claude_pool = parse_antigravity_usage(raw, "claude-opus-5-5-high", now_ts);
    assert_eq!(claude_pool[0].group, "session");
    assert_eq!(claude_pool[0].pct, Some(20.0)); // 100 - 80
    assert_eq!(claude_pool[1].group, "weekly");
    assert_eq!(claude_pool[1].pct, Some(60.0)); // 100 - 40
}

#[test]
fn antigravity_usage_tolerates_ansi_and_spaces_and_disk_cache() {
    let raw =
        "\x1b[32mGemini Models\x1b[0m  Five Hour Limit Remaining   70%   2026-11-24T09:47:52Z\n\
               Gemini Models  Weekly Limit Remaining      68%   2026-11-30T23:47:52Z\n";
    let now_ts = 1_795_478_400.0;
    let limits = parse_antigravity_usage(raw, "gemini-4-neon-high", now_ts);
    assert_eq!(limits.len(), 2);
    assert_eq!(limits[0].group, "session");
    assert_eq!(limits[0].pct, Some(30.0));
    assert_eq!(limits[1].group, "weekly");
    assert_eq!(limits[1].pct, Some(32.0));

    let cache = std::env::temp_dir().join(format!(
        "metabrain-agy-cache-{:x}.json",
        rand::random::<u64>()
    ));
    super::quota::save_antigravity_cache_at(&cache, "gemini-4-neon-high", now_ts, &limits);
    let (ts, loaded) =
        super::quota::load_antigravity_cache_at(&cache, "gemini-4-neon-high", now_ts + 60.0)
            .expect("cached antigravity quota must load");
    assert_eq!(ts, now_ts);
    assert_eq!(loaded[0].pct, Some(30.0));
    assert_eq!(loaded[1].pct, Some(32.0));

    // After the 5 h reset passes, session resets to 0% automatically while weekly stays intact.
    let after_5h = loaded[0].reset.unwrap() + 10.0;
    let (_, after) =
        super::quota::load_antigravity_cache_at(&cache, "gemini-4-neon-high", after_5h)
            .expect("cache still valid before 7 days");
    assert_eq!(after[0].pct, Some(0.0));
    assert_eq!(after[1].pct, Some(32.0));

    // Older than 7 days is discarded.
    assert!(super::quota::load_antigravity_cache_at(
        &cache,
        "gemini-4-neon-high",
        now_ts + 8.0 * 86400.0
    )
    .is_none());

    let _ = std::fs::remove_file(&cache);
}
