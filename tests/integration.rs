// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: Apache-2.0

use blofeld::explorer::{RunConfig, SavedTrace, explore, replay_trace, save_trace};
use blofeld::mcp::Target;
use blofeld::properties::{Rule, RuleSet};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum ServerBug {
    None,
    /// Crashes with HTTP 500 only after create_item -> lock_item(id) -> delete_item(id)
    StatefulDeleteLockedCrash,
    /// Declares outputSchema requiring `count: integer` but returns string
    OutputSchemaMismatch,
    /// Leaks a secret string caught by YAML rule
    LeakSecret,
    /// Returns SSE responses cleanly
    SseTransport,
}

#[derive(Default)]
struct SessionState {
    next_item: u64,
    items: HashMap<String, bool>, // id -> locked
}

/// Minimal in-process Streamable HTTP MCP server for end-to-end tests.
async fn spawn_test_mcp_server(bug: ServerBug) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sessions: Arc<Mutex<HashMap<String, SessionState>>> = Arc::new(Mutex::new(HashMap::new()));
    let next_sid = Arc::new(Mutex::new(1u64));

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let sessions = Arc::clone(&sessions);
            let next_sid = Arc::clone(&next_sid);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let Ok(n) = stream.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let Some((head, body)) = req_str.split_once("\r\n\r\n") else {
                    return;
                };

                // Extract method & Mcp-Session-Id header
                let method = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                let mut sid: Option<String> = None;
                let mut content_len: usize = 0;
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("mcp-session-id") {
                            sid = Some(v.trim().to_string());
                        } else if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }

                // Read any remaining body bytes if split across packets
                let mut body_bytes = body.as_bytes().to_vec();
                while body_bytes.len() < content_len {
                    let mut more = vec![0u8; content_len - body_bytes.len()];
                    match stream.read(&mut more).await {
                        Ok(0) | Err(_) => break,
                        Ok(m) => body_bytes.extend_from_slice(&more[..m]),
                    }
                }

                if method == "DELETE" {
                    if let Some(id) = sid {
                        sessions.lock().unwrap().remove(&id);
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }

                let Ok(rpc) = serde_json::from_slice::<Value>(&body_bytes) else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };

                let rpc_method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
                let id = rpc.get("id").cloned();

                // Notifications return 202 Accepted
                if id.is_none() {
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let id = id.unwrap();

                let mut extra_headers = String::new();
                let (status_line, resp_val) = match rpc_method {
                    "initialize" => {
                        let new_id = {
                            let mut guard = next_sid.lock().unwrap();
                            let v = *guard;
                            *guard += 1;
                            format!("sess-{v}")
                        };
                        sessions
                            .lock()
                            .unwrap()
                            .insert(new_id.clone(), SessionState::default());
                        extra_headers.push_str(&format!("Mcp-Session-Id: {new_id}\r\n"));
                        (
                            "HTTP/1.1 200 OK",
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": { "tools": {}, "resources": {} },
                                    "serverInfo": { "name": "test-mcp", "version": "0.1.0" }
                                }
                            }),
                        )
                    }
                    "ping" => (
                        "HTTP/1.1 200 OK",
                        json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                    ),
                    "resources/list" => (
                        "HTTP/1.1 200 OK",
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "resources": [
                                    { "uri": "memo://status", "name": "status", "mimeType": "text/plain" }
                                ]
                            }
                        }),
                    ),
                    "resources/read" => (
                        "HTTP/1.1 200 OK",
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "contents": [
                                    { "uri": "memo://status", "mimeType": "text/plain", "text": "healthy" }
                                ]
                            }
                        }),
                    ),
                    "prompts/list" => (
                        "HTTP/1.1 200 OK",
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "Method not found" }
                        }),
                    ),
                    "tools/list" => {
                        let mut tools = vec![
                            json!({
                                "name": "create_item",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "name": { "type": "string", "minLength": 1 } },
                                    "required": ["name"]
                                }
                            }),
                            json!({
                                "name": "lock_item",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "id": { "type": "string" } },
                                    "required": ["id"]
                                }
                            }),
                            json!({
                                "name": "delete_item",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "id": { "type": "string" } },
                                    "required": ["id"]
                                }
                            }),
                        ];
                        if matches!(bug, ServerBug::OutputSchemaMismatch) {
                            tools.push(json!({
                                "name": "get_stats",
                                "inputSchema": { "type": "object", "properties": {} },
                                "outputSchema": {
                                    "type": "object",
                                    "properties": { "count": { "type": "integer" } },
                                    "required": ["count"]
                                }
                            }));
                        }
                        (
                            "HTTP/1.1 200 OK",
                            json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } }),
                        )
                    }
                    "tools/call" => {
                        let params = rpc.get("params").cloned().unwrap_or(json!({}));
                        let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = params.get("arguments").cloned().unwrap_or(json!({}));
                        let session_key = sid.unwrap_or_else(|| "default".to_string());
                        let mut guard = sessions.lock().unwrap();
                        let state = guard.entry(session_key).or_default();

                        handle_tool_call(bug, tool_name, &args, id, state)
                    }
                    _ => (
                        "HTTP/1.1 200 OK",
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "Method not found" }
                        }),
                    ),
                };

                let (content_type, wire_body) = if matches!(bug, ServerBug::SseTransport) {
                    (
                        "text/event-stream",
                        format!("event: message\ndata: {resp_val}\n\n"),
                    )
                } else {
                    ("application/json", resp_val.to_string())
                };

                let response = format!(
                    "{status_line}\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{wire_body}",
                    wire_body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}/mcp")
}

fn handle_tool_call(
    bug: ServerBug,
    tool_name: &str,
    args: &Value,
    id: Value,
    state: &mut SessionState,
) -> (&'static str, Value) {
    match tool_name {
        "create_item" => {
            let Some(name) = args.get("name").and_then(Value::as_str) else {
                return (
                    "HTTP/1.1 200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": "missing string 'name'" }
                    }),
                );
            };
            if name.is_empty() {
                return (
                    "HTTP/1.1 200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": true,
                            "content": [{ "type": "text", "text": "name cannot be empty" }]
                        }
                    }),
                );
            }
            state.next_item += 1;
            let item_id = format!("item-{}", state.next_item);
            state.items.insert(item_id.clone(), false);
            let text = if matches!(bug, ServerBug::LeakSecret) {
                json!({ "id": item_id, "debug": "SECRET_TOKEN_XYZ" }).to_string()
            } else {
                json!({ "id": item_id, "name": name }).to_string()
            };
            (
                "HTTP/1.1 200 OK",
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "structuredContent": { "id": item_id }
                    }
                }),
            )
        }
        "lock_item" => {
            let Some(item_id) = args.get("id").and_then(Value::as_str) else {
                return (
                    "HTTP/1.1 200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": "missing string 'id'" }
                    }),
                );
            };
            if let Some(locked) = state.items.get_mut(item_id) {
                *locked = true;
                (
                    "HTTP/1.1 200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "content": [{ "type": "text", "text": "locked" }] }
                    }),
                )
            } else {
                (
                    "HTTP/1.1 200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "isError": true,
                            "content": [{ "type": "text", "text": "not found" }]
                        }
                    }),
                )
            }
        }
        "delete_item" => {
            let Some(item_id) = args.get("id").and_then(Value::as_str) else {
                return (
                    "HTTP/1.1 200 OK",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": "missing string 'id'" }
                    }),
                );
            };
            if matches!(bug, ServerBug::StatefulDeleteLockedCrash)
                && state.items.get(item_id) == Some(&true)
            {
                return (
                    "HTTP/1.1 500 Internal Server Error",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": "cannot delete locked item" }
                    }),
                );
            }
            state.items.remove(item_id);
            (
                "HTTP/1.1 200 OK",
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": "deleted" }] }
                }),
            )
        }
        "get_stats" => (
            "HTTP/1.1 200 OK",
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": "stats" }],
                    "structuredContent": { "count": "not-an-integer" }
                }
            }),
        ),
        _ => (
            "HTTP/1.1 200 OK",
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": "unknown tool" }
            }),
        ),
    }
}

fn base_config(url: String) -> RunConfig {
    RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(5),
        steps: 30,
        time_limit: None,
        seed: 42,
        timeout: Duration::from_secs(2),
        rules: RuleSet::default(),
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    }
}

#[tokio::test]
async fn clean_server_passes() {
    let url = spawn_test_mcp_server(ServerBug::None).await;
    let summary = explore(&base_config(url)).await.unwrap();
    assert!(
        summary.failure.is_none(),
        "expected pass, got {:?}",
        summary.failure
    );
    assert_eq!(summary.discovered_tools, 3);
    assert_eq!(summary.runs_completed, 5);
}

#[tokio::test]
async fn sse_transport_passes() {
    let url = spawn_test_mcp_server(ServerBug::SseTransport).await;
    let summary = explore(&base_config(url)).await.unwrap();
    assert!(
        summary.failure.is_none(),
        "expected SSE pass, got {:?}",
        summary.failure
    );
}

#[tokio::test]
async fn discovers_and_shrinks_three_step_stateful_bug() {
    let url = spawn_test_mcp_server(ServerBug::StatefulDeleteLockedCrash).await;
    let mut cfg = base_config(url.clone());
    cfg.runs = Some(10);
    cfg.steps = 40;
    cfg.seed = 7;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("blofeld should autonomously discover create -> lock -> delete crash");
    assert_eq!(fail.violation.property, "no_http_5xx");
    assert_eq!(
        fail.shrunk_actions.len(),
        3,
        "expected shrunk trace of 3 steps (create -> lock -> delete), got {:?}",
        fail.shrunk_actions
    );
    // Verify Hegel also shrunk the JSON argument payload (e.g. create_item.name down to 1 char)
    if let blofeld::mcp::Action::CallTool { arguments, .. } = &fail.shrunk_actions[0] {
        let name = arguments.get("name").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            name.chars().count(),
            1,
            "expected Hegel to shrink create_item.name to 1 char, got {:?}",
            name
        );
    }

    // Save and replay the shrunk trace to confirm deterministic reproduction
    let tmp = std::env::temp_dir().join("blofeld_test_trace.json");
    save_trace(
        &tmp,
        &SavedTrace {
            target: cfg.target.clone(),
            seed: fail.seed,
            violation: fail.violation.clone(),
            original_steps: fail.original_len,
            actions: fail.shrunk_actions.clone(),
        },
    )
    .unwrap();

    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replayed trace should reproduce violation");
    assert_eq!(step, 3);
    assert_eq!(v.property, "no_http_5xx");
}

#[tokio::test]
async fn catches_output_schema_violation_and_shrinks_to_one_step() {
    let url = spawn_test_mcp_server(ServerBug::OutputSchemaMismatch).await;
    let mut cfg = base_config(url);
    cfg.runs = Some(5);
    cfg.steps = 25;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("blofeld should catch outputSchema type mismatch");
    assert_eq!(fail.violation.property, "output_schema_conformance");
    assert_eq!(fail.shrunk_actions.len(), 1);
}

#[tokio::test]
async fn catches_yaml_rule_violation() {
    let url = spawn_test_mcp_server(ServerBug::LeakSecret).await;
    let mut cfg = base_config(url);
    cfg.rules = RuleSet {
        rules: vec![Rule {
            name: "no-secret-leak".into(),
            tool: None,
            never_contains: Some("SECRET_TOKEN".into()),
            max_latency_ms: None,
            valid_calls_succeed: false,
        }],
    };

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("blofeld should catch custom YAML rule violation");
    assert_eq!(fail.violation.property, "rule:no-secret-leak");
    assert_eq!(fail.shrunk_actions.len(), 1);
}

#[tokio::test]
async fn time_limit_explores_until_budget_and_passes() {
    let url = spawn_test_mcp_server(ServerBug::None).await;
    let mut cfg = base_config(url);
    cfg.runs = None;
    cfg.time_limit = Some(Duration::from_millis(250));
    cfg.steps = 15;

    let summary = explore(&cfg).await.unwrap();
    assert!(summary.failure.is_none());
    assert!(summary.runs_completed >= 1);
    assert!(summary.total_steps >= 1);
    assert!(
        summary.elapsed >= Duration::from_millis(200) && summary.elapsed < Duration::from_secs(3),
        "expected elapsed near 250ms, got {:?}",
        summary.elapsed
    );
}
