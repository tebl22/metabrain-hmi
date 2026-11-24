//! Brain adapters (port of brains/{base,api,claude_code}.py).
//!
//! * Claude Code brains read the session logs (`<config_dir>/projects/**.jsonl`), subagents and the
//!   real quota. Several brains can share one account: each claims the sessions whose first prompt /
//!   cwd / account matches its "match" rule; a brain without a rule takes the rest.
//! * API brains (OpenAI-compatible, Ollama, Anthropic) are fed by the local proxy (begin/phase/end)
//!   and, optionally, by a usage JSON file written by an app that calls the API directly.
mod api;
mod claude;
mod scanner;
#[cfg(test)]
mod tests;

pub use api::ApiInner;
pub use scanner::{
    line, matches, pretty, scanner_for, Common, Job, ScanInner, Scanner, Session, Spawn, Sub,
    ToolCall, TOOL_KEEP,
};

use crate::config::{Config, EFFORTS};
use crate::store::{Diary, Ledger, Usage};
use crate::util::{expand_s, now, J};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub enum Imp {
    Claude {
        scan: Arc<Scanner>,
        version: Mutex<i64>,
        seen_spawns: Mutex<HashSet<String>>,
    },
    Api(Mutex<ApiInner>),
    OpenCode(crate::opencode::OpenCode),
}

pub struct Brain {
    pub id: String,
    pub kind: String,
    pub config: Arc<Config>,
    pub ledger: Arc<Ledger>,
    pub diary: Arc<Diary>,
    pub imp: Imp,
}

pub fn build(config: &Arc<Config>, ledger: &Arc<Ledger>, diary: &Arc<Diary>) -> Vec<Arc<Brain>> {
    let days = config.key("history_days").as_i64().unwrap_or(30);
    let mut out = vec![];
    for c in config.brains() {
        if !c.b("enabled", true) {
            continue;
        }
        let kind = c.s("kind");
        let imp = match kind.as_str() {
            "claude_code" => Imp::Claude {
                scan: scanner_for(
                    &c.os("config_dir").unwrap_or_else(|| "~/.claude".into()),
                    days,
                ),
                version: Mutex::new(-1),
                seen_spawns: Mutex::new(HashSet::new()),
            },
            "openai" | "ollama" | "anthropic" | "codex" | "gemini_cli" | "antigravity" => {
                let mut inner = ApiInner {
                    balance: Value::Null,
                    ..Default::default()
                };
                if kind == "antigravity" {
                    let hint = c
                        .arr("ladder")
                        .first()
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some((_, limits)) = crate::cli_agents::cached_antigravity_quota(&hint) {
                        inner.quota = limits
                            .into_iter()
                            .filter_map(|q| serde_json::to_value(q).ok())
                            .collect();
                    }
                }
                Imp::Api(Mutex::new(inner))
            }
            "opencode" => Imp::OpenCode(crate::opencode::OpenCode::new()),
            _ => continue,
        };
        out.push(Arc::new(Brain {
            id: c.s("id"),
            kind,
            config: config.clone(),
            ledger: ledger.clone(),
            diary: diary.clone(),
            imp,
        }));
    }
    out
}

impl Brain {
    /// This brain's config (a copy; mutate through `config.with_brain`).
    pub fn cfg(&self) -> Value {
        self.config
            .brain(&self.id)
            .unwrap_or_else(|| json!({"id": self.id, "kind": self.kind}))
    }

    pub fn name(&self) -> String {
        self.cfg().os("name").unwrap_or_else(|| self.id.clone())
    }

    pub fn is_claude(&self) -> bool {
        self.kind == "claude_code"
    }

    pub fn scanner(&self) -> Option<&Arc<Scanner>> {
        match &self.imp {
            Imp::Claude { scan, .. } => Some(scan),
            _ => None,
        }
    }

    pub fn controls(&self) -> Vec<&'static str> {
        match self.kind.as_str() {
            "claude_code" => vec!["model", "effort", "thinking", "pause", "budget"],
            "ollama" => vec!["model", "max_output", "pause"],
            "anthropic" => vec!["model", "thinking", "max_output", "pause", "budget"],
            "opencode" => vec!["budget"],
            "codex" | "gemini_cli" | "antigravity" => vec!["model", "pause"],
            _ => vec!["model", "effort", "max_output", "pause", "budget"],
        }
    }

    /// Visible models: the config ladder if present, else the live-discovered list.
    pub fn models(&self) -> Vec<String> {
        let ladder = self.cfg().arr("ladder");
        let live = match &self.imp {
            Imp::Api(inner) => inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .live_models
                .clone(),
            _ => Vec::new(),
        };
        api::resolve_models(&ladder, &live)
    }

    pub fn efforts(&self) -> Vec<&'static str> {
        EFFORTS.to_vec()
    }

    pub fn say(&self, text: &str, level: &str) {
        self.diary.add(&self.name(), text, level);
    }

    pub(super) fn lobe(ts: f64, now_ts: f64) -> bool {
        ts != 0.0 && now_ts - ts < 25.0
    }

    // ------------------------------------------------------------------ perception
    pub fn poll(&self) {
        match &self.imp {
            Imp::Claude {
                scan,
                version,
                seen_spawns,
            } => self.poll_claude(scan, version, seen_spawns),
            Imp::Api(inner) => self.poll_usage_file(inner),
            Imp::OpenCode(oc) => {
                oc.poll(&self.cfg(), now());
                let vol: HashMap<String, Usage> = oc
                    .usages()
                    .into_iter()
                    .map(|(k, ts, model, v, usd)| {
                        (
                            k,
                            Usage {
                                ts,
                                brain: self.id.clone(),
                                model: Some(model),
                                inp: v[0],
                                out: v[1],
                                cache_read: v[2],
                                cache_write: v[3],
                                usd,
                            },
                        )
                    })
                    .collect();
                self.ledger.set_volatile(&self.id, vol);
            }
        }
    }

    /// Stop a running session right now (only OpenCode exposes this).
    pub fn abort(&self, session: &str) -> Result<String, String> {
        match &self.imp {
            Imp::OpenCode(oc) => oc.abort(&self.cfg(), session),
            _ => Err("solo los cerebros OpenCode se pueden abortar desde el panel".into()),
        }
    }

    pub fn slow_poll(&self) {
        match &self.imp {
            Imp::Claude { scan, .. } => self.slow_poll_claude(scan),
            Imp::OpenCode(_) => {}
            Imp::Api(inner) => self.slow_poll_api(inner),
        }
    }

    pub fn quota_pct(&self) -> Option<f64> {
        match &self.imp {
            Imp::Claude { scan, .. } => scan.quota_pct(),
            Imp::Api(inner) => self.quota_pct_api(inner),
            Imp::OpenCode(_) => None,
        }
    }

    // ------------------------------------------------------------------ snapshot
    /// Everything the HMI needs about this brain right now.
    pub fn snapshot(&self, now_ts: f64) -> Value {
        match &self.imp {
            Imp::Claude { scan, .. } => self.snapshot_claude(scan, now_ts),
            Imp::Api(inner) => self.snapshot_api(inner, now_ts),
            Imp::OpenCode(oc) => oc.snapshot(now_ts),
        }
    }

    pub fn config_dir(&self) -> String {
        expand_s(
            &self
                .cfg()
                .os("config_dir")
                .unwrap_or_else(|| "~/.claude".into()),
        )
    }
}

pub(super) fn tail(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    c[c.len().saturating_sub(n)..].iter().collect()
}
