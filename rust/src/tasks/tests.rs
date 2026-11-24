//! Unit tests for `TaskManager` (scheduler, worktrees, retries, stream parsing, history compaction).

use super::helpers::{checked_git, follow_up_prompt, mk_event};
use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn setup(tag: &str) -> std::sync::MutexGuard<'static, ()> {
    let g = crate::util::test_env_lock();
    let dir = std::env::temp_dir().join(format!("mb_tasks_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
    std::env::set_var("METABRAIN_HOME", &dir);
    g
}

fn wait_status(tm: &TaskManager, id: &str, want: &str) -> Option<Value> {
    for _ in 0..120 {
        if let Some(m) = tm.get(id) {
            if m.s("status") == want {
                return Some(m);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn follow_up_keeps_full_recent_answer_summarizes_old_and_caps_budget() {
    let mut events = vec![
        json!({"type": "text", "text": "Respuesta inicial con Decisión: usar rust/src/tasks.rs\nPendiente: añadir pruebas"}),
    ];
    for i in 1..=6 {
        events.push(json!({"type": "text", "text": format!("Tú: Turno {i}")}));
        events.push(
            json!({"type": "text", "text": format!("Respuesta {i} {}", "detalle ".repeat(400))}),
        );
    }
    let latest = format!(
        "### Plan final\n{}\nÚltima línea clave.",
        "Línea importante del análisis.\n".repeat(120)
    );
    events.push(json!({"type": "text", "text": "Tú: Revisa todo el código"}));
    events.push(json!({"type": "text", "text": latest.clone()}));

    let prompt = follow_up_prompt("Pedido base", &events, "Aplica los cambios");
    assert!(prompt.contains("Resumen de turnos anteriores:"));
    assert!(prompt.contains("rust/src/tasks.rs"));
    assert!(
        prompt.contains("Última línea clave."),
        "last answer (< 6000 chars) should not be truncated at 2200"
    );
    assert!(
        prompt.chars().count() < 20_500,
        "prompt budget exceeded: {}",
        prompt.chars().count()
    );

    let huge_last = vec![
        json!({"type": "text", "text": "Tú: Dame el informe largo"}),
        json!({"type": "text", "text": "Palabra importante.\n".repeat(500)}),
    ];
    let trimmed = follow_up_prompt("Base", &huge_last, "Sigue");
    assert!(trimmed.contains("… [recortado]"));
    assert!(!trimmed.contains("Palabra impor…"));
}

#[test]
fn retry_keeps_the_failed_follow_up_and_attachment_once() {
    let _g = setup("retry_follow_up");
    write_json(
        &data_dir().join("metabrain.json"),
        &json!({"brains": [{"id": "fixture", "kind": "openai", "base_url": "http://127.0.0.1:1"}]}),
    )
    .unwrap();
    let tm = Arc::new(TaskManager::new());
    let id = "retry";
    tm.save(&json!({"id": id, "kind": "openai", "brain": "fixture", "status": "failed", "prompt": "Original"}));
    let message = "Tú: Segundo pedido\n\nArchivos adjuntos (ábrelos desde estas rutas):\n- captura con espacios.png";
    tm.emit(id, mk_event("text", json!({"text": message})));
    assert!(tm.retry(id, "").is_ok());
    let state = wait_status(&tm, id, "failed").unwrap();
    assert!(!state.s("turn_id").is_empty());
    let history = tm.history(id, "Original");
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1].s("content"),
        message.strip_prefix("Tú: ").unwrap()
    );
    // Missing brain config must fail synchronously instead of queueing a broken turn.
    tm.save(&json!({"id": "missing_brain", "kind": "openai", "brain": "deleted_brain", "status": "failed", "prompt": "Original"}));
    assert!(tm.retry("missing_brain", "").is_err());
}

#[test]
fn worktree_diff_and_discard_use_the_isolated_copy() {
    let _g = setup("real_worktree_diff");
    let root = data_dir().join("repo");
    fs::create_dir(&root).unwrap();
    checked_git(&root, &["init", "-q"]).unwrap();
    fs::write(root.join("tracked.txt"), "original\n").unwrap();
    checked_git(&root, &["add", "tracked.txt"]).unwrap();
    checked_git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "base",
        ],
    )
    .unwrap();
    let wt = root.join(".metabrain").join("wt").join("diff");
    let wt_path = wt.to_str().unwrap();
    checked_git(&root, &["worktree", "add", "-b", "metabrain/diff", wt_path]).unwrap();
    fs::write(wt.join("tracked.txt"), "index\n").unwrap();
    checked_git(&wt, &["add", "tracked.txt"]).unwrap();
    fs::write(wt.join("tracked.txt"), "working tree\n").unwrap();
    fs::write(wt.join("new file.txt"), "untracked\n").unwrap();
    fs::write(wt.join("empty.txt"), "").unwrap();
    let tm = TaskManager::new();
    tm.save(
        &json!({"id":"diff","cwd":root,"wt_path":wt,"branch":"metabrain/diff","status":"review"}),
    );
    let diff = tm.diff("diff");
    assert!(diff.contains("+working tree"), "{diff}");
    assert!(diff.contains("+untracked"), "{diff}");
    assert!(diff.contains("empty.txt"), "{diff}");
    checked_git(&root, &["worktree", "lock", wt_path]).unwrap();
    assert!(tm.discard("diff").is_err());
    assert_eq!(tm.get("diff").unwrap().s("status"), "review");
    checked_git(&root, &["worktree", "unlock", wt_path]).unwrap();
    assert!(tm.discard("diff").is_ok());
    assert!(!wt.exists());
    assert!(root.join("tracked.txt").is_file());
}

#[test]
fn requested_worktree_failure_never_starts_an_agent_or_falls_back_on_retry() {
    let _g = setup("required_worktree");
    let tm = Arc::new(TaskManager::new());
    let root = data_dir();
    let id = "requires-worktree";
    tm.save(&json!({
        "id": id, "cwd": root, "mode": "build", "kind": "claude_code",
        "status": "queued", "worktree": true, "turn_id": id, "prompt": "edit files",
    }));
    Arc::clone(&tm).run(
        id.into(),
        Value::Null,
        "claude_code".into(),
        "edit files".into(),
        root,
        String::new(),
        String::new(),
        "build".into(),
        true,
    );
    let state = tm.get(id).unwrap();
    assert_eq!(state.s("status"), "failed");
    assert!(state.s("error").contains("No se pudo crear el worktree"));
    assert!(tm
        .retry(id, "")
        .unwrap_err()
        .contains("worktree solicitado"));
}

#[test]
fn approval_requires_review_and_committed_worktree_changes() {
    let _g = setup("worktree_approval");
    let root = data_dir().join("repo");
    fs::create_dir(&root).unwrap();
    checked_git(&root, &["init", "-q"]).unwrap();
    let commit = |dir: &Path, message: &str| {
        checked_git(
            dir,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                message,
            ],
        )
        .unwrap();
    };
    fs::write(root.join("change.txt"), "original").unwrap();
    checked_git(&root, &["add", "change.txt"]).unwrap();
    commit(&root, "base");
    let wt = root.join("isolated");
    let branch = "metabrain/approval";
    checked_git(
        &root,
        &["worktree", "add", "-b", branch, wt.to_str().unwrap()],
    )
    .unwrap();

    let tm = TaskManager::new();
    let mut task = json!({"id": "approval", "cwd": root, "wt_path": wt, "branch": branch, "status": "running"});
    tm.save(&task);
    assert!(tm.approve("approval").is_err());
    task["status"] = json!("review");
    tm.save(&task);
    fs::write(wt.join("change.txt"), "approved edit").unwrap();
    assert!(tm.approve("approval").unwrap_err().contains("commit"));
    assert_eq!(tm.get("approval").unwrap().s("status"), "review");

    checked_git(&wt, &["add", "change.txt"]).unwrap();
    commit(&wt, "reviewed change");
    assert!(tm
        .approve("approval")
        .unwrap()
        .contains("git merge metabrain/approval"));
    assert_eq!(tm.get("approval").unwrap().s("status"), "done");
    assert_eq!(
        checked_git(&root, &["show", "metabrain/approval:change.txt"]).unwrap(),
        "approved edit"
    );
    assert_eq!(
        fs::read_to_string(root.join("change.txt")).unwrap(),
        "original"
    );
}

#[test]
fn stopped_api_response_cannot_replace_status() {
    let _g = setup("late_response");
    let cfg = json!({"id":"test","kind":"openai","base_url":"http://localhost","key_env":"MB_TEST_LOCAL_KEY"});
    std::env::set_var("MB_TEST_LOCAL_KEY", "local-fixture");
    let tm = TaskManager::new();
    let id = "late";
    tm.save(&json!({"id":id,"brain":"test","kind":"openai","status":"queued","cwd":""}));
    tm.api_turn(id, &cfg, "local", "low", "hello", |_, _, body| {
        assert_eq!(body["messages"][1]["content"], "hello");
        tm.stop(id).unwrap();
        Ok(json!({"choices":[{"message":{"content":"stale"}}]}))
    });
    std::env::remove_var("MB_TEST_LOCAL_KEY");
    tm.finish_interruption(id, "");
    assert_eq!(tm.get(id).unwrap().s("status"), "stopped");
    assert!(!tm
        .events_since(id, 0)
        .0
        .iter()
        .any(|e| e.s("text") == "stale"));
}

#[test]
fn build_requires_an_editor() {
    let _g = setup("build_capability");
    let tm = Arc::new(TaskManager::new());
    let dir = tm.root.to_string_lossy().to_string();
    let cfg = json!({"kind":"openai","id":"text"});
    assert!(tm
        .start(&cfg, "", "", "edit", &dir, "build", false)
        .is_err());
    assert!(can_edit("codex") && can_edit("claude_code"));
}

#[test]
fn api_prefix_survives_project_changes_and_restart() {
    let _g = setup("api_prefix");
    let tm = TaskManager::new();
    let project = tm.root.join("project");
    fs::create_dir(&project).unwrap();
    let readme = project.join("README.md");
    fs::write(&readme, "Contexto original").unwrap();
    let cwd = project.to_str().unwrap();
    let url = "https://api.deepseek.com/v1";
    let initial = tm
        .openai_body("api", cwd, "model", "high", url, "Inicio")
        .unwrap();
    assert!(initial["messages"][0]
        .s("content")
        .contains("Contexto original"));
    assert_eq!(initial["thinking"]["type"], "enabled");

    // Remove the shared brief so a fresh conversation sees the new README.
    let brief = fs::read_dir(data_dir().join("briefs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    fs::remove_file(brief.path()).unwrap();
    fs::write(readme, "Contexto actualizado").unwrap();
    tm.emit(
        "api",
        mk_event("text", json!({"text": "Primera respuesta"})),
    );
    tm.emit("api", mk_event("text", json!({"text": "Tú: Seguimiento"})));

    let tm = TaskManager::new();
    let next = tm
        .openai_body("api", cwd, "model", "high", url, "Inicio")
        .unwrap();
    assert_eq!(next["messages"][0], initial["messages"][0]);
    assert_eq!(next["messages"][3]["content"], "Seguimiento");
    let fresh = tm
        .openai_body("fresh", cwd, "model", "high", url, "Inicio")
        .unwrap();
    assert!(fresh["messages"][0]
        .s("content")
        .contains("Contexto actualizado"));
}

#[test]
fn api_body_compacts_old_turns_and_keeps_four_exchanges_literal() {
    let _g = setup("api_body");
    let tm = TaskManager::new();
    let id = "api";
    let initial = format!("Objetivo original\n{}ARCHIVO_ANTIGUO", "x".repeat(40_000));
    tm.emit(id, mk_event("text", json!({"text": "Respuesta inicial"})));
    let mut recent = Vec::new();
    for i in 1..=4 {
        let user = format!("Solicitud {i}\n{}", "y".repeat(2000));
        let assistant = format!("Respuesta {i}\n{}", "z".repeat(2000));
        tm.emit(id, mk_event("text", json!({"text": format!("Tú: {user}")})));
        tm.emit(id, mk_event("text", json!({"text": assistant})));
        recent.push(json!({"role": "user", "content": user}));
        recent.push(json!({"role": "assistant", "content": assistant}));
    }
    tm.emit(id, mk_event("text", json!({"text": "Tú: Pendiente"})));

    let body = tm
        .openai_body(id, "", "model", "", "http://localhost", &initial)
        .unwrap();
    let serialized = body.to_string();
    assert!(serialized.len() < 24_000);
    assert!(!serialized.contains("ARCHIVO_ANTIGUO"));
    let messages = body["messages"].as_array().unwrap();
    let summary = messages[1].s("content");
    assert!(summary.starts_with("Resumen de mensajes anteriores"));
    assert!(summary.contains("Objetivo original") && summary.contains("Respuesta inicial"));
    assert!(summary.chars().count() <= 1600);
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(&messages[3..11], recent.as_slice());
    assert_eq!(
        messages[11],
        json!({"role": "user", "content": "Pendiente"})
    );
    assert_eq!(messages.len(), 12);
}

#[test]
fn api_history_bounds_prior_context_and_preserves_the_pending_turn() {
    let _g = setup("api_history_limit");
    let tm = TaskManager::new();
    let large = "界".repeat(24_000);
    for i in 0..4 {
        tm.emit("api", mk_event("text", json!({"text": format!("Tú: {i}")})));
    }
    tm.emit(
        "pending",
        mk_event("text", json!({"text": format!("Tú: {large}")})),
    );
    assert_eq!(
        tm.history("pending", "Original").last().unwrap()["content"],
        large
    );
    assert_eq!(
        tm.history("api", &large)[0].s("content").chars().count(),
        23_997
    );
    tm.emit("api", mk_event("text", json!({"text": "Tú: 4"})));
    let exact = "界".repeat(23_996);
    assert_eq!(tm.history("api", &exact)[0]["content"], exact);
    let over = format!("{exact}界");
    assert!(tm.history("api", &over)[0]
        .s("content")
        .starts_with("Resumen"));
    for _ in 0..125 {
        tm.emit("burst", mk_event("text", json!({"text": "界".repeat(200)})));
    }
    tm.emit("burst", mk_event("text", json!({"text": "Tú: Pendiente"})));
    let messages = tm.history("burst", "Objetivo");
    assert_eq!(messages.last().unwrap()["content"], "Pendiente");
    let historical_chars: usize = messages[..messages.len() - 1]
        .iter()
        .map(|message| message.s("content").chars().count())
        .sum();
    assert!(historical_chars <= 24_000);
}

#[test]
fn parallel_answer_keeps_streamed_text_and_markdown_intact() {
    let _g = setup("parallel_answer");
    let tm = TaskManager::new();
    let id = "branch";
    tm.emit(id, mk_event("text", json!({"text": "Tú: pregunta"})));
    for token in ["### Hall", "azgo", "\n", "\n", "`cargo", " ", "test`"] {
        tm.emit(id, mk_event("text", json!({"text": token, "delta": true})));
    }
    assert_eq!(
        tm.extract_assistant_text(id),
        "### Hallazgo\n\n`cargo test`"
    );
}

#[test]
fn parses_stream_json_lines() {
    let (evs, sess, u) =
        events_from_line(r#"{"type":"system","subtype":"init","session_id":"abc123"}"#);
    assert!(evs.is_empty());
    assert_eq!(sess.as_deref(), Some("abc123"));
    assert!(u.is_none());

    let (evs, _, _) = events_from_line(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hola"},{"type":"thinking","thinking":"pensando"},{"type":"tool_use","name":"Agent","input":{"description":"explorar"}}]}}"#,
    );
    let kinds: Vec<String> = evs.iter().map(|e| e.s("type")).collect();
    assert_eq!(kinds, vec!["text", "thinking", "tool_use", "subagent"]);
    assert_eq!(evs[0].s("text"), "Hola");
    assert_eq!(evs[1].s("text"), "pensando");
    assert_eq!(evs[3].s("text"), "explorar");

    let (evs, _, _) = events_from_line(
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"detalle"}]}]}}"#,
    );
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].s("text"), "detalle");

    let (evs, _, u) = events_from_line(
        r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":2,"cache_creation_input_tokens":3},"total_cost_usd":0.02,"is_error":false,"result":"fin"}"#,
    );
    assert!(evs.is_empty());
    let (t, c, err) = u.unwrap_or((0, 0.0, false));
    assert_eq!(t, 20);
    assert!((c - 0.02).abs() < 1e-9);
    assert!(!err);

    let (evs, _, u) = events_from_line(
        r#"{"type":"result","usage":{"input_tokens":1},"total_cost_usd":0.0,"is_error":true,"result":"boom"}"#,
    );
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].s("type"), "error");
    assert_eq!(evs[0].s("text"), "boom");
    assert!(u.map(|x| x.2).unwrap_or(false));

    // garbage must not panic
    let (evs, sess, u) = events_from_line("no es json");
    assert!(evs.is_empty() && sess.is_none() && u.is_none());
}

#[test]
fn events_since_uses_byte_offset() {
    let _g = setup("evt");
    let tm = TaskManager::new();
    let id = "aaaaaaaaaa";
    tm.emit(id, mk_event("status", json!({ "status": "queued" })));
    tm.emit(id, mk_event("text", json!({ "text": "hola" })));

    let (evs, off) = tm.events_since(id, 0);
    assert_eq!(evs.len(), 2);
    assert!(off > 0);
    assert_eq!(evs[1].s("text"), "hola");

    let (evs2, off2) = tm.events_since(id, off);
    assert!(evs2.is_empty());
    assert_eq!(off2, off);

    tm.emit(id, mk_event("text", json!({ "text": "mas" })));
    let (evs3, off3) = tm.events_since(id, off);
    assert_eq!(evs3.len(), 1);
    assert_eq!(evs3[0].s("text"), "mas");
    assert!(off3 > off);
}

#[test]
fn list_sorts_by_updated_desc() {
    let _g = setup("list");
    let tm = TaskManager::new();
    for (id, upd) in [
        ("bbbbbbbbbb", 2.0),
        ("aaaaaaaaaa", 1.0),
        ("cccccccccc", 3.0),
    ] {
        tm.save(&json!({ "id": id, "updated": upd, "status": "done" }));
    }
    let all = tm.list(0);
    let ids: Vec<String> = all
        .as_array()
        .map(|a| a.iter().map(|v| v.s("id")).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec!["cccccccccc", "bbbbbbbbbb", "aaaaaaaaaa"]);
    assert_eq!(tm.list(2).as_array().map(|a| a.len()).unwrap_or(0), 2);
}

#[test]
fn openai_without_key_fails() {
    let _g = setup("nokey");
    let tm = Arc::new(TaskManager::new());
    let cfg = json!({
        "id": "ds",
        "name": "DeepSeek",
        "kind": "openai",
        "base_url": "https://api.deepseek.com",
        "key_env": "MB_TEST_NO_SUCH_KEY_ZZZ"
    });
    let cwd = std::env::temp_dir().to_string_lossy().to_string();
    let meta = tm
        .start(
            &cfg,
            "deepseek-chat",
            "auto",
            "hola mundo",
            &cwd,
            "plan",
            false,
        )
        .expect("start debe aceptar");
    let id = meta.s("id");
    assert_eq!(id.len(), 10);
    let fin = wait_status(&tm, &id, "failed").expect("la tarea debe fallar");
    assert_eq!(fin.s("error"), "falta la clave");
    assert!(!tm.running(&id));
}

#[test]
fn start_rejects_bad_input() {
    let _g = setup("bad");
    let tm = Arc::new(TaskManager::new());
    let cfg = json!({ "kind": "claude_code" });
    assert!(tm.start(&cfg, "", "", "   ", "", "build", false).is_err());
    let cwd = std::env::temp_dir()
        .join("mb_no_existe_zzz")
        .to_string_lossy()
        .to_string();
    assert!(tm
        .start(&cfg, "", "", "hola", &cwd, "build", false)
        .is_err());
}

#[test]
fn orphans_become_failed_on_new() {
    let _g = setup("orphan");
    let root = data_dir().join("tasks");
    let _ = fs::create_dir_all(&root);
    let meta = json!({ "id": "0123456789", "status": "running", "updated": 1.0 });
    let _ = fs::write(
        root.join("0123456789.json"),
        serde_json::to_string(&meta).unwrap_or_default(),
    );
    let tm = TaskManager::new();
    let m = tm.get("0123456789").expect("meta guardada");
    assert_eq!(m.s("status"), "failed");
    assert_eq!(m.s("error"), "MetaBrain se reinició durante la tarea");
}

#[test]
fn same_brain_cupo_one_serializes() {
    let _g = setup("cupo1");
    let tm = Arc::new(TaskManager::new());
    tm.set_global_limit(10);
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for i in 0..3 {
        let tm2 = Arc::clone(&tm);
        let active2 = Arc::clone(&active);
        let max2 = Arc::clone(&max_active);
        let id = format!("t{i}");
        handles.push(std::thread::spawn(move || {
            tm2.test_run("brainA", 1, "A", &id, || {
                let cur = active2.fetch_add(1, Ordering::SeqCst) + 1;
                max2.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                active2.fetch_sub(1, Ordering::SeqCst);
            });
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
}

#[test]
fn two_brains_run_in_parallel() {
    let _g = setup("two_brains");
    let tm = Arc::new(TaskManager::new());
    tm.set_global_limit(10);
    let started = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for (brain, id) in [("brainA", "a1"), ("brainB", "b1")] {
        let tm2 = Arc::clone(&tm);
        let started2 = Arc::clone(&started);
        let active2 = Arc::clone(&active);
        let max2 = Arc::clone(&max_active);
        let brain = brain.to_string();
        let id = id.to_string();
        handles.push(std::thread::spawn(move || {
            tm2.test_run(&brain, 1, &brain, &id, || {
                started2.fetch_add(1, Ordering::SeqCst);
                let t0 = Instant::now();
                while started2.load(Ordering::SeqCst) < 2
                    && t0.elapsed() < Duration::from_millis(500)
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let cur = active2.fetch_add(1, Ordering::SeqCst) + 1;
                max2.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                active2.fetch_sub(1, Ordering::SeqCst);
            });
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
}

#[test]
fn global_limit_one_serializes() {
    let _g = setup("global1");
    let tm = Arc::new(TaskManager::new());
    tm.set_global_limit(1);
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for (brain, id) in [("brainA", "a1"), ("brainB", "b1"), ("brainC", "c1")] {
        let tm2 = Arc::clone(&tm);
        let active2 = Arc::clone(&active);
        let max2 = Arc::clone(&max_active);
        let brain = brain.to_string();
        let id = id.to_string();
        handles.push(std::thread::spawn(move || {
            tm2.test_run(&brain, 5, &brain, &id, || {
                let cur = active2.fetch_add(1, Ordering::SeqCst) + 1;
                max2.fetch_max(cur, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                active2.fetch_sub(1, Ordering::SeqCst);
            });
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 1);
}

#[test]
fn stop_queued_removes_and_frees_position() {
    let _g = setup("stopq");
    let tm = Arc::new(TaskManager::new());
    tm.set_global_limit(10);
    tm.save(&json!({ "id": "wait1", "status": "queued", "updated": now() }));
    let (tx, rx) = std::sync::mpsc::channel();
    let tm2 = Arc::clone(&tm);
    let h1 = std::thread::spawn(move || {
        tm2.test_run("brainA", 1, "A", "run1", || {
            let _ = tx.send(());
            std::thread::sleep(Duration::from_millis(300));
        });
    });
    rx.recv().expect("run1 started");
    let tm3 = Arc::clone(&tm);
    let h2 = std::thread::spawn(move || {
        tm3.test_run("brainA", 1, "A", "wait1", || {
            panic!("wait1 should not run");
        });
    });
    let mut found = false;
    for _ in 0..200 {
        let q = tm.queue_info();
        if let Some(arr) = q.get("waiting").and_then(|v| v.as_array()) {
            if arr
                .iter()
                .any(|x| x.get("id").and_then(|v| v.as_str()) == Some("wait1"))
            {
                found = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(found, "wait1 should be queued");
    tm.stop("wait1").expect("stop");
    h2.join().expect("wait1 thread");
    let q = tm.queue_info();
    let still = q
        .get("waiting")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .any(|x| x.get("id").and_then(|v| v.as_str()) == Some("wait1"))
        })
        .unwrap_or(false);
    assert!(!still, "wait1 should have been removed");
    h1.join().expect("run1");
}

#[test]
fn guard_releases_on_panic() {
    let _g = setup("panic");
    let tm = Arc::new(TaskManager::new());
    tm.set_global_limit(10);
    let tm2 = Arc::clone(&tm);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tm2.test_run("brainA", 1, "A", "p1", || {
            panic!("boom");
        });
    }));
    assert!(r.is_err());
    let tm3 = Arc::clone(&tm);
    let h = std::thread::spawn(move || {
        tm3.test_run("brainA", 1, "A", "p2", || {});
    });
    h.join().expect("p2");
    let q = tm.queue_info();
    assert_eq!(q.get("running_total").and_then(|v| v.as_u64()), Some(0));
    assert!(q
        .get("waiting")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false));
}

#[test]
fn queue_info_reflects_numbers() {
    let _g = setup("qinfo");
    let tm = Arc::new(TaskManager::new());
    tm.set_global_limit(3);
    let (tx, rx) = std::sync::mpsc::channel();
    let tm2 = Arc::clone(&tm);
    let h = std::thread::spawn(move || {
        tm2.test_run("brainA", 2, "A", "r1", || {
            let _ = tx.send(());
            std::thread::sleep(Duration::from_millis(300));
        });
    });
    rx.recv().expect("started");
    let q = tm.queue_info();
    assert_eq!(q.get("global_limit").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(q.get("running_total").and_then(|v| v.as_u64()), Some(1));
    let per = q.get("per_brain").and_then(|v| v.get("brainA"));
    assert_eq!(
        per.and_then(|v| v.get("running")).and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        per.and_then(|v| v.get("limit")).and_then(|v| v.as_u64()),
        Some(2)
    );
    h.join().expect("r1");
}
