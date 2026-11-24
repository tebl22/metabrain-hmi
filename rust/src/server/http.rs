//! HTTP request/response helpers, static HMI file serving, query parsing, QR SVG, and task SSE stream.
use super::security::safe_rel;
use crate::util::J;
use qrcodegen::{QrCode, QrCodeEcc};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;
use tiny_http::{Header, Request, Response, StatusCode};

static WEB: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../metabrain/web");

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn add_header<R: Read>(resp: &mut Response<R>, name: &str, value: &str) {
    if let Some(h) = header(name, value) {
        resp.add_header(h);
    }
}

pub(super) fn req_header(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

pub(super) fn send_raw(
    req: Request,
    status: u16,
    raw: Vec<u8>,
    ctype: &str,
    extra: &[(&str, String)],
) {
    let ct = if ctype.starts_with("text/")
        || ctype.starts_with("application/json")
        || ctype.starts_with("image/svg")
    {
        format!("{ctype}; charset=utf-8")
    } else {
        ctype.to_string()
    };
    let mut resp = Response::from_data(raw).with_status_code(StatusCode(status));
    add_header(&mut resp, "Content-Type", &ct);
    add_header(&mut resp, "Cache-Control", "no-store");
    add_header(&mut resp, "X-Content-Type-Options", "nosniff");
    add_header(&mut resp, "X-Frame-Options", "DENY");
    add_header(
        &mut resp,
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'",
    );
    add_header(&mut resp, "Referrer-Policy", "no-referrer");
    for (k, v) in extra {
        add_header(&mut resp, k, v.as_str());
    }
    let _ = req.respond(resp);
}

pub(super) fn send_json(req: Request, status: u16, value: &Value, extra: &[(&str, String)]) {
    let raw = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    send_raw(req, status, raw, "application/json", extra);
}

pub(super) fn read_body(req: &mut Request) -> Option<Value> {
    let n = req_header(req, "Content-Length")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    if n > 2_000_000 {
        return None;
    }
    let mut raw = Vec::new();
    if n > 0 {
        req.as_reader().take(n as u64).read_to_end(&mut raw).ok()?;
    }
    if raw.is_empty() {
        return Some(json!({}));
    }
    serde_json::from_slice(&raw).ok()
}

pub(super) fn proxy_parts(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim_start_matches('/');
    let mut it = trimmed.splitn(3, '/');
    let p = it.next()?;
    if p != "proxy" {
        return None;
    }
    let bid = it.next()?.to_string();
    let rest = it.next().unwrap_or("");
    Some((bid, format!("/{}", rest.trim_end_matches('/'))))
}

fn mime(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "js" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain",
        "csv" => "text/csv",
        _ => "application/octet-stream",
    }
}

pub(super) fn qr_svg(url: &str) -> Result<String, String> {
    let qr = QrCode::encode_text(url, QrCodeEcc::Medium).map_err(|e| format!("{e:?}"))?;
    let border = 4i32;
    let scale = 6i32;
    let size = qr.size();
    let dim = (size + 2 * border) * scale;
    let mut d = String::new();
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                let px = (x + border) * scale;
                let py = (y + border) * scale;
                d.push_str(&format!("M{px} {py}h{scale}v{scale}h-{scale}z"));
            }
        }
    }
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" version=\"1.1\" viewBox=\"0 0 {dim} {dim}\" shape-rendering=\"crispEdges\"><path fill=\"#E8EEF2\" d=\"{d}\"/></svg>"
    ))
}

/// Percent-decode a query value (also '+' as space).
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
                {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

pub(super) fn qparam(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?').map(|(_, q)| q)?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| pct_decode(v))
    })
}

/// Server-sent events for one task: tails its event log until the task ends.
pub(super) struct TaskEvents {
    pub(super) tasks: Arc<crate::tasks::TaskManager>,
    pub(super) id: String,
    pub(super) offset: u64,
    pub(super) buf: Vec<u8>,
    pub(super) pos: usize,
    pub(super) idle: std::time::Instant,
    pub(super) ended: Option<std::time::Instant>,
}

impl Read for TaskEvents {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = out.len().min(self.buf.len() - self.pos);
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            self.buf.clear();
            self.pos = 0;
            let (evs, off) = self.tasks.events_since(&self.id, self.offset);
            self.offset = off;
            if !evs.is_empty() {
                for e in evs {
                    self.buf
                        .extend_from_slice(format!("data: {}\n\n", e).as_bytes());
                }
                self.idle = std::time::Instant::now();
                continue;
            }
            if !self
                .tasks
                .get(&self.id)
                .is_some_and(|task| crate::tasks::turn_busy(&task.s("status")))
            {
                let t = *self.ended.get_or_insert_with(std::time::Instant::now);
                if t.elapsed().as_secs() >= 3 {
                    return Ok(0);
                }
            } else {
                self.ended = None;
            }
            if self.idle.elapsed().as_secs() >= 15 {
                self.buf.extend_from_slice(b": keepalive\n\n");
                self.idle = std::time::Instant::now();
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(350));
        }
    }
}

pub(super) fn send_static(req: Request, path: &str) {
    let p = if path.is_empty() || path == "/" {
        "/index.html"
    } else {
        path
    };
    let Some(rel) = safe_rel(p) else {
        send_raw(req, 404, b"not found".to_vec(), "text/plain", &[]);
        return;
    };
    // A "web" folder next to the exe (or METABRAIN_WEB) overrides the embedded panel,
    // so UI updates ship without recompiling.
    let disk_root = std::env::var_os("METABRAIN_WEB")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("web")))
        });
    let disk = disk_root.and_then(|root| {
        let root_canon = root.canonicalize().ok()?;
        let file_canon = root.join(&rel).canonicalize().ok()?;
        (file_canon.starts_with(&root_canon) && file_canon.is_file()).then_some(file_canon)
    });
    if let Some(f) = disk {
        if let Ok(raw) = std::fs::read(&f) {
            send_raw(req, 200, raw, mime(&rel), &[]);
            return;
        }
    }
    match WEB.get_file(&rel) {
        Some(f) => {
            let raw = f.contents().to_vec();
            let ct = mime(&rel);
            send_raw(req, 200, raw, ct, &[]);
        }
        None => send_raw(req, 404, b"not found".to_vec(), "text/plain", &[]),
    }
}
