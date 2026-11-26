// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use crate::mcp::{Action, Discovery, InputKind, McpClient, Target, ToolDef};
use crate::properties::{RuleSet, Violation, check_step};
use crate::schema::{ValuePool, extract_response_bindings, generate_tool_args, substitute_values};
use antithesis_sdk::lifecycle;
use anyhow::{Context, Result};
use hegel::generators as gs;
use hegel::{
    Backend, Database, HealthCheck, Hegel, NondeterminismStrictness, Phase, Settings, TestCase,
    Verbosity,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

const STEP_SPAN_LABEL: u64 = 0xB10F_0001;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub target: Target,
    /// Maximum number of exploration runs (`None` means unlimited until `time_limit` expires).
    pub runs: Option<usize>,
    pub steps: usize,
    /// Optional wall-clock exploration budget (e.g. `--time-limit 5m`).
    pub time_limit: Option<Duration>,
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

/// Parse a Bombadil-style duration string like `500ms`, `30s`, `5m`, `1h`, or `1d`.
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("duration cannot be empty; expected e.g. 500ms, 30s, 5m, 1h, or 1d");
    }
    let (num_str, mult_ms): (&str, u64) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1_000)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60_000)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3_600_000)
    } else if let Some(v) = s.strip_suffix('d') {
        (v, 86_400_000)
    } else {
        anyhow::bail!(
            "invalid duration '{raw}': include a unit suffix (ms, s, m, h, or d), e.g. 30s or 5m"
        );
    };
    let n: u64 = num_str
        .trim()
        .parse()
        .with_context(|| format!("invalid duration number in '{raw}'"))?;
    if n == 0 {
        anyhow::bail!("duration must be greater than zero");
    }
    let total_ms = n
        .checked_mul(mult_ms)
        .ok_or_else(|| anyhow::anyhow!("duration '{raw}' overflows"))?;
    Ok(Duration::from_millis(total_ms))
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

#[derive(Debug, Clone)]
struct TraceEntry {
    action: Action,
    produced: Vec<(String, Value)>,
}

#[derive(Debug, Clone)]
struct CapturedFailure {
    run_index: usize,
    violation: Violation,
    original_len: usize,
    entries: Vec<TraceEntry>,
    serialized_size: usize,
    failing_response: Option<Value>,
}

pub async fn explore(cfg: &RunConfig) -> Result<ExplorationSummary> {
    let started = Instant::now();

    // Probe discovery once up front so we fail fast on unreachable servers and signal setup_complete.
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

    let deadline = cfg.time_limit.map(|limit| started + limit);
    let max_runs = cfg.runs.unwrap_or(usize::MAX);
    let runs_counter = Arc::new(AtomicUsize::new(0));
    let total_steps = Arc::new(AtomicUsize::new(0));
    let captured: Arc<Mutex<Option<CapturedFailure>>> = Arc::new(Mutex::new(None));

    let mut phases = vec![Phase::Generate, Phase::Target];
    if cfg.shrink && !cfg.antithesis_mode {
        phases.push(Phase::Shrink);
    }

    let mut batch_idx: u64 = 0;
    while runs_counter.load(Ordering::Relaxed) < max_runs
        && deadline.is_none_or(|d| Instant::now() < d)
    {
        let remaining = max_runs.saturating_sub(runs_counter.load(Ordering::Relaxed));
        if remaining == 0 {
            break;
        }
        // When running under a wall-clock time limit, chunk Hegel into small batches so
        // expiration is checked cleanly between batches without abandoning a huge run budget.
        let batch_cases = if deadline.is_some() {
            remaining.min(10)
        } else {
            remaining
        };
        let batch_seed = cfg
            .seed
            .wrapping_add(batch_idx.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        batch_idx += 1;

        let handle = Handle::current();
        let runs_for_hegel = Arc::clone(&runs_counter);
        let steps_for_hegel = Arc::clone(&total_steps);
        let captured_for_hegel = Arc::clone(&captured);
        let cfg_for_hegel = cfg.clone();

        let settings = Settings::new()
            .test_cases(batch_cases.max(1) as u64)
            .seed(Some(batch_seed))
            .database(Database::Disabled)
            .print_blob(false)
            .verbosity(Verbosity::Quiet)
            .nondeterminism_strictness(NondeterminismStrictness::Quiet)
            .suppress_health_check(HealthCheck::all())
            .backend(if cfg.antithesis_mode {
                Backend::Urandom
            } else {
                Backend::Default
            })
            .phases(phases.clone());

        // Run Hegel's native Hypothesis engine (libhegel) on a blocking worker thread,
        // driving async MCP I/O through the Tokio runtime handle.
        tokio::task::spawn_blocking(move || {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                Hegel::new(move |tc: TestCase| {
                    // If time limit expired and we aren't actively shrinking a found failure,
                    // return immediately so this batch finishes promptly without failing.
                    if deadline.is_some_and(|d| Instant::now() >= d)
                        && captured_for_hegel.lock().unwrap().is_none()
                    {
                        return;
                    }

                    let run_idx = runs_for_hegel.fetch_add(1, Ordering::Relaxed);
                    let shrinking = captured_for_hegel.lock().unwrap().is_some();
                    let step_deadline = if shrinking { None } else { deadline };
                    let res = handle.block_on(run_single_episode(
                        &cfg_for_hegel,
                        &tc,
                        run_idx,
                        &steps_for_hegel,
                        step_deadline,
                    ));

                    match res {
                        Ok(None) => {}
                        Ok(Some((violation, entries, failing_response))) => {
                            let serialized_size: usize = entries
                                .iter()
                                .map(|e| {
                                    serde_json::to_string(&e.action)
                                        .map(|s| s.len())
                                        .unwrap_or(0)
                                })
                                .sum();

                            let mut guard = captured_for_hegel.lock().unwrap();
                            let should_replace = match guard.as_ref() {
                                None => true,
                                Some(prev) => {
                                    entries.len() < prev.entries.len()
                                        || (entries.len() == prev.entries.len()
                                            && serialized_size < prev.serialized_size)
                                }
                            };
                            let original_len = guard
                                .as_ref()
                                .map(|p| p.original_len)
                                .unwrap_or(entries.len());
                            let first_run = guard.as_ref().map(|p| p.run_index).unwrap_or(run_idx);

                            if should_replace {
                                *guard = Some(CapturedFailure {
                                    run_index: first_run,
                                    violation: violation.clone(),
                                    original_len,
                                    entries,
                                    serialized_size,
                                    failing_response,
                                });
                            }
                            drop(guard);

                            // Signal libhegel that this test case triggered an interesting bug
                            // so its shrinker minimizes both the action sequence and JSON arguments.
                            panic!("blofeld:{}", violation.property);
                        }
                        Err(err) => {
                            if cfg_for_hegel.verbose {
                                eprintln!("  [run {}] session connect error: {err}", run_idx + 1);
                            }
                            tc.assume(false);
                        }
                    }
                })
                .settings(settings)
                .run();
            }));
        })
        .await
        .context("hegel exploration task panicked")?;

        if captured.lock().unwrap().is_some() {
            break;
        }
    }

    let captured_failure = captured.lock().unwrap().take();
    let failure = match captured_failure {
        None => None,
        Some(fail) => {
            // Polish with symbolic-ID-remapping ddmin as a fast safety net in case any
            // residual prefix step can still be dropped after Hegel shrinks the payloads.
            let shrunk_actions = if cfg.shrink && !cfg.antithesis_mode && fail.entries.len() > 1 {
                shrink_trace(cfg, &fail.entries, &fail.violation.property).await
            } else {
                fail.entries.into_iter().map(|e| e.action).collect()
            };

            Some(FailureReport {
                run_index: fail.run_index,
                seed: cfg.seed,
                violation: fail.violation,
                original_len: fail.original_len,
                shrunk_actions,
                failing_response: fail.failing_response,
            })
        }
    };

    Ok(ExplorationSummary {
        discovered_tools: initial_discovery.tools.len(),
        discovered_resources: initial_discovery.resources.len(),
        discovered_prompts: initial_discovery.prompts.len(),
        runs_completed: runs_counter.load(Ordering::Relaxed).min(max_runs),
        total_steps: total_steps.load(Ordering::Relaxed),
        elapsed: started.elapsed(),
        failure,
    })
}

async fn run_single_episode(
    cfg: &RunConfig,
    tc: &TestCase,
    run_idx: usize,
    total_steps: &AtomicUsize,
    deadline: Option<Instant>,
) -> Result<Option<(Violation, Vec<TraceEntry>, Option<Value>)>> {
    let mut client = McpClient::connect(&cfg.target, cfg.timeout).await?;
    let (discovery, _) = client.initialize_and_discover().await?;
    let tools = filter_tools(&discovery.tools, cfg);

    let mut pool = ValuePool::default();
    let uris: Vec<String> = discovery.resources.iter().map(|r| r.uri.clone()).collect();
    pool.seed_resource_uris(&uris);

    // Bootstrap zero-required-arg read-only tools (e.g. `businesses`) once per session,
    // controlled by a shrinkable boolean so Hegel can omit bootstrap when not needed.
    let mut bootstrap_queue: Vec<Action> = tools
        .iter()
        .filter(|t| is_zero_arg_readonly(t))
        .map(|t| Action::CallTool {
            name: t.name.clone(),
            arguments: json!({}),
            input_kind: InputKind::Valid,
        })
        .collect();

    let max_steps = cfg.steps.max(1);
    let mut trace: Vec<TraceEntry> = Vec::with_capacity(max_steps);

    for step_idx in 0..max_steps {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        // After the first step, let Hegel choose whether to stop early so shrinking
        // naturally shortens traces.
        if step_idx > 0 && !tc.draw_silent(gs::weighted_booleans(0.95)) {
            break;
        }

        tc.start_span(STEP_SPAN_LABEL);
        let action = if !bootstrap_queue.is_empty() && tc.draw_silent(gs::weighted_booleans(0.9)) {
            bootstrap_queue.remove(0)
        } else {
            pick_next_action(&tools, &discovery, &pool, tc)
        };
        tc.stop_span(false);

        let outcome = client.execute(&action).await;
        total_steps.fetch_add(1, Ordering::Relaxed);

        if cfg.verbose {
            let status_str = if let Some(err) = outcome.transport_error.as_deref() {
                err.to_string()
            } else if let Some(code) = outcome.http_status.filter(|c| *c >= 400) {
                format!("http_{code}")
            } else if outcome.error().is_some() {
                "rpc_error".to_string()
            } else if outcome.is_tool_error() {
                "tool_error".to_string()
            } else {
                "ok".to_string()
            };
            eprintln!(
                "  [run {} step {}] {} -> {} ({}ms)",
                run_idx + 1,
                step_idx + 1,
                action.short_label(),
                status_str,
                outcome.latency.as_millis()
            );
        }

        let violations = check_step(&action, &outcome, &discovery, &cfg.rules);
        let produced = extract_response_bindings(&outcome);
        trace.push(TraceEntry { action, produced });
        pool.harvest_outcome(&outcome);

        if let Some(violation) = violations.into_iter().next() {
            let failing_response = outcome.raw_response.clone();
            client.close().await;
            return Ok(Some((violation, trace, failing_response)));
        }
    }

    // Guide Hegel's targeted search toward sessions that uncover richer server state.
    tc.target_labelled(pool.total_values() as f64, "harvested_values");

    client.close().await;
    Ok(None)
}

/// Delta-debugging (`ddmin`) trace shrinker with symbolic response-value remapping.
async fn shrink_trace(
    cfg: &RunConfig,
    entries: &[TraceEntry],
    target_property: &str,
) -> Vec<Action> {
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

fn is_zero_arg_readonly(tool: &ToolDef) -> bool {
    let read_only = tool
        .annotations
        .as_ref()
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false);
    if !read_only {
        return false;
    }
    let has_required = tool
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());
    !has_required
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

fn pick_next_action(
    tools: &[ToolDef],
    discovery: &Discovery,
    pool: &ValuePool,
    tc: &TestCase,
) -> Action {
    let roll: u8 = tc.draw_silent(gs::integers::<u8>().min_value(0).max_value(99));

    // 78% tool calls (if tools exist)
    if !tools.is_empty() && roll < 78 {
        let idx = if tools.len() <= 1 {
            0
        } else {
            tc.draw_silent(
                gs::integers::<usize>()
                    .min_value(0)
                    .max_value(tools.len() - 1),
            )
        };
        let tool = &tools[idx];
        let kind_roll: u8 = tc.draw_silent(gs::integers::<u8>().min_value(0).max_value(99));
        let input_kind = if kind_roll < 65 {
            InputKind::Valid
        } else if kind_roll < 85 {
            InputKind::Boundary
        } else {
            InputKind::Malformed
        };
        let arguments = generate_tool_args(&tool.input_schema, pool, tc, input_kind);
        return Action::CallTool {
            name: tool.name.clone(),
            arguments,
            input_kind,
        };
    }

    // 10% resource reads (if resources exist)
    if !discovery.resources.is_empty() && roll < 88 {
        let idx = if discovery.resources.len() <= 1 {
            0
        } else {
            tc.draw_silent(
                gs::integers::<usize>()
                    .min_value(0)
                    .max_value(discovery.resources.len() - 1),
            )
        };
        return Action::ReadResource {
            uri: discovery.resources[idx].uri.clone(),
        };
    }

    // 6% prompt gets (if prompts exist)
    if !discovery.prompts.is_empty() && roll < 94 {
        let idx = if discovery.prompts.len() <= 1 {
            0
        } else {
            tc.draw_silent(
                gs::integers::<usize>()
                    .min_value(0)
                    .max_value(discovery.prompts.len() - 1),
            )
        };
        let p = &discovery.prompts[idx];
        let mut args = serde_json::Map::new();
        for arg in &p.arguments {
            if arg.required || tc.draw_silent(gs::booleans()) {
                let s = tc.draw_silent(gs::text().max_size(16));
                args.insert(arg.name.clone(), json!(s));
            }
        }
        return Action::GetPrompt {
            name: p.name.clone(),
            arguments: Value::Object(args),
        };
    }

    if tc.draw_silent(gs::booleans()) {
        Action::ListTools
    } else {
        Action::Ping
    }
}
