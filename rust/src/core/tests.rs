//! Unit tests for `Supervisor` (command chain, quotas, budgets, actions, alarms).
use super::regulation::{add_series, comma};
use super::*;
use serde_json::json;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "metabrain_core_{}_{}_{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = std::fs::create_dir_all(&p);
    p
}

fn with_sup<F: FnOnce(Arc<Supervisor>)>(tag: &str, brains: Value, f: F) {
    let _g = crate::util::test_env_lock();
    let dir = tmpdir(tag);
    std::env::set_var("METABRAIN_HOME", &dir);
    let cfg = Arc::new(Config::from_value(
        dir.join("metabrain.json"),
        json!({"brains": brains}),
    ));
    let sup = Supervisor::new(cfg);
    f(sup);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn comma_groups_thousands() {
    assert_eq!(comma(0), "0");
    assert_eq!(comma(999), "999");
    assert_eq!(comma(1_234_567), "1,234,567");
    assert_eq!(comma(-1_200), "-1,200");
}

#[test]
fn add_series_sums_and_truncates() {
    let a = json!([1, 2, 3]);
    let b = json!([10, 20]);
    assert_eq!(json!(add_series(Some(&a), Some(&b))), json!([11, 22]));
    let empty = json!([]);
    assert_eq!(json!(add_series(Some(&empty), Some(&b))), json!([10, 20]));
    assert_eq!(json!(add_series(None, None)), json!([]));
}

#[test]
fn budget_ratio_uses_tokens_day() {
    with_sup(
        "budget",
        json!([{"id": "api", "kind": "openai", "name": "DeepSeek",
                "budget": {"tokens_day": 100, "usd_day": null, "quota_pct": null}}]),
        |sup| {
            let b = sup.brain("api").expect("brain");
            let summ = json!({"api": {"today": 150}});
            let (ratio, reasons) = sup.budget_ratio(&b, Some(&summ));
            assert_eq!(ratio, Some(1.5));
            assert_eq!(
                reasons.first().map(String::as_str),
                Some("150 / 100 tokens today")
            );
        },
    );
}

#[test]
fn readonly_is_stored_exposed_and_gated() {
    with_sup(
        "readonly",
        json!([{"id": "api", "kind": "openai", "name": "DeepSeek"}]),
        |sup| {
            let b = sup.brain("api").expect("brain");
            assert_eq!(
                sup.effective(&b, None)
                    .get("readonly")
                    .and_then(|v| v.as_bool()),
                Some(false)
            );
            assert_eq!(
                sup.act(&json!({"a": "set", "field": "readonly", "value": true, "brain": "api"})),
                "ok"
            );
            assert_eq!(
                sup.effective(&b, None)
                    .get("readonly")
                    .and_then(|v| v.as_bool()),
                Some(true)
            );
            let gate = crate::util::read_json(&sup.gate_path).expect("gate");
            assert_eq!(gate["brains"]["api"]["readonly"].as_bool(), Some(true));

            let snap = sup.snapshot();
            assert_eq!(
                snap.get("version").and_then(|v| v.as_str()),
                Some(env!("CARGO_PKG_VERSION"))
            );
            let alarms = snap
                .get("alarms")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            assert!(alarms
                .iter()
                .any(|a| a.s("text") == "DeepSeek: solo lectura"));
        },
    );
}

#[test]
fn stuck_alarm_is_logged_once() {
    with_sup("stuck", json!([]), |sup| {
        let entry = json!({
            "name": "Claude",
            "state": {"paused": false},
            "effective": {"downgraded": false, "reasons": []},
            "quota": [],
            "stuck": [{"session": "s1", "reason": "loop"}]
        });
        let empty: Vec<Value> = vec![];
        let a1 = sup.alarms(std::slice::from_ref(&entry), &empty);
        assert!(a1
            .iter()
            .any(|a| a.s("level") == "warning" && a.s("text") == "Claude: agente atascado (loop)"));
        let n1 = sup.diary.last(50).len();
        let a2 = sup.alarms(&[entry], &empty);
        assert!(a2.iter().any(|a| a.s("level") == "warning"));
        let n2 = sup.diary.last(50).len();
        assert_eq!(n1, n2);
    });
}

#[test]
fn pause_action_updates_state_and_gate() {
    with_sup(
        "pause",
        json!([{"id": "api", "kind": "openai", "name": "DeepSeek"}]),
        |sup| {
            assert_eq!(sup.act(&json!({"a": "pause", "brain": "api"})), "ok");
            let st = sup.state.get("api");
            assert_eq!(st.get("paused").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(st.get("paused_by").and_then(|v| v.as_str()), Some("user"));
            let gate = crate::util::read_json(&sup.gate_path).expect("gate");
            assert_eq!(gate["brains"]["api"]["paused"].as_bool(), Some(true));

            assert_eq!(sup.act(&json!({"a": "resume", "brain": "api"})), "ok");
            assert_eq!(
                sup.state.get("api").get("paused").and_then(|v| v.as_bool()),
                Some(false)
            );
        },
    );
}

#[test]
fn chain_orders_brains_and_picks_master() {
    with_sup(
        "chain",
        json!([
            {"id": "ds", "kind": "openai", "name": "DeepSeek"},
            {"id": "a", "kind": "claude_code", "name": "Claude A"}
        ]),
        |sup| {
            let ch = sup.chain(now());
            assert_eq!(ch.len(), 2);
            assert_eq!(ch[0].s("id"), "a");
            assert_eq!(ch[0].get("master").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(ch[1].s("id"), "ds");
            assert_eq!(ch[1].get("master").and_then(|v| v.as_bool()), Some(false));

            assert_eq!(
                sup.act(&json!({"a": "priority", "order": ["ds", "a"]})),
                "ok"
            );
            let ch = sup.chain(now());
            assert_eq!(ch[0].s("id"), "ds");
            assert_eq!(ch[0].get("master").and_then(|v| v.as_bool()), Some(true));

            assert_eq!(sup.act(&json!({"a": "pause", "brain": "ds"})), "ok");
            let ch = sup.chain(now());
            assert_eq!(ch[0].s("id"), "ds");
            assert_eq!(
                ch[0].get("available").and_then(|v| v.as_bool()),
                Some(false)
            );
            assert_eq!(ch[0].s("reason"), "pausado");
            assert_eq!(ch[1].s("id"), "a");
            assert_eq!(ch[1].get("master").and_then(|v| v.as_bool()), Some(true));
            // Same level: both lead, exactly one takes the next job.
            assert_eq!(sup.act(&json!({"a": "resume", "brain": "ds"})), "ok");
            assert_eq!(
                sup.act(
                    &json!({"a": "priority", "order": ["ds", "a"], "ranks": {"ds": 1, "a": 1}})
                ),
                "ok"
            );
            let ch = sup.chain(now());
            assert_eq!(Supervisor::masters_of(&ch).len(), 2, "{ch:?}");
            assert_eq!(ch.iter().filter(|e| e.b("lead", false)).count(), 1);
            assert_eq!(ch[0].get("rank"), ch[1].get("rank"));
            // "Mandar solo" puts that brain alone on top again.
            assert_eq!(sup.act(&json!({"a": "take_command", "brain": "a"})), "ok");
            let ch = sup.chain(now());
            assert_eq!(Supervisor::masters_of(&ch), vec!["a".to_string()]);
        },
    );
}

#[test]
fn chain_marks_codex_unavailable_when_quota_full() {
    let _g = crate::util::test_env_lock();
    let dir = tmpdir("codex_chain");
    let codex_home = dir.join("codex_home");
    let day = codex_home
        .join("sessions")
        .join("2026")
        .join("11")
        .join("23");
    std::fs::create_dir_all(&day).unwrap();
    let future_reset = (now() + 7200.0) as i64;
    std::fs::write(
        day.join("rollout-2026-11-23T20-25-50.jsonl"),
        format!(
            r#"{{"timestamp":"2026-11-24T01:34:24.891Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"codex","primary":{{"used_percent":100.0,"resets_at":{future_reset}}},"secondary":{{"used_percent":31.0,"resets_at":{future_reset}}}}}}}}}"#
        ),
    )
    .unwrap();

    std::env::set_var("METABRAIN_HOME", &dir);
    let cfg = Arc::new(Config::from_value(
        dir.join("metabrain.json"),
        json!({
            "priority": ["chatgpt", "ds"],
            "brains": [
                {"id": "chatgpt", "kind": "codex", "name": "ChatGPT", "codex_home": codex_home.to_string_lossy()},
                {"id": "ds", "kind": "openai", "name": "DeepSeek"}
            ]
        }),
    ));
    let sup = Supervisor::new(cfg);
    let ch = sup.chain_entries(now());
    assert_eq!(ch[0].id, "chatgpt");
    assert_eq!(ch[0].pct, Some(100.0));
    assert!(!ch[0].available);
    assert!(
        ch[0].reason.starts_with("cuota"),
        "reason was: {}",
        ch[0].reason
    );
    assert_eq!(ch[1].id, "ds");
    assert!(ch[1].master);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn take_command_moves_brain_to_the_front() {
    with_sup(
        "take",
        json!([
            {"id": "a", "kind": "claude_code", "name": "A"},
            {"id": "ds", "kind": "openai", "name": "DS"}
        ]),
        |sup| {
            assert_eq!(
                sup.act(&json!({"a": "priority", "order": ["a", "ds"]})),
                "ok"
            );
            assert_eq!(sup.act(&json!({"a": "take_command", "brain": "ds"})), "ok");
            let p = sup.priority_list();
            assert_eq!(p.first().map(String::as_str), Some("ds"));
            let ch = sup.chain(now());
            assert_eq!(ch[0].s("id"), "ds");
            assert_eq!(ch[0].get("master").and_then(|v| v.as_bool()), Some(true));
        },
    );
}
