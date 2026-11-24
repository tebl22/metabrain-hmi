use super::api::{models_due, models_fetch_plan, resolve_models, ModelsFetch};
use super::{Common, Sub};
use crate::util::write_json;
use serde_json::{json, Value};

#[test]
fn subagent_stops_when_its_metadata_finishes_or_its_transcript_disappears() {
    let base = std::env::temp_dir().join(format!("metabrain-sub-{:x}", rand::random::<u64>()));
    let session = base.join("session.jsonl");
    let dir = base.join("session").join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let transcript = dir.join("agent-test.jsonl");
    let meta = transcript.with_extension("meta.json");
    std::fs::write(&transcript, "").unwrap();
    let sub = Sub {
        id: "test".into(),
        c: Common {
            last: 1000.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let running = sub.alive(&session, 1001.0);
    let expired = sub.alive(&session, 1180.0);
    write_json(&meta, &json!({"finished": true})).unwrap();
    let finished = sub.alive(&session, 1001.0);
    std::fs::remove_file(meta).unwrap();
    std::fs::remove_file(transcript).unwrap();
    let deleted = sub.alive(&session, 1001.0);
    std::fs::remove_dir(dir).unwrap();
    std::fs::remove_dir(session.with_extension("")).unwrap();
    std::fs::remove_dir(base).unwrap();

    assert!(running);
    assert!(!expired);
    assert!(!finished);
    assert!(!deleted);
}

#[test]
fn models_use_ladder_when_present() {
    let ladder = vec![json!("gemini-2.5-pro"), json!("gemini-2.5-flash")];
    let live = vec!["gemini-3-pro".to_string()];
    assert_eq!(
        resolve_models(&ladder, &live),
        vec!["gemini-2.5-pro".to_string(), "gemini-2.5-flash".to_string()]
    );
}

#[test]
fn models_fall_back_to_live_when_ladder_empty() {
    let ladder: Vec<Value> = vec![];
    let live = vec!["gemini-3-pro".to_string(), "gemini-3-flash".to_string()];
    assert_eq!(resolve_models(&ladder, &live), live);
}

#[test]
fn no_key_means_no_request_and_no_panic() {
    // Gemini brain with an empty ladder and a due time, but no key: no HTTP call is made.
    let plan = models_fetch_plan("openai", "gemini", true, 0.0, 1_000_000.0, false);
    assert!(matches!(plan, ModelsFetch::MissingKey));
    // With a key it would fetch.
    let plan = models_fetch_plan("openai", "gemini", true, 0.0, 1_000_000.0, true);
    assert!(matches!(plan, ModelsFetch::Fetch));
    // Resolving models with no data must not panic either.
    assert!(resolve_models(&[], &[]).is_empty());
}

#[test]
fn interval_of_six_hours_is_respected() {
    let t0 = 1_000_000.0;
    // First poll is always due.
    assert!(models_due(0.0, t0));
    // Less than six hours later: not due.
    assert!(!models_due(t0, t0 + 3600.0));
    assert!(!models_due(t0, t0 + 5.0 * 3600.0));
    // Six hours later: due again.
    assert!(models_due(t0, t0 + 6.0 * 3600.0));
    // The fetch plan follows the same interval.
    let plan = models_fetch_plan("openai", "gemini", true, t0, t0 + 3600.0, true);
    assert!(matches!(plan, ModelsFetch::Skip));
}
