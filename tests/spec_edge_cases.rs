// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use blofeld::explorer::{RunConfig, SavedTrace, explore, replay_trace, save_trace};
use blofeld::mcp::{Action, InputKind, RpcOutcome, Target, ToolDef};
use blofeld::properties::RuleSet;
use blofeld::schema::ValuePool;
use blofeld::spec::SpecEngine;
use hegel::Hegel;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn bug_gen_float_accepts_integer_bounds_and_pick_value_and_helpers() {
    let rules = RuleSet::from_yaml(
        r#"
state:
  items:
    a: "open"
    b: "locked"
actions_only: true
actions:
  - name: float-action
    tool: act
    args:
      x: "gen_float(0, 10)"
      y: "gen_float(0.0, 1.5)"
      v: "pick_value(state.items)"
"#,
    )
    .unwrap();
    let engine = SpecEngine::new(rules.compile().unwrap());
    let tools = vec![ToolDef {
        name: "act".into(),
        description: None,
        input_schema: json!({
            "type": "object",
            "properties": {
                "x": { "type": "number" },
                "y": { "type": "number" },
                "v": { "type": "string" }
            },
            "required": ["x", "y", "v"]
        }),
        output_schema: None,
        annotations: None,
    }];
    let pool = ValuePool::default();
    let called = Arc::new(AtomicUsize::new(0));
    let called_clone = Arc::clone(&called);

    Hegel::new(move |tc| {
        let action = engine
            .try_pick_action(&tools, &pool, &tc, 1, true)
            .expect("action evaluation should succeed")
            .expect("gen_float(0, 10) with integer bounds should succeed");
        if let Action::CallTool { arguments, .. } = action {
            let x = arguments["x"].as_f64().unwrap();
            let y = arguments["y"].as_f64().unwrap();
            let v = arguments["v"].as_str().unwrap();
            assert!((0.0..=10.0).contains(&x));
            assert!((0.0..=1.5).contains(&y));
            assert!(v == "open" || v == "locked");
            called_clone.fetch_add(1, Ordering::Relaxed);
        }
    })
    .settings(hegel::Settings::new().test_cases(5))
    .run();

    assert!(called.load(Ordering::Relaxed) >= 5);
}

#[test]
fn bug_negative_int_keys_sorted_and_set_entry_remove_key() {
    let rules = RuleSet::from_yaml(
        r#"
state:
  m:
    a: 1
transitions:
  - name: check-helpers
    tool: t
    when: "ok"
    assert:
      - "keys({-10: 'x', -1: 'y', 0: 'z', 2: 'w'}) == [-10, -1, 0, 2]"
      - "set_entry(state.m, 'b', 2)['b'] == 2"
      - "!('a' in remove_key(state.m, 'a'))"
"#,
    )
    .unwrap();
    let mut engine = SpecEngine::new(rules.compile().unwrap());
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
    let violations = engine.apply_transitions(
        &Action::CallTool {
            name: "t".into(),
            arguments: json!({}),
            input_kind: InputKind::Valid,
        },
        &outcome,
        1,
        false,
    );
    assert!(violations.is_empty(), "got violations: {violations:?}");
}

#[test]
fn bug_delete_multiple_array_indices_removes_original_indices() {
    let rules = RuleSet::from_yaml(
        r#"
state:
  arr: ["a", "b", "c", "d"]
transitions:
  - name: drop-two
    tool: t
    when: "ok"
    delete:
      - "arr[0]"
      - "arr[1]"
"#,
    )
    .unwrap();
    let mut engine = SpecEngine::new(rules.compile().unwrap());
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
    let violations = engine.apply_transitions(
        &Action::CallTool {
            name: "t".into(),
            arguments: json!({}),
            input_kind: InputKind::Valid,
        },
        &outcome,
        1,
        false,
    );
    assert!(violations.is_empty(), "got violations: {violations:?}");
    assert_eq!(engine.state().get("arr"), Some(&json!(["c", "d"])));
}

#[test]
fn bug_compile_rejects_non_object_arguments_and_empty_filters() {
    assert!(
        RuleSet::from_yaml(
            r#"
actions:
  - tool: foo
    args: 123
"#
        )
        .is_err(),
        "literal integer args should be rejected at compile time"
    );
    assert!(
        RuleSet::from_yaml(
            r#"
init:
  - call:
      tool: foo
      arguments: [1, 2]
"#
        )
        .is_err(),
        "array init arguments should be rejected at compile time"
    );
    assert!(
        RuleSet::from_yaml(
            r#"
probes:
  - name: p
    after: []
    call:
      tool: foo
"#
        )
        .is_err(),
        "empty probe `after: []` should be rejected at compile time"
    );
    assert!(
        RuleSet::from_yaml(
            r#"
transitions:
  - name: tr
    tool: []
"#
        )
        .is_err(),
        "empty transition `tool: []` should be rejected at compile time"
    );
}

#[derive(Default)]
struct UuidSession {
    prefix: String,
    next_id: u64,
    items: BTreeMap<String, String>,
}

/// Spawns a server whose IDs include the session number (`s{sid}-item-{n}`) so every fresh
/// session produces unique IDs (simulating random UUIDs).
async fn spawn_random_id_mcp_server(
    mutating_calls: Arc<AtomicUsize>,
    allow_delete_locked: bool,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sessions: Arc<Mutex<BTreeMap<String, UuidSession>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let next_sid = Arc::new(Mutex::new(1u64));

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let sessions = Arc::clone(&sessions);
            let next_sid = Arc::clone(&next_sid);
            let mutating_calls = Arc::clone(&mutating_calls);
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
                let mut body_bytes = body.as_bytes().to_vec();
                while body_bytes.len() < content_len {
                    let mut more = vec![0u8; content_len - body_bytes.len()];
                    match stream.read(&mut more).await {
                        Ok(0) | Err(_) => break,
                        Ok(m) => body_bytes.extend_from_slice(&more[..m]),
                    }
                }
                if method == "DELETE" {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                let Ok(rpc) = serde_json::from_slice::<Value>(&body_bytes) else {
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
                        let sid_num = {
                            let mut g = next_sid.lock().unwrap();
                            let v = *g;
                            *g += 1;
                            v
                        };
                        let new_id = format!("sess-{sid_num}");
                        let mut sess = UuidSession {
                            prefix: format!("uuid-s{sid_num}"),
                            next_id: 0,
                            items: BTreeMap::new(),
                        };
                        sess.items
                            .insert(format!("uuid-s{sid_num}-seed"), "open".into());
                        sessions.lock().unwrap().insert(new_id.clone(), sess);
                        extra_headers.push_str(&format!("Mcp-Session-Id: {new_id}\r\n"));
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "uuid-mcp", "version": "0.1.0" }
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
                        let sess = guard.entry(session_key).or_default();
                        match tool {
                            "list_items" => {
                                let items: Vec<Value> = sess
                                    .items
                                    .iter()
                                    .map(|(k, st)| json!({ "id": k, "status": st }))
                                    .collect();
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{ "type": "text", "text": "ok" }],
                                        "structuredContent": { "items": items }
                                    }
                                })
                            }
                            "create_item" => {
                                mutating_calls.fetch_add(1, Ordering::Relaxed);
                                if args
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .is_none_or(|s| s.is_empty())
                                {
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "isError": true,
                                            "content": [{ "type": "text", "text": "invalid name" }]
                                        }
                                    })
                                } else {
                                    sess.next_id += 1;
                                    let item_id = format!("{}-item-{}", sess.prefix, sess.next_id);
                                    sess.items.insert(item_id.clone(), "open".into());
                                    json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "result": {
                                            "content": [{ "type": "text", "text": "created" }],
                                            "structuredContent": { "id": item_id, "status": "open" }
                                        }
                                    })
                                }
                            }
                            "lock_item" => {
                                mutating_calls.fetch_add(1, Ordering::Relaxed);
                                let item_id = args.get("id").and_then(Value::as_str).unwrap_or("");
                                if let Some(st) = sess.items.get_mut(item_id) {
                                    *st = "locked".into();
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
                            "delete_item" => {
                                mutating_calls.fetch_add(1, Ordering::Relaxed);
                                let item_id = args.get("id").and_then(Value::as_str).unwrap_or("");
                                if let Some(st) = sess.items.get(item_id).cloned() {
                                    if st == "locked" && !allow_delete_locked {
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": {
                                                "isError": true,
                                                "content": [{ "type": "text", "text": "locked" }]
                                            }
                                        })
                                    } else {
                                        sess.items.remove(item_id);
                                        json!({
                                            "jsonrpc": "2.0",
                                            "id": id,
                                            "result": {
                                                "content": [{ "type": "text", "text": "deleted" }]
                                            }
                                        })
                                    }
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
                            _ => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32602, "message": "unknown tool" }
                            }),
                        }
                    }
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "Method not found" }
                    }),
                };
                let wire = resp_val.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{wire}",
                    wire.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[tokio::test]
async fn bug_read_only_must_not_execute_mutating_init_or_probes() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(Arc::clone(&mutating_calls), false).await;
    let rules = RuleSet::from_yaml(
        r#"
init:
  - name: bad-mutating-init
    call:
      tool: create_item
      arguments:
        name: "'should-not-run'"
"#,
    )
    .unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(2),
        steps: 5,
        time_limit: None,
        seed: 1,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: true,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: false,
        antithesis_mode: false,
        verbose: false,
    };

    let res = explore(&cfg).await;
    assert_eq!(
        mutating_calls.load(Ordering::Relaxed),
        0,
        "--read-only must never call non-readOnlyHint init tool"
    );
    assert!(
        res.is_err(),
        "expected --read-only with mutating init tool to return an error"
    );
}

#[tokio::test]
async fn bug_replay_remaps_fresh_per_session_uuids() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(mutating_calls, true).await;
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
rules:
  - name: no-delete-locked
    tool: delete_item
    when: "args.id in state.items && state.items[args.id] == 'locked'"
    assert: "!ok"
actions:
  - tool: create_item
    args:
      name: "gen_string(1, 5)"
  - tool: lock_item
    when: "'open' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'open')"
  - tool: delete_item
    when: "'locked' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'locked')"
actions_only: true
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(15),
        steps: 20,
        time_limit: None,
        seed: 42,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    };
    let summary = explore(&cfg).await.unwrap();
    let fail = summary.failure.expect("should find delete-locked bug");
    assert_eq!(fail.violation.property, "rule:no-delete-locked");
    assert_eq!(fail.shrunk_actions.len(), 3);

    let tmp = std::env::temp_dir().join("blofeld_test_uuid_replay.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replay must remap fresh per-session UUIDs and reproduce bug");
    assert_eq!(step, 3);
    assert_eq!(v.property, "rule:no-delete-locked");
}

#[tokio::test]
async fn read_only_rejects_mutating_probe_and_allows_readonly_init_and_probe() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(Arc::clone(&mutating_calls), false).await;

    // Mutating probe under --read-only must fail fast without calling the mutating tool.
    let bad_probe_rules = RuleSet::from_yaml(
        r#"
probes:
  - name: bad-mutating-probe
    after: every_step
    call:
      tool: create_item
      arguments:
        name: "'nope'"
"#,
    )
    .unwrap();
    let bad_cfg = RunConfig {
        target: Target::Http {
            url: url.clone(),
            headers: vec![],
            insecure: false,
        },
        runs: Some(2),
        steps: 5,
        time_limit: None,
        seed: 1,
        timeout: Duration::from_secs(2),
        rules: bad_probe_rules,
        adversarial_rate: None,
        read_only: true,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: false,
        antithesis_mode: false,
        verbose: false,
    };
    assert!(explore(&bad_cfg).await.is_err());
    assert_eq!(mutating_calls.load(Ordering::Relaxed), 0);

    // Read-only init + read-only probe under --read-only must pass cleanly.
    let good_rules = RuleSet::from_yaml(
        r#"
state:
  seeded: 0
init:
  - name: seed-list
    call:
      tool: list_items
    assert: "ok && size(res.items) == 1"
    update:
      seeded: "size(res.items)"
probes:
  - name: check-list-every-step
    after: every_step
    call:
      tool: list_items
    assert: "ok && size(res.items) == state.seeded"
"#,
    )
    .unwrap();
    let good_cfg = RunConfig {
        rules: good_rules,
        ..bad_cfg
    };
    let summary = explore(&good_cfg).await.unwrap();
    assert!(summary.failure.is_none(), "got {:?}", summary.failure);
    assert_eq!(mutating_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn multi_probe_state_chaining_and_list_form_after_and_only_filter() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(mutating_calls, false).await;

    // Two probes on list-form `after: [create_item, lock_item]`:
    // Probe 1 updates `state.probe_count` and `state.last_seen`;
    // Probe 2 checks `when: "state.probe_count > 0"` and reads `state.last_seen` set by Probe 1!
    let rules = RuleSet::from_yaml(
        r#"
state:
  probe_count: 0
  last_seen: 0
probes:
  - name: probe-one-records
    after: [create_item, lock_item]
    call:
      tool: list_items
    assert: "ok"
    update:
      probe_count: "state.probe_count + 1"
      last_seen: "size(res.items)"
  - name: probe-two-sees-probe-one-update
    after: [create_item, lock_item]
    when: "state.probe_count > 0"
    call:
      tool: list_items
    assert:
      - "ok"
      - "size(res.items) == state.last_seen"
      - "state.probe_count >= 1"
actions:
  - tool: create_item
    args:
      name: "gen_string(1, 5)"
  - tool: delete_item
    args:
      id: "'never-allowed-by-only'"
actions_only: true
"#,
    )
    .unwrap();

    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(3),
        steps: 6,
        time_limit: None,
        seed: 7,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: false,
        // Restrict actions to `create_item` only; probes calling `list_items` still run!
        only_tools: vec!["create_item".into()],
        exclude_tools: vec![],
        shrink: false,
        antithesis_mode: false,
        verbose: false,
    };
    let summary = explore(&cfg).await.unwrap();
    assert!(summary.failure.is_none(), "got {:?}", summary.failure);
}

#[tokio::test]
async fn unguarded_empty_pick_skips_cleanly_while_cel_typo_fails_loudly() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(mutating_calls, false).await;

    // 1. Unguarded `pick_key(state.items)` alongside `create_item` in `actions_only: true`:
    //    empty pick should skip `lock_item` and still run `create_item` without failing or
    //    prematurely aborting every episode at step 0.
    let unguarded_rules = RuleSet::from_yaml(
        r#"
state:
  items: {}
transitions:
  - tool: create_item
    when: "ok"
    update:
      "items[res.id]": "'open'"
actions_only: true
actions:
  - name: unguarded-lock
    tool: lock_item
    weight: 5
    args:
      id: "pick_key(state.items)"
  - name: create
    tool: create_item
    args:
      name: "gen_string(1, 4)"
"#,
    )
    .unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url: url.clone(),
            headers: vec![],
            insecure: false,
        },
        runs: Some(4),
        steps: 6,
        time_limit: None,
        seed: 11,
        timeout: Duration::from_secs(2),
        rules: unguarded_rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: false,
        antithesis_mode: false,
        verbose: false,
    };
    let summary = explore(&cfg).await.unwrap();
    assert!(summary.failure.is_none(), "got {:?}", summary.failure);
    assert!(
        summary.total_steps >= 4,
        "expected episodes to execute actions after skipping empty pick, got {} steps",
        summary.total_steps
    );

    // 2. Terminal state where no action `when:` is satisfied terminates cleanly.
    let terminal_rules = RuleSet::from_yaml(
        r#"
state:
  done: false
transitions:
  - tool: create_item
    when: "ok"
    update:
      done: true
actions_only: true
actions:
  - name: create-once
    tool: create_item
    when: "!state.done"
    args:
      name: "'once'"
"#,
    )
    .unwrap();
    let term_summary = explore(&RunConfig {
        rules: terminal_rules,
        ..cfg.clone()
    })
    .await
    .unwrap();
    assert!(term_summary.failure.is_none());
    assert_eq!(term_summary.total_steps, 4);

    // 3. Genuine CEL typo in `actions.args` MUST surface loudly as a violation!
    let typo_rules = RuleSet::from_yaml(
        r#"
state:
  items: {}
actions_only: true
actions:
  - name: broken-typo
    tool: lock_item
    args:
      id: "pick_key(state.typo_field)"
"#,
    )
    .unwrap();
    let typo_summary = explore(&RunConfig {
        rules: typo_rules,
        ..cfg
    })
    .await
    .unwrap();
    let fail = typo_summary
        .failure
        .expect("CEL typo in actions.args must fail loudly instead of silently passing");
    assert_eq!(fail.violation.property, "action:broken-typo");
}

#[tokio::test]
async fn init_seeded_uuid_trace_shrinks_and_replays_cleanly() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(mutating_calls, true).await;

    // Seed the fresh per-session `uuid-s{sid}-seed` item via `init:`, then lock -> delete it.
    // Tests both `init_produced` refreshing across ddmin passes and `--replay` with init UUIDs!
    let yaml = r#"
state:
  items: {}
init:
  - name: seed-from-server
    call:
      tool: list_items
    assert: "ok && size(res.items) == 1"
    update:
      "items[res.items[0].id]": "res.items[0].status"
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
rules:
  - name: no-delete-locked
    tool: delete_item
    when: "args.id in state.items && state.items[args.id] == 'locked'"
    assert: "!ok"
actions:
  - tool: create_item
    args:
      name: "'noise'"
  - tool: lock_item
    when: "'open' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'open')"
  - tool: delete_item
    when: "'locked' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'locked')"
actions_only: true
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(15),
        steps: 15,
        time_limit: None,
        seed: 5,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    };
    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("should find delete-locked on init-seeded item");
    assert_eq!(fail.violation.property, "rule:no-delete-locked");
    // Minimal trace using the init-seeded item is 2 steps: lock_item(seed) -> delete_item(seed)
    assert_eq!(
        fail.shrunk_actions.len(),
        2,
        "expected 2-step trace using init-seeded UUID, got {:?}",
        fail.shrunk_actions
    );

    let tmp = std::env::temp_dir().join("blofeld_test_init_uuid_replay.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("replay must remap init-seeded per-session UUID");
    assert_eq!(step, 2);
    assert_eq!(v.property, "rule:no-delete-locked");
}

#[tokio::test]
async fn stdio_transport_runs_stateful_spec_and_catches_probe_bug() {
    // Minimal stateful MCP server over stdio (Python 3) with a ghost-delete bug.
    let py_server = r#"
import json, sys

items = {}
next_id = 0

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    rid = req.get("id")
    if rid is None:
        continue
    method = req.get("method", "")
    if method == "initialize":
        res = {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "stdio-mcp", "version": "0.1"}}
    elif method == "tools/list":
        res = {"tools": [
            {"name": "list_items", "annotations": {"readOnlyHint": True}, "inputSchema": {"type": "object", "properties": {}}},
            {"name": "create_item", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
            {"name": "delete_item", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
        ]}
    elif method == "resources/list":
        res = {"resources": []}
    elif method == "prompts/list":
        res = {"prompts": []}
    elif method == "ping":
        res = {}
    elif method == "tools/call":
        name = req["params"]["name"]
        args = req["params"].get("arguments", {})
        if name == "list_items":
            res = {"content": [{"type": "text", "text": "ok"}], "structuredContent": {"count": len(items)}}
        elif name == "create_item":
            next_id += 1
            iid = f"s-{next_id}"
            items[iid] = args.get("name", "")
            res = {"content": [{"type": "text", "text": "created"}], "structuredContent": {"id": iid}}
        elif name == "delete_item":
            # Ghost delete bug: returns ok without removing from items!
            res = {"content": [{"type": "text", "text": "deleted"}]}
        else:
            sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "error": {"code": -32602, "message": "unknown"}}) + "\n")
            sys.stdout.flush()
            continue
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "not found"}}) + "\n")
        sys.stdout.flush()
        continue
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": res}) + "\n")
    sys.stdout.flush()
"#;

    let yaml = r#"
state:
  items: {}
transitions:
  - tool: create_item
    when: "ok"
    update:
      "items[res.id]": "'open'"
  - tool: delete_item
    when: "ok"
    delete: ["items[args.id]"]
probes:
  - name: count-matches
    after: mutating
    call:
      tool: list_items
    assert: "ok && res.count == size(state.items)"
actions:
  - tool: create_item
    args:
      name: "gen_string(1, 4)"
  - tool: delete_item
    when: "size(state.items) > 0"
    args:
      id: "pick_key(state.items)"
actions_only: true
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Stdio {
            command: vec!["python3".into(), "-u".into(), "-c".into(), py_server.into()],
        },
        runs: Some(10),
        steps: 10,
        time_limit: None,
        seed: 77,
        timeout: Duration::from_secs(3),
        rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    };

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("stdio stateful spec should catch ghost delete via probe");
    assert_eq!(fail.violation.property, "probe:count-matches");
    assert_eq!(fail.shrunk_actions.len(), 2);

    let tmp = std::env::temp_dir().join("blofeld_test_stdio_spec_replay.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("stdio replay should reproduce probe violation");
    assert_eq!(step, 2);
    assert_eq!(v.property, "probe:count-matches");
}

#[test]
fn old_saved_trace_json_without_bindings_still_deserializes() {
    let legacy_json = r#"{
        "target": { "Http": { "url": "http://127.0.0.1:1/mcp", "headers": [], "insecure": false } },
        "seed": 42,
        "violation": { "property": "no_http_5xx", "message": "500" },
        "original_steps": 3,
        "actions": []
    }"#;
    let parsed: SavedTrace = serde_json::from_str(legacy_json).expect("legacy trace JSON parses");
    assert!(parsed.init_produced.is_empty());
    assert!(parsed.step_produced.is_empty());
}

#[test]
fn adversarial_rate_validation_rejects_out_of_range() {
    assert!(RuleSet::from_yaml("adversarial_rate: -0.1\n").is_err());
    assert!(RuleSet::from_yaml("adversarial_rate: 1.5\n").is_err());
    let valid = RuleSet::from_yaml("adversarial_rate: 0.25\n").unwrap();
    assert!((valid.compile().unwrap().adversarial_rate - 0.25).abs() < f64::EPSILON);
}

#[tokio::test]
async fn adversarial_cross_state_breaks_positive_only_state_machine_and_shrinks() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(mutating_calls, true).await;

    // Notice: `actions:` ONLY contains positive happy-path actions (`delete_item` explicitly
    // guards on `'open'`). Without adversarial mode, `delete_item` would NEVER receive a locked
    // item. With `adversarial_rate: 0.35`, Blofeld relaxes `pick_key_where` to try deleting a
    // locked item, catches `rule:no-delete-locked`, shrinks to 3 steps, and replays cleanly!
    let yaml = r#"
state:
  items: {}
actions_only: true
adversarial_rate: 0.35
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
  - name: deleted
    tool: delete_item
    when: "ok"
    delete:
      - "items[args.id]"
actions:
  - name: create
    tool: create_item
    args:
      name: "gen_string(1, 5)"
  - name: lock-open
    tool: lock_item
    when: "'open' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'open')"
  - name: delete-open-only
    tool: delete_item
    when: "'open' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'open')"
rules:
  - name: no-delete-locked
    tool: delete_item
    when: "args.id in state.items && state.items[args.id] == 'locked'"
    assert: "!ok"
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(25),
        steps: 25,
        time_limit: None,
        seed: 99,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    };

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("adversarial cross-state fuzzing should break positive-only delete_item spec");
    assert_eq!(fail.violation.property, "rule:no-delete-locked");
    assert_eq!(fail.shrunk_actions.len(), 3);

    let tmp = std::env::temp_dir().join("blofeld_test_adversarial_cross_state.json");
    save_trace(&tmp, &SavedTrace::from_failure(cfg.target.clone(), &fail)).unwrap();
    let replayed = replay_trace(&cfg, &tmp).await.unwrap();
    let _ = std::fs::remove_file(&tmp);
    let (step, v) = replayed.expect("adversarial counterexample must replay deterministically");
    assert_eq!(step, 3);
    assert_eq!(v.property, "rule:no-delete-locked");
}

#[tokio::test]
async fn adversarial_respects_read_only_mode() {
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(Arc::clone(&mutating_calls), false).await;

    let yaml = r#"
state:
  seeded: 0
actions_only: true
init:
  - name: seed
    call: { tool: list_items }
    update:
      seeded: "size(res.items)"
actions:
  - name: read
    tool: list_items
    args: {}
  - name: hidden-write
    tool: create_item
    args:
      name: "'must-never-run'"
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(5),
        steps: 10,
        time_limit: None,
        seed: 7,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: Some(0.5),
        read_only: true,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: false,
        antithesis_mode: false,
        verbose: false,
    };

    let summary = explore(&cfg).await.unwrap();
    assert!(summary.failure.is_none(), "got {:?}", summary.failure);
    assert_eq!(
        mutating_calls.load(Ordering::Relaxed),
        0,
        "--read-only with --adversarial must never invoke non-readOnly tools"
    );
}

#[tokio::test]
async fn adversarial_payload_mutation_and_replay_catch_nested_bug() {
    // Server crashes with HTTP 500 when a nested `input.amount` is empty or negative,
    // even though the spec's positive `actions:` only generates positive amounts ("100").
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let Ok(n) = stream.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let Some((head, body)) = req.split_once("\r\n\r\n") else {
                    return;
                };
                let mut content_len: usize = 0;
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
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
                let Ok(rpc) = serde_json::from_slice::<Value>(&body_bytes) else {
                    return;
                };
                let method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
                let Some(id) = rpc.get("id").cloned() else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                let (status, resp) = match method {
                    "initialize" => (
                        "HTTP/1.1 200 OK",
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "nested-bug", "version": "0.1.0" }
                            }
                        }),
                    ),
                    "tools/list" => (
                        "HTTP/1.1 200 OK",
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "post_entry",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "business": { "type": "string" },
                                            "input": { "type": "object" }
                                        },
                                        "required": ["business", "input"]
                                    }
                                }]
                            }
                        }),
                    ),
                    "tools/call" => {
                        let args = rpc
                            .pointer("/params/arguments")
                            .cloned()
                            .unwrap_or(json!({}));
                        let amount = args.pointer("/input/amount").and_then(Value::as_str);
                        if matches!(amount, Some("") | Some("-1")) {
                            (
                                "HTTP/1.1 500 Internal Server Error",
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": { "code": -32603, "message": "crash on negative/empty amount" }
                                }),
                            )
                        } else {
                            (
                                "HTTP/1.1 200 OK",
                                json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": { "content": [{ "type": "text", "text": "{\"ok\":true}" }] }
                                }),
                            )
                        }
                    }
                    _ => (
                        "HTTP/1.1 200 OK",
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "resources": [], "prompts": [] } }),
                    ),
                };
                let wire = resp.to_string();
                let out = format!(
                    "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{wire}",
                    wire.len()
                );
                let _ = stream.write_all(out.as_bytes()).await;
            });
        }
    });

    let yaml = r#"
actions_only: true
actions:
  - name: valid-post
    tool: post_entry
    args:
      business: "'acme'"
      input:
        amount: "'100'"
        memo: "'ok'"
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url: format!("http://{addr}/mcp"),
            headers: vec![],
            insecure: false,
        },
        runs: Some(25),
        steps: 15,
        time_limit: None,
        seed: 42,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: Some(0.5),
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    };

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("adversarial payload mutation should hit nested 500 bug");
    assert_eq!(fail.violation.property, "no_http_5xx");
    assert_eq!(fail.shrunk_actions.len(), 1);
}

#[tokio::test]
async fn adversarial_replay_catches_non_idempotent_create() {
    // Server ignores idempotency_key on `create_item` (always creates a second item),
    // while the spec only issues unique idempotency keys in positive `actions:`.
    // Adversarial step replay re-fires an earlier `create_item` verbatim; our probe checks
    // that server count matches shadow `state.seen_keys` count and catches the double-create!
    let mutating_calls = Arc::new(AtomicUsize::new(0));
    let url = spawn_random_id_mcp_server(mutating_calls, false).await;

    let yaml = r#"
state:
  seq: 0
  base: 0
  seen_names: {}
actions_only: true
adversarial_rate: 0.5
init:
  - name: seed
    call: { tool: list_items }
    update:
      base: "size(res.items)"
transitions:
  - name: created
    tool: create_item
    when: "ok"
    update:
      "seen_names[args.name]": "true"
      seq: "state.seq + 1"
probes:
  - name: no-duplicate-creates-on-replay
    after: [create_item]
    call: { tool: list_items }
    assert: "ok && size(res.items) == state.base + size(state.seen_names)"
actions:
  - name: create-unique
    tool: create_item
    args:
      name: "'unique_' + string(state.seq)"
"#;
    let rules = RuleSet::from_yaml(yaml).unwrap();
    let cfg = RunConfig {
        target: Target::Http {
            url,
            headers: vec![],
            insecure: false,
        },
        runs: Some(15),
        steps: 12,
        time_limit: None,
        seed: 13,
        timeout: Duration::from_secs(2),
        rules,
        adversarial_rate: None,
        read_only: false,
        only_tools: vec![],
        exclude_tools: vec![],
        shrink: true,
        antithesis_mode: false,
        verbose: false,
    };

    let summary = explore(&cfg).await.unwrap();
    let fail = summary
        .failure
        .expect("adversarial replay should catch non-idempotent double-create");
    assert_eq!(
        fail.violation.property,
        "probe:no-duplicate-creates-on-replay"
    );
    assert_eq!(fail.shrunk_actions.len(), 2);
}
