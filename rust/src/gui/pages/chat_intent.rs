//! Live intent detection for chat messages: parses explicit model, brain, mode, and parallel
//! orchestration requests as the user types so the UI auto-adapts immediately and routes the
//! turn to the exact brain/model requested without manual clicks.

use crate::util::J;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatIntent {
    /// Target brain id (e.g. `"claude_1"`, `"parallel"`, `"auto"`).
    pub brain_id: String,
    /// Human-readable brain name (e.g. `"Claude Code"`, `"Motor paralelo"`).
    pub brain_name: String,
    /// Specific model slug to use (empty = brain's default / automatic model).
    pub model: String,
    /// Optional mode override (`Some(true)` = build, `Some(false)` = plan).
    pub build_mode: Option<bool>,
    /// True when the user asked for parallel multi-agent orchestration.
    pub parallel: bool,
    /// True if the resolved brain is currently available in the command chain.
    pub available: bool,
    /// Unavailability reason when `!available`.
    pub reason: String,
    /// Config index of the resolved brain (for consistent `series(i)` tinting in UI).
    pub brain_index: Option<usize>,
    /// Short key identifying this directive so manual overrides don't fight the user.
    pub key: String,
}

#[derive(Clone)]
struct BrainCandidate {
    index: usize,
    id: String,
    name: String,
    kind: String,
    models: Vec<String>,
    available: bool,
    reason: String,
}

/// Normalize accents & casing for reliable Spanish/English directive matching.
fn normalize(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'á' | 'Á' => 'a',
            'é' | 'É' => 'e',
            'í' | 'Í' => 'i',
            'ó' | 'Ó' => 'o',
            'ú' | 'Ú' | 'ü' | 'Ü' => 'u',
            'ñ' | 'Ñ' => 'n',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// Check if `needle` appears in `haystack` bounded by non-alphanumeric edges.
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let nlen = needle.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let right_idx = abs + nlen;
        let right_ok = right_idx >= bytes.len() || !bytes[right_idx].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Extract tokens following explicit directive cues like:
/// `/model X`, `/modelo X`, `@X`, `usa X`, `usar X`, `con X`, `cambia a X`, `pasa a X`,
/// `modelo X`, `cerebro X`, `use X`, `with X`, `switch to X`.
fn extract_directive_windows(norm: &str) -> Vec<String> {
    let mut windows: Vec<String> = Vec::new();

    // 1. Slash commands and @mentions anywhere in text.
    let tokens: Vec<&str> = norm
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '[' | ']'))
        .filter(|t| !t.is_empty())
        .collect();

    for (i, tok) in tokens.iter().enumerate() {
        if let Some(rest) = tok.strip_prefix('@') {
            let clean = rest.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '.'
            });
            if clean.len() >= 2 {
                let mut win = clean.to_string();
                if let Some(next) = tokens.get(i + 1) {
                    win.push(' ');
                    win.push_str(next);
                }
                windows.push(win);
            }
        } else if matches!(
            *tok,
            "/model" | "/modelo" | "/brain" | "/cerebro" | "/use" | "/usa"
        ) {
            let slice = tokens[i + 1..(i + 4).min(tokens.len())].join(" ");
            if !slice.is_empty() {
                windows.push(slice);
            }
        } else if matches!(*tok, "/paralelo" | "/parallel" | "/agentes" | "/orquestar") {
            windows.push("paralelo".into());
        } else if let Some(rest) = tok
            .strip_prefix("modelo:")
            .or_else(|| tok.strip_prefix("model:"))
        {
            let clean = rest.trim();
            if !clean.is_empty() {
                let mut win = clean.to_string();
                if let Some(next) = tokens.get(i + 1) {
                    win.push(' ');
                    win.push_str(next);
                }
                windows.push(win);
            }
        }
    }

    // 2. Natural-language directive triggers (Spanish + English).
    const TRIGGERS: &[&str] = &[
        "cambia a ",
        "cambiar a ",
        "cambialo a ",
        "cambiame a ",
        "pasa a ",
        "pasar a ",
        "pasalo a ",
        "usa el modelo ",
        "usar el modelo ",
        "usa a ",
        "usa ",
        "usar ",
        "usando ",
        "utiliza ",
        "utilizando ",
        "con el modelo ",
        "con el cerebro ",
        "con ",
        "en el modelo ",
        "modelo ",
        "cerebro ",
        "switch to ",
        "change to ",
        "use model ",
        "use ",
        "using ",
        "with model ",
        "with ",
    ];

    for trig in TRIGGERS {
        let mut search_from = 0;
        while let Some(pos) = norm[search_from..].find(trig) {
            let abs = search_from + pos;
            let left_ok = abs == 0 || !norm.as_bytes()[abs - 1].is_ascii_alphanumeric();
            if left_ok {
                let after = &norm[abs + trig.len()..];
                let snippet: String = after
                    .split([',', '.', ';', '\n', '?', '!'])
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .take(4)
                    .collect::<Vec<_>>()
                    .join(" ");
                if !snippet.is_empty() {
                    windows.push(snippet);
                }
            }
            search_from = abs + trig.len();
        }
    }

    windows
}

fn detect_mode(norm: &str) -> Option<bool> {
    const PLAN_CUES: &[&str] = &[
        "/plan",
        "modo plan",
        "en modo plan",
        "solo planea",
        "solo analiza",
        "plan mode",
    ];
    const BUILD_CUES: &[&str] = &[
        "/build",
        "/construir",
        "modo construir",
        "en modo construir",
        "build mode",
    ];
    if PLAN_CUES.iter().any(|c| contains_phrase(norm, c)) {
        return Some(false);
    }
    if BUILD_CUES.iter().any(|c| contains_phrase(norm, c)) {
        return Some(true);
    }
    None
}

fn detect_parallel(norm: &str) -> bool {
    const PARALLEL_CUES: &[&str] = &[
        "/paralelo",
        "/parallel",
        "/agentes",
        "/orquestar",
        "en paralelo",
        "modo paralelo",
        "motor paralelo",
        "orquesta ",
        "orquestar ",
        "con varios agentes",
        "multiples agentes",
        "varios cerebros",
        "in parallel",
        "parallel mode",
        "orchestrate",
    ];
    PARALLEL_CUES.iter().any(|c| {
        if c.ends_with(' ') {
            let trimmed = c.trim_end();
            contains_phrase(norm, trimmed)
        } else {
            contains_phrase(norm, c)
        }
    })
}

fn build_candidates(brains: &[Value], chain: &[Value]) -> Vec<BrainCandidate> {
    let mut out = Vec::with_capacity(brains.len());
    for (index, b) in brains.iter().enumerate() {
        let id = b.s("id");
        let name = b.s("name");
        let kind = b.s("kind");
        let mut models: Vec<String> = b
            .arr("models")
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(m) = b.os("model").filter(|s| !s.is_empty()) {
            if !models.iter().any(|x| x == &m) {
                models.push(m);
            }
        }
        if let Some(eff_m) = b
            .get("effective")
            .and_then(|e| e.os("model"))
            .filter(|s| !s.is_empty())
        {
            if !models.iter().any(|x| x == &eff_m) {
                models.push(eff_m);
            }
        }
        let ch = chain.iter().find(|e| e.s("id") == id);
        let available = ch.map(|e| e.b("available", false)).unwrap_or(true);
        let reason = ch.map(|e| e.s("reason")).unwrap_or_default();
        out.push(BrainCandidate {
            index,
            id,
            name,
            kind,
            models,
            available,
            reason,
        });
    }
    out
}

/// Match a model alias or exact slug inside a directive window against a brain's ladder.
fn match_model_in_window(window: &str, brain: &BrainCandidate) -> Option<(String, usize)> {
    // 1. Exact or normalized slug match against the brain's actual configured models (longest wins).
    let mut best: Option<(String, usize)> = None;
    for slug in &brain.models {
        let slug_norm = normalize(slug);
        let slug_spaced = slug_norm.replace(['-', '_'], " ");
        let stripped = slug_norm
            .trim_start_matches("claude-")
            .trim_start_matches("gemini-")
            .trim_start_matches("deepseek-")
            .trim_start_matches("gpt-");
        let stripped_spaced = stripped.replace(['-', '_'], " ");

        for variant in [
            slug_norm.as_str(),
            slug_spaced.as_str(),
            stripped,
            stripped_spaced.as_str(),
        ] {
            if variant.len() >= 3 && contains_phrase(window, variant) {
                let score = variant.len() * 10 + 50;
                if best.as_ref().is_none_or(|(_, s)| score > *s) {
                    best = Some((slug.clone(), score));
                }
            }
        }
    }

    // 2. Well-known shorthand aliases (e.g. "opus", "sonnet", "haiku", "neon", "flash", "pro", "sol").
    let aliases: &[(&str, &[&str], &str)] = &[
        ("opus", &["claude_code", "anthropic"], "opus"),
        ("sonnet", &["claude_code", "anthropic"], "sonnet"),
        ("haiku", &["claude_code", "anthropic"], "haiku"),
        ("gemini 4 neon", &["antigravity", "gemini_cli"], "neon"),
        ("gemini neon", &["antigravity", "gemini_cli"], "neon"),
        ("neon", &["antigravity", "gemini_cli"], "neon"),
        (
            "gemini 3 pro",
            &["antigravity", "gemini_cli", "openai"],
            "pro",
        ),
        (
            "gemini pro",
            &["antigravity", "gemini_cli", "openai"],
            "pro",
        ),
        (
            "gemini flash",
            &["antigravity", "gemini_cli", "openai"],
            "flash",
        ),
        ("deepseek pro", &["openai"], "pro"),
        ("deepseek v4 pro", &["openai"], "pro"),
        ("deepseek flash", &["openai"], "flash"),
        ("deepseek chat", &["openai"], "chat"),
        ("deepseek reasoner", &["openai"], "reasoner"),
        ("r1", &["openai"], "reasoner"),
        ("gpt-6.2", &["codex", "openai"], "gpt-6.2"),
        ("gpt 6.2", &["codex", "openai"], "gpt-6.2"),
        ("gpt-5", &["codex", "openai"], "gpt-5"),
        ("gpt 5", &["codex", "openai"], "gpt-5"),
        ("o3", &["codex", "openai"], "o3"),
    ];

    for (alias, kinds, hint) in aliases {
        if !kinds.contains(&brain.kind.as_str()) {
            continue;
        }
        if contains_phrase(window, alias) {
            // Prefer a matching slug from the brain's ladder; else use the alias/hint.
            let resolved = brain
                .models
                .iter()
                .find(|m| normalize(m).contains(hint))
                .cloned()
                .unwrap_or_else(|| (*hint).to_string());
            let score = alias.len() * 8 + 40;
            if best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((resolved, score));
            }
        }
    }

    best
}

/// Match a brain by id, name, or provider keyword inside a directive window.
fn match_brain_in_window(window: &str, brain: &BrainCandidate) -> Option<usize> {
    let mut best: Option<usize> = None;
    let id_norm = normalize(&brain.id);
    let name_norm = normalize(&brain.name);

    if id_norm.len() >= 2 && contains_phrase(window, &id_norm) {
        best = Some(id_norm.len() * 6 + 30);
    }
    if name_norm.len() >= 2 && contains_phrase(window, &name_norm) {
        let score = name_norm.len() * 6 + 35;
        if best.is_none_or(|s| score > s) {
            best = Some(score);
        }
    }

    let kind_aliases: &[&str] = match brain.kind.as_str() {
        "claude_code" | "anthropic" => &["claude code", "claude"],
        "codex" => &["codex", "chatgpt", "openai codex"],
        "antigravity" | "gemini_cli" => &["antigravity", "gemini", "google"],
        "openai" => {
            if name_norm.contains("deepseek") || id_norm.contains("deepseek") {
                &["deepseek", "barato"]
            } else if name_norm.contains("gemini") {
                &["gemini api", "barato"]
            } else if name_norm.contains("ollama") {
                &["ollama", "local"]
            } else {
                &["barato"]
            }
        }
        _ => &[],
    };
    for alias in kind_aliases {
        if contains_phrase(window, alias) {
            let score = alias.len() * 5 + 20;
            if best.is_none_or(|s| score > s) {
                best = Some(score);
            }
        }
    }
    best
}

/// Inspect live chat input and detect if the user explicitly specified a model, brain,
/// parallel orchestration, or mode.
pub fn detect_chat_intent(text: &str, brains: &[Value], chain: &[Value]) -> Option<ChatIntent> {
    let trimmed = text.trim();
    if trimmed.len() < 3 {
        return None;
    }
    let norm = normalize(trimmed);
    let build_mode = detect_mode(&norm);

    // 1. Explicit parallel orchestration directive takes priority when requested.
    if detect_parallel(&norm) {
        let any_avail = if chain.is_empty() {
            !brains.is_empty()
        } else {
            chain.iter().any(|e| e.b("available", false))
        };
        return Some(ChatIntent {
            brain_id: "parallel".into(),
            brain_name: "Parallel Engine".into(),
            model: String::new(),
            build_mode,
            parallel: true,
            available: any_avail,
            reason: if any_avail {
                String::new()
            } else {
                "no brain available".into()
            },
            brain_index: None,
            key: format!(
                "parallel:{}",
                build_mode.map(|b| if b { "b" } else { "p" }).unwrap_or("x")
            ),
        });
    }

    let candidates = build_candidates(brains, chain);
    let windows = extract_directive_windows(&norm);

    // Check explicit "auto" reset directive: "usa auto", "/model auto", "en automatico"
    for w in &windows {
        if contains_phrase(w, "auto") || contains_phrase(w, "automatico") {
            return Some(ChatIntent {
                brain_id: "auto".into(),
                brain_name: "Auto".into(),
                model: String::new(),
                build_mode,
                parallel: false,
                available: true,
                reason: String::new(),
                brain_index: None,
                key: format!(
                    "auto:{}",
                    build_mode.map(|b| if b { "b" } else { "p" }).unwrap_or("x")
                ),
            });
        }
    }

    // Score (candidate, optional model) across all extracted directive windows.
    // Prefer available brains over unavailable ones on equal score, and specific model matches
    // over bare brain matches.
    let mut best: Option<(usize, &BrainCandidate, String)> = None;
    for w in &windows {
        for cand in &candidates {
            let model_hit = match_model_in_window(w, cand);
            let brain_hit = match_brain_in_window(w, cand);
            if model_hit.is_none() && brain_hit.is_none() {
                continue;
            }
            let (model_slug, m_score) = model_hit.unwrap_or_default();
            let b_score = brain_hit.unwrap_or(0);
            let avail_bonus = if cand.available { 25 } else { 0 };
            let total = m_score + b_score + avail_bonus;
            if best.as_ref().is_none_or(|(s, _, _)| total > *s) {
                best = Some((total, cand, model_slug));
            }
        }
    }

    if let Some((_, cand, model)) = best {
        let key = format!(
            "{}:{}:{}",
            cand.id,
            model,
            build_mode.map(|b| if b { "b" } else { "p" }).unwrap_or("x")
        );
        return Some(ChatIntent {
            brain_id: cand.id.clone(),
            brain_name: cand.name.clone(),
            model,
            build_mode,
            parallel: false,
            available: cand.available,
            reason: cand.reason.clone(),
            brain_index: Some(cand.index),
            key,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_brains() -> (Vec<Value>, Vec<Value>) {
        let brains = vec![
            json!({
                "id": "claude_work",
                "name": "Claude Code",
                "kind": "claude_code",
                "models": ["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5"]
            }),
            json!({
                "id": "codex_main",
                "name": "ChatGPT Codex",
                "kind": "codex",
                "models": ["gpt-6.2-sol", "gpt-5.4"]
            }),
            json!({
                "id": "google_main",
                "name": "Gemini",
                "kind": "antigravity",
                "models": ["gemini-4-neon-high", "gemini-4-neon-medium", "gemini-3-flash"]
            }),
            json!({
                "id": "ds_api",
                "name": "DeepSeek",
                "kind": "openai",
                "models": ["deepseek-v4-pro", "deepseek-flash"]
            }),
        ];
        let chain = vec![
            json!({"id": "claude_work", "available": true, "reason": "disponible"}),
            json!({"id": "codex_main", "available": true, "reason": "disponible"}),
            json!({"id": "google_main", "available": true, "reason": "disponible"}),
            json!({"id": "ds_api", "available": true, "reason": "disponible"}),
        ];
        (brains, chain)
    }

    #[test]
    fn detects_model_and_resolves_owning_brain_automatically() {
        let (brains, chain) = sample_brains();
        let intent =
            detect_chat_intent("Usa opus para revisar la arquitectura", &brains, &chain).unwrap();
        assert_eq!(intent.brain_id, "claude_work");
        assert_eq!(intent.model, "claude-opus-4-6");
        assert!(intent.available);
        assert!(!intent.parallel);
    }

    #[test]
    fn detects_spanish_switch_with_gemini_and_deepseek() {
        let (brains, chain) = sample_brains();
        let g =
            detect_chat_intent("cambia a gemini 4 neon y revisa esto", &brains, &chain).unwrap();
        assert_eq!(g.brain_id, "google_main");
        assert!(g.model.contains("neon"));

        let d = detect_chat_intent("con deepseek flash haz un resumen rápido", &brains, &chain)
            .unwrap();
        assert_eq!(d.brain_id, "ds_api");
        assert_eq!(d.model, "deepseek-flash");
    }

    #[test]
    fn detects_slash_and_at_mentions() {
        let (brains, chain) = sample_brains();
        let m1 = detect_chat_intent("/model sonnet explica este archivo", &brains, &chain).unwrap();
        assert_eq!(m1.brain_id, "claude_work");
        assert_eq!(m1.model, "claude-sonnet-4-6");

        let m2 = detect_chat_intent("@codex arregla los tests", &brains, &chain).unwrap();
        assert_eq!(m2.brain_id, "codex_main");
        assert_eq!(m2.model, "");
    }

    #[test]
    fn detects_parallel_orchestration_and_mode() {
        let (brains, chain) = sample_brains();
        let p = detect_chat_intent(
            "Hazlo en paralelo y solo analiza en modo plan",
            &brains,
            &chain,
        )
        .unwrap();
        assert!(p.parallel);
        assert_eq!(p.brain_id, "parallel");
        assert_eq!(p.build_mode, Some(false));
    }

    #[test]
    fn avoids_false_positives_on_ordinary_word_modelo() {
        let (brains, chain) = sample_brains();
        assert!(
            detect_chat_intent("Analiza el modelo de datos del usuario", &brains, &chain).is_none()
        );
        assert!(
            detect_chat_intent("Necesito revisar las pruebas unitarias", &brains, &chain).is_none()
        );
    }

    #[test]
    fn parallel_intent_reports_unavailable_when_chain_has_no_available_brains() {
        let (brains, _) = sample_brains();
        let all_down = vec![
            json!({"id": "claude_work", "available": false, "reason": "paused"}),
            json!({"id": "ds_api", "available": false, "reason": "budget exhausted"}),
        ];
        let p = detect_chat_intent("Run this in parallel", &brains, &all_down).unwrap();
        assert!(p.parallel);
        assert!(!p.available);
    }
}
