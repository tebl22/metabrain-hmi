//! Pure planning, LLMCompiler DAG decomposition, and topological wave validation.

use super::{clip, PlanNode, PlanSpec, MAX_BRANCHES};
use crate::architect::tier_of;
use crate::store::Usage;
use crate::tasks::can_edit;
use crate::util::{http_json, now, secret, J};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Deserialize, Default)]
struct RawPlan {
    #[serde(default)]
    strategy: String,
    #[serde(default, alias = "subtasks")]
    nodes: Vec<RawPlanNode>,
}

#[derive(Deserialize, Default)]
struct RawPlanNode {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    deps: Vec<Value>,
    #[serde(default)]
    tier: Option<u64>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    files: Vec<Value>,
}

#[derive(Deserialize, Default)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize, Default)]
struct ChatChoice {
    #[serde(default)]
    message: ChatMessage,
}

#[derive(Deserialize, Default)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}

/// Filter available task runners. The API transport currently accepts text only.
pub fn available_brains(cfg: &Value, chain: &[Value], has_image: bool) -> Vec<Value> {
    let brains = cfg.arr("brains");
    let mut out = Vec::new();
    for entry in chain {
        if !entry.b("available", false) {
            continue;
        }
        let id = entry.s("id");
        let Some(b) = brains
            .iter()
            .find(|x| x.s("id") == id && x.b("enabled", true))
        else {
            continue;
        };
        let kind = b.s("kind");
        if !crate::tasks::can_run_task_on(&kind, "plan") || (has_image && kind == "openai") {
            continue;
        }
        out.push(b.clone());
    }
    out
}

/// Choose the commanding Director from available brains (highest tier first; build requires editable brain).
pub fn pick_director(avail: &[Value], chain: &[Value], mode: &str) -> (String, String) {
    let mut best: Option<(u8, usize, &Value)> = None;
    for (idx, b) in avail.iter().enumerate() {
        if mode == "build" && !can_edit(&b.s("kind")) {
            continue;
        }
        let t = tier_of(b);
        let is_master = chain
            .iter()
            .any(|e| e.s("id") == b.s("id") && e.b("master", false));
        let rank_key = if is_master { 0 } else { t };
        let better = match best {
            Some((bt, bi, _)) => (rank_key, idx) < (bt, bi),
            None => true,
        };
        if better {
            best = Some((rank_key, idx, b));
        }
    }
    let chosen = best.map(|(_, _, b)| b).or_else(|| avail.first());
    match chosen {
        Some(b) => (b.s("id"), b.os("name").unwrap_or_else(|| b.s("id"))),
        None => (String::new(), "Director".into()),
    }
}

/// Ask a fast cheap model (DeepSeek Flash if available) to emit a compact DAG plan;
/// on any failure or timeout, fall back instantaneously to the Nexa heuristic planner.
pub(super) fn plan_task(
    cfg: &Value,
    chain: &[Value],
    avail: &[Value],
    prompt: &str,
    mode: &str,
) -> (PlanSpec, Option<Usage>) {
    if avail.len() > 1 && crate::router::classify(prompt).0 >= 3 {
        if let Some((plan, usage)) = llm_plan_deepseek(cfg, chain, avail, prompt, mode) {
            return (
                plan.unwrap_or_else(|| heuristic_plan(prompt, mode, avail)),
                usage,
            );
        }
    }
    (heuristic_plan(prompt, mode, avail), None)
}

fn llm_plan_deepseek(
    cfg: &Value,
    chain: &[Value],
    avail: &[Value],
    prompt: &str,
    mode: &str,
) -> Option<(Option<PlanSpec>, Option<Usage>)> {
    let ds = cfg.arr("brains").into_iter().find(|b| {
        b.b("enabled", true)
            && b.s("kind") == "openai"
            && (b.s("id").to_lowercase().contains("deepseek")
                || b.s("base_url").to_lowercase().contains("deepseek"))
            && chain
                .iter()
                .any(|e| e.s("id") == b.s("id") && e.b("available", false))
    })?;
    let key = secret(&ds.s("key_env"))?;
    let base = ds.os("base_url")?;
    let roster: Vec<String> = avail
        .iter()
        .map(|b| {
            format!(
                "{}(T{},edit={})",
                b.s("name"),
                tier_of(b),
                can_edit(&b.s("kind"))
            )
        })
        .collect();

    let system = format!(
        "Eres el planificador DAG de MetaBrain. Divide el pedido en 2 a 4 subtareas ejecutables en paralelo \
para ahorrar tokens de Tier 1 (Claude/ChatGPT) usando Tier 2 (Gemini) y Tier 3 (DeepSeek) en la Ola 1, \
y dejando la síntesis o implementación final al Director (tier 1).\n\
Modo global: {mode}. Cerebros disponibles: {}.\n\
Reglas:\n\
- Si mode=='plan', todas las subtareas usan mode:'plan'.\n\
- Si mode=='build', la Ola 1 investiga/diseña en mode:'plan' en paralelo, y el nodo final implementa en mode:'build' con deps a la Ola 1.\n\
- Cada nodo tiene: id ('s1','s2','synth'), title (<=60 chars), prompt (instrucción concreta), deps ([ids]), tier (1|2|3), mode ('plan'|'build'), role ('explore'|'analyze'|'review'|'implement'|'synthesize'), files ([]).\n\
Responde SOLO JSON válido: {{\"strategy\":\"dag|fanout\",\"nodes\":[...]}}",
        roster.join(", ")
    );

    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let body = json!({
        "model": "deepseek-flash",
        "max_tokens": 700,
        "temperature": 0.1,
        "thinking": {"type": "disabled"},
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": clip(prompt, 2400)}
        ]
    });
    let auth = format!("Bearer {key}");
    let resp = http_json(&url, &[("Authorization", auth.as_str())], Some(&body), 10).ok()?;
    let usage = resp.get("usage").map(|u| {
        let input = u["prompt_tokens"].as_i64().unwrap_or(0).max(0);
        let out = u["completion_tokens"].as_i64().unwrap_or(0).max(0);
        let cache_read = u["prompt_cache_hit_tokens"]
            .as_i64()
            .or_else(|| u["prompt_tokens_details"]["cached_tokens"].as_i64())
            .unwrap_or(0)
            .clamp(0, input);
        let inp = input - cache_read;
        let rates = ds["prices"]
            .get("deepseek-flash")
            .filter(|rates| rates.is_object())
            .unwrap_or(&ds["pricing"]["deepseek-flash"]);
        let usd = rates["input"]
            .as_f64()
            .zip(rates["output"].as_f64())
            .map(|(i, o)| {
                (inp as f64 * i
                    + out as f64 * o
                    + cache_read as f64 * rates["cached_input"].as_f64().unwrap_or(i))
                    / 1_000_000.0
            })
            .unwrap_or(0.0);
        Usage {
            ts: now(),
            brain: ds.s("id"),
            model: Some("deepseek-flash".into()),
            inp,
            out,
            cache_read,
            cache_write: 0,
            usd,
        }
    });
    let plan = serde_json::from_value::<ChatCompletion>(resp)
        .ok()
        .and_then(|parsed| parsed.choices.into_iter().next()?.message.content)
        .and_then(|text| parse_plan_json(&text));
    Some((plan, usage))
}

pub fn parse_plan_json(raw: &str) -> Option<PlanSpec> {
    let trimmed = raw.trim();
    let slice = if trimmed.starts_with('{') {
        trimmed
    } else {
        let s = trimmed.find('{')?;
        let e = trimmed.rfind('}')?;
        trimmed.get(s..=e)?
    };
    let raw_plan: RawPlan = serde_json::from_str(slice).ok()?;
    let strategy = match raw_plan.strategy.as_str() {
        "dag" => "dag".to_string(),
        "fanout" => "fanout".to_string(),
        _ => "dag".to_string(),
    };
    if raw_plan.nodes.is_empty() {
        return None;
    }
    let mut nodes = Vec::new();
    for (i, item) in raw_plan.nodes.into_iter().take(MAX_BRANCHES).enumerate() {
        let id = item
            .id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("s{}", i + 1));
        let title = item
            .title
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("Subtarea {}", i + 1));
        let prompt = item.prompt.filter(|s| !s.trim().is_empty())?;
        let deps = item
            .deps
            .into_iter()
            .filter_map(|d| d.as_str().map(String::from))
            .collect();
        let tier = item.tier.unwrap_or(2).clamp(1, 3) as u8;
        let mode = if item.mode.as_deref() == Some("build") {
            "build".into()
        } else {
            "plan".into()
        };
        let role = item
            .role
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "analyze".into());
        let files = item
            .files
            .into_iter()
            .filter_map(|f| f.as_str().map(String::from))
            .collect();
        nodes.push(PlanNode {
            id,
            title: clip(&title, 70),
            prompt,
            deps,
            tier,
            mode,
            role,
            files,
        });
    }
    if nodes.is_empty() {
        None
    } else {
        Some(PlanSpec { strategy, nodes })
    }
}

/// Deterministic Nexa Parallel-to-Sequential plan when LLM planning is skipped or unavailable.
pub fn heuristic_plan(prompt: &str, mode: &str, avail: &[Value]) -> PlanSpec {
    if avail.len() <= 1 {
        let tier = avail.first().map(tier_of).unwrap_or(1);
        return PlanSpec {
            strategy: "direct".into(),
            nodes: vec![PlanNode {
                id: "s1".into(),
                title: "Ejecución directa".into(),
                prompt: prompt.to_string(),
                deps: vec![],
                tier,
                mode: mode.to_string(),
                role: if mode == "build" {
                    "implement".into()
                } else {
                    "analyze".into()
                },
                files: vec![],
            }],
        };
    }

    if mode == "build" {
        return PlanSpec {
            strategy: "dag".into(),
            nodes: vec![
                PlanNode {
                    id: "s1".into(),
                    title: "Diseño técnico y archivos objetivo".into(),
                    prompt: format!(
                        "Analiza el proyecto para resolver este pedido sin editar archivos todavía.\n\
Identifica los archivos exactos, funciones y plan mínimo de cambios:\n\n{prompt}"
                    ),
                    deps: vec![],
                    tier: 2,
                    mode: "plan".into(),
                    role: "analyze".into(),
                    files: vec![],
                },
                PlanNode {
                    id: "s2".into(),
                    title: "Casos límite, riesgos y pruebas".into(),
                    prompt: format!(
                        "Sin editar archivos, revisa riesgos, casos límite, regresiones posibles \
y qué pruebas deben ejecutarse para validar este pedido:\n\n{prompt}"
                    ),
                    deps: vec![],
                    tier: 3,
                    mode: "plan".into(),
                    role: "explore".into(),
                    files: vec![],
                },
                PlanNode {
                    id: "impl".into(),
                    title: "Implementación y verificación (Director)".into(),
                    prompt: format!(
                        "Usa los hallazgos paralelos adjuntos como referencia, aplica los cambios exactos \
en el proyecto y verifica que compile y pase las pruebas:\n\n{prompt}"
                    ),
                    deps: vec!["s1".into(), "s2".into()],
                    tier: 1,
                    mode: "build".into(),
                    role: "implement".into(),
                    files: vec![],
                },
            ],
        };
    }

    // Plan / analysis mode: Nexa Wave 1 (parallel perspectives on Tier 2 + Tier 3) -> Wave 2 (Director synthesis).
    PlanSpec {
        strategy: "fanout".into(),
        nodes: vec![
            PlanNode {
                id: "s1".into(),
                title: "Propuesta principal y arquitectura".into(),
                prompt: format!(
                    "Propón la mejor solución técnica, pasos concretos y casos límite para:\n\n{prompt}"
                ),
                deps: vec![],
                tier: 2,
                mode: "plan".into(),
                role: "analyze".into(),
                files: vec![],
            },
            PlanNode {
                id: "s2".into(),
                title: "Crítica, riesgos y alternativa ligera".into(),
                prompt: format!(
                    "Analiza desde un ángulo crítico y práctico: qué puede fallar, qué simplificarías \
para ahorrar complejidad/tokens y qué alternativa directa propones para:\n\n{prompt}"
                ),
                deps: vec![],
                tier: 3,
                mode: "plan".into(),
                role: "explore".into(),
                files: vec![],
            },
            PlanNode {
                id: "synth".into(),
                title: "Síntesis final del Director".into(),
                prompt: format!(
                    "Eres el Director. Compara las propuestas paralelas recibidas, resuelve contradicciones, \
quédate con lo mejor de cada una y entrega una respuesta final unificada, clara y accionable para:\n\n{prompt}"
                ),
                deps: vec!["s1".into(), "s2".into()],
                tier: 1,
                mode: "plan".into(),
                role: "synthesize".into(),
                files: vec![],
            },
        ],
    }
}

/// Compute wave indices (0, 1, 2, ...) using Kahn's topological sort.
/// Returns `Err` if the DAG contains a cycle.
pub fn topological_waves(nodes: &[PlanNode]) -> Result<HashMap<String, usize>, String> {
    let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut indeg: HashMap<String, usize> = HashMap::new();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for n in nodes {
        indeg.entry(n.id.clone()).or_insert(0);
        for d in &n.deps {
            if ids.contains(d.as_str()) && d != &n.id {
                *indeg.entry(n.id.clone()).or_insert(0) += 1;
                adj.entry(d.clone()).or_default().push(n.id.clone());
            }
        }
    }
    let mut q: VecDeque<String> = indeg
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut wave: HashMap<String, usize> = HashMap::new();
    for id in &q {
        wave.insert(id.clone(), 0);
    }
    let mut visited = 0usize;
    while let Some(u) = q.pop_front() {
        visited += 1;
        let uw = *wave.get(&u).unwrap_or(&0);
        if let Some(nexts) = adj.get(&u) {
            for v in nexts {
                let entry = wave.entry(v.clone()).or_insert(0);
                *entry = (*entry).max(uw + 1);
                if let Some(deg) = indeg.get_mut(v) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        q.push_back(v.clone());
                    }
                }
            }
        }
    }
    if visited != nodes.len() {
        return Err("El plan contiene un ciclo de dependencias.".into());
    }
    Ok(wave)
}

/// Validate the DAG and keep a single final writer, since the parent owns one Git branch.
pub fn validate_and_finalize_dag(
    mut spec: PlanSpec,
    original_prompt: &str,
    global_mode: &str,
    avail_count: usize,
) -> PlanSpec {
    let known: HashSet<&str> = spec.nodes.iter().map(|n| n.id.as_str()).collect();
    let invalid = spec.nodes.is_empty()
        || spec.nodes.len() > MAX_BRANCHES
        || known.len() != spec.nodes.len()
        || known.contains("")
        || spec.nodes.iter().any(|n| {
            n.deps
                .iter()
                .any(|d| d == &n.id || !known.contains(d.as_str()))
        })
        || topological_waves(&spec.nodes).is_err();
    let writers: Vec<_> = spec.nodes.iter().filter(|n| n.mode == "build").collect();
    let invalid_build = global_mode == "build"
        && (writers.len() != 1 || spec.nodes.iter().any(|n| n.deps.contains(&writers[0].id)));
    let synthesis: Vec<_> = spec
        .nodes
        .iter()
        .filter(|n| n.role == "synthesize")
        .collect();
    let invalid_synthesis = global_mode == "plan"
        && spec.nodes.len() > 1
        && (synthesis.len() != 1 || spec.nodes.iter().any(|n| n.deps.contains(&synthesis[0].id)));
    if invalid || invalid_build || invalid_synthesis {
        let dummy: Vec<Value> = (0..avail_count.max(1))
            .map(|i| json!({"id": format!("b{i}")}))
            .collect();
        return heuristic_plan(original_prompt, global_mode, &dummy);
    }

    let ids: Vec<_> = spec.nodes.iter().map(|n| n.id.clone()).collect();
    for n in &mut spec.nodes {
        n.tier = n.tier.clamp(1, 3);
        if global_mode == "plan" {
            n.mode = "plan".into();
        } else if n.mode == "build" {
            n.role = "implement".into();
        }
        if n.mode == "build" || (global_mode == "plan" && n.role == "synthesize") {
            n.deps = ids.iter().filter(|id| *id != &n.id).cloned().collect();
            n.tier = 1;
            if !n.prompt.contains(original_prompt) {
                n.prompt
                    .push_str(&format!("\n\nPedido completo:\n{original_prompt}"));
            }
        }
    }

    spec
}
