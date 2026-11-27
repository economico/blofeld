// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use crate::mcp::{Action, Discovery, InputKind, RpcOutcome};
use crate::schema::validate_against_schema;
use crate::spec::{
    ActionSpec, CompiledSpec, InitStep, ProbeSpec, SpecEngine, StringOrVec, TransitionSpec,
};
use antithesis_sdk::{assert_always, assert_sometimes};
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::path::Path;

const LEAK_SIGNATURES: &[&str] = &[
    "Traceback (most recent call last)",
    "panicked at ",
    "RUST_BACKTRACE",
    "UnhandledPromiseRejection",
    "SIGSEGV",
    "Segmentation fault",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub property: String,
    pub message: String,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Optional declarative YAML rules / stateful spec file (`--rules rules.yaml` or `--spec spec.yaml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleSet {
    /// Initial episode shadow state (reset on every run, shrink attempt, and `--replay`).
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub state: Map<String, Value>,
    /// Optional initial probe calls run at episode start to seed `state` from live server reality.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub init: Vec<InitStep>,
    /// Optional state-machine transitions matching tool calls and updating `state`.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub transitions: Vec<TransitionSpec>,
    /// Optional read probes executed after steps to cross-check server reality against `state`.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub probes: Vec<ProbeSpec>,
    /// Optional state-guided action generators backed by shrinkable Hegel draws.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub actions: Vec<ActionSpec>,
    /// When true and `actions` is non-empty, only generate actions from `actions:` (no random schema fuzzing).
    #[serde(default)]
    pub actions_only: bool,
    /// Optional probability (`0.0..=1.0`) of attempting an adversarial state-machine break
    /// (cross-state/precondition violation, deep arg mutation, or step replay) on each step.
    #[serde(default)]
    pub adversarial_rate: Option<f64>,
    /// Declarative per-step invariant rules (substring, latency, valid-call checks, and CEL expressions).
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    /// Optional tool filter (applies to all actions if omitted).
    #[serde(default)]
    pub tool: Option<String>,
    /// Fail if the serialized JSON response contains this substring.
    #[serde(default)]
    pub never_contains: Option<String>,
    /// Fail if response latency exceeds this threshold in milliseconds.
    #[serde(default)]
    pub max_latency_ms: Option<u64>,
    /// Fail if a schema-valid call to this tool returns a JSON-RPC error or `isError: true`.
    #[serde(default)]
    pub valid_calls_succeed: bool,
    /// Optional CEL guard expression (`when: "ok && args.id in state.items"`).
    #[serde(default)]
    pub when: Option<String>,
    /// Optional CEL assertion expression (single string or list of strings).
    #[serde(default)]
    pub assert: Option<StringOrVec>,
}

impl RuleSet {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed reading rules file {}", path.display()))?;
        Self::from_yaml(&raw)
            .with_context(|| format!("failed parsing YAML rules/spec in {}", path.display()))
    }

    pub fn from_yaml(raw: &str) -> Result<Self> {
        let set: Self = serde_yaml::from_str(raw)?;
        // Validate all CEL expressions and state paths eagerly so errors surface before fuzzing.
        let _ = set.compile()?;
        Ok(set)
    }

    pub fn compile(&self) -> Result<CompiledSpec> {
        CompiledSpec::compile(self)
    }
}

/// Evaluate all built-in zero-config MCP invariants + optional YAML rules for one step.
/// Also emits `antithesis_sdk` assertions so runs integrate directly into Antithesis triage reports.
pub fn check_step(
    action: &Action,
    outcome: &RpcOutcome,
    discovery: &Discovery,
    rules: &RuleSet,
) -> Vec<Violation> {
    let mut violations = check_builtin_step(action, outcome, discovery);
    if !violations.is_empty() || outcome.transport_error.is_some() {
        return violations;
    }

    if !rules.rules.is_empty() {
        match rules.compile() {
            Ok(compiled) => {
                let engine = SpecEngine::new(compiled);
                violations.extend(engine.check_rules(action, outcome, 1, false));
            }
            Err(err) => {
                violations.push(Violation {
                    property: "spec_compile_error".into(),
                    message: format!("{err:#}"),
                });
            }
        }
    }

    violations
}

/// Evaluate built-in zero-config MCP protocol invariants only (no custom YAML rules).
pub fn check_builtin_step(
    action: &Action,
    outcome: &RpcOutcome,
    discovery: &Discovery,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    let details = json!({
        "action": action.short_label(),
        "http_status": outcome.http_status,
        "latency_ms": outcome.latency.as_millis() as u64,
    });

    // 1. Stdio stdout hygiene
    let clean_stdout = outcome.stdout_pollution.is_empty();
    assert_always!(clean_stdout, "mcp_stdout_hygiene", &details);
    if !clean_stdout {
        violations.push(Violation {
            property: "stdout_hygiene".into(),
            message: format!(
                "non-JSON-RPC output written to stdout: {:?}",
                outcome.stdout_pollution
            ),
        });
    }

    // 2. No transport failure / crash / timeout
    let transport_ok = outcome.transport_error.is_none();
    assert_always!(transport_ok, "mcp_no_transport_error", &details);
    if let Some(err) = &outcome.transport_error {
        violations.push(Violation {
            property: "no_transport_error".into(),
            message: err.clone(),
        });
        return violations;
    }

    // 3. No HTTP 5xx internal server errors
    let no_5xx = outcome.http_status.is_none_or(|s| s < 500);
    assert_always!(no_5xx, "mcp_no_http_5xx", &details);
    if let Some(status) = outcome.http_status.filter(|s| *s >= 500) {
        violations.push(Violation {
            property: "no_http_5xx".into(),
            message: format!("server returned HTTP {status} on {}", action.short_label()),
        });
    }

    // 4. Valid JSON-RPC 2.0 envelope
    // Per MCP Streamable HTTP spec, HTTP 4xx transport-level refusals (e.g. 401/403/400/405)
    // may return a plain HTTP error body without a JSON-RPC envelope; HTTP 2xx must always be
    // valid JSON-RPC 2.0.
    let is_http_4xx = outcome.http_status.is_some_and(|s| (400..500).contains(&s));
    let Some(raw) = &outcome.raw_response else {
        if !is_http_4xx {
            violations.push(Violation {
                property: "valid_jsonrpc_envelope".into(),
                message: "missing parsed JSON-RPC response".into(),
            });
        }
        return violations;
    };

    if is_http_4xx && raw.get("jsonrpc").is_none() {
        // Check for internal error leaks even on HTTP 4xx bodies, then return cleanly.
        let serialized = raw.to_string();
        let leaked = LEAK_SIGNATURES
            .iter()
            .find(|sig| serialized.contains(**sig));
        assert_always!(leaked.is_none(), "mcp_no_internal_error_leak", &details);
        if let Some(sig) = leaked {
            violations.push(Violation {
                property: "no_internal_error_leak".into(),
                message: format!("HTTP 4xx response leaked internal runtime signature '{sig}'"),
            });
        }
        return violations;
    }

    let envelope_err = check_jsonrpc_envelope(raw, outcome.request_id);
    let envelope_ok = envelope_err.is_none();
    assert_always!(envelope_ok, "mcp_valid_jsonrpc_envelope", &details);
    if let Some(msg) = envelope_err {
        violations.push(Violation {
            property: "valid_jsonrpc_envelope".into(),
            message: msg,
        });
        return violations;
    }

    // 5. No raw panic / stack trace leaks
    let serialized = raw.to_string();
    let leaked = LEAK_SIGNATURES
        .iter()
        .find(|sig| serialized.contains(**sig));
    assert_always!(leaked.is_none(), "mcp_no_internal_error_leak", &details);
    if let Some(sig) = leaked {
        violations.push(Violation {
            property: "no_internal_error_leak".into(),
            message: format!("response leaked internal runtime signature '{sig}'"),
        });
    }

    // 6. Action-specific MCP contract checks
    match action {
        Action::CallTool {
            name, input_kind, ..
        } => {
            if let Some(result) = outcome.result() {
                if let Some(msg) = check_tool_result_shape(result) {
                    assert_always!(false, "mcp_valid_tool_result_shape", &details);
                    violations.push(Violation {
                        property: "valid_tool_result".into(),
                        message: msg,
                    });
                } else {
                    assert_always!(true, "mcp_valid_tool_result_shape", &details);
                }

                // Check outputSchema when declared and call did not return isError: true
                if !outcome.is_tool_error() {
                    if let Some(tool) = discovery.tools.iter().find(|t| t.name == *name)
                        && let Some(out_schema) = &tool.output_schema
                    {
                        match result.get("structuredContent") {
                            Some(sc) => {
                                let errs =
                                    validate_against_schema(sc, out_schema, "structuredContent");
                                let conforms = errs.is_empty();
                                assert_always!(conforms, "mcp_output_schema_conforms", &details);
                                if !conforms {
                                    violations.push(Violation {
                                        property: "output_schema_conformance".into(),
                                        message: errs.join("; "),
                                    });
                                }
                            }
                            None => {
                                assert_always!(false, "mcp_output_schema_conforms", &details);
                                violations.push(Violation {
                                        property: "output_schema_conformance".into(),
                                        message: format!(
                                            "tool '{name}' declares outputSchema but response omitted structuredContent"
                                        ),
                                    });
                            }
                        }
                    }
                    if *input_kind == InputKind::Valid {
                        assert_sometimes!(true, "mcp_valid_tool_call_succeeded", &details);
                    }
                } else if *input_kind == InputKind::Malformed {
                    assert_sometimes!(true, "mcp_malformed_input_rejected_cleanly", &details);
                }
            } else if outcome.error().is_some() && *input_kind == InputKind::Malformed {
                assert_sometimes!(true, "mcp_malformed_input_rejected_cleanly", &details);
            }
        }
        Action::ReadResource { uri } => {
            if let Some(result) = outcome.result() {
                let valid = result.get("contents").and_then(Value::as_array).is_some();
                assert_always!(valid, "mcp_valid_resource_result", &details);
                if !valid {
                    violations.push(Violation {
                        property: "valid_resource_result".into(),
                        message: format!("resources/read({uri}) result missing 'contents' array"),
                    });
                } else {
                    assert_sometimes!(true, "mcp_resource_read_succeeded", &details);
                }
            }
        }
        Action::ListTools => {
            if let Some(result) = outcome.result() {
                let valid = result.get("tools").and_then(Value::as_array).is_some();
                assert_always!(valid, "mcp_valid_tools_list", &details);
                if !valid {
                    violations.push(Violation {
                        property: "valid_tools_list".into(),
                        message: "tools/list result missing 'tools' array".into(),
                    });
                }
            }
        }
        Action::GetPrompt { .. } | Action::Ping => {}
    }

    violations
}

fn check_jsonrpc_envelope(raw: &Value, expected_id: u64) -> Option<String> {
    let obj = raw.as_object()?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(format!(
            "expected jsonrpc='2.0', got {:?}",
            obj.get("jsonrpc")
        ));
    }
    if obj.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Some(format!(
            "response id {:?} did not match request id {expected_id}",
            obj.get("id")
        ));
    }
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");
    match (has_result, has_error) {
        (true, true) => Some("JSON-RPC response has both 'result' and 'error'".into()),
        (false, false) => Some("JSON-RPC response has neither 'result' nor 'error'".into()),
        (false, true) => {
            let err = obj.get("error").and_then(Value::as_object);
            let Some(err_obj) = err else {
                return Some("JSON-RPC 'error' field is not an object".into());
            };
            if err_obj.get("code").and_then(Value::as_i64).is_none() {
                return Some("JSON-RPC error missing integer 'code'".into());
            }
            if err_obj.get("message").and_then(Value::as_str).is_none() {
                return Some("JSON-RPC error missing string 'message'".into());
            }
            None
        }
        (true, false) => None,
    }
}

fn check_tool_result_shape(result: &Value) -> Option<String> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "tools/call result missing 'content' array".to_string());
    let arr = match content {
        Ok(a) => a,
        Err(e) => return Some(e),
    };
    for (idx, item) in arr.iter().enumerate() {
        let Some(ty) = item.get("type").and_then(Value::as_str) else {
            return Some(format!("content[{idx}] missing string 'type' field"));
        };
        if ty == "text" && item.get("text").and_then(Value::as_str).is_none() {
            return Some(format!(
                "content[{idx}] has type='text' without string 'text'"
            ));
        }
    }
    None
}
