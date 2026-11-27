// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use blofeld::explorer::{RunConfig, SavedTrace, explore, replay_trace, save_trace};
use blofeld::mcp::{Action, Discovery, InputKind, RpcOutcome, Target};
use blofeld::properties::{RuleSet, check_step};
use blofeld::spec::{SpecEngine, StatePath, cel_to_json, json_to_cel, normalize_outcome_res};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum StatefulBug {
    /// Completely faithful server implementation.
    None,
    /// Starts each session with 2 pre-seeded open items (`seed-1`, `seed-2`).
    PreSeededClean,
    /// Allows `delete_item` to succeed (`ok: true`) even when item is locked (semantic violation).
    AllowDeleteLocked,
    /// `delete_item` returns `ok: true` but forgets to actually remove the item from storage (ghost item).
    GhostDelete,
    /// `lock_item` returns `ok: true` without actually persisting `locked = true`.
    SilentUnpersistedLock,
    /// `archive_item` fails with a semantic violation (`status: "corrupted"`) only when called on a locked item.
    CorruptArchiveLocked,
}

#[derive(Default)]
struct ServerSession {
    next_id: u64,
    /// id -> (name, status: "open" | "locked" | "archived")
    items: BTreeMap<String, (String, String)>,
}

impl ServerSession {
    fn new(bug: StatefulBug) -> Self {
        let mut s = Self::default();
        if matches!(bug, StatefulBug::PreSeededClean) {
            s.items
                .insert("seed-1".into(), ("alpha".into(), "open".into()));
            s.items
                .insert("seed-2".into(), ("beta".into(), "open".into()));
        }
        s
    }
}

async fn spawn_stateful_mcp_server(bug: StatefulBug) -> String {
    spawn_stateful_mcp_server_with_transport(bug, false).await
}

async fn spawn_stateful_mcp_server_with_transport(bug: StatefulBug, sse: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sessions: Arc<Mutex<BTreeMap<String, ServerSession>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
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

                let method = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                let mut sid: Option<String> = None;
                let mut accept_header = String::new();
                let mut content_len: usize = 0;
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("mcp-session-id") {
                            sid = Some(v.trim().to_string());
                        } else if k.eq_ignore_ascii_case("accept") {
                            accept_header = v.trim().to_ascii_lowercase();
                        } else if k.eq_ignore_ascii_case("content-length") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }

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
                        .write_all(
                            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                };

                let rpc_method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
                let Some(id) = rpc.get("id").cloned() else {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                };

                let mut extra_headers = String::new();
                let resp_val = match rpc_method {
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
                            .insert(new_id.clone(), ServerSession::new(bug));
                        extra_headers.push_str(&format!("Mcp-Session-Id: {new_id}\r\n"));
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "stateful-test-mcp", "version": "0.1.0" }
                            }
                        })
                    }
                    "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                    "resources/list" => {
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "resources": [] } })
                    }
                    "prompts/list" => {
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "prompts": [] } })
                    }
                    "tools/list" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "list_items",
                                    "annotations": { "readOnlyHint": true },
                                    "inputSchema": { "type": "object", "properties": {} }
                                },
                                {
                                    "name": "get_item",
                                    "annotations": { "readOnlyHint": true },
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "id": { "type": "string" } },
                                        "required": ["id"]
                                    }
                                },
                                {
                                    "name": "create_item",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "name": { "type": "string", "minLength": 1 } },
                                        "required": ["name"]
                                    }
                                },
                                {
                                    "name": "lock_item",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "id": { "type": "string" } },
                                        "required": ["id"]
                                    }
                                },
                                {
                                    "name": "archive_item",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "id": { "type": "string" },
                                            "priority": { "type": "integer", "minimum": 1, "maximum": 5 }
                                        },
                                        "required": ["id", "priority"]
                                    }
                                },
                                {
                                    "name": "delete_item",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": { "id": { "type": "string" } },
                                        "required": ["id"]
                                    }
                                }
                            ]
                        }
                    }),
                    "tools/call" => {
                        let params = rpc.get("params").cloned().unwrap_or(json!({}));
                        let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = params.get("arguments").cloned().unwrap_or(json!({}));
                        let session_key = sid.unwrap_or_else(|| "default".to_string());
                        let mut guard = sessions.lock().unwrap();
                        let state = guard
                            .entry(session_key)
                            .or_insert_with(|| ServerSession::new(bug));
                        handle_call(bug, tool, &args, id, state)
                    }
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "Method not found" }
                    }),
                };

                if sse && !accept_header.contains("text/event-stream") {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 406 Not Acceptable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }

                let (content_type, wire_body) = if sse {
                    (
                        "text/event-stream; charset=utf-8",
                        format!(
                            ": keepalive\n\nevent: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"progress\":1}}}}\n\nevent: message\ndata: {resp_val}\n\n"
                        ),
                    )
                } else {
                    ("application/json", resp_val.to_string())
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{wire_body}",
                    wire_body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}/mcp")
}

fn handle_call(
    bug: StatefulBug,
    tool: &str,
    args: &Value,
    id: Value,
    state: &mut ServerSession,
) -> Value {
    match tool {
        "list_items" => {
            let items: Vec<Value> = state
                .items
                .iter()
                .map(|(k, (name, status))| json!({ "id": k, "name": name, "status": status }))
                .collect();
            let payload = json!({ "items": items, "count": items.len() });
            // Deliberately return JSON via content[0].text (no structuredContent) to test fallback!
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": payload.to_string() }]
                }
            })
        }
        "get_item" => {
            let Some(item_id) = args.get("id").and_then(Value::as_str) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing id" }
                });
            };
            if let Some((name, status)) = state.items.get(item_id) {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "ok" }],
                        "structuredContent": { "id": item_id, "name": name, "status": status }
                    }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": "not found" }]
                    }
                })
            }
        }
        "create_item" => {
            let Some(name) = args.get("name").and_then(Value::as_str) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing name" }
                });
            };
            if name.is_empty() {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": "empty name" }]
                    }
                });
            }
            state.next_id += 1;
            let item_id = format!("item-{}", state.next_id);
            state
                .items
                .insert(item_id.clone(), (name.to_string(), "open".into()));
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": json!({ "id": item_id, "status": "open" }).to_string() }],
                    "structuredContent": { "id": item_id, "name": name, "status": "open" }
                }
            })
        }
        "lock_item" => {
            let Some(item_id) = args.get("id").and_then(Value::as_str) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing id" }
                });
            };
            if let Some(entry) = state.items.get_mut(item_id) {
                if !matches!(bug, StatefulBug::SilentUnpersistedLock) {
                    entry.1 = "locked".into();
                }
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "locked" }],
                        "structuredContent": { "id": item_id, "status": "locked" }
                    }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": "not found" }]
                    }
                })
            }
        }
        "archive_item" => {
            let Some(item_id) = args.get("id").and_then(Value::as_str) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing id" }
                });
            };
            let Some(priority) = args.get("priority").and_then(Value::as_i64) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing priority" }
                });
            };
            if let Some(entry) = state.items.get_mut(item_id) {
                if matches!(bug, StatefulBug::CorruptArchiveLocked)
                    && entry.1 == "locked"
                    && priority >= 3
                {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": "corrupted" }],
                            "structuredContent": { "id": item_id, "status": "corrupted" }
                        }
                    });
                }
                entry.1 = "archived".into();
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "archived" }],
                        "structuredContent": { "id": item_id, "status": "archived" }
                    }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": "not found" }]
                    }
                })
            }
        }
        "delete_item" => {
            let Some(item_id) = args.get("id").and_then(Value::as_str) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing id" }
                });
            };
            let Some((_, status)) = state.items.get(item_id) else {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": "not found" }]
                    }
                });
            };
            if status == "locked" && !matches!(bug, StatefulBug::AllowDeleteLocked) {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isError": true,
                        "content": [{ "type": "text", "text": "cannot delete locked item" }]
                    }
                });
            }
            if !matches!(bug, StatefulBug::GhostDelete) {
                state.items.remove(item_id);
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": "deleted" }]
                }
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": "unknown tool" }
        }),
    }
}

fn config_with_yaml(url: String, yaml: &str) -> RunConfig {
    let rules = RuleSet::from_yaml(yaml).expect("valid spec YAML");
    RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(10),
        steps: 30,
        time_limit: None,
        seed: 1337,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    }
}

const FULL_STATE_MACHINE_YAML: &str = r#"
state:
  items: {}
  count: 0

transitions:
  - name: item-created
    tool: create_item
    when: "ok"
    assert:
      - "has(res.id)"
      - "!(res.id in state.items)"
    update:
      "items[res.id]": "'open'"
      count: "state.count + 1"

  - name: item-locked
    tool: lock_item
    when: "ok"
    assert: "args.id in state.items"
    update:
      "items[args.id]": "'locked'"

  - name: item-archived
    tool: archive_item
    when: "ok"
    assert:
      - "args.id in state.items"
      - "res.status == 'archived'"
    update:
      "items[args.id]": "'archived'"

  - name: item-deleted
    tool: delete_item
    when: "ok"
    assert: "args.id in state.items"
    delete:
      - "items[args.id]"
    update:
      count: "state.count - 1"

probes:
  - name: list-matches-model
    after: mutating
    call:
      tool: list_items
      arguments: {}
    assert:
      - "ok"
      - "res.count == state.count"
      - "size(res.items) == size(state.items)"
      - "res.items.all(it, it.id in state.items && state.items[it.id] == it.status)"

rules:
  - name: locked-item-cannot-be-deleted
    tool: delete_item
    when: "args.id in state.items && state.items[args.id] == 'locked'"
    assert: "!ok"

actions:
  - name: create
    tool: create_item
    weight: 2
    args:
      name: "gen_string(1, 10)"
  - name: lock
    tool: lock_item
    when: "'open' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'open')"
  - name: archive
    tool: archive_item
    when: "size(state.items) > 0"
    args:
      id: "pick_key(state.items)"
      priority: "gen_int(1, 5)"
  - name: delete
    tool: delete_item
    when: "size(state.items) > 0"
    args:
      id: "pick_key(state.items)"
"#;

#[tokio::test]
async fn clean_server_passes_full_state_machine_spec() {
    let url = spawn_stateful_mcp_server(StatefulBug::None).await;
    let mut cfg = config_with_yaml(url, FULL_STATE_MACHINE_YAML);
    cfg.runs = Some(8);
    cfg.steps = 25;

    let summary = explore(&cfg).await.unwrap();
    assert!(
        summary.failure.is_none(),
        "clean server should satisfy full state-machine spec, got {:?}",
        summary.failure
    );
    assert_eq!(summary.runs_completed, 8);
}

#[tokio::test]
async fn init_hook_seeds_state_from_prepopulated_server() {
    let url = spawn_stateful_mcp_server(StatefulBug::PreSeededClean).await;
    let yaml = r#"
state:
  initial_count: 0
  created: 0

init:
  - name: seed-initial-count
    call:
      tool: list_items
      arguments: {}
    assert:
      - "ok"
      - "res.count == 2"
    update:
      initial_count: "res.count"

transitions:
  - name: created
    tool: create_item
    when: "ok"
    update:
      created: "state.created + 1"

probes:
  - name: count-matches-seeded-plus-created
    after: create_item
    call:
      tool: list_items
    assert: "ok && res.count == state.initial_count + state.created"

actions:
  - tool: create_item
    args:
      name: "gen_string(1, 8)"
actions_only: true
"#;
    let mut cfg = config_with_yaml(url, yaml);
    cfg.runs = Some(6);
    cfg.steps = 12;

    let summary = explore(&cfg).await.unwrap();
    assert!(
        summary.failure.is_none(),
        "expected init-seeded model to pass, got {:?}",
        summary.failure
    );
}

#[tokio::test]
async fn init_assertion_failure_is_caught_and_replayed() {
    let url = spawn_stateful_mcp_server(StatefulBug::None).await;
    let yaml = r#"
state:
  initial_count: 0

init:
  - name: expect-prepopulated
    call:
      tool: list_items
    assert: "ok && res.count >= 1"
"#;
    let mut cfg = config_with_yaml(url, yaml);
    cfg.runs = Some(3);
    cfg.steps = 5;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary.failure.expect("init assertion should fail");
    assert_eq!(fail.violation.property, "init:expect-prepopulated");

    let tmp = std::env::temp_dir().join("blofeld_test_init_fail.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replay should reproduce init failure");
    assert_eq!(step, 0);
    assert_eq!(v.property, "init:expect-prepopulated");
}

#[tokio::test]
async fn stateful_rule_catches_delete_locked_shrinks_and_replays() {
    let url = spawn_stateful_mcp_server(StatefulBug::AllowDeleteLocked).await;
    let mut cfg = config_with_yaml(url, FULL_STATE_MACHINE_YAML);
    cfg.runs = Some(15);
    cfg.steps = 30;
    cfg.seed = 42;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("stateful rule should catch deleting a locked item");
    assert_eq!(
        fail.violation.property,
        "rule:locked-item-cannot-be-deleted"
    );
    assert_eq!(
        fail.shrunk_actions.len(),
        3,
        "expected minimal 3-step trace (create -> lock -> delete), got {:?}",
        fail.shrunk_actions
    );

    let tmp = std::env::temp_dir().join("blofeld_test_delete_locked.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replay should reproduce stateful rule violation");
    assert_eq!(step, 3);
    assert_eq!(v.property, "rule:locked-item-cannot-be-deleted");
}

#[tokio::test]
async fn probe_catches_ghost_delete_shrinks_to_two_steps_and_replays() {
    let url = spawn_stateful_mcp_server(StatefulBug::GhostDelete).await;
    let mut cfg = config_with_yaml(url, FULL_STATE_MACHINE_YAML);
    cfg.runs = Some(15);
    cfg.steps = 30;
    cfg.seed = 99;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("probe should catch ghost delete divergence between server and shadow state");
    assert_eq!(fail.violation.property, "probe:list-matches-model");
    assert_eq!(
        fail.shrunk_actions.len(),
        2,
        "expected minimal 2-step trace (create -> delete), got {:?}",
        fail.shrunk_actions
    );

    let tmp = std::env::temp_dir().join("blofeld_test_ghost_delete.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replay should reproduce probe violation");
    assert_eq!(step, 2);
    assert_eq!(v.property, "probe:list-matches-model");
}

#[tokio::test]
async fn targeted_tool_probe_with_dynamic_arguments_catches_unpersisted_lock() {
    let url = spawn_stateful_mcp_server(StatefulBug::SilentUnpersistedLock).await;
    let yaml = r#"
state:
  items: {}

transitions:
  - name: created
    tool: create_item
    when: "ok"
    update:
      "items[res.id]": "'open'"
  - name: locked
    tool: lock_item
    when: "ok"
    update:
      "items[args.id]": "'locked'"

probes:
  - name: verify-get-item-status
    after: lock_item
    when: "ok"
    call:
      tool: get_item
      arguments:
        id: "args.id"
    assert: "ok && res.status == state.items[trigger.args.id]"

actions:
  - tool: create_item
    args:
      name: "gen_string(1, 8)"
  - tool: lock_item
    when: "size(state.items) > 0"
    args:
      id: "pick_key(state.items)"
actions_only: true
"#;
    let mut cfg = config_with_yaml(url, yaml);
    cfg.runs = Some(10);
    cfg.steps = 20;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("probe calling get_item(id=args.id) should catch unpersisted lock");
    assert_eq!(fail.violation.property, "probe:verify-get-item-status");
    assert_eq!(fail.shrunk_actions.len(), 2);
}

#[tokio::test]
async fn actions_guided_generation_discovers_and_shrinks_deep_workflow_and_args() {
    let url = spawn_stateful_mcp_server(StatefulBug::CorruptArchiveLocked).await;
    let yaml = format!("{FULL_STATE_MACHINE_YAML}\nactions_only: true\n");
    let mut cfg = config_with_yaml(url, &yaml);
    cfg.runs = Some(20);
    cfg.steps = 25;
    cfg.seed = 7;

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("actions-guided search should discover create -> lock -> archive(priority>=3)");
    assert_eq!(fail.violation.property, "transition:item-archived");
    assert_eq!(
        fail.shrunk_actions.len(),
        3,
        "expected 3-step shrunk trace (create -> lock -> archive), got {:?}",
        fail.shrunk_actions
    );
    // Verify Hegel shrank `gen_string(1, 10)` down to 1 char and `gen_int(1, 5)` down to 3!
    if let Action::CallTool { arguments, .. } = &fail.shrunk_actions[0] {
        let name = arguments.get("name").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            name.chars().count(),
            1,
            "expected gen_string to shrink to 1 char, got {:?}",
            name
        );
    }
    if let Action::CallTool { arguments, .. } = &fail.shrunk_actions[2] {
        let priority = arguments
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        assert_eq!(
            priority, 3,
            "expected gen_int(1, 5) to shrink to minimal failing boundary 3, got {priority}"
        );
    }
}

#[test]
fn yaml_compile_rejects_invalid_cel_paths_and_weights() {
    // Bad CEL syntax in rule assert
    let bad_cel = r#"
rules:
  - name: broken
    assert: "ok && ("
"#;
    assert!(RuleSet::from_yaml(bad_cel).is_err());

    // Bad state path: unclosed bracket
    let bad_path_1 = r#"
transitions:
  - name: bad
    tool: foo
    update:
      "items[res.id": "'x'"
"#;
    assert!(RuleSet::from_yaml(bad_path_1).is_err());

    // Bad state path: trailing garbage after `]`
    let bad_path_2 = r#"
transitions:
  - name: bad
    tool: foo
    update:
      "items[res.id]status": "'x'"
"#;
    assert!(RuleSet::from_yaml(bad_path_2).is_err());

    // Bad action weight == 0
    let bad_weight = r#"
actions:
  - tool: foo
    weight: 0
"#;
    assert!(RuleSet::from_yaml(bad_weight).is_err());

    // Valid empty sections and null YAML values
    let empty_sections = r#"
state:
init:
transitions:
probes:
actions:
rules:
"#;
    assert!(RuleSet::from_yaml(empty_sections).is_ok());

    // Valid StatePath variations
    assert!(StatePath::parse("count").is_ok());
    assert!(StatePath::parse("state.count").is_ok());
    assert!(StatePath::parse("items[res.id].status").is_ok());
    assert!(StatePath::parse("state[res.id]").is_ok());
    assert!(StatePath::parse("items['a.b[0]']").is_ok());
    assert!(StatePath::parse("").is_err());
    assert!(StatePath::parse(".foo").is_err());
    assert!(StatePath::parse("foo.").is_err());
    assert!(StatePath::parse("foo[]").is_err());
}

#[test]
fn normalized_res_prefers_structured_content_then_text_json_then_raw() {
    let sc_outcome = RpcOutcome {
        request_id: 1,
        latency: Duration::from_millis(1),
        http_status: Some(200),
        www_authenticate: None,
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "structuredContent": { "id": "from-sc" },
                "content": [{ "type": "text", "text": "{\"id\":\"from-text\"}" }]
            }
        })),
        transport_error: None,
        stdout_pollution: vec![],
    };
    assert_eq!(
        normalize_outcome_res(&sc_outcome),
        json!({ "id": "from-sc" })
    );

    let text_json_outcome = RpcOutcome {
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "{\"id\":\"from-text\",\"n\":42}" }]
            }
        })),
        ..sc_outcome.clone()
    };
    assert_eq!(
        normalize_outcome_res(&text_json_outcome),
        json!({ "id": "from-text", "n": 42 })
    );

    let plain_text_outcome = RpcOutcome {
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "plain text" }]
            }
        })),
        ..sc_outcome.clone()
    };
    assert_eq!(
        normalize_outcome_res(&plain_text_outcome),
        json!({ "content": [{ "type": "text", "text": "plain text" }] })
    );
}

#[test]
fn when_guard_tolerates_missing_fields_from_malformed_calls_and_gates_legacy_rules() {
    let yaml = r#"
rules:
  - name: positive-amount-returns-ok
    tool: charge
    when: "args.amount > 0"
    assert: "ok"
  - name: ok-never-contains-warn
    tool: charge
    when: "ok"
    never_contains: "WARN_CODE"
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();

    // Malformed call missing `args.amount` and returning an error should NOT trigger either rule!
    let malformed_action = Action::CallTool {
        name: "charge".into(),
        arguments: json!({}),
        input_kind: InputKind::Malformed,
    };
    let err_outcome = RpcOutcome {
        request_id: 1,
        latency: Duration::from_millis(1),
        http_status: Some(200),
        www_authenticate: None,
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "isError": true,
                "content": [{ "type": "text", "text": "WARN_CODE: missing amount" }]
            }
        })),
        transport_error: None,
        stdout_pollution: vec![],
    };
    let violations = check_step(
        &malformed_action,
        &err_outcome,
        &Discovery::default(),
        &rules,
    );
    assert!(
        violations.is_empty(),
        "expected malformed call to be skipped by `when:` guards, got {violations:?}"
    );

    // Valid call with `ok == true` containing `WARN_CODE` MUST trigger `ok-never-contains-warn`!
    let ok_action = Action::CallTool {
        name: "charge".into(),
        arguments: json!({ "amount": 10 }),
        input_kind: InputKind::Valid,
    };
    let ok_warn_outcome = RpcOutcome {
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": "WARN_CODE: ok" }]
            }
        })),
        ..err_outcome
    };
    let violations = check_step(&ok_action, &ok_warn_outcome, &Discovery::default(), &rules);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].property, "rule:ok-never-contains-warn");
}

#[test]
fn cel_helpers_and_atomic_state_swap_work_correctly() {
    let yaml = r#"
state:
  a: 1
  b: 2
  items:
    x: 10
    y: 20

transitions:
  - name: swap-and-sum
    tool: ping_tool
    when: "ok"
    assert:
      - "keys(state.items) == ['x', 'y']"
      - "values(state.items) == [10, 20]"
      - "sum(values(state.items)) == 30"
      - "get(state.items, 'missing', 99) == 99"
    update:
      a: "state.b"
      b: "state.a"
      "items[args.k]": "args.v"
      "nested.flag": true
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let mut engine = SpecEngine::new(rules.compile().unwrap());

    let action = Action::CallTool {
        name: "ping_tool".into(),
        arguments: json!({ "k": "z", "v": 30 }),
        input_kind: InputKind::Valid,
    };
    let outcome = RpcOutcome {
        request_id: 1,
        latency: Duration::from_millis(1),
        http_status: Some(200),
        www_authenticate: None,
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "content": [{ "type": "text", "text": "ok" }] }
        })),
        transport_error: None,
        stdout_pollution: vec![],
    };

    let violations = engine.apply_transitions(&action, &outcome, 1, false);
    assert!(violations.is_empty(), "got violations: {violations:?}");
    assert_eq!(engine.state().get("a"), Some(&json!(2)));
    assert_eq!(engine.state().get("b"), Some(&json!(1)));
    assert_eq!(engine.state().get("items").unwrap()["z"], json!(30));
    assert_eq!(engine.state().get("nested").unwrap()["flag"], json!(true));

    // Also verify json_to_cel / cel_to_json preserves integer arithmetic
    let v = json!({ "n": 0, "neg": -5, "arr": [1, 2, 3] });
    assert_eq!(cel_to_json(&json_to_cel(&v)).unwrap(), v);
}

#[tokio::test]
async fn cli_spec_alias_runs_against_live_server() {
    let url = spawn_stateful_mcp_server(StatefulBug::None).await;
    let spec_path = std::env::temp_dir().join("blofeld_cli_spec_test.yaml");
    std::fs::write(&spec_path, FULL_STATE_MACHINE_YAML).unwrap();

    let bin = env!("CARGO_BIN_EXE_blofeld");
    let output = tokio::process::Command::new(bin)
        .args([
            url.as_str(),
            "--spec",
            spec_path.to_str().unwrap(),
            "--runs",
            "3",
            "--steps",
            "15",
            "--seed",
            "123",
        ])
        .output()
        .await
        .unwrap();

    let _ = std::fs::remove_file(&spec_path);
    assert!(
        output.status.success(),
        "expected CLI --spec run to pass, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn stateful_spec_transitions_and_probe_over_sse_transport() {
    // Clean SSE server passes full state-machine spec (transitions + probes + rules + actions)
    let clean_url = spawn_stateful_mcp_server_with_transport(StatefulBug::None, true).await;
    let mut clean_cfg = config_with_yaml(clean_url, FULL_STATE_MACHINE_YAML);
    clean_cfg.runs = Some(6);
    clean_cfg.steps = 20;
    clean_cfg.seed = 2026;

    let clean_summary = explore(&clean_cfg).await.unwrap();
    assert!(
        clean_summary.failure.is_none(),
        "clean SSE server should satisfy stateful spec, got {:?}",
        clean_summary.failure
    );
    assert_eq!(clean_summary.runs_completed, 6);

    // Buggy SSE server (GhostDelete) is caught by probe, shrinks to 2 steps, and replays red
    let buggy_url = spawn_stateful_mcp_server_with_transport(StatefulBug::GhostDelete, true).await;
    let mut buggy_cfg = config_with_yaml(buggy_url, FULL_STATE_MACHINE_YAML);
    buggy_cfg.runs = Some(15);
    buggy_cfg.steps = 30;
    buggy_cfg.seed = 99;

    let buggy_summary = explore(&buggy_cfg).await.unwrap();
    let fail = buggy_summary
        .failure
        .expect("probe should catch ghost delete over SSE transport");
    assert_eq!(fail.violation.property, "probe:list-matches-model");
    assert_eq!(
        fail.shrunk_actions.len(),
        2,
        "expected minimal 2-step trace (create -> delete) over SSE, got {:?}",
        fail.shrunk_actions
    );

    let tmp = std::env::temp_dir().join("blofeld_test_sse_ghost_delete.json");
    save_trace(
        &tmp,
        &SavedTrace::from_failure(buggy_cfg.target.clone(), &fail),
    )
    .unwrap();
    let replayed = replay_trace(&buggy_cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replay over SSE should reproduce probe violation");
    assert_eq!(step, 2);
    assert_eq!(v.property, "probe:list-matches-model");

    // Pre-seeded SSE server with `init:` + `transitions:` + `probes:` + `actions_only: true`
    let preseeded_url =
        spawn_stateful_mcp_server_with_transport(StatefulBug::PreSeededClean, true).await;
    let init_yaml = r#"
state:
  count: 0
init:
  - name: seed-count
    call: { tool: list_items, arguments: {} }
    assert: "ok && res.count == 2"
    update:
      count: "res.count"
transitions:
  - name: created
    tool: create_item
    when: "ok"
    update:
      count: "state.count + 1"
probes:
  - name: list-count-matches
    after: create_item
    call: { tool: list_items, arguments: {} }
    assert: "ok && res.count == state.count"
actions:
  - tool: create_item
    args: { name: "gen_string(1, 6)" }
actions_only: true
"#;
    let mut init_cfg = config_with_yaml(preseeded_url, init_yaml);
    init_cfg.runs = Some(4);
    init_cfg.steps = 10;
    let init_summary = explore(&init_cfg).await.unwrap();
    assert!(
        init_summary.failure.is_none(),
        "pre-seeded SSE server should pass init + transitions + probe spec, got {:?}",
        init_summary.failure
    );
}

fn extract_all_markdown_yaml(md: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = md;
    while let Some(pos) = rest.find("```yaml\n") {
        let after = &rest[pos + "```yaml\n".len()..];
        let end = after
            .find("\n```")
            .expect("markdown ```yaml fence should be closed");
        blocks.push(after[..end].to_string());
        rest = &after[end + "\n```".len()..];
    }
    assert!(
        !blocks.is_empty(),
        "markdown should contain at least one ```yaml code fence"
    );
    blocks
}

fn unescape_html(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
}

fn extract_all_html_spec_yaml(html: &str) -> Vec<(String, String)> {
    let mut specs = Vec::new();
    let mut rest = html;
    let marker = r#"<pre class="codeblock" id=""#;
    while let Some(pos) = rest.find(marker) {
        let after_id = &rest[pos + marker.len()..];
        let id_end = after_id.find('"').expect("id attribute should be closed");
        let id = after_id[..id_end].to_string();
        let after_tag = &after_id[id_end..];
        let tag_close = after_tag.find('>').expect("<pre> opening tag should close");
        let attrs = &after_tag[..tag_close];
        let content_start = &after_tag[tag_close + 1..];
        let content_end = content_start
            .find("</pre>")
            .expect("<pre> element should be closed");
        if attrs.contains("data-spec-yaml") {
            specs.push((id, unescape_html(&content_start[..content_end])));
        }
        rest = &content_start[content_end + "</pre>".len()..];
    }
    assert!(
        !specs.is_empty(),
        "index.html should contain at least one data-spec-yaml <pre> block"
    );
    specs
}

#[tokio::test]
async fn docs_and_site_yaml_examples_stay_in_sync_and_pass_clean_server() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(manifest_dir.join("README.md")).unwrap();
    let skill = std::fs::read_to_string(manifest_dir.join("skills/blofeld/SKILL.md")).unwrap();
    let site_skill = std::fs::read_to_string(manifest_dir.join("site/public/skill.md")).unwrap();
    let index_html = std::fs::read_to_string(manifest_dir.join("site/public/index.html")).unwrap();

    assert_eq!(
        skill, site_skill,
        "site/public/skill.md must stay byte-for-byte in sync with skills/blofeld/SKILL.md"
    );

    // Every YAML example in README.md, SKILL.md, and site/public/index.html must parse & compile cleanly.
    let readme_yamls = extract_all_markdown_yaml(&readme);
    assert!(
        readme_yamls.len() >= 4,
        "README.md should showcase multiple YAML spec examples"
    );
    for (i, yaml) in readme_yamls.iter().enumerate() {
        RuleSet::from_yaml(yaml)
            .unwrap_or_else(|e| panic!("README.md YAML example #{i} failed to compile: {e:#}"));
    }

    let skill_yamls = extract_all_markdown_yaml(&skill);
    for (i, yaml) in skill_yamls.iter().enumerate() {
        RuleSet::from_yaml(yaml)
            .unwrap_or_else(|e| panic!("SKILL.md YAML example #{i} failed to compile: {e:#}"));
    }

    let site_yamls = extract_all_html_spec_yaml(&index_html);
    assert!(
        site_yamls.len() >= 4,
        "site/public/index.html should showcase multiple YAML spec examples"
    );
    for (id, yaml) in &site_yamls {
        RuleSet::from_yaml(yaml)
            .unwrap_or_else(|e| panic!("site/public/index.html #{id} failed to compile: {e:#}"));
    }

    let readme_stateful_yaml = readme_yamls
        .iter()
        .find(|y| y.contains("list-matches-model"))
        .expect("README.md should include the stateful item lifecycle example");
    let skill_yaml = &skill_yamls[0];
    let site_yaml = &site_yamls
        .iter()
        .find(|(id, _)| id == "rules-yaml")
        .expect("site/public/index.html should contain #rules-yaml")
        .1;

    let clean_url = spawn_stateful_mcp_server(StatefulBug::None).await;
    for (label, yaml) in [
        ("README.md", readme_stateful_yaml),
        ("skills/blofeld/SKILL.md", skill_yaml),
        ("site/public/index.html", site_yaml),
    ] {
        let mut cfg = config_with_yaml(clean_url.clone(), yaml);
        cfg.runs = Some(5);
        cfg.steps = 20;
        cfg.seed = 42;
        cfg.exclude_tools = vec!["archive_item".into()];
        let summary = explore(&cfg).await.unwrap();
        assert!(
            summary.failure.is_none(),
            "YAML example in {label} must pass against a clean server, got {:?}",
            summary.failure
        );
    }

    // Confirm site/public/index.html example catches a real ghost-delete bug
    let ghost_url = spawn_stateful_mcp_server(StatefulBug::GhostDelete).await;
    let mut ghost_cfg = config_with_yaml(ghost_url, site_yaml);
    ghost_cfg.runs = Some(15);
    ghost_cfg.steps = 30;
    ghost_cfg.seed = 99;
    ghost_cfg.exclude_tools = vec!["archive_item".into()];
    let ghost_summary = explore(&ghost_cfg).await.unwrap();
    let ghost_fail = ghost_summary
        .failure
        .expect("site/public/index.html YAML example should catch GhostDelete");
    assert_eq!(ghost_fail.violation.property, "probe:list-matches-model");

    // Confirm site/public/index.html and skills/blofeld/SKILL.md examples enforce locked-item-cannot-be-deleted
    let locked_url = spawn_stateful_mcp_server(StatefulBug::AllowDeleteLocked).await;
    for (label, yaml) in [
        ("skills/blofeld/SKILL.md", skill_yaml),
        ("site/public/index.html", site_yaml),
    ] {
        let mut locked_cfg = config_with_yaml(locked_url.clone(), yaml);
        locked_cfg.runs = Some(25);
        locked_cfg.steps = 30;
        locked_cfg.seed = 42;
        locked_cfg.exclude_tools = vec!["archive_item".into()];
        let locked_summary = explore(&locked_cfg).await.unwrap();
        let locked_fail = locked_summary
            .failure
            .unwrap_or_else(|| panic!("{label} YAML example should catch AllowDeleteLocked"));
        assert_eq!(
            locked_fail.violation.property,
            "rule:locked-item-cannot-be-deleted"
        );
    }
}
