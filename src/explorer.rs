use crate::mcp::{Action, Discovery, InputKind, McpClient, Target, ToolDef};
use crate::properties::{RuleSet, Violation, check_step};
use crate::rng::ExplorerRng;
use crate::schema::{ValuePool, extract_response_bindings, generate_tool_args, substitute_values};
use antithesis_sdk::lifecycle;
use anyhow::{Context, Result};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub target: Target,
    pub runs: usize,
    pub steps: usize,
    pub seed: u64,
    pub timeout: Duration,
    pub rules: RuleSet,
    pub read_only: bool,
    pub only_tools: Vec<String>,
    pub exclude_tools: Vec<String>,
    pub shrink: bool,
    pub antithesis_mode: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTrace {
    pub target: Target,
    pub seed: u64,
    pub violation: Violation,
    pub original_steps: usize,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone)]
pub struct FailureReport {
    pub run_index: usize,
    pub seed: u64,
    pub violation: Violation,
    pub original_len: usize,
    pub shrunk_actions: Vec<Action>,
    pub failing_response: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ExplorationSummary {
    pub discovered_tools: usize,
    pub discovered_resources: usize,
    pub discovered_prompts: usize,
    pub runs_completed: usize,
    pub total_steps: usize,
    pub elapsed: Duration,
    pub failure: Option<FailureReport>,
}

pub async fn explore(cfg: &RunConfig) -> Result<ExplorationSummary> {
    let started = Instant::now();
    let mut total_steps = 0usize;

    // Probe discovery once up front so we fail fast on unreachable servers and signal setup_complete
    let mut probe = McpClient::connect(&cfg.target, cfg.timeout).await?;
    let (initial_discovery, _) = probe.initialize_and_discover().await?;
    probe.close().await;

    let active_tools = filter_tools(&initial_discovery.tools, cfg);
    lifecycle::setup_complete(&json!({
        "tools": active_tools.len(),
        "resources": initial_discovery.resources.len(),
        "prompts": initial_discovery.prompts.len(),
    }));

    if cfg.verbose {
        eprintln!(
            "discovered {} tool(s) ({} active), {} resource(s), {} prompt(s)",
            initial_discovery.tools.len(),
            active_tools.len(),
            initial_discovery.resources.len(),
            initial_discovery.prompts.len(),
        );
    }

    for run_idx in 0..cfg.runs {
        let run_seed = cfg
            .seed
            .wrapping_add((run_idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut rng = if cfg.antithesis_mode {
            ExplorerRng::antithesis()
        } else {
            ExplorerRng::seeded(run_seed)
        };

        let mut client = McpClient::connect(&cfg.target, cfg.timeout).await?;
        let (discovery, _) = client.initialize_and_discover().await?;
        let tools = filter_tools(&discovery.tools, cfg);

        let mut pool = ValuePool::default();
        let uris: Vec<String> = discovery.resources.iter().map(|r| r.uri.clone()).collect();
        pool.seed_resource_uris(&uris);

        let mut trace: Vec<TraceEntry> = Vec::with_capacity(cfg.steps);

        for step_idx in 0..cfg.steps {
            let action = pick_next_action(&tools, &discovery, &pool, &mut rng);
            let outcome = client.execute(&action).await;
            total_steps += 1;

            if cfg.verbose {
                eprintln!(
                    "  [run {} step {}] {} -> {} ({}ms)",
                    run_idx + 1,
                    step_idx + 1,
                    action.short_label(),
                    outcome
                        .transport_error
                        .as_deref()
                        .unwrap_or(if outcome.error().is_some() {
                            "rpc_error"
                        } else if outcome.is_tool_error() {
                            "tool_error"
                        } else {
                            "ok"
                        }),
                    outcome.latency.as_millis()
                );
            }

            let violations = check_step(&action, &outcome, &discovery, &cfg.rules);
            let produced = extract_response_bindings(&outcome);
            trace.push(TraceEntry { action, produced });
            pool.harvest_outcome(&outcome);

            if let Some(violation) = violations.into_iter().next() {
                let original_len = trace.len();
                let failing_response = outcome.raw_response.clone();
                client.close().await;

                let shrunk_actions = if cfg.shrink && !cfg.antithesis_mode && trace.len() > 1 {
                    shrink_trace(cfg, &trace, &violation.property).await
                } else {
                    trace.into_iter().map(|e| e.action).collect()
                };

                return Ok(ExplorationSummary {
                    discovered_tools: initial_discovery.tools.len(),
                    discovered_resources: initial_discovery.resources.len(),
                    discovered_prompts: initial_discovery.prompts.len(),
                    runs_completed: run_idx + 1,
                    total_steps,
                    elapsed: started.elapsed(),
                    failure: Some(FailureReport {
                        run_index: run_idx,
                        seed: run_seed,
                        violation,
                        original_len,
                        shrunk_actions,
                        failing_response,
                    }),
                });
            }
        }

        client.close().await;
    }

    Ok(ExplorationSummary {
        discovered_tools: initial_discovery.tools.len(),
        discovered_resources: initial_discovery.resources.len(),
        discovered_prompts: initial_discovery.prompts.len(),
        runs_completed: cfg.runs,
        total_steps,
        elapsed: started.elapsed(),
        failure: None,
    })
}

#[derive(Debug, Clone)]
struct TraceEntry {
    action: Action,
    produced: Vec<(String, Value)>,
}

/// Delta-debugging (`ddmin`) trace shrinker with symbolic response-value remapping:
/// finds a minimal subsequence of actions that reproduces the same property violation
/// even when the server issues fresh session-specific IDs (UUIDs, sequential IDs).
async fn shrink_trace(
    cfg: &RunConfig,
    entries: &[TraceEntry],
    target_property: &str,
) -> Vec<Action> {
    // Fast path: check if the last action alone reproduces the violation (stateless bug).
    if let Some(last) = entries.last() {
        let single = vec![last.clone()];
        if let Some(resolved) = reproduces(cfg, &single, target_property).await {
            return resolved;
        }
    }

    let mut current = entries.to_vec();
    let mut best_resolved: Option<Vec<Action>> = None;
    let mut granularity = 2usize;

    while current.len() > 1 {
        let chunk_size = current.len().div_ceil(granularity);
        let mut reduced = false;

        let mut i = 0;
        while i < current.len() {
            let end = (i + chunk_size).min(current.len());
            let mut candidate = Vec::with_capacity(current.len() - (end - i));
            candidate.extend_from_slice(&current[..i]);
            candidate.extend_from_slice(&current[end..]);

            if !candidate.is_empty()
                && let Some(resolved) = reproduces(cfg, &candidate, target_property).await
            {
                current = candidate;
                best_resolved = Some(resolved);
                granularity = granularity.saturating_sub(1).max(2);
                reduced = true;
                break;
            }
            i = end;
        }

        if !reduced {
            if granularity >= current.len() {
                break;
            }
            granularity = (granularity * 2).min(current.len());
        }
    }

    best_resolved.unwrap_or_else(|| current.into_iter().map(|e| e.action).collect())
}

async fn reproduces(
    cfg: &RunConfig,
    entries: &[TraceEntry],
    target_property: &str,
) -> Option<Vec<Action>> {
    let mut client = McpClient::connect(&cfg.target, cfg.timeout).await.ok()?;
    let (discovery, _) = match client.initialize_and_discover().await {
        Ok(d) => d,
        Err(_) => {
            client.close().await;
            return None;
        }
    };

    let mut subs: Vec<(Value, Value)> = Vec::new();
    let mut resolved = Vec::with_capacity(entries.len());
    let mut matched = false;

    for entry in entries {
        let action = apply_substitutions(&entry.action, &subs);
        let outcome = client.execute(&action).await;
        let violations = check_step(&action, &outcome, &discovery, &cfg.rules);
        resolved.push(action);

        if violations.iter().any(|v| v.property == target_property) {
            matched = true;
            break;
        }

        let live_bindings = extract_response_bindings(&outcome);
        for (path, orig_val) in &entry.produced {
            if let Some((_, live_val)) = live_bindings.iter().find(|(p, _)| p == path)
                && orig_val != live_val
            {
                subs.push((orig_val.clone(), live_val.clone()));
            }
        }
    }

    client.close().await;
    if matched { Some(resolved) } else { None }
}

fn apply_substitutions(action: &Action, subs: &[(Value, Value)]) -> Action {
    match action {
        Action::CallTool {
            name,
            arguments,
            input_kind,
        } => Action::CallTool {
            name: name.clone(),
            arguments: substitute_values(arguments, subs),
            input_kind: *input_kind,
        },
        Action::GetPrompt { name, arguments } => Action::GetPrompt {
            name: name.clone(),
            arguments: substitute_values(arguments, subs),
        },
        other => other.clone(),
    }
}

/// Replay a previously saved counterexample trace and return the first violation if any.
pub async fn replay_trace(cfg: &RunConfig, path: &Path) -> Result<Option<(usize, Violation)>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading trace {}", path.display()))?;
    let saved: SavedTrace = serde_json::from_str(&raw)
        .with_context(|| format!("failed parsing trace JSON {}", path.display()))?;

    let mut client = McpClient::connect(&cfg.target, cfg.timeout).await?;
    let (discovery, _) = client.initialize_and_discover().await?;

    for (idx, action) in saved.actions.iter().enumerate() {
        let outcome = client.execute(action).await;
        if cfg.verbose {
            eprintln!("  [replay {}] {}", idx + 1, action.short_label());
        }
        let violations = check_step(action, &outcome, &discovery, &cfg.rules);
        if let Some(v) = violations.into_iter().next() {
            client.close().await;
            return Ok(Some((idx + 1, v)));
        }
    }

    client.close().await;
    Ok(None)
}

pub fn save_trace(path: &Path, trace: &SavedTrace) -> Result<()> {
    let json = serde_json::to_string_pretty(trace)?;
    std::fs::write(path, json)
        .with_context(|| format!("failed writing trace to {}", path.display()))
}

fn filter_tools(tools: &[ToolDef], cfg: &RunConfig) -> Vec<ToolDef> {
    tools
        .iter()
        .filter(|t| {
            if cfg.read_only {
                let ro = t
                    .annotations
                    .as_ref()
                    .and_then(|a| a.read_only_hint)
                    .unwrap_or(false);
                if !ro {
                    return false;
                }
            }
            if !cfg.only_tools.is_empty() && !cfg.only_tools.contains(&t.name) {
                return false;
            }
            if cfg.exclude_tools.contains(&t.name) {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn pick_next_action<R: Rng>(
    tools: &[ToolDef],
    discovery: &Discovery,
    pool: &ValuePool,
    rng: &mut R,
) -> Action {
    let roll: u8 = rng.random_range(0..100);

    // 78% tool calls (if tools exist)
    if !tools.is_empty() && roll < 78 {
        let tool = &tools[rng.random_range(0..tools.len())];
        let kind_roll: u8 = rng.random_range(0..100);
        let input_kind = if kind_roll < 65 {
            InputKind::Valid
        } else if kind_roll < 85 {
            InputKind::Boundary
        } else {
            InputKind::Malformed
        };
        let arguments = generate_tool_args(&tool.input_schema, pool, rng, input_kind);
        return Action::CallTool {
            name: tool.name.clone(),
            arguments,
            input_kind,
        };
    }

    // 10% resource reads (if resources exist)
    if !discovery.resources.is_empty() && roll < 88 {
        let res = &discovery.resources[rng.random_range(0..discovery.resources.len())];
        return Action::ReadResource {
            uri: res.uri.clone(),
        };
    }

    // 6% prompt gets (if prompts exist)
    if !discovery.prompts.is_empty() && roll < 94 {
        let p = &discovery.prompts[rng.random_range(0..discovery.prompts.len())];
        let mut args = serde_json::Map::new();
        for arg in &p.arguments {
            if arg.required || rng.random_bool(0.5) {
                args.insert(arg.name.clone(), json!("sample"));
            }
        }
        return Action::GetPrompt {
            name: p.name.clone(),
            arguments: Value::Object(args),
        };
    }

    // Remaining: periodic tools/list consistency check or ping
    if rng.random_bool(0.5) {
        Action::ListTools
    } else {
        Action::Ping
    }
}
