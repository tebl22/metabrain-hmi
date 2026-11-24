use super::auth::cookie;
use super::security::{
    host_allowed, is_local, origin_allowed, redact_remote_secrets, safe_rel, safe_upload_dir,
};
use super::{Auth, COOKIE};
use serde_json::json;

#[test]
fn api_attachment_text_has_one_budget_and_warns_when_truncated() {
    let base = std::env::temp_dir().join(format!("mb_attachments_{:x}", rand::random::<u64>()));
    let files: Vec<_> = ["one", "two", "three"]
        .iter()
        .map(|name| base.with_extension(name).to_string_lossy().into_owned())
        .collect();
    std::fs::write(&files[0], "界".repeat(30_000)).unwrap();
    std::fs::write(&files[1], "λ".repeat(10_000)).unwrap();
    std::fs::write(&files[2], "OMITIDO").unwrap();
    let exact = crate::docs::attach_to_prompt("Inicio", &files[..2], true);
    let limited = crate::docs::attach_to_prompt("Inicio", &files, true);
    std::fs::write(&files[1], format!("{}RECORTADO", "λ".repeat(10_000))).unwrap();
    let partial = crate::docs::attach_to_prompt("Inicio", &files[..2], true);
    let cli = crate::docs::attach_to_prompt("Inicio", &files, false);
    for file in &files {
        std::fs::remove_file(file).unwrap();
    }

    assert!(!exact.contains("Note:"));
    for prompt in [&limited, &partial] {
        assert_eq!(prompt.matches('界').count(), 30_000);
        assert_eq!(prompt.matches('λ').count(), 10_000);
        assert!(!prompt.contains("OMITIDO") && !prompt.contains("RECORTADO"));
        assert!(prompt.contains("40,000-character limit"));
    }
    assert!(cli.contains(&files[0]) && !cli.contains('界') && !cli.contains("Note:"));
}

#[test]
fn cookie_parser_finds_session() {
    assert_eq!(
        cookie("a=1; mb_session=abc; b=2", COOKIE).as_deref(),
        Some("abc")
    );
    assert_eq!(cookie("a=1", COOKIE), None);
}

#[test]
fn local_addresses_are_free() {
    assert!(is_local("127.0.0.1"));
    assert!(is_local("::1"));
    assert!(!is_local("192.168.1.10"));
}

#[test]
fn login_locks_after_five_wrong_pins() {
    let a = Auth::new();
    for _ in 0..5 {
        let (tok, msg) = a.login("8.8.8.8", "000000", "424242");
        assert!(tok.is_none());
        assert_eq!(msg, "wrong PIN");
    }
    let (tok, msg) = a.login("8.8.8.8", "424242", "424242");
    assert!(tok.is_none());
    assert!(msg.starts_with("locked"));
}

#[test]
fn login_ok_creates_session_and_safe_paths() {
    let a = Auth::new();
    let (tok, msg) = a.login("8.8.8.8", "424242", "424242");
    assert_eq!(msg, "ok");
    let tok = tok.unwrap_or_default();
    assert_eq!(tok.len(), 64);
    assert!(a.ok("8.8.8.8", &format!("{COOKIE}={tok}")));
    assert_eq!(safe_rel("/").as_deref(), Some("index.html"));
    assert_eq!(safe_rel("/../app.py"), None);
    assert_eq!(safe_rel("/index.html\0.png"), None);
}

#[test]
fn host_header_blocks_dns_rebinding_and_allows_local_or_lan_ips() {
    assert!(host_allowed(Some("127.0.0.1:8790"), 8790, false));
    assert!(host_allowed(Some("127.0.0.1"), 8790, false));
    assert!(host_allowed(Some("localhost:8790"), 8790, false));
    assert!(host_allowed(Some("[::1]:8790"), 8790, false));

    assert!(!host_allowed(None, 8790, false));
    assert!(!host_allowed(Some("evil.com:8790"), 8790, false));
    assert!(!host_allowed(Some("evil.com:8790"), 8790, true));
    assert!(!host_allowed(Some("127.0.0.1.evil.com:8790"), 8790, true));
    assert!(!host_allowed(Some("127.0.0.1:9999"), 8790, false));
    assert!(!host_allowed(Some("192.168.1.50:8790"), 8790, false));
    assert!(host_allowed(Some("192.168.1.50:8790"), 8790, true));
    assert!(host_allowed(Some("[fe80::1]:8790"), 8790, true));
}

#[test]
fn origin_and_fetch_metadata_block_csrf_and_cross_site_api_reads() {
    // Non-browser local clients (CLI, MCP, GUI, SDKs): no Origin / Sec-Fetch-Site.
    assert!(origin_allowed(
        "POST",
        "/api/tasks",
        "127.0.0.1:8790",
        None,
        None,
        None
    ));
    assert!(origin_allowed(
        "POST",
        "/proxy/openai/v1/chat/completions",
        "127.0.0.1:8790",
        None,
        None,
        None
    ));

    // Same-origin browser requests pass.
    assert!(origin_allowed(
        "POST",
        "/api/act",
        "127.0.0.1:8790",
        Some("http://127.0.0.1:8790"),
        Some("same-origin"),
        None
    ));

    // Cross-origin POST CSRF (e.g. fetch with mode: no-cors / text/plain) is blocked.
    assert!(!origin_allowed(
        "POST",
        "/api/tasks",
        "127.0.0.1:8790",
        Some("https://evil.com"),
        Some("cross-site"),
        None
    ));
    // Null origin (sandboxed iframe / file:// HTML attack) is blocked.
    assert!(!origin_allowed(
        "POST",
        "/api/act",
        "127.0.0.1:8790",
        Some("null"),
        None,
        None
    ));
    // Cross-site GET to /api/* or /proxy/* is blocked, while top-level static navigation is allowed.
    assert!(!origin_allowed(
        "GET",
        "/api/state",
        "127.0.0.1:8790",
        None,
        Some("cross-site"),
        None
    ));
    assert!(!origin_allowed(
        "GET",
        "/proxy/a/v1/models",
        "127.0.0.1:8790",
        None,
        Some("cross-site"),
        None
    ));
    assert!(origin_allowed(
        "GET",
        "/",
        "127.0.0.1:8790",
        None,
        Some("cross-site"),
        None
    ));
    // OPTIONS preflight is rejected.
    assert!(!origin_allowed(
        "OPTIONS",
        "/api/act",
        "127.0.0.1:8790",
        Some("https://evil.com"),
        Some("cross-site"),
        None
    ));
    // Referer mismatch without Origin/Sec-Fetch-Site on mutating POST is blocked.
    assert!(!origin_allowed(
        "POST",
        "/api/act",
        "127.0.0.1:8790",
        None,
        None,
        Some("https://evil.com/page")
    ));
}

#[test]
fn ipv4_mapped_loopback_and_secret_redaction_work() {
    assert!(is_local("::ffff:127.0.0.1"));
    assert!(!is_local("::ffff:192.168.1.5"));

    let mut cfg = json!({
        "lan": {"pin": "123456"},
        "telegram": {"token_env": "METABRAIN_TELEGRAM_TOKEN", "token": "bot-secret"},
        "brains": [{"id": "a", "key_env": "OPENAI_API_KEY", "api_key": "sk-123", "password": "pw"}]
    });
    redact_remote_secrets(&mut cfg);
    assert_eq!(cfg["lan"]["pin"], json!("******"));
    assert_eq!(
        cfg["telegram"]["token_env"],
        json!("METABRAIN_TELEGRAM_TOKEN")
    );
    assert_eq!(cfg["telegram"]["token"], json!("******"));
    assert_eq!(cfg["brains"][0]["key_env"], json!("OPENAI_API_KEY"));
    assert_eq!(cfg["brains"][0]["api_key"], json!("******"));
    assert_eq!(cfg["brains"][0]["password"], json!("******"));
}

#[test]
fn upload_dir_rejects_traversal_and_system_dirs() {
    let _lock = crate::util::test_env_lock();
    let fallback = crate::util::data_dir().join("adjuntos");
    assert_eq!(safe_upload_dir(""), fallback);
    assert_eq!(safe_upload_dir("..\\outside"), fallback);
    assert_eq!(safe_upload_dir("C:\\no_existe_seguro_12345"), fallback);
    #[cfg(windows)]
    if let Ok(sr) = std::env::var("SystemRoot") {
        assert_eq!(safe_upload_dir(&sr), fallback);
    }
    let tmp = std::env::temp_dir();
    let resolved = safe_upload_dir(&tmp.to_string_lossy());
    assert!(resolved.ends_with(std::path::Path::new(".metabrain").join("adjuntos")));
}
