//! End-to-end smoke test for the real `MetaBrain` binary:
//! boots `MetaBrain serve --no-tray --no-browser --port <ephemeral>` in an isolated
//! `METABRAIN_HOME`, exercises core HTTP endpoints and PIN auth, and guarantees clean teardown.

use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard {
    child: Child,
    home: PathBuf,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn resp_json(resp: ureq::Response) -> Value {
    let raw = resp.into_string().expect("read response body");
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("invalid JSON ({e}): {raw}"))
}

#[test]
fn binary_smoke_serves_api_and_auth() {
    let port = free_port();
    let home = std::env::temp_dir().join(format!(
        "mb_smoke_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).expect("create temp METABRAIN_HOME");

    // Minimal isolated config: no external brains, notifications off, known LAN PIN.
    let cfg = json!({
        "port": port,
        "notifications": false,
        "lan": { "enabled": false, "pin": "654321" },
        "brains": []
    });
    std::fs::write(home.join("metabrain.json"), cfg.to_string()).expect("write config");

    let bin = env!("CARGO_BIN_EXE_MetaBrain");
    let child = Command::new(bin)
        .args([
            "serve",
            "--no-tray",
            "--no-browser",
            "--port",
            &port.to_string(),
        ])
        .env("METABRAIN_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn MetaBrain binary");

    let _guard = ChildGuard { child, home };
    let base = format!("http://127.0.0.1:{port}");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build();

    // Wait for server startup (up to 15s).
    let deadline = Instant::now() + Duration::from_secs(15);
    let state: Value = loop {
        match agent.get(&format!("{base}/api/state")).call() {
            Ok(resp) if resp.status() == 200 => break resp_json(resp),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(150)),
            Err(e) => panic!("MetaBrain did not answer /api/state within 15s: {e}"),
            Ok(resp) => panic!("unexpected status from /api/state: {}", resp.status()),
        }
    };
    assert!(
        state.get("brains").and_then(|v| v.as_array()).is_some(),
        "state should have brains array"
    );

    let health = resp_json(
        agent
            .get(&format!("{base}/api/health"))
            .call()
            .expect("GET /api/health"),
    );
    assert_eq!(health["pid"], json!(_guard.child.id()));

    // /api/tasks returns a JSON array.
    let tasks = resp_json(
        agent
            .get(&format!("{base}/api/tasks"))
            .call()
            .expect("GET /api/tasks"),
    );
    assert!(
        tasks.is_array(),
        "/api/tasks must return an array, got: {tasks}"
    );

    // /api/savings returns relay + routing fields.
    let savings = resp_json(
        agent
            .get(&format!("{base}/api/savings"))
            .call()
            .expect("GET /api/savings"),
    );
    assert!(savings.get("saved_tokens").is_some());
    assert!(savings.get("relay_saved_tokens").is_some());
    assert!(savings.get("routing_saved_tokens").is_some());
    assert!(savings.get("routing").is_some());

    // /api/lan accurately reports localhost-only when started without --lan.
    let lan = resp_json(
        agent
            .get(&format!("{base}/api/lan"))
            .call()
            .expect("GET /api/lan"),
    );
    assert_eq!(lan["listening"], json!(false));
    assert_eq!(lan["host"], json!("127.0.0.1"));
    assert_eq!(lan["tls"], json!(false));

    // /api/login rejects wrong PIN (403) and accepts right PIN (200 + mb_session cookie).
    let bad = agent
        .post(&format!("{base}/api/login"))
        .set("Content-Type", "application/json")
        .send_string(&json!({"pin": "000000"}).to_string());
    assert!(
        matches!(bad, Err(ureq::Error::Status(403, _))),
        "wrong PIN must return 403"
    );

    let ok_resp = agent
        .post(&format!("{base}/api/login"))
        .set("Content-Type", "application/json")
        .send_string(&json!({"pin": "654321"}).to_string())
        .expect("valid PIN login");
    assert_eq!(ok_resp.status(), 200);
    let cookie = ok_resp.header("Set-Cookie").unwrap_or("");
    assert!(
        cookie.contains("mb_session="),
        "missing session cookie: {cookie}"
    );
    assert!(
        cookie.contains("HttpOnly"),
        "cookie must be HttpOnly: {cookie}"
    );
    let ok_body = resp_json(ok_resp);
    assert_eq!(ok_body["ok"], json!(true));

    // Root serves embedded HTML panel with anti-clickjacking headers.
    let index = agent.get(&format!("{base}/")).call().expect("GET /");
    assert_eq!(index.status(), 200);
    assert!(index
        .header("Content-Type")
        .unwrap_or("")
        .contains("text/html"));
    assert_eq!(index.header("X-Frame-Options"), Some("DENY"));
    assert!(index
        .header("Content-Security-Policy")
        .unwrap_or("")
        .contains("frame-ancestors 'none'"));

    // Security hardening: block DNS rebinding (foreign Host), CSRF (foreign Origin),
    // and cross-site API reads (Sec-Fetch-Site: cross-site).
    let rebind = agent
        .get(&format!("{base}/api/state"))
        .set("Host", &format!("evil.com:{port}"))
        .call();
    assert!(
        matches!(rebind, Err(ureq::Error::Status(403, _))),
        "foreign Host must return 403"
    );

    let csrf = agent
        .post(&format!("{base}/api/act"))
        .set("Origin", "https://evil.com")
        .set("Content-Type", "text/plain")
        .send_string(r#"{"a":"pause_all"}"#);
    assert!(
        matches!(csrf, Err(ureq::Error::Status(403, _))),
        "cross-origin POST must return 403"
    );

    let cross_read = agent
        .get(&format!("{base}/api/state"))
        .set("Sec-Fetch-Site", "cross-site")
        .call();
    assert!(
        matches!(cross_read, Err(ureq::Error::Status(403, _))),
        "cross-site API GET must return 403"
    );
}
