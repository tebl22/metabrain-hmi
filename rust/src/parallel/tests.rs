use super::*;

fn sample_brains() -> Vec<Value> {
    vec![
        json!({"id": "A", "kind": "claude_code", "name": "Claude 1", "enabled": true, "max_parallel": 2, "ladder": ["claude-opus-5-5", "claude-sonnet-5-5"]}),
        json!({"id": "chatgpt", "kind": "codex", "name": "ChatGPT", "enabled": true, "max_parallel": 2, "ladder": ["gpt-6.2-sol"]}),
        json!({"id": "G", "kind": "antigravity", "name": "Gemini Neon", "enabled": true, "max_parallel": 2, "ladder": ["gemini-4-neon-high", "gemini-4-neon-medium"]}),
        json!({"id": "DS", "kind": "openai", "name": "DeepSeek", "base_url": "https://api.deepseek.com", "enabled": true, "max_parallel": 4, "ladder": ["deepseek-flash", "deepseek-v4-pro"]}),
    ]
}

fn sample_chain(avail: &[&str]) -> Vec<Value> {
    ["A", "chatgpt", "G", "DS"]
        .iter()
        .map(|id| json!({"id": id, "name": id, "available": avail.contains(id), "pct": 20.0, "master": *id == "A"}))
        .collect()
}

#[test]
fn dag_validation_computes_waves_and_rejects_cycles() {
    let acyclic = vec![
        PlanNode {
            id: "s1".into(),
            title: "A".into(),
            prompt: "p1".into(),
            deps: vec![],
            tier: 2,
            mode: "plan".into(),
            role: "analyze".into(),
            files: vec![],
        },
        PlanNode {
            id: "s2".into(),
            title: "B".into(),
            prompt: "p2".into(),
            deps: vec![],
            tier: 3,
            mode: "plan".into(),
            role: "explore".into(),
            files: vec![],
        },
        PlanNode {
            id: "s3".into(),
            title: "C".into(),
            prompt: "p3".into(),
            deps: vec!["s1".into(), "s2".into()],
            tier: 1,
            mode: "plan".into(),
            role: "synthesize".into(),
            files: vec![],
        },
    ];
    let waves = topological_waves(&acyclic).expect("acyclic");
    assert_eq!(waves["s1"], 0);
    assert_eq!(waves["s2"], 0);
    assert_eq!(waves["s3"], 1);

    let cyclic = vec![
        PlanNode {
            id: "a".into(),
            title: "A".into(),
            prompt: "p".into(),
            deps: vec!["b".into()],
            tier: 2,
            mode: "plan".into(),
            role: "analyze".into(),
            files: vec![],
        },
        PlanNode {
            id: "b".into(),
            title: "B".into(),
            prompt: "p".into(),
            deps: vec!["a".into()],
            tier: 2,
            mode: "plan".into(),
            role: "analyze".into(),
            files: vec![],
        },
    ];
    assert!(topological_waves(&cyclic).is_err());

    // Fallback replaces cyclic LLM plan with a valid heuristic DAG.
    let fixed = validate_and_finalize_dag(
        PlanSpec {
            strategy: "dag".into(),
            nodes: cyclic,
        },
        "analiza",
        "plan",
        3,
    );
    assert!(topological_waves(&fixed.nodes).is_ok());
    assert_eq!(fixed.nodes.len(), 3);
}

#[test]
fn fate_assigner_matches_tiers_spreads_parallel_waves_and_respects_edit_capability() {
    let brains = sample_brains();
    let chain = sample_chain(&["A", "chatgpt", "G", "DS"]);
    let queue = json!({"per_brain": {}});
    let recent = HashSet::new();
    let mut active = HashMap::new();

    // Tier 2 analysis -> Gemini Neon
    let b1 = BranchState {
        id: "s1".into(),
        title: "Diseño".into(),
        role: "analyze".into(),
        tier: 2,
        mode: "plan".into(),
        deps: vec![],
        files: vec![],
        prompt: "".into(),
        worktree: false,
        task_id: "".into(),
        brain: "".into(),
        brain_name: "".into(),
        model: "".into(),
        status: "pending".into(),
        summary: "".into(),
        tokens: 0,
        usd: 0.0,
        error: "".into(),
        wave: 0,
        started_at: 0.0,
        finished_at: 0.0,
    };
    let pick1 = assign_brain(&b1, &brains, &chain, &queue, &active, &recent, "A").unwrap();
    assert_eq!(pick1.s("id"), "G");
    active.insert("G".into(), 1);

    // Tier 3 exploration -> DeepSeek (runs concurrently with Gemini!)
    let mut b2 = b1.clone();
    b2.id = "s2".into();
    b2.tier = 3;
    b2.role = "explore".into();
    let pick2 = assign_brain(&b2, &brains, &chain, &queue, &active, &recent, "A").unwrap();
    assert_eq!(pick2.s("id"), "DS");

    // Another Tier 2 sibling in the same wave avoids busy Gemini and spreads to another free brain.
    let pick3 = assign_brain(&b1, &brains, &chain, &queue, &active, &recent, "A").unwrap();
    assert_ne!(pick3.s("id"), "G");

    // Tier 1 build synthesis -> Claude (never DeepSeek since OpenAI cannot edit).
    let mut b4 = b1.clone();
    b4.id = "impl".into();
    b4.tier = 1;
    b4.role = "implement".into();
    b4.mode = "build".into();
    let pick4 = assign_brain(&b4, &brains, &chain, &queue, &HashMap::new(), &recent, "A").unwrap();
    assert_eq!(pick4.s("id"), "A");
    assert!(can_edit(&pick4.s("kind")));
}

#[test]
fn parallel_build_replaces_multiple_writers_with_one_final_implementation() {
    let spec = PlanSpec {
        strategy: "dag".into(),
        nodes: vec![
            PlanNode {
                id: "s1".into(),
                title: "Edit 1".into(),
                prompt: "e1".into(),
                deps: vec![],
                tier: 2,
                mode: "build".into(),
                role: "implement".into(),
                files: vec![],
            },
            PlanNode {
                id: "s2".into(),
                title: "Edit 2".into(),
                prompt: "e2".into(),
                deps: vec![],
                tier: 2,
                mode: "build".into(),
                role: "implement".into(),
                files: vec![],
            },
        ],
    };
    let safe = validate_and_finalize_dag(spec, "edita", "build", 3);
    assert_eq!(safe.nodes[0].mode, "plan");
    assert_eq!(safe.nodes[1].mode, "plan");
    assert_eq!(safe.nodes.last().unwrap().mode, "build");
    assert_eq!(safe.nodes.last().unwrap().deps, vec!["s1", "s2"]);
}

#[test]
fn parallel_writer_receives_the_whole_request_and_waits_for_independent_branches() {
    let spec = parse_plan_json(
        r#"{"nodes":[
        {"id":"a","prompt":"Analiza A"},
        {"id":"b","prompt":"Analiza B"},
        {"id":"write","prompt":"Implementa A","mode":"build","deps":["a"]}
    ]}"#,
    )
    .unwrap();
    let request = "A".repeat(3000);
    let plan = validate_and_finalize_dag(spec, &request, "build", 2);
    let writer = plan.nodes.iter().find(|n| n.mode == "build").unwrap();
    assert_eq!(writer.deps, ["a", "b"]);
    assert_eq!(writer.role, "implement");
    assert!(writer.prompt.ends_with(&request));
}

#[test]
fn aorchestra_prompt_injects_compact_dependency_summaries() {
    let br = BranchState {
        id: "synth".into(),
        title: "Síntesis".into(),
        role: "synthesize".into(),
        tier: 1,
        mode: "plan".into(),
        deps: vec!["s1".into(), "s2".into()],
        files: vec!["src/core.rs".into()],
        prompt: "Unifica todo".into(),
        worktree: false,
        task_id: "".into(),
        brain: "".into(),
        brain_name: "".into(),
        model: "".into(),
        status: "pending".into(),
        summary: "".into(),
        tokens: 0,
        usd: 0.0,
        error: "".into(),
        wave: 1,
        started_at: 0.0,
        finished_at: 0.0,
    };
    let deps = vec![
        ("Diseño".into(), "Gemini Neon".into(), "Hallazgo A".into()),
        ("Crítica".into(), "DeepSeek".into(), "Hallazgo B".into()),
    ];
    let prompt = build_branch_prompt("Pedido", &br, &deps, &[], false);
    assert!(prompt.contains("Archivos foco: src/core.rs"));
    assert!(prompt.contains("Diseño (by Gemini Neon)"));
    assert!(prompt.contains("<untrusted_worker_output>"));
    assert!(prompt.contains("Hallazgo A"));
    assert!(prompt.contains("Crítica (by DeepSeek)"));
    assert!(prompt.contains("Hallazgo B"));
    let deps = vec![
        ("A".into(), "uno".into(), "α".repeat(3000)),
        ("B".into(), "dos".into(), "β".repeat(3000)),
    ];
    let prompt = build_branch_prompt("Pedido", &br, &deps, &[], false);
    let summary_chars = prompt.chars().filter(|c| matches!(c, 'α' | 'β')).count();
    assert!(summary_chars <= SYNTH_DEP_BUDGET);
}

#[test]
fn parse_plan_json_accepts_wrapped_json_and_clamps_tiers() {
    let raw = r#"Aquí está el plan:
    {"strategy":"dag","nodes":[
        {"id":"s1","title":"Explorar","prompt":"Lee el repo","deps":[],"tier":9,"mode":"plan","role":"explore"},
        {"id":"s2","title":"Sintetizar","prompt":"Resume","deps":["s1"],"tier":0,"mode":"plan","role":"synthesize"}
    ]}"#;
    let parsed = parse_plan_json(raw).expect("valid plan");
    assert_eq!(parsed.strategy, "dag");
    assert_eq!(parsed.nodes.len(), 2);
    assert_eq!(parsed.nodes[0].id, "s1");
    let fixed = validate_and_finalize_dag(parsed, "p", "plan", 2);
    assert_eq!(fixed.nodes[0].tier, 3);
    assert_eq!(fixed.nodes[1].tier, 1);
}

#[test]
fn single_brain_uses_direct_strategy_without_overhead() {
    let one = vec![sample_brains()[3].clone()];
    let plan = heuristic_plan("hola", "plan", &one);
    assert_eq!(plan.strategy, "direct");
    assert_eq!(plan.nodes.len(), 1);
    assert_eq!(plan.nodes[0].tier, 3);
}
