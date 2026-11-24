//! Pure analysis helpers for the HMI: stuck-agent detector and tool-call timeline.
//!
//! Both functions work on sessions already parsed by `crate::brains` (no disk, no network),
//! so they are cheap enough to run on every poll.

use crate::brains::{Session, ToolCall};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// "Recent" window used by the stuck detector (10 min).
const ACTIVE_WINDOW: f64 = 600.0;
/// An open tool call older than this counts as stuck (20 min).
const LONG_TOOL: f64 = 20.0 * 60.0;
/// Tools that mean real progress on the code base.
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

const MAX_LANES: usize = 12;
const MAX_SPANS: usize = 400;

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// =====================================================================================
// Stuck agent
// =====================================================================================

/// Detects a stuck agent in one session. At most one entry per `kind`, always
/// `{"session": <8 chars>, "reason": <Spanish text>, "kind": <kind>}`.
pub fn stuck(s: &Session, now: f64) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // Only sessions with activity in the last 10 minutes (NaN-safe comparison).
    let age = now - s.c.last;
    if !age.is_finite() || age > ACTIVE_WINDOW {
        return out;
    }
    let sid = short_id(&s.id);
    let recent: Vec<&ToolCall> = s
        .tools
        .iter()
        .filter(|t| t.start > 0.0 && t.start >= now - ACTIVE_WINDOW && t.start <= now)
        .collect();

    // --- errors: run of 3+ consecutive failed calls ---
    let mut run: usize = 0;
    let mut run_head: Option<&ToolCall> = None;
    let mut errors_at: Option<&ToolCall> = None;
    for t in &recent {
        if t.error {
            run += 1;
            if run == 1 {
                run_head = Some(*t);
            }
            if run >= 3 {
                errors_at = run_head;
            }
        } else {
            run = 0;
            run_head = None;
        }
    }
    if let Some(t) = errors_at {
        out.push(json!({
            "session": sid.clone(),
            "reason": format!("3 errores seguidos en {}", t.name),
            "kind": "errors"
        }));
    }

    // --- repeat: same (name, key) 4+ times among the last 12 calls ---
    let mut counts: HashMap<(&str, &str), (usize, f64)> = HashMap::new();
    for t in s.tools.iter().rev().take(12) {
        let e = counts
            .entry((t.name.as_str(), t.key.as_str()))
            .or_insert((0, 0.0));
        e.0 += 1;
        if t.start > e.1 {
            e.1 = t.start;
        }
    }
    let mut best: Option<((&str, &str), usize, f64)> = None;
    for (pair, (n, last)) in counts {
        if n < 4 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, bn, bl)) => n > bn || (n == bn && last > bl),
        };
        if better {
            best = Some((pair, n, last));
        }
    }
    if let Some(((name, key), _, _)) = best {
        out.push(json!({
            "session": sid.clone(),
            "reason": format!("repite {} {}", name, cut(key, 40)),
            "kind": "repeat"
        }));
    }

    // --- no_progress: 25+ calls, no edits, mostly shells ---
    if recent.len() >= 25 {
        let edited = recent.iter().any(|t| EDIT_TOOLS.contains(&t.name.as_str()));
        let cmds = recent
            .iter()
            .filter(|t| matches!(t.name.as_str(), "Bash" | "PowerShell"))
            .count();
        if !edited && cmds * 2 > recent.len() {
            out.push(json!({
                "session": sid.clone(),
                "reason": "25 comandos sin editar nada",
                "kind": "no_progress"
            }));
        }
    }

    // --- long_tool: an open call started more than 20 min ago ---
    let oldest = s
        .tools
        .iter()
        .filter(|t| t.end.is_none() && t.start > 0.0 && now - t.start > LONG_TOOL)
        .min_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    if let Some(t) = oldest {
        let mins = ((now - t.start) / 60.0).floor() as i64;
        out.push(json!({
            "session": sid.clone(),
            "reason": format!("{} lleva {} min sin terminar", t.name, mins),
            "kind": "long_tool"
        }));
    }

    out
}

// =====================================================================================
// Timeline (Gantt data)
// =====================================================================================

struct Lane {
    id: String,
    label: String,
    kind: &'static str,
    session: String,
    last: f64,
    sub: bool,
}

struct Span {
    lane: String,
    name: String,
    key: String,
    start: f64,
    end: f64,
    open: bool,
    error: bool,
}

fn push_lane(lanes: &mut Vec<Lane>, ix: &mut HashMap<String, usize>, lane: Lane) {
    if let Some(&i) = ix.get(&lane.id) {
        if lane.last > lanes[i].last {
            lanes[i].last = lane.last;
        }
        return;
    }
    ix.insert(lane.id.clone(), lanes.len());
    lanes.push(lane);
}

/// Gantt data for the last `window_s` seconds: one lane per active session and per subagent
/// with tool calls in the window, spans clipped to the window, capped at 12 lanes / 400 spans.
pub fn timeline(sessions: &[Session], now: f64, window_s: f64) -> Value {
    let window = if window_s.is_finite() && window_s > 0.0 {
        window_s
    } else {
        1.0
    };
    let from = now - window;
    let mut lanes: Vec<Lane> = Vec::new();
    let mut lane_ix: HashMap<String, usize> = HashMap::new();
    let mut spans: Vec<Span> = Vec::new();

    for s in sessions {
        let sid = s.id.clone();
        let mut local: Vec<Span> = Vec::new();
        // lane id -> subagent id
        let mut subs: HashMap<String, String> = HashMap::new();
        for t in &s.tools {
            if t.start <= 0.0 || t.start > now {
                continue;
            }
            let raw_end = t.end.unwrap_or(now);
            if raw_end < from {
                continue;
            }
            let start = t.start.max(from);
            let end = raw_end.min(now).max(start);
            let lane = if t.lane.is_empty() {
                sid.clone()
            } else {
                let id = format!("{}#{}", sid, t.lane);
                subs.insert(id.clone(), t.lane.clone());
                id
            };
            local.push(Span {
                lane,
                name: t.name.clone(),
                key: t.key.clone(),
                start,
                end,
                open: t.end.is_none(),
                error: t.error,
            });
        }
        let agent_active = s.c.last >= from;
        if local.is_empty() && !agent_active {
            continue;
        }
        let has_agent = local.iter().any(|x| x.lane == sid);
        if has_agent || agent_active {
            let title: String = s.title.trim().chars().take(40).collect();
            let last = local
                .iter()
                .filter(|x| x.lane == sid)
                .map(|x| x.end)
                .fold(s.c.last, f64::max)
                .min(now);
            push_lane(
                &mut lanes,
                &mut lane_ix,
                Lane {
                    id: sid.clone(),
                    label: if title.is_empty() {
                        short_id(&sid)
                    } else {
                        title
                    },
                    kind: "agent",
                    session: sid.clone(),
                    last,
                    sub: false,
                },
            );
        }
        for (lid, sub_id) in &subs {
            let label = match s.subs.get(sub_id) {
                Some(x) if !x.desc.is_empty() => x.desc.clone(),
                Some(x) if !x.typ.is_empty() => x.typ.clone(),
                _ => sub_id.clone(),
            };
            let last = local
                .iter()
                .filter(|x| x.lane == *lid)
                .map(|x| x.end)
                .fold(from, f64::max);
            push_lane(
                &mut lanes,
                &mut lane_ix,
                Lane {
                    id: lid.clone(),
                    label: cut(&label, 40),
                    kind: "subagent",
                    session: sid.clone(),
                    last,
                    sub: true,
                },
            );
        }
        spans.extend(local);
    }

    // Keep the busiest lanes, then their spans.
    if lanes.len() > MAX_LANES {
        lanes.sort_by(|a, b| {
            b.last
                .partial_cmp(&a.last)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        lanes.truncate(MAX_LANES);
        let keep: HashSet<String> = lanes.iter().map(|l| l.id.clone()).collect();
        spans.retain(|sp| keep.contains(&sp.lane));
    }
    // Most recent 400 spans, then chronological for the chart.
    if spans.len() > MAX_SPANS {
        spans.sort_by(|a, b| {
            b.start
                .partial_cmp(&a.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        spans.truncate(MAX_SPANS);
    }
    spans.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.lane.cmp(&b.lane))
    });
    // Agents first, then subagents; newest activity first inside each group.
    lanes.sort_by(|a, b| {
        a.sub.cmp(&b.sub).then_with(|| {
            b.last
                .partial_cmp(&a.last)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    let lane_json: Vec<Value> = lanes
        .iter()
        .map(|l| {
            json!({
                "id": l.id,
                "label": l.label,
                "kind": l.kind,
                "session": short_id(&l.session)
            })
        })
        .collect();
    let span_json: Vec<Value> = spans
        .iter()
        .map(|sp| {
            json!({
                "lane": sp.lane,
                "name": sp.name,
                "key": sp.key,
                "start": sp.start,
                "end": sp.end,
                "open": sp.open,
                "error": sp.error
            })
        })
        .collect();

    json!({"from": from, "to": now, "lanes": lane_json, "spans": span_json})
}

// =====================================================================================
// Tests
// =====================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brains::Common;

    fn t0() -> f64 {
        1_000_000.0
    }

    fn call(
        name: &str,
        key: &str,
        start: f64,
        end: Option<f64>,
        error: bool,
        lane: &str,
    ) -> ToolCall {
        ToolCall {
            id: format!("{}-{}", name, start),
            name: name.to_string(),
            key: key.to_string(),
            start,
            end,
            error,
            lane: lane.to_string(),
        }
    }

    fn session(id: &str, last: f64, tools: Vec<ToolCall>) -> Session {
        Session {
            id: id.to_string(),
            c: Common {
                last,
                ..Default::default()
            },
            tools,
            ..Default::default()
        }
    }

    #[test]
    fn flags_three_failed_calls_in_a_row() {
        let t = t0();
        let s = session(
            "abcdef1234567890",
            t - 20.0,
            vec![
                call("Bash", "ls", t - 120.0, Some(t - 119.0), false, ""),
                call("Bash", "ls", t - 110.0, Some(t - 109.0), true, ""),
                call("Bash", "ls", t - 100.0, Some(t - 99.0), true, ""),
                call("Read", "x.rs", t - 90.0, Some(t - 89.0), true, ""),
            ],
        );
        let out = stuck(&s, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "errors");
        assert_eq!(out[0]["session"], "abcdef12");
        assert_eq!(out[0]["reason"], "3 errores seguidos en Bash");
    }

    #[test]
    fn flags_repeated_call() {
        let t = t0();
        let mut tools = Vec::new();
        for i in 0..4 {
            let start = t - 60.0 + i as f64 * 10.0;
            tools.push(call(
                "Read",
                "src/main.rs",
                start,
                Some(start + 1.0),
                false,
                "",
            ));
        }
        let s = session("repeat-session-1", t - 5.0, tools);
        let out = stuck(&s, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "repeat");
        assert_eq!(out[0]["reason"], "repite Read src/main.rs");
    }

    #[test]
    fn flags_no_progress() {
        let t = t0();
        let tools: Vec<ToolCall> = (0..26)
            .map(|i| {
                let start = t - 300.0 + i as f64;
                call(
                    "Bash",
                    &format!("cmd {i}"),
                    start,
                    Some(start + 0.5),
                    false,
                    "",
                )
            })
            .collect();
        let s = session("bash-loop-session", t - 5.0, tools);
        let out = stuck(&s, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "no_progress");
        assert_eq!(out[0]["reason"], "25 comandos sin editar nada");
    }

    #[test]
    fn flags_long_open_tool_and_skips_idle_sessions() {
        let t = t0();
        let s = session(
            "long-tool-session",
            t - 30.0,
            vec![call("Bash", "npm test", t - 1500.0, None, false, "")],
        );
        let out = stuck(&s, t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "long_tool");
        assert_eq!(out[0]["reason"], "Bash lleva 25 min sin terminar");

        let idle = session(
            "idle-session-xx",
            t - 3600.0,
            vec![call("Bash", "ls", t - 1500.0, None, false, "")],
        );
        assert!(stuck(&idle, t).is_empty());
    }

    #[test]
    fn builds_timeline_with_lanes_and_clipping() {
        let t = t0();
        let s = session(
            "sess-0001-abcdef",
            t - 5.0,
            vec![
                call("Bash", "cargo test", t - 300.0, Some(t - 200.0), false, ""),
                call("Read", "a.rs", t - 100.0, None, false, ""),
                call("Grep", "needle", t - 80.0, Some(t - 79.0), true, "sub1"),
            ],
        );

        let v = timeline(std::slice::from_ref(&s), t, 600.0);
        assert_eq!(v["from"].as_f64(), Some(t - 600.0));
        assert_eq!(v["to"].as_f64(), Some(t));
        let lanes = v["lanes"].as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(lanes, 2);
        assert_eq!(v["lanes"][0]["kind"], "agent");
        assert_eq!(v["lanes"][1]["kind"], "subagent");
        let spans = v["spans"].as_array().cloned().unwrap_or_default();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0]["start"].as_f64(), Some(t - 300.0));
        assert_eq!(spans[1]["open"], true);
        assert_eq!(spans[1]["end"].as_f64(), Some(t));
        assert_eq!(spans[2]["error"], true);

        // Narrow window: the old span is dropped, the running one is clipped to `from`.
        let v = timeline(&[s], t, 100.0);
        let spans = v["spans"].as_array().cloned().unwrap_or_default();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["start"].as_f64(), Some(t - 100.0));
        assert_eq!(spans[0]["name"], "Read");
        assert_eq!(spans[1]["name"], "Grep");
    }
}
