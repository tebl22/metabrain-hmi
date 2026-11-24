//! Work architecture: from the brains you have connected and what is available right now,
//! decide who directs, who produces and who carries the load (the cheapest API, e.g.
//! DeepSeek), and optionally re-organise the command hierarchy by itself when that changes
//! (both Claude accounts out of quota -> ChatGPT/Gemini lead, DeepSeek does the bulk work).

use crate::util::J;
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Director,
    Producer,
    Workhorse,
    Reserve,
}

impl Role {
    pub fn es(self) -> &'static str {
        match self {
            Role::Director => "dirige",
            Role::Producer => "produce",
            Role::Workhorse => "burro de carga",
            Role::Reserve => "reserva",
        }
    }
}

/// Explicit capability tier of a brain:
/// - Tier 1: frontier / commanding accounts (Claude Code, Codex/ChatGPT, Anthropic, OpenAI official)
/// - Tier 2: strong secondary / parallel producers (Gemini via Antigravity/CLI/API)
/// - Tier 3: cheap workhorse / high-throughput APIs & local models (DeepSeek, Groq, Ollama, etc.)
pub fn tier_of(b: &Value) -> u8 {
    let kind = b.s("kind").to_lowercase();
    let url = b.s("base_url").to_lowercase();
    let provider = b.s("provider").to_lowercase();
    match kind.as_str() {
        "claude_code" | "codex" | "anthropic" => 1,
        "openai" if url.contains("openai.com") => 1,
        "antigravity" | "gemini_cli" => 2,
        "openai" if url.contains("generativelanguage") || provider == "gemini" => 2,
        _ => 3,
    }
}

/// How good a brain is at leading work within its tier (higher = preferred first).
fn lead_score(b: &Value) -> i32 {
    let kind = b.s("kind").to_lowercase();
    let url = b.s("base_url").to_lowercase();
    match kind.as_str() {
        "claude_code" => 100,
        "codex" => 95,
        "antigravity" | "gemini_cli" => 80,
        "anthropic" => 75,
        "openai" if url.contains("openai.com") => 70,
        "openai" if url.contains("generativelanguage") => 60,
        "openai" if url.contains("deepseek") => 40,
        "openai" => 35,
        "ollama" => 10,
        _ => 20,
    }
}

/// How cheap a brain is for bulk work (higher = better workhorse).
fn cheap_score(b: &Value) -> i32 {
    let url = b.s("base_url").to_lowercase();
    match b.s("kind").to_lowercase().as_str() {
        "openai" if url.contains("deepseek") => 100,
        "openai" if url.contains("generativelanguage") => 85,
        "openai" if url.contains("groq") || url.contains("openrouter") => 70,
        "ollama" => 60,
        "openai" => 40,
        _ => 0,
    }
}

/// Recommendation for the current moment.
/// Groups available brains into Tier 1 (shared command / mesa redonda), Tier 2 (submanagers),
/// Tier 3 (workhorses), and unavailable brains in reserve.
/// Returns {levels: [[id]], roles: {id: role}, summary, lines: [..]}.
pub fn recommend(cfg: &Value, chain: &[Value]) -> Value {
    let brains: Vec<Value> = cfg
        .arr("brains")
        .into_iter()
        .filter(|b| b.b("enabled", true))
        .collect();
    let avail = |id: &str| {
        chain
            .iter()
            .find(|e| e.s("id") == id)
            .map(|e| e.b("available", false))
            .unwrap_or(false)
    };
    let name = |id: &str| {
        brains
            .iter()
            .find(|b| b.s("id") == id)
            .map(|b| b.s("name"))
            .unwrap_or_else(|| id.to_string())
    };

    let mut t1: Vec<&Value> = brains
        .iter()
        .filter(|b| avail(&b.s("id")) && tier_of(b) == 1)
        .collect();
    let mut t2: Vec<&Value> = brains
        .iter()
        .filter(|b| avail(&b.s("id")) && tier_of(b) == 2)
        .collect();
    let mut t3: Vec<&Value> = brains
        .iter()
        .filter(|b| avail(&b.s("id")) && tier_of(b) == 3)
        .collect();

    t1.sort_by_key(|b| -lead_score(b));
    t2.sort_by_key(|b| -lead_score(b));
    t3.sort_by_key(|b| -cheap_score(b));

    let mut levels: Vec<Vec<String>> = vec![];
    let mut roles = Map::new();

    if !t1.is_empty() {
        let ids: Vec<String> = t1.iter().map(|b| b.s("id")).collect();
        for id in &ids {
            roles.insert(id.clone(), json!("director"));
        }
        levels.push(ids);
    }
    if !t2.is_empty() {
        let ids: Vec<String> = t2.iter().map(|b| b.s("id")).collect();
        let role = if levels.is_empty() {
            "director"
        } else {
            "submanager"
        };
        for id in &ids {
            roles.insert(id.clone(), json!(role));
        }
        levels.push(ids);
    }
    if !t3.is_empty() {
        let ids: Vec<String> = t3.iter().map(|b| b.s("id")).collect();
        let role = if levels.is_empty() {
            "director"
        } else {
            "workhorse"
        };
        for id in &ids {
            roles.insert(id.clone(), json!(role));
        }
        levels.push(ids);
    }

    // Unavailable brains stay at the bottom, grouped in reserve, in their current order.
    let mut reserve: Vec<String> = vec![];
    for e in chain {
        let id = e.s("id");
        if !levels.iter().flatten().any(|x| *x == id) && brains.iter().any(|b| b.s("id") == id) {
            reserve.push(id.clone());
            roles.insert(id, json!("reserve"));
        }
    }
    for b in &brains {
        let id = b.s("id");
        if !levels.iter().flatten().any(|x| *x == id) && !reserve.contains(&id) {
            reserve.push(id.clone());
            roles.insert(id, json!("reserve"));
        }
    }
    if !reserve.is_empty() {
        levels.push(reserve);
    }

    let join_names = |list: &[&Value]| {
        list.iter()
            .map(|b| name(&b.s("id")))
            .collect::<Vec<_>>()
            .join(" + ")
    };

    let mut lines = vec![];
    let directors: Vec<&Value> = if !t1.is_empty() {
        t1.clone()
    } else if !t2.is_empty() {
        t2.clone()
    } else {
        t3.clone()
    };
    if !directors.is_empty() {
        let verb = if directors.len() > 1 {
            "dirigen en paralelo"
        } else {
            "dirige"
        };
        lines.push(format!(
            "{} {verb}: planifican, arbitran y cierran.",
            join_names(&directors)
        ));
    }
    if !t1.is_empty() && !t2.is_empty() {
        lines.push(format!(
            "{} (Nivel 2): produce y analiza en paralelo sin gastar cuota de Nivel 1.",
            join_names(&t2)
        ));
    }
    if (!t1.is_empty() || !t2.is_empty()) && !t3.is_empty() {
        lines.push(format!("{} (Nivel 3): burro de carga en paralelo para exploración, rutina y borradores baratos.", join_names(&t3)));
    }
    let out: Vec<String> = chain
        .iter()
        .filter(|e| !e.b("available", false))
        .map(|e| format!("{} ({})", e.s("name"), e.s("reason")))
        .collect();
    if !out.is_empty() {
        lines.push(format!("En reserva hasta que vuelvan: {}.", out.join(", ")));
    }
    if directors.is_empty() {
        lines.push("Ningún cerebro está disponible ahora mismo.".into());
    }

    let summary = if directors.is_empty() {
        "Sin cerebros disponibles".into()
    } else if !t3.is_empty() && (!t1.is_empty() || !t2.is_empty()) {
        format!(
            "{} dirigen · {} carga",
            join_names(&directors),
            join_names(&t3)
        )
    } else {
        format!("{} dirige", join_names(&directors))
    };

    json!({
        "levels": levels,
        "roles": roles,
        "summary": summary,
        "lines": lines,
        "available": chain.iter().filter(|e| e.b("available", false)).map(|e| e.s("id")).collect::<Vec<_>>()
    })
}

/// Group chain entries by consecutive equal rank (matching how Supervisor::chain assigns levels).
fn chain_levels(chain: &[Value]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![];
    let mut last_rank: Option<i64> = None;
    for e in chain {
        let id = e.s("id");
        let r = e.get("rank").and_then(|v| v.as_i64());
        if r.is_some() && r == last_rank {
            if let Some(lane) = out.last_mut() {
                lane.push(id);
                continue;
            }
        }
        out.push(vec![id]);
        last_rank = r;
    }
    out
}

/// Does the recommendation differ from the hierarchy in force (comparing both order and tier groupings)?
pub fn differs(rec: &Value, chain: &[Value]) -> bool {
    let cur = chain_levels(chain);
    let want: Vec<Vec<String>> = rec
        .arr("levels")
        .iter()
        .map(|l| {
            l.as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .filter(|l| !l.is_empty())
        .collect();
    cur != want
}

/// The act() payload that applies a recommendation.
pub fn apply_payload(rec: &Value) -> Value {
    let mut order = vec![];
    let mut ranks = Map::new();
    for (i, l) in rec.arr("levels").iter().enumerate() {
        for id in l.as_array().cloned().unwrap_or_default() {
            if let Some(s) = id.as_str() {
                order.push(json!(s));
                ranks.insert(s.to_string(), json!(i as i64 + 1));
            }
        }
    }
    json!({"a": "priority", "order": order, "ranks": ranks})
}

// ---------------------------------------------------------------- organizer model

/// Ask a good model (Gemini 4 Neon through your Google account, else a capable API brain)
/// to organise the hierarchy. The answer is validated; `None` means "keep the heuristic".
pub fn organize_with_model(cfg: &Value, chain: &[Value]) -> Option<Value> {
    let brains: Vec<Value> = cfg
        .arr("brains")
        .into_iter()
        .filter(|b| b.b("enabled", true))
        .collect();
    if brains.len() < 2 {
        return None;
    }
    let avail = |id: &str| {
        chain
            .iter()
            .find(|e| e.s("id") == id)
            .map(|e| e.b("available", false))
            .unwrap_or(false)
    };
    let reason = |id: &str| {
        chain
            .iter()
            .find(|e| e.s("id") == id)
            .map(|e| e.s("reason"))
            .unwrap_or_default()
    };
    let roster: Vec<Value> = brains
        .iter()
        .map(|b| {
            let id = b.s("id");
            json!({"id": id, "name": b.s("name"), "kind": b.s("kind"), "models": b.get("ladder").cloned().unwrap_or(Value::Null),
                   "available": avail(&id), "status": reason(&id)})
        })
        .collect();
    let prompt = format!(
        "Eres el organizador de MetaBrain, un orquestador de cerebros de IA. Objetivo: gastar los menos tokens caros \
posible. Los modelos de frontera (Opus, Gemini 4 Neon, GPT-6.2 Sol) piensan, planifican y revisan; el más barato \
(DeepSeek Flash) es el burro de carga que hace el trabajo mecánico. Varios cerebros pueden compartir un nivel \
(mesa redonda: mandan juntos y se reparten la carga). Los no disponibles van al final en reserva.\n\
Cerebros: {}\n\
Responde SOLO un JSON: {{\"levels\":[[\"id\",...],...],\"roles\":{{\"id\":\"director|submanager|producer|workhorse|reserve\"}},\
\"summary\":\"frase corta\",\"lines\":[\"explicación breve por rol\"]}}. Usa cada id exactamente una vez.",
        Value::Array(roster)
    );
    let text = ask_organizer(&brains, &|id| avail(id), &prompt)?;
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    validate(&v, &brains, chain)
}

fn validate(v: &Value, brains: &[Value], chain: &[Value]) -> Option<Value> {
    let known: Vec<String> = brains.iter().map(|b| b.s("id")).collect();
    let mut seen: Vec<String> = vec![];
    let mut levels: Vec<Vec<String>> = vec![];
    for l in v.arr("levels") {
        let mut lane = vec![];
        for id in l.as_array().cloned().unwrap_or_default() {
            let id = id.as_str().unwrap_or("").to_string();
            if known.contains(&id) && !seen.contains(&id) {
                seen.push(id.clone());
                lane.push(id);
            }
        }
        if !lane.is_empty() {
            levels.push(lane);
        }
    }
    // Something must lead, and it must be available.
    let first_ok = levels
        .first()
        .map(|l| {
            l.iter().any(|id| {
                chain
                    .iter()
                    .any(|e| e.s("id") == *id && e.b("available", false))
            })
        })
        .unwrap_or(false);
    if !first_ok {
        return None;
    }
    for id in known {
        if !seen.contains(&id) {
            levels.push(vec![id]);
        }
    }
    let mut out = v.clone();
    out["levels"] = json!(levels);
    out["by_model"] = json!(true);
    if out.s("summary").is_empty() {
        out["summary"] = json!("organizado por IA");
    }
    Some(out)
}

/// Run the prompt on the best organizer available. Returns the raw text.
fn ask_organizer(brains: &[Value], avail: &dyn Fn(&str) -> bool, prompt: &str) -> Option<String> {
    // 1) Gemini 4 Neon through the Google account (Antigravity CLI): good thinker, no API cost.
    if let Some(agy_brain) = brains
        .iter()
        .find(|b| b.s("kind") == "antigravity" && avail(&b.s("id")))
    {
        let ladder = agy_brain.arr("ladder");
        let model = ladder
            .iter()
            .filter_map(|v| v.as_str())
            .find(|m| m.to_lowercase().contains("neon-medium"))
            .or_else(|| ladder.first().and_then(|v| v.as_str()))
            .unwrap_or("gemini-4-neon-medium")
            .to_string();
        let tmp_cwd = std::env::temp_dir().to_string_lossy().into_owned();
        if let Ok(resp) = crate::cli_agents::run_blocking_until(
            crate::cli_agents::Agent::Antigravity,
            prompt,
            &tmp_cwd,
            &model,
            "medium",
            "plan",
            &[],
            120,
            &mut |_| {},
            &mut |_| true,
            &mut || false,
        ) {
            if !resp.trim().is_empty() {
                return Some(resp);
            }
        }
    }
    // 2) An OpenAI-compatible API brain, using its strongest model (e.g. DeepSeek V4 Pro).
    for b in brains
        .iter()
        .filter(|b| b.s("kind") == "openai" && avail(&b.s("id")))
    {
        let Some(key) = crate::util::secret(&b.s("key_env")) else {
            continue;
        };
        let base = b.s("base_url");
        if base.is_empty() {
            continue;
        }
        let ladder = b.arr("ladder");
        let model = ladder
            .iter()
            .filter_map(|m| m.as_str())
            .find(|m| m.contains("pro"))
            .or_else(|| ladder.first().and_then(|m| m.as_str()))
            .unwrap_or("deepseek-flash")
            .to_string();
        let body = json!({"model": model, "max_tokens": 1200, "messages": [{"role": "user", "content": prompt}]});
        let auth = format!("Bearer {key}");
        let h: [(&str, &str); 2] = [
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
        ];
        if let Ok(r) = crate::util::http_json(
            &format!("{}/chat/completions", base.trim_end_matches('/')),
            &h,
            Some(&body),
            120,
        ) {
            let t = r
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .map(|m| m.s("content"))
                .unwrap_or_default();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_plan_is_validated() {
        let brains = vec![json!({"id":"A"}), json!({"id":"B"}), json!({"id":"DS"})];
        let chain = vec![
            json!({"id":"A","available":true}),
            json!({"id":"B","available":true}),
            json!({"id":"DS","available":true}),
        ];
        let v = validate(&json!({"levels":[["A","B","X"],["A"]]}), &brains, &chain).unwrap();
        assert_eq!(v["levels"], json!([["A", "B"], ["DS"]]));
        assert!(validate(&json!({"levels":[["Z"]]}), &brains, &chain).is_none());
    }

    fn cfg() -> Value {
        json!({"brains": [
            {"id": "A", "kind": "claude_code", "name": "Claude 1"},
            {"id": "B", "kind": "claude_code", "name": "Claude 2"},
            {"id": "DS", "kind": "openai", "name": "DeepSeek", "base_url": "https://api.deepseek.com"},
            {"id": "G", "kind": "antigravity", "name": "Gemini"},
            {"id": "C", "kind": "codex", "name": "ChatGPT"}
        ]})
    }
    fn chain(av: &[&str]) -> Vec<Value> {
        ["A", "B", "DS", "G", "C"].iter().map(|id| json!({"id": id, "name": id, "available": av.contains(id), "reason": if av.contains(id) { "disponible" } else { "cuota" }})).collect()
    }

    #[test]
    fn claudes_out_chatgpt_leads_deepseek_carries() {
        let r = recommend(&cfg(), &chain(&["DS", "G", "C"]));
        let lv: Vec<Vec<String>> = r
            .arr("levels")
            .iter()
            .map(|l| {
                l.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .collect();
        assert_eq!(lv[0], vec!["C"]);
        assert_eq!(lv[1], vec!["G"]);
        assert_eq!(lv[2], vec!["DS"]);
        assert_eq!(lv[3], vec!["A", "B"]);
        assert_eq!(r["roles"]["DS"], "workhorse");
        assert_eq!(r["roles"]["C"], "director");
        assert_eq!(r["roles"]["G"], "submanager");
        assert_eq!(r["roles"]["A"], "reserve");
    }

    #[test]
    fn all_available_claude_and_chatgpt_share_tier1() {
        let r = recommend(&cfg(), &chain(&["A", "B", "DS", "G", "C"]));
        let lv: Vec<Vec<String>> = r
            .arr("levels")
            .iter()
            .map(|l| {
                l.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .collect();
        assert_eq!(lv[0], vec!["A", "B", "C"]);
        assert_eq!(lv[1], vec!["G"]);
        assert_eq!(lv[2], vec!["DS"]);
        assert_eq!(r["roles"]["A"], "director");
        assert_eq!(r["roles"]["C"], "director");
        assert_eq!(r["roles"]["G"], "submanager");
        assert_eq!(r["roles"]["DS"], "workhorse");
        assert!(differs(&r, &chain(&["A", "B", "DS", "G", "C"])));
    }

    #[test]
    fn differs_detects_rank_grouping_change_with_same_flat_order() {
        let r = recommend(&cfg(), &chain(&["A", "B", "DS", "G", "C"]));
        // Same flat order [A, B, C, G, DS] but separate ranks instead of shared Tier 1.
        let sep_chain = vec![
            json!({"id": "A", "rank": 1, "available": true}),
            json!({"id": "B", "rank": 2, "available": true}),
            json!({"id": "C", "rank": 3, "available": true}),
            json!({"id": "G", "rank": 4, "available": true}),
            json!({"id": "DS", "rank": 5, "available": true}),
        ];
        assert!(differs(&r, &sep_chain));

        let grouped_chain = vec![
            json!({"id": "A", "rank": 1, "available": true}),
            json!({"id": "B", "rank": 1, "available": true}),
            json!({"id": "C", "rank": 1, "available": true}),
            json!({"id": "G", "rank": 2, "available": true}),
            json!({"id": "DS", "rank": 3, "available": true}),
        ];
        assert!(!differs(&r, &grouped_chain));
    }

    #[test]
    fn only_deepseek_left_it_leads() {
        let r = recommend(&cfg(), &chain(&["DS"]));
        assert_eq!(r["roles"]["DS"], "director");
    }
}
