use super::policy::is_readonly_command;
use super::{_config, install, resolve_brain, run_hook};
use crate::util::{read_json, write_json};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl EnvGuard {
    fn new() -> EnvGuard {
        let keys = [
            "METABRAIN_HOME",
            "CLAUDE_CONFIG_DIR",
            "CEREBRO",
            "METABRAIN_BRAIN",
        ];
        EnvGuard {
            saved: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(*k, val),
                None => std::env::remove_var(*k),
            }
        }
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("mb_hooks_{}_{}_{}", tag, std::process::id(), n));
    let _ = fs::create_dir_all(&p);
    p
}

fn pstr(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

fn setup(tag: &str) -> (PathBuf, PathBuf) {
    let home = tmpdir(tag);
    let cfgdir = home.join("acct");
    let _ = fs::create_dir_all(&cfgdir);
    std::env::set_var("METABRAIN_HOME", &home);
    std::env::set_var("CLAUDE_CONFIG_DIR", &cfgdir);
    std::env::remove_var("CEREBRO");
    std::env::remove_var("METABRAIN_BRAIN");
    (home, cfgdir)
}

fn write_cfg(home: &Path, v: &Value) {
    write_json(&home.join("metabrain.json"), v).expect("write cfg");
}

fn write_gate(home: &Path, v: &Value) {
    // A live supervisor stamps the gate; tests do the same so it counts as fresh.
    let mut v = v.clone();
    if v.get("ts").is_none() {
        v["ts"] = json!(crate::util::now());
    }
    write_json(&home.join("gate.json"), &v).expect("write gate");
}

fn run(name: &str, ev: &Value) -> (i32, String, String) {
    run_hook(name, &ev.to_string())
}

#[test]
fn resolve_by_env_and_prompt() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let (home, cfgdir) = setup("resolve");

    write_cfg(
        &home,
        &json!({
            "brains": [
                {"id": "A", "kind": "claude_code", "config_dir": pstr(&cfgdir), "subagents": {}},
                {"id": "B", "kind": "claude_code", "config_dir": pstr(&cfgdir), "match": {"prompt": "CUENTA B"}}
            ],
            "memory": {"dir": pstr(&home.join("mem"))}
        }),
    );

    let cfg = _config();
    assert!(cfg.is_object());

    let tr = home.join("t.jsonl");
    fs::write(
        &tr,
        json!({"type": "queue-operation", "content": "CUENTA B: haz algo"}).to_string() + "\n",
    )
    .expect("transcript");
    assert_eq!(
        resolve_brain(&cfg, &json!({"transcript_path": pstr(&tr)})),
        (Some("B".to_string()), true)
    );
    assert_eq!(
        resolve_brain(&cfg, &json!({})),
        (Some("A".to_string()), false)
    );

    fs::write(
        &tr,
        json!({"type": "user", "message": {"content": "hola"}}).to_string() + "\n",
    )
    .expect("transcript");
    assert_eq!(
        resolve_brain(&cfg, &json!({"transcript_path": pstr(&tr)})),
        (Some("A".to_string()), true)
    );

    std::env::set_var("CEREBRO", "b");
    assert_eq!(
        resolve_brain(&cfg, &json!({})),
        (Some("B".to_string()), true)
    );
}

#[test]
fn stale_gate_never_blocks() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let (home, cfgdir) = setup("stale");
    write_cfg(
        &home,
        &json!({"brains": [{"id": "A", "kind": "claude_code", "config_dir": pstr(&cfgdir)}],
                "memory": {"dir": pstr(&home.join("mem"))}}),
    );
    write_json(
        &home.join("gate.json"),
        &json!({"ts": 1.0, "brains": {"A": {"paused": true, "readonly": true}}}),
    )
    .unwrap();
    std::env::set_var("METABRAIN_BRAIN", "A");
    let (_, out, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Edit", "tool_input": {"file_path": "x"}}),
    );
    std::env::remove_var("METABRAIN_BRAIN");
    assert!(out.trim().is_empty(), "an old gate must not deny: {out}");
}

#[test]
fn pause_and_readonly_denies() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let (home, cfgdir) = setup("ro");

    write_cfg(
        &home,
        &json!({
            "brains": [
                {"id": "A", "kind": "claude_code", "config_dir": pstr(&cfgdir), "subagents": {}},
                {"id": "B", "kind": "claude_code", "config_dir": pstr(&cfgdir), "match": {"prompt": "CUENTA B"}}
            ],
            "memory": {"dir": pstr(&home.join("mem"))}
        }),
    );
    write_gate(
        &home,
        &json!({"brains": {"A": {"paused": true, "reason": "budget 100%"}}}),
    );

    let (rc, out, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Read", "tool_input": {}}),
    );
    assert_eq!(rc, 0);
    assert!(out.contains("paused") && out.contains("budget 100%"));

    let (_, out2, _) = run("session-start", &json!({"session_id": "s2"}));
    assert!(out2.contains("PAUSED") && out2.contains("[METABRAIN]"));

    write_gate(&home, &json!({"brains": {"A": {"readonly": true}}}));
    let (_, out3, _) = run(
        "pre-tool",
        &json!({"session_id": "s3", "tool_name": "Edit", "tool_input": {"file_path": pstr(&home.join("x.py"))}}),
    );
    assert!(out3.contains("SOLO LECTURA"));

    let (_, out4, _) = run(
        "pre-tool",
        &json!({"session_id": "s3", "tool_name": "Bash", "tool_input": {"command": "rm -rf x"}}),
    );
    assert!(out4.contains("SOLO LECTURA"));

    let (_, out5, _) = run(
        "pre-tool",
        &json!({"session_id": "s3", "tool_name": "Bash", "tool_input": {"command": "git status --short"}}),
    );
    assert!(out5.is_empty());
}

#[test]
fn subagent_policy() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let (home, cfgdir) = setup("agents");

    write_cfg(
        &home,
        &json!({
            "brains": [{
                "id": "A",
                "kind": "claude_code",
                "config_dir": pstr(&cfgdir),
                "subagents": {
                    "Explore": {"model": "haiku"},
                    "Plan": {"allow": false},
                    "*": {"max_parallel": 1}
                }
            }],
            "memory": {"dir": pstr(&home.join("mem"))}
        }),
    );
    write_gate(&home, &json!({"brains": {}}));

    let (_, r1, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Agent", "tool_use_id": "t1", "tool_input": {"subagent_type": "Explore"}}),
    );
    assert!(r1.contains("must use model 'haiku'"));

    let (_, r2, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Agent", "tool_use_id": "t1", "tool_input": {"subagent_type": "Plan"}}),
    );
    assert!(r2.contains("blocked"));

    let (_, r3, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Agent", "tool_use_id": "t1", "tool_input": {"subagent_type": "Explore", "model": "haiku"}}),
    );
    assert!(r3.is_empty());

    let (_, r4, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Agent", "tool_use_id": "t2", "tool_input": {"subagent_type": "other"}}),
    );
    assert!(r4.contains("at most 1"));

    run(
        "post-tool",
        &json!({"session_id": "s", "tool_name": "Agent", "tool_use_id": "t1"}),
    );
    let (_, r5, _) = run(
        "pre-tool",
        &json!({"session_id": "s", "tool_name": "Agent", "tool_use_id": "t2", "tool_input": {"subagent_type": "other"}}),
    );
    assert!(r5.is_empty());
}

#[test]
fn file_lock_between_brains() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let (home, cfgdir) = setup("lock");

    write_cfg(
        &home,
        &json!({
            "brains": [
                {"id": "A", "kind": "claude_code", "config_dir": pstr(&cfgdir)},
                {"id": "B", "kind": "claude_code", "config_dir": pstr(&cfgdir), "match": {"prompt": "CUENTA B"}}
            ],
            "memory": {"dir": pstr(&home.join("mem"))}
        }),
    );
    write_gate(&home, &json!({"brains": {}}));

    let f = home.join("x.py");
    let edit_a =
        json!({"session_id": "sa", "tool_name": "Edit", "tool_input": {"file_path": pstr(&f)}});
    let (_, r0, _) = run("pre-tool", &edit_a);
    assert!(r0.is_empty());

    std::env::set_var("CEREBRO", "B");
    let write_b =
        json!({"session_id": "sb", "tool_name": "Write", "tool_input": {"file_path": pstr(&f)}});
    let (_, r1, _) = run("pre-tool", &write_b);
    assert!(r1.contains("locked by brain 'A'"));

    std::env::remove_var("CEREBRO");
    run("session-end", &json!({"session_id": "sa"}));
    std::env::set_var("CEREBRO", "B");
    let (_, r2, _) = run("pre-tool", &write_b);
    assert!(r2.is_empty());
}

#[test]
fn install_replaces_legacy_and_is_idempotent() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let (home, cfgdir) = setup("install");

    write_cfg(
        &home,
        &json!({"brains": [{"id": "A", "kind": "claude_code", "config_dir": pstr(&cfgdir)}]}),
    );

    let settings = json!({
        "model": "x",
        "hooks": {
            "PreToolUse": [
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "python C:/x/cerebros.py hook-candado"}]},
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "mine.sh"}]}
            ]
        }
    });
    fs::write(
        cfgdir.join("settings.json"),
        serde_json::to_string(&settings).expect("settings json"),
    )
    .expect("write settings");

    let cfg = _config();
    let exe = "C:\\Tools\\metabrain-hook.exe";
    let msgs1 = install(&cfg, exe, false);
    assert!(msgs1.iter().any(|m| m.contains("hooks installed in")));
    let msgs2 = install(&cfg, exe, false);
    assert!(!msgs2.is_empty());

    let s = read_json(&cfgdir.join("settings.json")).expect("settings");
    assert_eq!(s.get("model").and_then(|v| v.as_str()), Some("x"));
    let pre = s
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|v| v.as_array())
        .expect("PreToolUse");
    assert_eq!(pre.len(), 2);
    assert_eq!(
        pre[0]
            .get("hooks")
            .and_then(|h| h.get(0))
            .and_then(|x| x.get("command"))
            .and_then(|v| v.as_str()),
        Some("mine.sh")
    );
    assert!(pre[1]
        .get("hooks")
        .and_then(|h| h.get(0))
        .and_then(|x| x.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .contains("pre-tool"));

    let hooks = s.get("hooks").and_then(|v| v.as_object()).expect("hooks");
    let keys: BTreeSet<&str> = hooks.keys().map(|k| k.as_str()).collect();
    let want: BTreeSet<&str> = [
        "PreToolUse",
        "PostToolUse",
        "SessionStart",
        "SessionEnd",
        "UserPromptSubmit",
        "Stop",
        "PermissionRequest",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, want);
    assert!(cfgdir.join("settings.json.metabrain.bak").exists());

    // BOM is tolerated; broken JSON is left untouched.
    fs::write(
        cfgdir.join("settings.json"),
        b"\xef\xbb\xbf{\"model\":\"keep\"}",
    )
    .expect("bom settings");
    let _ = install(&cfg, exe, false);
    let s2 = read_json(&cfgdir.join("settings.json")).expect("bom settings");
    assert_eq!(s2.get("model").and_then(|v| v.as_str()), Some("keep"));
    assert!(s2.get("hooks").and_then(|h| h.get("PreToolUse")).is_some());

    fs::write(cfgdir.join("settings.json"), "{broken").expect("broken settings");
    let _ = install(&cfg, exe, false);
    assert_eq!(
        fs::read_to_string(cfgdir.join("settings.json")).unwrap_or_default(),
        "{broken"
    );
}

#[test]
fn readonly_command_blocks_shell_injection_and_destructive_flags() {
    // Legitimate read-only commands still pass (including regex alternation inside quotes)
    assert!(is_readonly_command("ls -la"));
    assert!(is_readonly_command("git status"));
    assert!(is_readonly_command("git diff HEAD~1"));
    assert!(is_readonly_command(r#"rg "foo|bar" src/"#));
    assert!(is_readonly_command("Get-Content Cargo.toml"));

    // Shell chaining / redirection / command substitution bypasses must be denied
    assert!(!is_readonly_command("echo malicioso > rust/src/main.rs"));
    assert!(!is_readonly_command("ls && git push --force"));
    assert!(!is_readonly_command("ls || rm -rf /"));
    assert!(!is_readonly_command("git status; whoami"));
    assert!(!is_readonly_command("cat a | tee b"));
    assert!(!is_readonly_command("echo $(rm -rf tmp)"));
    assert!(!is_readonly_command("echo \"$(id)\""));
    assert!(!is_readonly_command("echo `id`"));
    assert!(!is_readonly_command("ls\nrm -rf /"));
    assert!(!is_readonly_command("echo 'unclosed"));

    // Destructive flags on otherwise read-only binaries must be denied
    assert!(!is_readonly_command("find . -delete"));
    assert!(!is_readonly_command("find . -name '*.rs' -exec rm {} +"));
    assert!(!is_readonly_command("find . -execdir sh -c true {} ;"));
    assert!(!is_readonly_command("git diff --output=pwn.txt"));
    assert!(!is_readonly_command("git -c core.fsmonitor=evil status"));
}

#[test]
fn broken_input_never_breaks() {
    let _lock = crate::util::test_env_lock();
    let _env = EnvGuard::new();
    let _home = setup("broken");

    let (rc, out, err) = run_hook("pre-tool", "{not json");
    assert_eq!((rc, out.as_str(), err.as_str()), (0, "", ""));
    let (rc2, out2, err2) = run_hook("unknown-hook", "{}");
    assert_eq!((rc2, out2.as_str(), err2.as_str()), (0, "", ""));
}
