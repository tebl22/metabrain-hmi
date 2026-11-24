//! Metering / control proxy for API brains (port of metabrain/proxy.py).
use crate::brains::Brain;
use crate::core::Supervisor;
use crate::util::{truthy, J};
use rand::Rng;
use serde_json::{json, Map, Value};
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Request, Response, StatusCode};

const HOP: [&str; 12] = [
    "host",
    "content-length",
    "connection",
    "accept-encoding",
    "transfer-encoding",
    "keep-alive",
    "authorization",
    "x-api-key",
    "proxy-authorization",
    "cookie",
    "content-encoding",
    "te",
];

fn is_hop(name: &str) -> bool {
    HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

fn map_effort(e: &str) -> &str {
    match e {
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "max" => "high",
        other => other,
    }
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn add_header<R: Read>(resp: &mut Response<R>, name: &str, value: &str) {
    if let Some(h) = header(name, value) {
        resp.add_header(h);
    }
}

fn num_json(n: f64) -> Value {
    if n.fract() == 0.0 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        (n as i64).to_string()
    } else {
        n.to_string()
    }
}

fn as_i64_val(v: Option<&Value>) -> i64 {
    v.and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn nonzero_i64(v: Option<&Value>) -> Option<i64> {
    as_i64_val(v).ne(&0).then(|| as_i64_val(v))
}

/// Accept the state/effective values whether the core returns Value, &Value, Option<Value> or Option<&Value>.
trait AsState {
    fn as_state(&self) -> Value;
}

impl AsState for Value {
    fn as_state(&self) -> Value {
        self.clone()
    }
}

impl AsState for &Value {
    fn as_state(&self) -> Value {
        (*self).clone()
    }
}

impl AsState for Option<Value> {
    fn as_state(&self) -> Value {
        self.clone().unwrap_or_else(|| json!({}))
    }
}

impl AsState for Option<&Value> {
    fn as_state(&self) -> Value {
        self.cloned().unwrap_or_else(|| json!({}))
    }
}

impl AsState for Map<String, Value> {
    fn as_state(&self) -> Value {
        Value::Object(self.clone())
    }
}

/// Mutate the request body with the brain's effective controls. Returns the list of changes.
pub fn apply_controls(
    brain: &Brain,
    eff: &Value,
    state: &Value,
    body: &mut Map<String, Value>,
    anthropic: bool,
) -> Vec<String> {
    apply_controls_core(&brain.cfg(), eff, state, body, anthropic)
}

fn apply_controls_core(
    cfg: &Value,
    eff: &Value,
    state: &Value,
    body: &mut Map<String, Value>,
    anthropic: bool,
) -> Vec<String> {
    let mut changes = Vec::new();

    if let Some(model) = eff.os("model") {
        if (truthy(state.get("model")) || truthy(eff.get("downgraded")))
            && body.get("model").and_then(|v| v.as_str()) != Some(model.as_str())
        {
            let old = body
                .get("model")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "None".into());
            changes.push(format!("model {old} → {model}"));
            body.insert("model".into(), json!(model));
        }
    }

    if let Some(cap) = eff.f("max_output").filter(|c| *c != 0.0) {
        let k = if anthropic || body.contains_key("max_tokens") {
            "max_tokens"
        } else if body.contains_key("max_completion_tokens") {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let cur = body.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        if cur == 0.0 || cur > cap {
            body.insert(k.to_string(), num_json(cap));
            changes.push(format!("{k} ≤ {}", fmt_num(cap)));
        }
    }

    if anthropic {
        if let Some(th) = eff.f("thinking") {
            if th > 0.0 {
                let mx = body
                    .get("max_tokens")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let budget = if mx != 0.0 { th.min(mx - 1.0) } else { th };
                if budget >= 1024.0 {
                    body.insert(
                        "thinking".into(),
                        json!({"type": "enabled", "budget_tokens": budget as i64}),
                    );
                    changes.push(format!("thinking {}", budget as i64));
                }
            } else {
                body.remove("thinking");
                changes.push("thinking off".into());
            }
        }
    } else {
        if let Some(effort) = eff.os("effort") {
            let reasoning = cfg
                .os("reasoning")
                .unwrap_or_else(|| "reasoning_effort".into());
            if reasoning == "reasoning_effort"
                && (truthy(state.get("effort")) || truthy(eff.get("downgraded")))
            {
                let e = map_effort(&effort);
                body.insert("reasoning_effort".into(), json!(e));
                changes.push(format!("reasoning {e}"));
            }
        }
        if truthy(body.get("stream")) {
            if !body
                .get("stream_options")
                .map(|v| v.is_object())
                .unwrap_or(false)
            {
                body.insert("stream_options".into(), json!({}));
            }
            if let Some(o) = body
                .get_mut("stream_options")
                .and_then(|v| v.as_object_mut())
            {
                o.insert("include_usage".into(), json!(true));
            }
        }
    }

    changes
}

/// Reads usage (and phase changes) out of a JSON body or an SSE stream.
struct Meter {
    b: Arc<Brain>,
    rid: String,
    anth: bool,
    active: bool,
    inp: i64,
    out: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    model: Option<String>,
    fallback_model: Option<String>,
    phase: String,
    done: bool,
}

impl Meter {
    fn new(b: Arc<Brain>, rid: String, anth: bool, model: Option<String>, active: bool) -> Meter {
        Meter {
            b,
            rid,
            anth,
            active,
            inp: 0,
            out: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            model: None,
            fallback_model: model,
            phase: "receive".into(),
            done: false,
        }
    }

    fn set_phase(&mut self, p: &str) {
        if p != self.phase {
            self.phase = p.to_string();
            self.b.phase(&self.rid, p);
        }
    }

    fn usage(&mut self, u: &Value) {
        if !u.is_object() {
            return;
        }
        if self.anth {
            if u.get("input_tokens").is_some() || u.get("cache_read_input_tokens").is_some() {
                if let Some(v) = nonzero_i64(u.get("cache_read_input_tokens")) {
                    self.cache_read = v;
                }
                if let Some(v) = nonzero_i64(u.get("cache_creation_input_tokens")) {
                    self.cache_write = v;
                }
                if u.get("input_tokens").is_some() {
                    self.inp = as_i64_val(u.get("input_tokens")) + self.cache_read;
                }
            }
            if let Some(v) = nonzero_i64(u.get("output_tokens")) {
                self.out = v;
            }
        } else {
            if let Some(v) = nonzero_i64(u.get("prompt_tokens")) {
                self.inp = v;
            }
            if let Some(v) = nonzero_i64(u.get("completion_tokens")) {
                self.out = v;
            }
            let cached = nonzero_i64(u.get("prompt_cache_hit_tokens")).or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| nonzero_i64(Some(v)))
            });
            if let Some(v) = cached {
                self.cache_read = v;
            }
            if let Some(v) = u
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .and_then(|v| nonzero_i64(Some(v)))
            {
                self.reasoning = v;
            }
        }
    }

    fn event(&mut self, d: &Value) {
        if !d.is_object() {
            return;
        }
        if let Some(m) = d
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                d.get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
            })
        {
            self.model = Some(m.to_string());
        }

        if self.anth {
            let t = d.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match t {
                "message_start" => {
                    if let Some(u) = d.get("message").and_then(|m| m.get("usage")) {
                        self.usage(u);
                    }
                }
                "message_delta" => {
                    if let Some(u) = d.get("usage") {
                        self.usage(u);
                    }
                }
                "content_block_delta" => {
                    let dt = d
                        .get("delta")
                        .and_then(|x| x.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    self.set_phase(if dt == "thinking_delta" {
                        "think"
                    } else {
                        "respond"
                    });
                }
                "" | "message" => {
                    if let Some(u) = d.get("usage") {
                        self.usage(u);
                    }
                }
                _ => {}
            }
        } else {
            if let Some(u) = d.get("usage") {
                self.usage(u);
            }
            if let Some(choices) = d.get("choices").and_then(|v| v.as_array()) {
                for ch in choices {
                    if let Some(delta) = ch.get("delta") {
                        if truthy(delta.get("reasoning_content")) || truthy(delta.get("reasoning"))
                        {
                            self.set_phase("think");
                        } else if truthy(delta.get("content")) {
                            self.set_phase("respond");
                        }
                    }
                }
            }
        }
    }

    fn sse_line(&mut self, line: &[u8]) {
        let s = trim_ascii(line);
        if let Some(rest) = s.strip_prefix(b"data:") {
            let data = trim_ascii(rest);
            if !data.is_empty() && data != b"[DONE]" {
                if let Ok(v) = serde_json::from_slice::<Value>(data) {
                    self.event(&v);
                }
            }
        }
    }

    fn finish(&mut self, error: Option<String>) {
        if self.done {
            return;
        }
        self.done = true;
        if self.active {
            let model = self
                .model
                .clone()
                .or_else(|| self.fallback_model.clone())
                .unwrap_or_default();
            self.b.end(
                &self.rid,
                &model,
                self.inp,
                self.out,
                self.cache_read,
                self.cache_write,
                self.reasoning,
                error,
            );
        }
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &b[start..end]
}

const MAX_SSE_LINE: usize = 2 * 1024 * 1024;
const MAX_PROXY_BODY: usize = 64 * 1024 * 1024;

trait SseMeter {
    fn sse_line(&mut self, line: &[u8]);
    fn finish(&mut self, error: Option<String>);
}

impl SseMeter for Meter {
    fn sse_line(&mut self, line: &[u8]) {
        Meter::sse_line(self, line);
    }

    fn finish(&mut self, error: Option<String>) {
        Meter::finish(self, error);
    }
}

struct SseStream<M: SseMeter = Meter> {
    inner: Box<dyn Read + Send>,
    meter: M,
    buf: Vec<u8>,
    pending: Vec<u8>,
    discard_line: bool,
    pos: usize,
    status_err: Option<String>,
    done: bool,
}

impl<M: SseMeter> SseStream<M> {
    fn new(inner: Box<dyn Read + Send>, meter: M, status_err: Option<String>) -> Self {
        Self {
            inner,
            meter,
            buf: Vec::new(),
            pending: Vec::new(),
            discard_line: false,
            pos: 0,
            status_err,
            done: false,
        }
    }

    fn finish(&mut self, error: Option<String>) {
        if !self.done {
            self.meter.finish(error);
            self.done = true;
        }
    }

    fn feed_lines(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() {
            if let Some(ix) = chunk.iter().position(|&c| c == b'\n') {
                if self.discard_line {
                    self.discard_line = false;
                    self.pending.clear();
                } else if self.pending.len().saturating_add(ix + 1) <= MAX_SSE_LINE {
                    self.pending.extend_from_slice(&chunk[..=ix]);
                    let line = std::mem::take(&mut self.pending);
                    self.meter.sse_line(&line);
                } else {
                    self.pending.clear();
                }
                chunk = &chunk[ix + 1..];
            } else {
                if !self.discard_line {
                    if self.pending.len().saturating_add(chunk.len()) <= MAX_SSE_LINE {
                        self.pending.extend_from_slice(chunk);
                    } else {
                        self.pending.clear();
                        self.discard_line = true;
                    }
                }
                break;
            }
        }
    }
}

impl<M: SseMeter> Read for SseStream<M> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.buf.len() {
                let n = (self.buf.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            self.buf.clear();
            self.pos = 0;
            let mut tmp = [0u8; 8192];
            match self.inner.read(&mut tmp) {
                Ok(0) => {
                    if !self.discard_line && !self.pending.is_empty() {
                        let line = std::mem::take(&mut self.pending);
                        self.meter.sse_line(&line);
                    }
                    let err = self.status_err.take();
                    self.finish(err);
                    return Ok(0);
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    self.feed_lines(&tmp[..n]);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.finish(Some(format!("stream error: {e}")));
                    return Err(e);
                }
            }
        }
    }
}

impl<M: SseMeter> Drop for SseStream<M> {
    fn drop(&mut self) {
        if !self.done {
            self.finish(Some("client left (ConnectionResetError)".into()));
        }
    }
}

fn json_error(req: Request, status: u16, msg: &str, anthropic: bool) {
    let body = if anthropic {
        json!({
            "type": "error",
            "error": {
                "type": if status == 429 { "rate_limit_error" } else { "api_error" },
                "message": msg
            }
        })
    } else {
        json!({"error": {"message": msg, "type": "metabrain", "code": status}})
    };
    let raw = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = Response::from_data(raw).with_status_code(StatusCode(status));
    add_header(&mut resp, "Content-Type", "application/json");
    if status == 429 {
        add_header(&mut resp, "Retry-After", "60");
    }
    let _ = req.respond(resp);
}

fn req_header(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn rid_hex() -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(32);
    for _ in 0..32 {
        s.push_str(&format!("{:x}", rng.gen_range(0u8..16)));
    }
    s
}

fn error_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Status(_, _) => "HTTPError",
        ureq::Error::Transport(_) => "URLError",
    }
}

/// Handle one proxied request. `subpath` = what follows `/proxy/<bid>`.
pub fn handle(sup: &Supervisor, brain_id: &str, subpath: &str, mut req: Request) {
    let anthropic = subpath.trim_end_matches('/').ends_with("/messages")
        || sup
            .brain(brain_id)
            .map(|b| b.kind == "anthropic")
            .unwrap_or(false);

    let b = match sup.brain(brain_id) {
        Some(b) => b,
        None => {
            json_error(
                req,
                404,
                &format!("unknown API brain '{brain_id}'"),
                anthropic,
            );
            return;
        }
    };
    let b_arc: Arc<Brain> = Arc::clone(&b);
    let brain: &Brain = &b_arc;

    if !matches!(brain.kind.as_str(), "openai" | "ollama" | "anthropic") {
        json_error(
            req,
            404,
            &format!("unknown API brain '{brain_id}'"),
            anthropic,
        );
        return;
    }

    let eff = sup.effective(brain, None).as_state();
    let st = sup.state.get(brain.id.as_str()).as_state();

    if eff.b("paused", false) {
        let reasons = eff.arr("reasons");
        let why = reasons
            .first()
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| eff.os("paused_by"))
            .unwrap_or_else(|| "by the user".into());
        let name = brain
            .cfg()
            .os("name")
            .unwrap_or_else(|| brain_id.to_string());
        json_error(
            req,
            429,
            &format!("MetaBrain paused '{name}': {why}"),
            anthropic,
        );
        return;
    }

    if subpath.contains('\r') || subpath.contains('\n') || subpath.split('/').any(|s| s == "..") {
        json_error(req, 400, "invalid proxy path", anthropic);
        return;
    }

    let base_url = brain.cfg().s("base_url");
    let base_url = base_url.trim().trim_end_matches('/');
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        json_error(req, 502, "invalid upstream base_url", anthropic);
        return;
    }

    let method = req.method().as_str().to_string();
    let content_len = req_header(&req, "Content-Length").and_then(|s| s.parse::<u64>().ok());
    if let Some(len) = content_len {
        if len > MAX_PROXY_BODY as u64 {
            json_error(req, 413, "request body too large", anthropic);
            return;
        }
    }

    let has_body = content_len.unwrap_or(0) > 0
        || req_header(&req, "Transfer-Encoding").is_some()
        || !matches!(method.as_str(), "GET" | "HEAD" | "DELETE");
    let mut raw = Vec::new();
    if has_body {
        let limit = content_len
            .unwrap_or(MAX_PROXY_BODY as u64)
            .min(MAX_PROXY_BODY as u64);
        if req
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut raw)
            .is_err()
        {
            json_error(req, 400, "failed to read request body", anthropic);
            return;
        }
        if raw.len() > MAX_PROXY_BODY {
            json_error(req, 413, "request body too large", anthropic);
            return;
        }
    }

    let mut body_obj: Option<Map<String, Value>> = None;
    if !raw.is_empty() && method == "POST" {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&raw) {
            body_obj = Some(m);
        }
    }

    if let Some(bm) = body_obj.as_mut() {
        let _ = apply_controls(brain, &eff, &st, bm, anthropic);
        if let Ok(v) = serde_json::to_vec(&Value::Object(bm.clone())) {
            raw = v;
        }
    }

    let model = body_obj
        .as_ref()
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let metered = body_obj.is_some() && method == "POST";

    let mut url = format!("{base_url}{subpath}");
    if let Some((_, q)) = req.url().split_once('?') {
        url.push('?');
        url.push_str(q);
    }

    let mut hdr: Vec<(String, String)> = Vec::new();
    for h in req.headers() {
        let k = h.field.as_str().to_string();
        let v = h.value.as_str().to_string();
        if !is_hop(&k) {
            hdr.push((k, v));
        }
    }

    if let Some(key) = brain.key() {
        if anthropic {
            hdr.push(("x-api-key".into(), key));
            if !hdr
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
            {
                hdr.push(("anthropic-version".into(), "2023-06-01".into()));
            }
        } else {
            hdr.push(("Authorization".into(), format!("Bearer {key}")));
        }
    } else {
        for k in ["Authorization", "x-api-key"] {
            if let Some(v) = req_header(&req, k) {
                hdr.push((k.to_string(), v));
            }
        }
    }

    let app = req_header(&req, "X-MetaBrain-App")
        .or_else(|| req_header(&req, "User-Agent"))
        .unwrap_or_else(|| "app".into())
        .split('/')
        .next()
        .unwrap_or("app")
        .chars()
        .take(40)
        .collect::<String>();

    let rid = rid_hex();
    if metered {
        brain.begin(&rid, &app, model.as_deref().unwrap_or(""));
    }

    let mut meter = Meter::new(
        b_arc.clone(),
        rid.clone(),
        anthropic,
        model.clone(),
        metered,
    );

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(600))
        .redirects(0)
        .build();
    let mut r = agent.request(&method, &url);
    for (k, v) in &hdr {
        r = r.set(k, v);
    }

    let sent = if !matches!(method.as_str(), "GET" | "HEAD") {
        r.send_bytes(&raw)
    } else {
        r.call()
    };
    let resp = match sent {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(e) => {
            let kind = error_kind(&e);
            meter.finish(Some(kind.into()));
            json_error(
                req,
                502,
                &format!("upstream not reachable: {kind}"),
                anthropic,
            );
            return;
        }
    };

    let status = resp.status();
    let ctype = resp.header("Content-Type").unwrap_or("").to_string();
    let err = if status >= 400 {
        Some(format!("HTTP {status}"))
    } else {
        None
    };
    let is_sse = ctype.to_lowercase().contains("text/event-stream");

    if is_sse {
        let mut headers: Vec<Header> = Vec::new();
        if !ctype.is_empty() {
            if let Some(h) = header("Content-Type", &ctype) {
                headers.push(h);
            }
        }
        if let Some(h) = header("Cache-Control", "no-cache") {
            headers.push(h);
        }
        let stream = SseStream::new(resp.into_reader(), meter, err);
        let body: Box<dyn Read + Send> = Box::new(stream);
        let response = Response::new(StatusCode(status), headers, body, None, None);
        let _ = req.respond(response);
        return;
    }

    const MAX_PROXY_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
    let mut data = Vec::new();
    let mut reader = resp.into_reader().take(MAX_PROXY_RESPONSE_BYTES + 1);
    if let Err(e) = reader.read_to_end(&mut data) {
        meter.finish(Some(format!("upstream read error: {e}")));
        json_error(req, 502, &format!("upstream not reachable: {e}"), anthropic);
        return;
    }
    if data.len() as u64 > MAX_PROXY_RESPONSE_BYTES {
        meter.finish(Some("upstream response exceeded 16 MiB".into()));
        json_error(req, 502, "upstream response too large", anthropic);
        return;
    }

    if status < 400 && data.first() == Some(&b'{') {
        if let Ok(v) = serde_json::from_slice::<Value>(&data) {
            meter.event(&v);
        }
    }

    let mut response = Response::from_data(data).with_status_code(StatusCode(status));
    if !ctype.is_empty() {
        add_header(&mut response, "Content-Type", &ctype);
    }
    let result = req.respond(response);
    let err = if result.is_err() {
        Some("client left (BrokenPipeError)".into())
    } else {
        err
    };
    meter.finish(err);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn openai_controls_apply_model_cap_and_effort() {
        let cfg = json!({"reasoning": "reasoning_effort"});
        let eff = json!({"model": "small", "max_output": 100, "effort": "max"});
        let st = json!({"model": "small", "effort": "max"});
        let mut body = obj(json!({"model": "big", "max_tokens": 5000, "stream": true}));
        let ch = apply_controls_core(&cfg, &eff, &st, &mut body, false);
        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("small"));
        assert_eq!(body.get("max_tokens").and_then(|v| v.as_i64()), Some(100));
        assert_eq!(
            body.get("reasoning_effort").and_then(|v| v.as_str()),
            Some("high")
        );
        assert_eq!(
            body.get("stream_options")
                .and_then(|v| v.get("include_usage"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(ch.len() >= 3);
    }

    #[test]
    fn anthropic_thinking_budget_is_capped_by_max_tokens() {
        let cfg = json!({});
        let eff = json!({"thinking": 4000});
        let st = json!({});
        let mut body = obj(json!({"model": "opus", "max_tokens": 2000}));
        let _ = apply_controls_core(&cfg, &eff, &st, &mut body, true);
        let th = body
            .get("thinking")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        assert_eq!(th.get("type").and_then(|v| v.as_str()), Some("enabled"));
        assert_eq!(th.get("budget_tokens").and_then(|v| v.as_i64()), Some(1999));
    }

    #[test]
    fn anthropic_thinking_off_removes_block() {
        let cfg = json!({});
        let eff = json!({"thinking": 0});
        let st = json!({});
        let mut body = obj(json!({"thinking": {"type": "enabled", "budget_tokens": 4000}}));
        let ch = apply_controls_core(&cfg, &eff, &st, &mut body, true);
        assert!(!body.contains_key("thinking"));
        assert!(ch.iter().any(|s| s == "thinking off"));
    }

    #[derive(Clone, Default)]
    struct MockMeter {
        lines: Arc<std::sync::Mutex<Vec<String>>>,
        finished: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    impl SseMeter for MockMeter {
        fn sse_line(&mut self, line: &[u8]) {
            self.lines
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(line).into_owned());
        }

        fn finish(&mut self, error: Option<String>) {
            self.finished.lock().unwrap().push(error);
        }
    }

    struct ChunkedReader {
        chunks: Vec<io::Result<Vec<u8>>>,
        ix: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.ix >= self.chunks.len() {
                return Ok(0);
            }
            let item = std::mem::replace(&mut self.chunks[self.ix], Ok(Vec::new()));
            self.ix += 1;
            match item {
                Ok(data) => {
                    let n = data.len().min(out.len());
                    out[..n].copy_from_slice(&data[..n]);
                    if n < data.len() {
                        self.ix -= 1;
                        self.chunks[self.ix] = Ok(data[n..].to_vec());
                    }
                    Ok(n)
                }
                Err(e) => Err(e),
            }
        }
    }

    #[test]
    fn sse_stream_forwards_exact_bytes_across_small_and_large_buffers() {
        let payload = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet\",\"usage\":{\"input_tokens\":12}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hola\"}}\n\n",
            "data: [DONE]\n"
        );
        for read_buf_size in [1usize, 7, 31, 128, 16384] {
            let meter = MockMeter::default();
            let reader = ChunkedReader {
                chunks: vec![
                    Ok(payload.as_bytes()[..17].to_vec()),
                    Err(io::Error::new(io::ErrorKind::Interrupted, "retry")),
                    Ok(payload.as_bytes()[17..85].to_vec()),
                    Ok(payload.as_bytes()[85..].to_vec()),
                ],
                ix: 0,
            };
            let mut stream = SseStream::new(Box::new(reader), meter.clone(), None);
            let mut out = Vec::new();
            let mut buf = vec![0xAAu8; read_buf_size];
            loop {
                let n = stream.read(&mut buf).expect("sse read");
                assert!(
                    n <= buf.len(),
                    "Read violated contract: {n} > {}",
                    buf.len()
                );
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            assert_eq!(String::from_utf8(out).unwrap(), payload);
            let lines = meter.lines.lock().unwrap();
            assert!(lines
                .iter()
                .any(|l| l.starts_with("data: {\"type\":\"message_start\"")));
            assert!(lines.iter().any(|l| l.starts_with("data: [DONE]")));
            assert_eq!(*meter.finished.lock().unwrap(), vec![None]);
        }
    }

    #[test]
    fn sse_stream_propagates_http_status_error_and_drop_disconnect() {
        let meter = MockMeter::default();
        let reader = io::Cursor::new(b"data: {\"error\":true}\n".to_vec());
        let mut stream = SseStream::new(Box::new(reader), meter.clone(), Some("HTTP 429".into()));
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"data: {\"error\":true}\n");
        assert_eq!(
            *meter.finished.lock().unwrap(),
            vec![Some("HTTP 429".into())]
        );

        let meter_drop = MockMeter::default();
        {
            let reader = io::Cursor::new(b"data: partial\n".to_vec());
            let mut stream = SseStream::new(Box::new(reader), meter_drop.clone(), None);
            let mut small = [0u8; 4];
            let n = stream.read(&mut small).unwrap();
            assert_eq!(&small[..n], b"data");
        }
        assert_eq!(
            *meter_drop.finished.lock().unwrap(),
            vec![Some("client left (ConnectionResetError)".into())]
        );
    }

    #[test]
    fn sse_stream_handles_oversized_line_without_oom_and_preserves_stream() {
        let meter = MockMeter::default();
        let mut raw = vec![b'x'; MAX_SSE_LINE + 128];
        raw.push(b'\n');
        raw.extend_from_slice(b"data: {\"ok\":true}\n");
        let expected_len = raw.len();
        let mut stream = SseStream::new(Box::new(io::Cursor::new(raw)), meter.clone(), None);
        let mut out = Vec::new();
        stream.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), expected_len);
        assert!(out.ends_with(b"data: {\"ok\":true}\n"));
        let lines = meter.lines.lock().unwrap();
        assert_eq!(lines.as_slice(), &["data: {\"ok\":true}\n".to_string()]);
    }
}
