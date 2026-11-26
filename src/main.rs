use anyhow::{Result, anyhow};
use blofeld::explorer::{RunConfig, SavedTrace, explore, replay_trace, save_trace};
use blofeld::mcp::Target;
use blofeld::properties::RuleSet;
use blofeld::rng::ExplorerRng;
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Blofeld — fast, zero-config property-based testing and autonomous fuzzing for MCP servers.
#[derive(Debug, Parser)]
#[command(name = "blofeld", version, about)]
struct Cli {
    /// MCP server Streamable HTTP URL (e.g. https://example.com/mcp or http://localhost:3000/mcp)
    target: Option<String>,

    /// Run a local stdio MCP server instead of an HTTP URL (pass command & args after --stdio)
    #[arg(long, num_args = 1.., value_name = "CMD")]
    stdio: Option<Vec<String>>,

    /// Extra HTTP headers for Streamable HTTP requests, e.g. -H "Authorization: Bearer ..."
    #[arg(short = 'H', long = "header", value_name = "KEY: VALUE")]
    headers: Vec<String>,

    /// Number of independent exploration sessions to run
    #[arg(long, default_value_t = 10)]
    runs: usize,

    /// Maximum MCP actions per exploration session
    #[arg(long, default_value_t = 40)]
    steps: usize,

    /// Deterministic PRNG seed (auto-generated and printed if omitted)
    #[arg(long)]
    seed: Option<u64>,

    /// Per-RPC timeout in milliseconds
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,

    /// Optional YAML rules file for custom invariants
    #[arg(long)]
    rules: Option<PathBuf>,

    /// Only call tools annotated with readOnlyHint: true
    #[arg(long)]
    read_only: bool,

    /// Restrict exploration to specific tool names (repeatable)
    #[arg(long = "only", value_name = "TOOL")]
    only_tools: Vec<String>,

    /// Exclude specific tool names from exploration (repeatable)
    #[arg(long = "exclude", value_name = "TOOL")]
    exclude_tools: Vec<String>,

    /// Replay a saved counterexample trace JSON file instead of fuzzing
    #[arg(long)]
    replay: Option<PathBuf>,

    /// Path to write the shrunk counterexample trace JSON on failure
    #[arg(long)]
    save_trace: Option<PathBuf>,

    /// Disable trace shrinking on failure
    #[arg(long)]
    no_shrink: bool,

    /// Drive randomness directly from Antithesis SDK (auto-enabled when ANTITHESIS_OUTPUT_DIR is set)
    #[arg(long)]
    antithesis: bool,

    /// Print every action executed
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    antithesis_sdk::antithesis_init();
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let target = build_target(&cli)?;
    let rules = match &cli.rules {
        Some(p) => RuleSet::load(p)?,
        None => RuleSet::default(),
    };
    let seed = cli.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x0B10_FE1D)
    });
    let antithesis_mode = cli.antithesis || ExplorerRng::in_antithesis_env();

    let cfg = RunConfig {
        target: target.clone(),
        runs: cli.runs,
        steps: cli.steps,
        seed,
        timeout: Duration::from_millis(cli.timeout_ms),
        rules,
        read_only: cli.read_only,
        only_tools: cli.only_tools,
        exclude_tools: cli.exclude_tools,
        shrink: !cli.no_shrink,
        antithesis_mode,
        verbose: cli.verbose,
    };

    if let Some(replay_path) = &cli.replay {
        println!("Replaying trace {}…", replay_path.display());
        return match replay_trace(&cfg, replay_path).await? {
            Some((step, violation)) => {
                println!(
                    "FAIL at step {step} [{}]: {}",
                    violation.property, violation.message
                );
                Ok(ExitCode::from(1))
            }
            None => {
                println!("PASS — replayed trace completed with no violations.");
                Ok(ExitCode::SUCCESS)
            }
        };
    }

    let summary = explore(&cfg).await?;

    match summary.failure {
        None => {
            println!(
                "PASS  {} run(s), {} step(s) in {:.2?} (seed={seed}) | discovered {} tool(s), {} resource(s), {} prompt(s)",
                summary.runs_completed,
                summary.total_steps,
                summary.elapsed,
                summary.discovered_tools,
                summary.discovered_resources,
                summary.discovered_prompts,
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(fail) => {
            println!(
                "FAIL  property '{}' violated on run {} (seed={})",
                fail.violation.property,
                fail.run_index + 1,
                fail.seed
            );
            println!("      {}", fail.violation.message);
            println!(
                "\nMinimal counterexample ({} step(s), shrunk from {}):",
                fail.shrunk_actions.len(),
                fail.original_len
            );
            for (i, action) in fail.shrunk_actions.iter().enumerate() {
                println!("  {}. {}", i + 1, action.short_label());
            }
            if let Some(resp) = &fail.failing_response {
                println!("\nFailing response:\n  {resp}");
            }
            if let Some(path) = &cli.save_trace {
                let saved = SavedTrace {
                    target,
                    seed: fail.seed,
                    violation: fail.violation.clone(),
                    original_steps: fail.original_len,
                    actions: fail.shrunk_actions.clone(),
                };
                save_trace(path, &saved)?;
                println!("\nSaved counterexample trace to {}", path.display());
            }
            Ok(ExitCode::from(1))
        }
    }
}

fn build_target(cli: &Cli) -> Result<Target> {
    if let Some(cmd) = &cli.stdio {
        return Ok(Target::Stdio {
            command: cmd.clone(),
        });
    }
    let Some(raw_url) = &cli.target else {
        return Err(anyhow!(
            "provide an MCP server URL (e.g. `blofeld http://localhost:3000/mcp`) or `--stdio <cmd>`"
        ));
    };
    let parsed =
        url::Url::parse(raw_url).map_err(|e| anyhow!("invalid MCP server URL '{raw_url}': {e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(anyhow!(
            "unsupported URL scheme '{}'; use http:// or https:// (or --stdio for local commands)",
            parsed.scheme()
        ));
    }
    let mut headers = Vec::with_capacity(cli.headers.len());
    for h in &cli.headers {
        let (k, v) = h
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid header '{h}', expected 'Key: Value'"))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    Ok(Target::Http {
        url: raw_url.clone(),
        headers,
    })
}
