//! HTTP security guards: loopback checks, Host/Origin/CSRF validation, secret redaction, safe paths.
use super::LOCAL;
use serde_json::{json, Value};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

pub(super) static UPLOAD_NONCE: AtomicU64 = AtomicU64::new(1);

pub(super) fn is_local(ip: &str) -> bool {
    if LOCAL.contains(&ip) {
        return true;
    }
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => {
            if v6.is_loopback() {
                return true;
            }
            let seg = v6.segments();
            if seg[0] == 0
                && seg[1] == 0
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0xffff
            {
                std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    seg[6] as u8,
                    (seg[7] >> 8) as u8,
                    seg[7] as u8,
                )
                .is_loopback()
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

fn parse_host_header(raw: &str) -> Option<(String, Option<u16>)> {
    let s = raw.trim();
    if s.is_empty()
        || s.bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'@' | b'\\' | b'?'))
    {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        if addr.is_empty() || addr.parse::<std::net::Ipv6Addr>().is_err() {
            return None;
        }
        if tail.is_empty() {
            return Some((addr.to_ascii_lowercase(), None));
        }
        let port = tail.strip_prefix(':')?.parse::<u16>().ok()?;
        return Some((addr.to_ascii_lowercase(), Some(port)));
    }
    if let Some((host, p_str)) = s.rsplit_once(':') {
        if host.is_empty() || host.contains(':') {
            return None;
        }
        let port = p_str.parse::<u16>().ok()?;
        return Some((host.to_ascii_lowercase(), Some(port)));
    }
    Some((s.to_ascii_lowercase(), None))
}

/// Defends against DNS rebinding: only loopback names/IPs (and raw IP literals when LAN is enabled)
/// are accepted in the `Host` header, never arbitrary attacker-controlled DNS domains.
pub(super) fn host_allowed(
    host_header: Option<&str>,
    listen_port: u16,
    lan_listening: bool,
) -> bool {
    let Some((host, hdr_port)) = host_header.and_then(parse_host_header) else {
        return false;
    };
    if let Some(p) = hdr_port {
        if listen_port != 0 && p != listen_port {
            return false;
        }
    }
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return true;
    }
    if lan_listening {
        return host.parse::<IpAddr>().is_ok();
    }
    false
}

fn extract_url_authority(url: &str) -> Option<&str> {
    let s = url.trim();
    let rest = if s.len() >= 7 && s[..7].eq_ignore_ascii_case("http://") {
        &s[7..]
    } else if s.len() >= 8 && s[..8].eq_ignore_ascii_case("https://") {
        &s[8..]
    } else {
        return None;
    };
    let auth = rest.split(&['/', '?', '#'][..]).next().unwrap_or("");
    if auth.is_empty() || auth.contains('@') || auth.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some(auth)
}

/// Defends against cross-origin CSRF and cross-site reads of `/api/*` and `/proxy/*`.
/// Non-browser local clients (CLI, GUI, MCP, SDKs) omit browser fetch headers and continue working.
pub(super) fn origin_allowed(
    method: &str,
    path: &str,
    host_header: &str,
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    referer: Option<&str>,
) -> bool {
    if method == "OPTIONS" {
        return false;
    }
    let is_mutating = !matches!(method, "GET" | "HEAD");
    let is_sensitive = path.starts_with("/api/") || path.starts_with("/proxy/");

    if let Some(sfs) = sec_fetch_site {
        let sfs = sfs.trim().to_ascii_lowercase();
        if sfs != "same-origin" && sfs != "none" && (is_mutating || is_sensitive) {
            return false;
        }
    }
    if let Some(orig) = origin {
        let orig = orig.trim();
        if orig.eq_ignore_ascii_case("null") {
            return false;
        }
        let Some(auth) = extract_url_authority(orig) else {
            return false;
        };
        if !auth.eq_ignore_ascii_case(host_header.trim()) {
            return false;
        }
    } else if is_mutating && sec_fetch_site.is_none() {
        if let Some(r) = referer {
            let Some(auth) = extract_url_authority(r) else {
                return false;
            };
            if !auth.eq_ignore_ascii_case(host_header.trim()) {
                return false;
            }
        }
    }
    true
}

pub(super) fn redact_remote_secrets(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let kl = k.to_ascii_lowercase();
                let is_env_ref = kl.ends_with("_env");
                let is_sensitive_key = !is_env_ref
                    && (kl == "pin"
                        || kl.contains("secret")
                        || kl.contains("password")
                        || kl.contains("passwd")
                        || kl.contains("token")
                        || kl.contains("api_key")
                        || kl.contains("apikey")
                        || kl.contains("authorization")
                        || kl.contains("bearer")
                        || kl.contains("cookie"));
                if is_sensitive_key && val.is_string() {
                    *val = json!("******");
                } else {
                    redact_remote_secrets(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_remote_secrets(item);
            }
        }
        _ => {}
    }
}

pub(super) fn constant_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff = ab.len() ^ bb.len();
    let n = ab.len().max(bb.len());
    for i in 0..n {
        let x = ab.get(i).copied().unwrap_or(0) as usize;
        let y = bb.get(i).copied().unwrap_or(0) as usize;
        diff |= x ^ y;
    }
    diff == 0
}

pub(super) fn safe_rel(path: &str) -> Option<String> {
    if path.bytes().any(|b| b == 0 || b < 0x20) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.trim_start_matches('/').split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains('\\') {
            return None;
        }
        parts.push(seg);
    }
    if parts.is_empty() {
        parts.push("index.html");
    }
    Some(parts.join("/"))
}

pub(super) fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("archivo");
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "._- ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() {
        "archivo".into()
    } else {
        s.chars().take(120).collect()
    }
}

/// Resolve a safe upload directory. Rejects path traversal (`..`), UNC/network paths,
/// env-var syntax, non-directories, and OS system directories, falling back to MetaBrain's own `adjuntos` folder.
pub(super) fn safe_upload_dir(cwd: &str) -> PathBuf {
    let fallback = crate::util::data_dir().join("adjuntos");
    let trimmed = cwd.trim();
    if trimmed.is_empty()
        || trimmed.starts_with(r"\\")
        || trimmed.starts_with("//")
        || trimmed.contains('%')
        || trimmed.contains('$')
        || trimmed.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return fallback;
    }
    let raw = std::path::Path::new(trimmed);
    if raw
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return fallback;
    }
    let Ok(canon) = raw.canonicalize() else {
        return fallback;
    };
    if !canon.is_dir() {
        return fallback;
    }
    // Reject OS system roots on Windows and Unix so `/api/upload` cannot target system folders.
    #[cfg(windows)]
    {
        let norm_canon = crate::util::norm(&canon);
        for env_key in [
            "SystemRoot",
            "WINDIR",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramData",
        ] {
            if let Ok(sys) = std::env::var(env_key) {
                if let Ok(sys_c) = std::path::Path::new(&sys).canonicalize() {
                    let ns = crate::util::norm(&sys_c);
                    if norm_canon == ns || norm_canon.starts_with(&format!("{ns}\\")) {
                        return fallback;
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let unix_canon = canon.to_string_lossy();
        for blocked in [
            "/etc",
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/boot",
            "/proc",
            "/sys",
            "/dev",
            "/System",
            "/Library",
            "/Applications",
            "/private/etc",
            "/private/var/db",
            "/private/var/log",
            "/private/var/root",
            "/private/var/run",
            "/private/var/spool",
            "/private/var/vm",
        ] {
            if unix_canon == blocked || unix_canon.starts_with(&format!("{blocked}/")) {
                return fallback;
            }
        }
    }
    canon.join(".metabrain").join("adjuntos")
}
