//! Incremental JSONL scanner and session data model for Claude Code config directories.
use crate::util::{expand, http_json, iso_ts, norm, now, read_json, J};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const DS_START: [&str; 2] = ["start_", "delegate_"];

#[derive(Clone, Default, Debug)]
pub struct Common {
    pub start: Option<f64>,
    pub last: f64,
    pub model: String,
    pub status: String,
}

#[derive(Clone, Default, Debug)]
pub struct Sub {
    pub id: String,
    pub desc: String,
    pub typ: String,
    pub tok: i64,
    pub done: bool,
    pub tool: Option<String>,
    pub c: Common,
}

impl Sub {
    pub(super) fn alive(&self, session_path: &Path, now_ts: f64) -> bool {
        if self.done || now_ts - self.c.last >= 180.0 {
            return false;
        }
        let transcript = session_path
            .with_extension("")
            .join("subagents")
            .join(format!("agent-{}.jsonl", self.id));
        if !transcript.is_file() {
            return false;
        }
        // Claude can mark the subagent finished without appending to the transcript.
        !read_json(&transcript.with_extension("meta.json"))
            .is_some_and(|meta| crate::util::truthy(meta.get("finished")))
    }
}

#[derive(Clone, Debug)]
pub struct Spawn {
    pub desc: String,
    pub typ: String,
    pub model: String,
    pub ts: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub server: String,
    pub tool: String,
    pub model: String,
    pub task: String,
    pub ts: Option<f64>,
    pub end: Option<f64>,
    pub background: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub project: String,
    pub cwd: String,
    pub prompt: String,
    pub title: String,
    pub effort: String,
    pub c: Common,
    /// key -> (ts, model, [input, output, cache_read, cache_write])
    pub usages: HashMap<String, (f64, String, [i64; 4])>,
    pub cost: f64,
    pub subs: HashMap<String, Sub>,
    pub spawns: HashMap<String, Spawn>,
    pub jobs: HashMap<String, Job>,
    /// plan / act / delegate
    pub lobes: [f64; 3],
    pub account: Option<String>,
    /// Recent tool calls (main agent and subagents), newest last, capped at TOOL_KEEP.
    pub tools: Vec<ToolCall>,
}

/// One tool call, for the timeline and the stuck detector.
#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Short, stable summary of the input (command / file / pattern), max 160 chars.
    pub key: String,
    pub start: f64,
    pub end: Option<f64>,
    pub error: bool,
    /// "" for the main agent, else the subagent id.
    pub lane: String,
}

pub const TOOL_KEEP: usize = 300;

fn tool_key(name: &str, inp: &Value) -> String {
    let k = [
        "command",
        "file_path",
        "notebook_path",
        "pattern",
        "url",
        "query",
        "description",
        "path",
    ]
    .iter()
    .find_map(|f| inp.get(*f).and_then(|v| v.as_str()).map(|s| s.to_string()))
    .unwrap_or_default();
    let k = if k.is_empty() { name.to_string() } else { k };
    k.chars().take(160).collect()
}

impl Session {
    pub fn tokens(&self) -> i64 {
        self.usages
            .values()
            .map(|(_, _, v)| v[0] + v[1] + v[3])
            .sum()
    }

    /// The fields the match rules look at, as JSON.
    pub fn match_view(&self) -> Value {
        json!({"prompt": self.prompt, "cwd": self.cwd, "account": self.account})
    }
}

struct FileState {
    off: u64,
    rest: Vec<u8>,
}

pub struct ScanInner {
    files: HashMap<PathBuf, FileState>,
    pub sessions: HashMap<String, Session>,
    pub version: u64,
    pub ready: bool,
    last_scan: f64,
}

pub struct Scanner {
    pub dir: PathBuf,
    pub days: f64,
    pub inner: Mutex<ScanInner>,
    /// {"limits": [...], "ts", "warning", "email", "plan", "account"}
    pub quota: Mutex<Value>,
}

static SCANNERS: OnceLock<Mutex<HashMap<String, Arc<Scanner>>>> = OnceLock::new();

/// One scanner per config dir (expanded, case-insensitive on Windows).
pub fn scanner_for(config_dir: &str, days: i64) -> Arc<Scanner> {
    let d = expand(config_dir);
    let key = norm(&d);
    let m = SCANNERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
    m.entry(key)
        .or_insert_with(|| Arc::new(Scanner::new(d, days)))
        .clone()
}

pub(super) fn mtime(p: &Path) -> Option<f64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}

impl Scanner {
    pub fn new(dir: PathBuf, days: i64) -> Scanner {
        Scanner {
            dir,
            days: days as f64,
            inner: Mutex::new(ScanInner {
                files: HashMap::new(),
                sessions: HashMap::new(),
                version: 0,
                ready: false,
                last_scan: 0.0,
            }),
            quota: Mutex::new(json!({
                "limits": [],
                "ts": null,
                "warning": "",
                "email": "",
                "plan": "",
                "account": ""
            })),
        }
    }

    fn paths(&self) -> Vec<PathBuf> {
        let root = self.dir.join("projects");
        let lim = now() - self.days * 86400.0;
        let mut out: Vec<(f64, PathBuf)> = vec![];
        for pat in ["*/*.jsonl", "*/*/subagents/agent-*.jsonl"] {
            let full = format!("{}/{}", glob::Pattern::escape(&root.to_string_lossy()), pat);
            if let Ok(g) = glob::glob(&full) {
                for f in g.flatten() {
                    if let Some(mt) = mtime(&f) {
                        if mt >= lim {
                            out.push((mt, f));
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        out.into_iter().map(|x| x.1).collect()
    }

    pub fn scan(&self, min_interval: f64) {
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if now() - i.last_scan < min_interval {
                return;
            }
            i.last_scan = now();
        }
        let mut changed = false;
        for f in self.paths() {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            changed |= read_file(&mut i, &f);
        }
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if changed {
            i.version += 1;
        }
        i.ready = true;
    }

    pub fn version(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).version
    }

    pub fn ready(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).ready
    }

    /// Copy of every session (cheap enough: called a few times per poll).
    pub fn sessions(&self) -> Vec<Session> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .values()
            .cloned()
            .collect()
    }

    // ---------- quota (real numbers from Anthropic for this account) ----------
    pub fn read_quota(&self) {
        let cred = self.dir.join(".credentials.json");
        let is_main = self
            .dir
            .file_name()
            .map(|n| n == ".claude")
            .unwrap_or(false);
        let acc_file = if is_main {
            self.dir
                .parent()
                .map(|p| p.join(".claude.json"))
                .unwrap_or_else(|| self.dir.join(".claude.json"))
        } else {
            self.dir.join(".claude.json")
        };
        let acc = read_json(&acc_file).unwrap_or(json!({})).o("oauthAccount");
        {
            let mut q = self.quota.lock().unwrap_or_else(|e| e.into_inner());
            q["email"] = json!(acc.s("emailAddress"));
            q["plan"] = json!(acc.s("organizationType"));
            q["account"] = json!(acc.s("accountUuid"));
        }
        let o = read_json(&cred).unwrap_or(json!({})).o("claudeAiOauth");
        let Some(tok) = o.os("accessToken") else {
            self.quota.lock().unwrap_or_else(|e| e.into_inner())["warning"] =
                json!("not signed in");
            return;
        };
        if let Some(exp) = o.f("expiresAt").filter(|x| *x != 0.0) {
            if exp / 1000.0 < now() {
                self.quota.lock().unwrap_or_else(|e| e.into_inner())["warning"] =
                    json!("session expired: open Claude Code with this account to refresh");
                return;
            }
        }
        let auth = format!("Bearer {tok}");
        let r = match http_json(
            "https://api.anthropic.com/api/oauth/usage",
            &[
                ("Authorization", &auth),
                ("anthropic-beta", "oauth-2025-04-20"),
                ("User-Agent", "metabrain-hmi"),
            ],
            None,
            15,
        ) {
            Ok(r) => r,
            Err(e) => {
                let name = if e.starts_with("HTTPError") {
                    "HTTPError"
                } else {
                    "URLError"
                };
                self.quota.lock().unwrap_or_else(|e| e.into_inner())["warning"] =
                    json!(format!("quota not available ({name})"));
                return;
            }
        };
        let mut lims: Vec<Value> = r
            .arr("limits")
            .iter()
            .map(|x| {
                json!({
                    "kind": x.get("kind"),
                    "group": x.get("group"),
                    "pct": x.get("percent"),
                    "reset": iso_ts(x.get("resets_at").unwrap_or(&Value::Null))
                })
            })
            .collect();
        if lims.is_empty() {
            for (k, g) in [("five_hour", "session"), ("seven_day", "weekly")] {
                if let Some(v) = r.get(k).filter(|v| crate::util::truthy(Some(v))) {
                    lims.push(json!({
                        "kind": g,
                        "group": g,
                        "pct": v.get("utilization"),
                        "reset": iso_ts(v.get("resets_at").unwrap_or(&Value::Null))
                    }));
                }
            }
        }
        let mut q = self.quota.lock().unwrap_or_else(|e| e.into_inner());
        q["limits"] = Value::Array(lims);
        q["ts"] = json!(now());
        q["warning"] = json!("");
    }

    /// Highest of session (5 h) / weekly usage.
    pub fn quota_pct(&self) -> Option<f64> {
        let q = self.quota.lock().unwrap_or_else(|e| e.into_inner());
        q.arr("limits")
            .iter()
            .filter(|l| matches!(l.s("group").as_str(), "session" | "weekly"))
            .filter_map(|l| l.f("pct"))
            .fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))
    }
}

fn base_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn session_entry<'a>(i: &'a mut ScanInner, sid: &str, path: &Path) -> &'a mut Session {
    i.sessions
        .entry(sid.to_string())
        .or_insert_with(|| Session {
            id: sid.to_string(),
            path: path.to_path_buf(),
            project: path.parent().map(base_name).unwrap_or_default(),
            ..Default::default()
        })
}

fn read_file(i: &mut ScanInner, f: &Path) -> bool {
    let Ok(md) = std::fs::metadata(f) else {
        return false;
    };
    let size = md.len();
    let st = i.files.entry(f.to_path_buf()).or_insert(FileState {
        off: 0,
        rest: vec![],
    });
    if size < st.off {
        st.off = 0;
        st.rest.clear();
    }
    if size == st.off {
        return false;
    }
    let mut fh = match std::fs::File::open(f) {
        Ok(fh) => fh,
        Err(_) => return false,
    };
    if fh.seek(SeekFrom::Start(st.off)).is_err() {
        return false;
    }
    let mut chunk = Vec::new();
    let Ok(read_bytes) = fh.read_to_end(&mut chunk) else {
        return false;
    };
    if read_bytes == 0 {
        return false;
    }
    st.off = st.off.saturating_add(read_bytes as u64);
    let mut data = std::mem::take(&mut st.rest);
    data.extend_from_slice(&chunk);
    let mut lines: Vec<&[u8]> = data.split(|c| *c == b'\n').collect();
    let rest = lines.pop().unwrap_or(&[]).to_vec();
    st.rest = rest;
    let parent = f.parent().unwrap_or(Path::new(""));
    let is_sub = base_name(parent) == "subagents";
    let (sid, sub_id) = if is_sub {
        let sess_dir = parent.parent().unwrap_or(Path::new(""));
        let sid = base_name(sess_dir);
        let spath = PathBuf::from(format!("{}.jsonl", sess_dir.display()));
        let fname = base_name(f);
        let aid = fname
            .get(6..fname.len().saturating_sub(6))
            .unwrap_or("")
            .to_string();
        let meta_path = PathBuf::from(format!(
            "{}.meta.json",
            &f.to_string_lossy()[..f.to_string_lossy().len() - 6]
        ));
        let meta = read_json(&meta_path).unwrap_or(json!({}));
        let ses = session_entry(i, &sid, &spath);
        let sub = ses.subs.entry(aid.clone()).or_insert_with(|| Sub {
            id: aid.clone(),
            ..Default::default()
        });
        if let Some(d) = meta.get("description").and_then(|v| v.as_str()) {
            sub.desc = d.to_string();
        }
        if let Some(t) = meta.get("agentType").and_then(|v| v.as_str()) {
            sub.typ = t.to_string();
        }
        sub.done = crate::util::truthy(meta.get("finished"));
        sub.tool = meta
            .get("toolUseId")
            .and_then(|v| v.as_str())
            .map(String::from);
        if sub.c.model.is_empty() {
            sub.c.model = meta.s("pickedEffortModel");
        }
        (sid, Some(aid))
    } else {
        let fname = base_name(f);
        let sid = fname[..fname.len().saturating_sub(6)].to_string();
        session_entry(i, &sid, f);
        (sid, None)
    };
    let ses = i.sessions.get_mut(&sid).unwrap();
    for raw in lines {
        if raw.is_empty()
            || raw.starts_with(b"{\"type\":\"api-request")
            || raw.starts_with(b"{\"type\": \"api-request")
        {
            continue;
        }
        if let Ok(d) = serde_json::from_slice::<Value>(raw) {
            line(&d, ses, sub_id.as_deref());
        }
    }
    true
}

fn obj<'a>(ses: &'a mut Session, sub: Option<&str>) -> &'a mut Common {
    match sub.and_then(|a| ses.subs.get_mut(a)) {
        Some(s) => &mut s.c,
        None => &mut ses.c,
    }
}

fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(x) => x.to_string(),
    }
}

/// One JSONL line (Scanner._line).
pub fn line(d: &Value, ses: &mut Session, sub: Option<&str>) {
    if !d.is_object() {
        return;
    }
    let t = d.s("type");
    let ts = iso_ts(d.get("timestamp").unwrap_or(&Value::Null));
    if let Some(ts) = ts {
        let o = obj(ses, sub);
        o.last = o.last.max(ts);
        if o.start.is_none() {
            o.start = Some(ts);
        }
    }
    if let Some(cwd) = d.os("cwd") {
        if ses.cwd.is_empty() {
            ses.cwd = cwd;
        }
    }
    match t.as_str() {
        "session-owner" => {
            ses.account = d
                .get("accountUuid")
                .and_then(|v| v.as_str())
                .map(String::from);
            return;
        }
        "cost-state" if sub.is_none() => {
            if let Some(c) = d.f("totalCostUSD").filter(|c| *c != 0.0) {
                ses.cost = c;
            }
            return;
        }
        "queue-operation" if crate::util::truthy(d.get("content")) && ses.prompt.is_empty() => {
            if let Some(c) = d.get("content").and_then(|c| c.as_str()) {
                set_prompt(ses, c);
            }
            return;
        }
        "user" => {
            let c = d
                .o("message")
                .get("content")
                .cloned()
                .unwrap_or(Value::Null);
            let meta = crate::util::truthy(d.get("isMeta"));
            match c {
                Value::String(s) if sub.is_none() && !meta => set_prompt(ses, &s),
                Value::Array(items) => {
                    for b in items {
                        if !b.is_object() {
                            return; // Python: b.get() raised -> the rest of the line is skipped
                        }
                        let bt = b.s("type");
                        if bt == "text" && sub.is_none() && !meta {
                            set_prompt(ses, &b.s("text"));
                        } else if bt == "tool_result" {
                            let tid = b.s("tool_use_id");
                            let err = crate::util::truthy(b.get("is_error"));
                            if let Some(t) = ses.tools.iter_mut().rev().find(|t| t.id == tid) {
                                t.end = ts.or(Some(t.start));
                                t.error = err;
                            }
                            if let Some(j) = ses.jobs.get_mut(&tid) {
                                if !j.background {
                                    j.end = ts;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        "assistant" => {}
        _ => return,
    }
    let m = d.o("message");
    if let Some(model) = m.os("model").filter(|x| x != "<synthetic>") {
        obj(ses, sub).model = model;
    }
    if sub.is_none() {
        if let Some(e) = d.os("effort") {
            ses.effort = e;
        }
    }
    let u = m.o("usage");
    let key = format!("{}|{}", py_str(m.get("id")), py_str(d.get("requestId")));
    if let (false, Some(ts)) = (u.as_object().map(|o| o.is_empty()).unwrap_or(true), ts) {
        let g = |k: &str| u.f(k).unwrap_or(0.0) as i64;
        let v = [
            g("input_tokens"),
            g("output_tokens"),
            g("cache_read_input_tokens"),
            g("cache_creation_input_tokens"),
        ];
        // Streaming emits multiple assistant chunks with the same message.id; keep the
        // latest/highest usage snapshot instead of freezing on the first chunk.
        let prev_tok = ses
            .usages
            .get(&key)
            .map(|(_, _, old)| old[0] + old[1] + old[3])
            .unwrap_or(0);
        let new_tok = v[0] + v[1] + v[3];
        if !ses.usages.contains_key(&key) || new_tok >= prev_tok {
            let model = obj(ses, sub).model.clone();
            ses.usages.insert(key, (ts, model, v));
            if let Some(a) = sub {
                if let Some(s) = ses.subs.get_mut(a) {
                    s.tok += (new_tok - prev_tok).max(0);
                }
            }
        }
    }
    let tsz = ts.unwrap_or(0.0);
    for b in m.arr("content") {
        let bt = b.s("type");
        if matches!(bt.as_str(), "thinking" | "text" | "redacted_thinking") {
            if sub.is_none() {
                ses.lobes[0] = tsz;
            }
            obj(ses, sub).status = "thinking".into();
        } else if bt == "tool_use" {
            let name = b.s("name");
            let inp = b.o("input");
            let bid = b.s("id");
            ses.tools.push(ToolCall {
                id: bid.clone(),
                name: name.clone(),
                key: tool_key(&name, &inp),
                start: tsz,
                end: None,
                error: false,
                lane: sub.map(|x| x.to_string()).unwrap_or_default(),
            });
            if ses.tools.len() > TOOL_KEEP {
                let cut = ses.tools.len() - TOOL_KEEP;
                ses.tools.drain(..cut);
            }
            if (name == "Agent" || name == "Task") && sub.is_none() {
                let typ = inp.os("subagent_type").unwrap_or_else(|| "general".into());
                ses.spawns.insert(
                    bid,
                    Spawn {
                        desc: inp.s("description"),
                        typ,
                        model: inp.s("model"),
                        ts,
                    },
                );
                ses.lobes[2] = tsz;
                obj(ses, sub).status = "spawning a subagent".into();
            } else if let (true, None) = (name.starts_with("mcp__"), sub) {
                let rest = &name[5..];
                let (server, tool) = match rest.find("__") {
                    Some(p) => (rest[..p].to_string(), rest[p + 2..].to_string()),
                    None => (rest.to_string(), String::new()),
                };
                if DS_START.iter().any(|p| tool.starts_with(p)) {
                    let task = ["task", "prompt", "instructions", "message"]
                        .iter()
                        .find_map(|k| inp.get(*k).filter(|v| crate::util::truthy(Some(v))))
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            x => x.to_string(),
                        })
                        .unwrap_or_default();
                    ses.jobs.insert(
                        bid,
                        Job {
                            server: server.clone(),
                            tool: tool.clone(),
                            model: inp.s("model"),
                            task: task.chars().take(160).collect(),
                            ts,
                            end: None,
                            background: tool.starts_with("start_"),
                        },
                    );
                    ses.lobes[2] = tsz;
                    obj(ses, sub).status = format!("delegating to {server}");
                } else if tool.contains("result") || tool.starts_with("cancel") {
                    for j in ses.jobs.values_mut() {
                        if j.server == server && j.background && j.end.is_none() {
                            j.end = ts;
                        }
                    }
                    ses.lobes[2] = tsz;
                } else {
                    ses.lobes[1] = tsz;
                    obj(ses, sub).status =
                        format!("using {}", if tool.is_empty() { &server } else { &tool });
                }
            } else {
                if sub.is_none() {
                    ses.lobes[1] = tsz;
                }
                obj(ses, sub).status = format!("using {name}");
            }
        }
    }
}

fn set_prompt(ses: &mut Session, text: &str) {
    static TICKET: OnceLock<regex::Regex> = OnceLock::new();
    static WHITESPACE: OnceLock<regex::Regex> = OnceLock::new();
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') || !ses.prompt.is_empty() {
        return;
    }
    ses.prompt = text.chars().take(2000).collect();
    let re = TICKET.get_or_init(|| regex::Regex::new(r"=== TICKET \(([^)]+)\)").unwrap());
    ses.title = match re.captures(text) {
        Some(c) => format!("Ticket: {}", c[1].replace(".md", "").replace('_', " ")),
        None => WHITESPACE
            .get_or_init(|| regex::Regex::new(r"\s+").unwrap())
            .replace_all(text, " ")
            .chars()
            .take(90)
            .collect(),
    };
}

/// Does a session (prompt/cwd/account view) match a brain's "match" rule?
pub fn matches(rule: &Value, ses: &Value) -> bool {
    let re = |pat: &str, txt: &str| -> Option<bool> {
        regex::RegexBuilder::new(pat)
            .case_insensitive(true)
            .build()
            .ok()
            .map(|r| r.is_match(txt))
    };
    if let Some(p) = rule.os("prompt") {
        match re(&p, &ses.s("prompt")) {
            Some(true) => {}
            _ => return false,
        }
    }
    if let Some(p) = rule.os("cwd") {
        match re(&p, &ses.s("cwd")) {
            Some(true) => {}
            _ => return false,
        }
    }
    if let Some(a) = rule.os("account") {
        if Some(a) != ses.os("account") {
            return false;
        }
    }
    true
}

pub fn pretty(s: &Session) -> String {
    if !s.cwd.is_empty() {
        let t = s.cwd.trim_end_matches(['\\', '/']);
        let b = t.rsplit(['\\', '/']).next().unwrap_or("");
        return if b.is_empty() {
            s.cwd.clone()
        } else {
            b.to_string()
        };
    }
    if s.project.contains("scratch-workspaces") {
        return "Claude app".into();
    }
    static PROJECT_PREFIX: OnceLock<regex::Regex> = OnceLock::new();
    PROJECT_PREFIX
        .get_or_init(|| regex::Regex::new(r"^[A-Za-z]--Users-[^-]+-").unwrap())
        .replace(&s.project, "")
        .to_string()
}
