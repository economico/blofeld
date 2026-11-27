// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use crate::mcp::{Action, Discovery, InputKind, McpClient, RpcOutcome, Target, ToolDef};
use crate::properties::{RuleSet, Violation, check_builtin_step};
use crate::schema::{ValuePool, extract_response_bindings, generate_tool_args, substitute_values};
use crate::spec::{CompiledProbeAfter, CompiledSpec, SpecEngine};
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
    /// Optional CLI override for adversarial state-machine fuzzing probability (`0.0..=1.0`).
    pub adversarial_rate: Option<f64>,
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
    /// Response value bindings recorded during `init:` hooks so `--replay` can remap
    /// session-scoped random IDs/UUIDs across fresh sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init_produced: Vec<Vec<(String, Value)>>,
    /// Per-step response value bindings recorded during execution so `--replay` can remap
    /// session-scoped random IDs/UUIDs across fresh sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_produced: Vec<Vec<(String, Value)>>,
}

impl SavedTrace {
    pub fn from_failure(target: Target, fail: &FailureReport) -> Self {
        Self {
            target,
            seed: fail.seed,
            violation: fail.violation.clone(),
            original_steps: fail.original_len,
            actions: fail.shrunk_actions.clone(),
            init_produced: fail.init_produced.clone(),
            step_produced: fail.shrunk_produced.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FailureReport {
    pub run_index: usize,
    pub seed: u64,
    pub violation: Violation,
    pub original_len: usize,
    pub shrunk_actions: Vec<Action>,
    pub init_produced: Vec<Vec<(String, Value)>>,
    pub shrunk_produced: Vec<Vec<(String, Value)>>,
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
    init_produced: Vec<Vec<(String, Value)>>,
    entries: Vec<TraceEntry>,
    serialized_size: usize,
    serialized_key: String,
    failing_response: Option<Value>,
}

fn is_tool_read_only(discovery: &Discovery, tool_name: &str) -> bool {
    discovery
        .tools
        .iter()
        .find(|t| t.name == tool_name)
        .and_then(|t| t.annotations.as_ref())
        .and_then(|a| a.read_only_hint)
        .unwrap_or(false)
}

fn validate_read_only_spec(spec: &CompiledSpec, discovery: &Discovery) -> Result<()> {
    for step in &spec.init {
        if !is_tool_read_only(discovery, &step.tool) {
            anyhow::bail!(
                "--read-only forbids executing init '{}' calling tool '{}' without readOnlyHint: true",
                step.name,
                step.tool
            );
        }
    }
    for probe in &spec.probes {
        if matches!(probe.after, CompiledProbeAfter::Never) {
            continue;
        }
        if !is_tool_read_only(discovery, &probe.tool) {
            anyhow::bail!(
                "--read-only forbids executing probe '{}' calling tool '{}' without readOnlyHint: true",
                probe.name,
                probe.tool
            );
        }
    }
    Ok(())
}

pub async fn explore(cfg: &RunConfig) -> Result<ExplorationSummary> {
    let started = Instant::now();
    if let Some(rate) = cfg.adversarial_rate
        && (!rate.is_finite() || !(0.0..=1.0).contains(&rate))
    {
        anyhow::bail!("--adversarial-rate must be between 0.0 and 1.0, got {rate}");
    }
    let compiled_spec = cfg.rules.compile()?;

    // Probe discovery once up front so we fail fast on unreachable servers and signal setup_complete.
    let mut probe = McpClient::connect(&cfg.target, cfg.timeout).await?;
    let (initial_discovery, _) = probe.initialize_and_discover().await?;
    probe.close().await;

    if cfg.read_only {
        validate_read_only_spec(&compiled_spec, &initial_discovery)?;
    }

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
        let spec_for_hegel = compiled_spec.clone();

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
                        &spec_for_hegel,
                        &tc,
                        run_idx,
                        &steps_for_hegel,
                        step_deadline,
                    ));

                    match res {
                        Ok(None) => {}
                        Ok(Some((violation, init_produced, entries, failing_response))) => {
                            let serialized_key: String = entries
                                .iter()
                                .map(|e| serde_json::to_string(&e.action).unwrap_or_default())
                                .collect::<Vec<_>>()
                                .join("\n");
                            let serialized_size = serialized_key.len();

                            let mut guard = captured_for_hegel.lock().unwrap();
                            let should_replace = match guard.as_ref() {
                                None => true,
                                Some(prev) => {
                                    (entries.len(), serialized_size, &serialized_key)
                                        < (
                                            prev.entries.len(),
                                            prev.serialized_size,
                                            &prev.serialized_key,
                                        )
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
                                    init_produced,
                                    entries,
                                    serialized_size,
                                    serialized_key,
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
            let shrunk = if cfg.shrink && !cfg.antithesis_mode && fail.entries.len() > 1 {
                shrink_trace(
                    cfg,
                    &compiled_spec,
                    fail.init_produced,
                    fail.entries,
                    fail.violation,
                    fail.failing_response,
                )
                .await
            } else {
                ReproducedTrace {
                    init_produced: fail.init_produced,
                    entries: fail.entries,
                    violation: fail.violation,
                    failing_response: fail.failing_response,
                }
            };

            let (shrunk_actions, shrunk_produced): (Vec<Action>, Vec<Vec<(String, Value)>>) =
                shrunk
                    .entries
                    .into_iter()
                    .map(|e| (e.action, e.produced))
                    .unzip();

            Some(FailureReport {
                run_index: fail.run_index,
                seed: cfg.seed,
                violation: shrunk.violation,
                original_len: fail.original_len,
                shrunk_actions,
                init_produced: shrunk.init_produced,
                shrunk_produced,
                failing_response: shrunk.failing_response,
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

fn outcome_status_str(outcome: &RpcOutcome) -> String {
    if let Some(err) = outcome.transport_error.as_deref() {
        err.to_string()
    } else if let Some(code) = outcome.http_status.filter(|c| *c >= 400) {
        format!("http_{code}")
    } else if outcome.error().is_some() {
        "rpc_error".to_string()
    } else if outcome.is_tool_error() {
        "tool_error".to_string()
    } else {
        "ok".to_string()
    }
}

struct InitResult {
    produced: Vec<Vec<(String, Value)>>,
    failure: Option<(Violation, Option<Value>)>,
}

async fn run_init_hooks(
    client: &mut McpClient,
    engine: &mut SpecEngine,
    discovery: &Discovery,
    mut pool: Option<&mut ValuePool>,
    total_steps: Option<&AtomicUsize>,
    cfg: &RunConfig,
) -> InitResult {
    let mut produced = Vec::with_capacity(engine.spec().init.len());
    for idx in 0..engine.spec().init.len() {
        let step_name = engine.spec().init[idx].name.clone();
        let action = match engine.prepare_init_step(idx, cfg.verbose) {
            Ok(Some(a)) => a,
            Ok(None) => {
                produced.push(Vec::new());
                continue;
            }
            Err(err) => {
                return InitResult {
                    produced,
                    failure: Some((
                        Violation {
                            property: format!("init:{step_name}"),
                            message: format!("failed preparing init step: {err:#}"),
                        },
                        None,
                    )),
                };
            }
        };

        if cfg.read_only
            && let Action::CallTool { name, .. } = &action
            && !is_tool_read_only(discovery, name)
        {
            return InitResult {
                produced,
                failure: Some((
                    Violation {
                        property: format!("init:{step_name}"),
                        message: format!(
                            "--read-only forbids executing init tool '{name}' without readOnlyHint: true"
                        ),
                    },
                    None,
                )),
            };
        }

        let outcome = client.execute(&action).await;
        if let Some(counter) = total_steps {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if cfg.verbose {
            eprintln!(
                "  [init {}] {} -> {} ({}ms)",
                step_name,
                action.short_label(),
                outcome_status_str(&outcome),
                outcome.latency.as_millis()
            );
        }

        produced.push(extract_response_bindings(&outcome));
        if let Some(p) = pool.as_deref_mut() {
            p.harvest_outcome(&outcome);
        }

        let builtin = check_builtin_step(&action, &outcome, discovery);
        if let Some(v) = builtin.into_iter().next() {
            return InitResult {
                produced,
                failure: Some((v, outcome.raw_response)),
            };
        }

        let init_violations = engine.finish_init_step(idx, &action, &outcome, cfg.verbose);
        if let Some(v) = init_violations.into_iter().next() {
            return InitResult {
                produced,
                failure: Some((v, outcome.raw_response)),
            };
        }
    }

    InitResult {
        produced,
        failure: None,
    }
}

struct StepRuntime<'a> {
    discovery: &'a Discovery,
    total_steps: Option<&'a AtomicUsize>,
    read_only: bool,
    verbose: bool,
}

/// Evaluate one executed step across all four phases:
/// 1. Built-in zero-config MCP invariants
/// 2. Built-in rule checks + CEL `rules:` (against pre-transition `state`)
/// 3. Matching `transitions:` (assertions + `delete`/`update` shadow `state`)
/// 4. Active `probes:` (against post-transition `state`, evaluated sequentially so later probes
///    observe any `update:`/`delete:` applied by earlier probes in the same step)
async fn evaluate_step_and_probes(
    client: &mut McpClient,
    engine: &mut SpecEngine,
    action: &Action,
    outcome: &RpcOutcome,
    rt: &StepRuntime<'_>,
    step_num: usize,
) -> std::result::Result<(), (Vec<Violation>, Option<Value>)> {
    // Phase 1: Built-in protocol invariants
    let builtin = check_builtin_step(action, outcome, rt.discovery);
    if !builtin.is_empty() {
        return Err((builtin, outcome.raw_response.clone()));
    }
    if outcome.transport_error.is_some() {
        return Ok(());
    }

    // Phase 2: Declarative rules (legacy + CEL expressions against pre-transition state)
    let rule_violations = engine.check_rules(action, outcome, step_num, rt.verbose);
    if !rule_violations.is_empty() {
        return Err((rule_violations, outcome.raw_response.clone()));
    }

    // Phase 3: State-machine transitions (asserts + state update/delete)
    let transition_violations = engine.apply_transitions(action, outcome, step_num, rt.verbose);
    if !transition_violations.is_empty() {
        return Err((transition_violations, outcome.raw_response.clone()));
    }

    // Phase 4: Active probes against post-transition state (sequentially)
    for probe_idx in 0..engine.spec().probes.len() {
        let Some(probe) = engine
            .prepare_probe(
                probe_idx,
                action,
                outcome,
                rt.discovery,
                step_num,
                rt.verbose,
            )
            .map_err(|v| (vec![v], outcome.raw_response.clone()))?
        else {
            continue;
        };

        if rt.read_only
            && let Action::CallTool { name, .. } = &probe.action
            && !is_tool_read_only(rt.discovery, name)
        {
            return Err((
                vec![Violation {
                    property: format!("probe:{}", probe.name),
                    message: format!(
                        "--read-only forbids executing probe tool '{name}' without readOnlyHint: true"
                    ),
                }],
                None,
            ));
        }

        let probe_outcome = client.execute(&probe.action).await;
        if let Some(counter) = rt.total_steps {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if rt.verbose {
            eprintln!(
                "  [probe {}] {} -> {} ({}ms)",
                probe.name,
                probe.action.short_label(),
                outcome_status_str(&probe_outcome),
                probe_outcome.latency.as_millis()
            );
        }

        let probe_builtin = check_builtin_step(&probe.action, &probe_outcome, rt.discovery);
        if !probe_builtin.is_empty() {
            return Err((probe_builtin, probe_outcome.raw_response.clone()));
        }

        let probe_violations = engine.finish_probe(&probe, &probe_outcome, step_num, rt.verbose);
        if !probe_violations.is_empty() {
            return Err((probe_violations, probe_outcome.raw_response.clone()));
        }
    }

    Ok(())
}

fn state_complexity(state: &serde_json::Map<String, Value>) -> usize {
    fn count(v: &Value, depth: usize) -> usize {
        if depth > 4 {
            return 1;
        }
        match v {
            Value::Object(m) => 1 + m.values().map(|c| count(c, depth + 1)).sum::<usize>(),
            Value::Array(a) => 1 + a.iter().map(|c| count(c, depth + 1)).sum::<usize>(),
            _ => 1,
        }
    }
    state.values().map(|v| count(v, 0)).sum()
}

type EpisodeFailure = (
    Violation,
    Vec<Vec<(String, Value)>>,
    Vec<TraceEntry>,
    Option<Value>,
);

async fn run_single_episode(
    cfg: &RunConfig,
    compiled_spec: &CompiledSpec,
    tc: &TestCase,
    run_idx: usize,
    total_steps: &AtomicUsize,
    deadline: Option<Instant>,
) -> Result<Option<EpisodeFailure>> {
    let mut client = McpClient::connect(&cfg.target, cfg.timeout).await?;
    let (discovery, _) = client.initialize_and_discover().await?;
    let tools = filter_tools(&discovery.tools, cfg);

    let mut pool = ValuePool::default();
    let uris: Vec<String> = discovery.resources.iter().map(|r| r.uri.clone()).collect();
    pool.seed_resource_uris(&uris);

    let mut engine = SpecEngine::new(compiled_spec.clone());

    let init_res = run_init_hooks(
        &mut client,
        &mut engine,
        &discovery,
        Some(&mut pool),
        Some(total_steps),
        cfg,
    )
    .await;
    if let Some((violation, failing_response)) = init_res.failure {
        client.close().await;
        return Ok(Some((
            violation,
            init_res.produced,
            Vec::new(),
            failing_response,
        )));
    }
    let init_produced = init_res.produced;

    // Bootstrap zero-required-arg read-only tools (e.g. `businesses`) once per session,
    // controlled by a shrinkable boolean so Hegel can omit bootstrap when not needed.
    let mut bootstrap_queue: Vec<Action> = if compiled_spec.actions_only {
        Vec::new()
    } else {
        tools
            .iter()
            .filter(|t| is_zero_arg_readonly(t))
            .map(|t| Action::CallTool {
                name: t.name.clone(),
                arguments: json!({}),
                input_kind: InputKind::Valid,
            })
            .collect()
    };

    let max_steps = cfg.steps.max(1);
    let mut trace: Vec<TraceEntry> = Vec::with_capacity(max_steps);
    let adversarial_rate = cfg
        .adversarial_rate
        .unwrap_or(compiled_spec.adversarial_rate);

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
            let adversarial_action = if !compiled_spec.actions.is_empty()
                && adversarial_rate > 0.0
                && tc.draw_silent(gs::weighted_booleans(adversarial_rate))
            {
                let history: Vec<Action> = trace.iter().map(|e| e.action.clone()).collect();
                engine.try_pick_adversarial_action(
                    &tools,
                    &pool,
                    &history,
                    tc,
                    step_idx + 1,
                    cfg.verbose,
                )
            } else {
                None
            };

            if let Some(adv) = adversarial_action {
                adv
            } else {
                match engine.try_pick_action(&tools, &pool, tc, step_idx + 1, cfg.verbose) {
                    Ok(Some(spec_action)) => spec_action,
                    Ok(None) if compiled_spec.actions_only && !compiled_spec.actions.is_empty() => {
                        tc.stop_span(false);
                        break;
                    }
                    Ok(None) => pick_next_action(&tools, &discovery, &pool, tc),
                    Err(violation) => {
                        tc.stop_span(false);
                        client.close().await;
                        return Ok(Some((violation, init_produced, trace, None)));
                    }
                }
            }
        };
        tc.stop_span(false);

        let outcome = client.execute(&action).await;
        total_steps.fetch_add(1, Ordering::Relaxed);

        if cfg.verbose {
            eprintln!(
                "  [run {} step {}] {} -> {} ({}ms)",
                run_idx + 1,
                step_idx + 1,
                action.short_label(),
                outcome_status_str(&outcome),
                outcome.latency.as_millis()
            );
        }

        let step_rt = StepRuntime {
            discovery: &discovery,
            total_steps: Some(total_steps),
            read_only: cfg.read_only,
            verbose: cfg.verbose,
        };
        let eval = evaluate_step_and_probes(
            &mut client,
            &mut engine,
            &action,
            &outcome,
            &step_rt,
            step_idx + 1,
        )
        .await;

        let produced = extract_response_bindings(&outcome);
        trace.push(TraceEntry { action, produced });
        pool.harvest_outcome(&outcome);

        if let Err((violations, failing_response)) = eval
            && let Some(violation) = violations.into_iter().next()
        {
            client.close().await;
            return Ok(Some((violation, init_produced, trace, failing_response)));
        }
    }

    // Guide Hegel's targeted search toward sessions that uncover richer server/model state.
    tc.target_labelled(pool.total_values() as f64, "harvested_values");
    if compiled_spec.has_stateful_features() {
        tc.target_labelled(state_complexity(engine.state()) as f64, "spec_state_size");
    }

    client.close().await;
    Ok(None)
}

struct ReproducedTrace {
    init_produced: Vec<Vec<(String, Value)>>,
    entries: Vec<TraceEntry>,
    violation: Violation,
    failing_response: Option<Value>,
}

fn extend_substitutions(
    subs: &mut Vec<(Value, Value)>,
    orig_bindings: &[(String, Value)],
    live_bindings: &[(String, Value)],
) {
    for (path, orig_val) in orig_bindings {
        if let Some((_, live_val)) = live_bindings.iter().find(|(p, _)| p == path)
            && orig_val != live_val
        {
            subs.push((orig_val.clone(), live_val.clone()));
        }
    }
}

/// Delta-debugging (`ddmin`) trace shrinker with symbolic response-value remapping.
async fn shrink_trace(
    cfg: &RunConfig,
    compiled_spec: &CompiledSpec,
    init_produced: Vec<Vec<(String, Value)>>,
    entries: Vec<TraceEntry>,
    violation: Violation,
    failing_response: Option<Value>,
) -> ReproducedTrace {
    let target_property = violation.property.clone();
    if let Some(last) = entries.last() {
        let single = vec![last.clone()];
        if let Some(rep) = reproduces(
            cfg,
            compiled_spec,
            &init_produced,
            &single,
            &target_property,
        )
        .await
        {
            return rep;
        }
    }

    let mut current_init = init_produced;
    let mut current = entries;
    let mut current_violation = violation;
    let mut current_failing_response = failing_response;
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
                && let Some(rep) = reproduces(
                    cfg,
                    compiled_spec,
                    &current_init,
                    &candidate,
                    &target_property,
                )
                .await
            {
                // Keep the resolved actions AND their fresh init + step `produced` bindings so
                // subsequent ddmin iterations remap against the newly shortened session's IDs.
                current_init = rep.init_produced;
                current = rep.entries;
                current_violation = rep.violation;
                current_failing_response = rep.failing_response;
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

    ReproducedTrace {
        init_produced: current_init,
        entries: current,
        violation: current_violation,
        failing_response: current_failing_response,
    }
}

async fn reproduces(
    cfg: &RunConfig,
    compiled_spec: &CompiledSpec,
    init_produced: &[Vec<(String, Value)>],
    entries: &[TraceEntry],
    target_property: &str,
) -> Option<ReproducedTrace> {
    let mut client = McpClient::connect(&cfg.target, cfg.timeout).await.ok()?;
    let (discovery, _) = match client.initialize_and_discover().await {
        Ok(d) => d,
        Err(_) => {
            client.close().await;
            return None;
        }
    };

    let mut engine = SpecEngine::new(compiled_spec.clone());
    let mut subs: Vec<(Value, Value)> = Vec::new();

    let init_res = run_init_hooks(&mut client, &mut engine, &discovery, None, None, cfg).await;
    if let Some((v, resp)) = init_res.failure {
        client.close().await;
        return if v.property == target_property {
            Some(ReproducedTrace {
                init_produced: init_res.produced,
                entries: Vec::new(),
                violation: v,
                failing_response: resp,
            })
        } else {
            None
        };
    }
    for (orig_bindings, live_bindings) in init_produced.iter().zip(init_res.produced.iter()) {
        extend_substitutions(&mut subs, orig_bindings, live_bindings);
    }

    let mut resolved = Vec::with_capacity(entries.len());
    let mut matched_violation: Option<(Violation, Option<Value>)> = None;

    let step_rt = StepRuntime {
        discovery: &discovery,
        total_steps: None,
        read_only: cfg.read_only,
        verbose: cfg.verbose,
    };
    for (idx, entry) in entries.iter().enumerate() {
        let action = apply_substitutions(&entry.action, &subs);
        let outcome = client.execute(&action).await;
        let eval = evaluate_step_and_probes(
            &mut client,
            &mut engine,
            &action,
            &outcome,
            &step_rt,
            idx + 1,
        )
        .await;
        let live_bindings = extract_response_bindings(&outcome);
        resolved.push(TraceEntry {
            action,
            produced: live_bindings.clone(),
        });

        if let Err((violations, resp)) = eval {
            if let Some(v) = violations
                .into_iter()
                .find(|v| v.property == target_property)
            {
                matched_violation = Some((v, resp));
            }
            break;
        }

        extend_substitutions(&mut subs, &entry.produced, &live_bindings);
    }

    client.close().await;
    matched_violation.map(|(violation, failing_response)| ReproducedTrace {
        init_produced: init_res.produced,
        entries: resolved,
        violation,
        failing_response,
    })
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
    let compiled_spec = cfg.rules.compile()?;
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading trace {}", path.display()))?;
    let saved: SavedTrace = serde_json::from_str(&raw)
        .with_context(|| format!("failed parsing trace JSON {}", path.display()))?;

    let mut client = McpClient::connect(&cfg.target, cfg.timeout).await?;
    let (discovery, _) = client.initialize_and_discover().await?;
    if cfg.read_only {
        validate_read_only_spec(&compiled_spec, &discovery)?;
    }
    let mut engine = SpecEngine::new(compiled_spec);
    let mut subs: Vec<(Value, Value)> = Vec::new();

    let init_res = run_init_hooks(&mut client, &mut engine, &discovery, None, None, cfg).await;
    if let Some((v, _)) = init_res.failure {
        client.close().await;
        return Ok(Some((0, v)));
    }
    for (orig_bindings, live_bindings) in saved.init_produced.iter().zip(init_res.produced.iter()) {
        extend_substitutions(&mut subs, orig_bindings, live_bindings);
    }

    let step_rt = StepRuntime {
        discovery: &discovery,
        total_steps: None,
        read_only: cfg.read_only,
        verbose: cfg.verbose,
    };
    for (idx, orig_action) in saved.actions.iter().enumerate() {
        let action = apply_substitutions(orig_action, &subs);
        let outcome = client.execute(&action).await;
        if cfg.verbose {
            eprintln!("  [replay {}] {}", idx + 1, action.short_label());
        }
        let eval = evaluate_step_and_probes(
            &mut client,
            &mut engine,
            &action,
            &outcome,
            &step_rt,
            idx + 1,
        )
        .await;
        if let Err((violations, _)) = eval
            && let Some(v) = violations.into_iter().next()
        {
            client.close().await;
            return Ok(Some((idx + 1, v)));
        }
        if let Some(orig_bindings) = saved.step_produced.get(idx) {
            let live_bindings = extract_response_bindings(&outcome);
            extend_substitutions(&mut subs, orig_bindings, &live_bindings);
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
