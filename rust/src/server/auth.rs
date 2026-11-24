//! Session cookie authentication and PIN brute-force rate limiting for LAN clients.
use super::security::{constant_eq, is_local};
use super::COOKIE;
use crate::util::now;
use rand::{rngs::OsRng, RngCore};
use std::collections::HashMap;
use std::sync::Mutex;

const MAX_FAILS: i32 = 5;
const LOCK_SECS: f64 = 300.0;
const MAX_SESSIONS: usize = 64;
const MAX_FAIL_IPS: usize = 1024;

pub(super) fn cookie(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let p = part.trim();
        if let Some((k, v)) = p.split_once('=') {
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

struct AuthInner {
    sessions: HashMap<String, f64>,
    fails: HashMap<String, (i32, f64)>,
}

pub struct Auth {
    inner: Mutex<AuthInner>,
}

impl Auth {
    pub fn new() -> Auth {
        Auth {
            inner: Mutex::new(AuthInner {
                sessions: HashMap::new(),
                fails: HashMap::new(),
            }),
        }
    }

    pub fn ok(&self, ip: &str, cookie_header: &str) -> bool {
        if is_local(ip) {
            return true;
        }
        let Some(tok) = cookie(cookie_header, COOKIE).filter(|t| !t.is_empty()) else {
            return false;
        };
        let Ok(g) = self.inner.lock() else {
            return false;
        };
        g.sessions
            .get(&tok)
            .map(|exp| *exp > now())
            .unwrap_or(false)
    }

    pub fn login(&self, ip: &str, pin: &str, real_pin: &str) -> (Option<String>, String) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return (None, "locked: too many attempts, wait 5 minutes".into()),
        };
        let t_now = now();
        g.sessions.retain(|_, exp| *exp > t_now);
        g.fails.retain(|_, (_, t0)| t_now - *t0 < LOCK_SECS);
        let (mut n, mut t0) = g.fails.get(ip).copied().unwrap_or((0, t_now));
        if n >= MAX_FAILS && t_now - t0 < LOCK_SECS {
            return (None, "locked: too many attempts, wait 5 minutes".into());
        }
        if n >= MAX_FAILS {
            n = 0;
            t0 = t_now;
        }
        if !real_pin.is_empty() && constant_eq(pin, real_pin) {
            g.fails.remove(ip);
            if g.sessions.len() >= MAX_SESSIONS {
                if let Some(oldest) = g
                    .sessions
                    .iter()
                    .min_by(|a, b| a.1.total_cmp(b.1))
                    .map(|(k, _)| k.clone())
                {
                    g.sessions.remove(&oldest);
                }
            }
            let tok = token_hex();
            g.sessions.insert(tok.clone(), t_now + 30.0 * 86400.0);
            return (Some(tok), "ok".into());
        }
        if !g.fails.contains_key(ip) && g.fails.len() >= MAX_FAIL_IPS {
            if let Some(oldest) = g
                .fails
                .iter()
                .min_by(|a, b| (a.1).1.total_cmp(&(b.1).1))
                .map(|(k, _)| k.clone())
            {
                g.fails.remove(&oldest);
            }
        }
        let first = if n > 0 { t0 } else { t_now };
        g.fails.insert(ip.to_string(), (n + 1, first));
        (None, "wrong PIN".into())
    }
}

impl Default for Auth {
    fn default() -> Self {
        Self::new()
    }
}

fn token_hex() -> String {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    let mut s = String::with_capacity(64);
    for b in raw {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
