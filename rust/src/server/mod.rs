//! HTTP server: JSON API, static HMI, API proxy (port of metabrain/server.py).
mod api;
mod auth;
mod http;
mod security;
#[cfg(test)]
mod tests;

pub use auth::Auth;

use crate::core::Supervisor;
use crate::proxy;
use crate::util::{log, J};
use http::{proxy_parts, read_body, req_header, send_json, send_static};
use security::{host_allowed, is_local, origin_allowed};
use serde_json::json;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tiny_http::{Request, Server};

pub const COOKIE: &str = "mb_session";
const MAX_CONCURRENT_REQUESTS: usize = 128;
pub const LOCAL: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

pub fn lan_ip() -> String {
    let Ok(s) = UdpSocket::bind("0.0.0.0:0") else {
        return "127.0.0.1".into();
    };
    if s.connect("10.255.255.255:1").is_err() {
        return "127.0.0.1".into();
    }
    s.local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn handle_http(
    sup: &Arc<Supervisor>,
    auth: &Auth,
    port: u16,
    lan_listening: bool,
    mut req: Request,
) {
    let ip = req
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().as_str().to_uppercase();
    let cookie_header = req_header(&req, "Cookie").unwrap_or_default();
    let host_header = req_header(&req, "Host");
    let origin_header = req_header(&req, "Origin");
    let sec_fetch_site = req_header(&req, "Sec-Fetch-Site");
    let referer_header = req_header(&req, "Referer");
    let local = is_local(&ip);

    if !host_allowed(host_header.as_deref(), port, lan_listening) {
        send_json(req, 403, &json!({"error": "forbidden host"}), &[]);
        return;
    }
    let host_str = host_header.as_deref().unwrap_or("");
    if !origin_allowed(
        &method,
        &path,
        host_str,
        origin_header.as_deref(),
        sec_fetch_site.as_deref(),
        referer_header.as_deref(),
    ) {
        send_json(
            req,
            403,
            &json!({"error": "cross-origin request blocked"}),
            &[],
        );
        return;
    }

    if path.starts_with("/proxy/") {
        if !local {
            send_json(req, 403, &json!({"error": "only from this computer"}), &[]);
            return;
        }
        if let Some((bid, subpath)) = proxy_parts(&path) {
            proxy::handle(sup, &bid, &subpath, req);
        } else {
            send_json(req, 404, &json!({"error": "not found"}), &[]);
        }
        return;
    }

    if path == "/api/login" && method == "POST" {
        let rb = read_body(&mut req);
        let d = rb.unwrap_or_else(|| json!({}));
        let pin = d
            .get("pin")
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default();
        let real_pin = sup.cfg.key("lan").s("pin");
        let (tok, msg) = auth.login(&ip, &pin, &real_pin);
        if let Some(tok) = tok {
            let extra = [(
                "Set-Cookie",
                format!(
                    "{COOKIE}={tok}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
                    30 * 86400
                ),
            )];
            send_json(req, 200, &json!({"ok": true}), &extra);
        } else {
            send_json(req, 403, &json!({"error": msg}), &[]);
        }
        return;
    }

    if path.starts_with("/api/") {
        if !auth.ok(&ip, &cookie_header) {
            send_json(req, 401, &json!({"error": "login required"}), &[]);
            return;
        }
        if !local {
            let lan_allowed = method == "GET"
                && matches!(
                    path.as_str(),
                    "/api/state" | "/api/tasks" | "/api/lan" | "/api/qr.svg"
                );
            if !lan_allowed {
                send_json(
                    req,
                    403,
                    &json!({"error": "The LAN monitor is strictly read-only."}),
                    &[],
                );
                return;
            }
        }
        api::dispatch(sup, req, &method, &path, local, lan_listening);
        return;
    }

    send_static(req, &path);
}

struct ActiveGuard(Arc<AtomicUsize>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Start the server. Blocks, handling requests concurrently up to `MAX_CONCURRENT_REQUESTS`.
pub fn serve(server: Server, sup: Arc<Supervisor>, port: u16, lan: bool) -> Result<(), String> {
    let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
    let auth = Arc::new(Auth::new());
    let active = Arc::new(AtomicUsize::new(0));

    log(&format!(
        "HMI on http://{}:{}/{}{}",
        if host == "0.0.0.0" { "127.0.0.1" } else { host },
        port,
        "",
        if lan { " (LAN on)" } else { "" }
    ));

    for req in server.incoming_requests() {
        let prev = active.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_CONCURRENT_REQUESTS {
            active.fetch_sub(1, Ordering::AcqRel);
            send_json(req, 503, &json!({"error": "server busy"}), &[]);
            continue;
        }
        let sup = sup.clone();
        let auth = auth.clone();
        let guard = ActiveGuard(active.clone());
        if let Err(e) = std::thread::Builder::new().spawn(move || {
            let _guard = guard;
            handle_http(&sup, &auth, port, lan, req);
        }) {
            log(&format!("server spawn failed: {e}"));
        }
    }
    Ok(())
}
