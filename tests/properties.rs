// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: Apache-2.0

use blofeld::explorer::parse_duration;
use blofeld::mcp::{
    Action, Discovery, InputKind, RpcOutcome, Target, parse_sse_response, parse_token_str,
};
use blofeld::properties::{RuleSet, check_step};
use blofeld::schema::{
    ValuePool, extract_response_bindings, generate_tool_args, substitute_values,
    validate_against_schema,
};
use hegel::TestCase;
use hegel::extras::serde_json as hegel_json;
use hegel::generators as gs;
use serde_json::{Map, Value, json};
use std::time::Duration;

fn draw_leaf_schema(tc: &TestCase) -> Value {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
        0 => {
            let min = tc.draw(gs::integers::<u64>().min_value(0).max_value(8));
            let max = tc.draw(gs::integers::<u64>().min_value(min).max_value(min + 16));
            json!({ "type": "string", "minLength": min, "maxLength": max })
        }
        1 => {
            let min = tc.draw(gs::integers::<i64>().min_value(-500).max_value(500));
            let max = tc.draw(gs::integers::<i64>().min_value(min).max_value(min + 500));
            json!({ "type": "integer", "minimum": min, "maximum": max })
        }
        2 => {
            let min = tc.draw(gs::integers::<i32>().min_value(-100).max_value(100)) as f64;
            let span = tc.draw(gs::integers::<u32>().min_value(0).max_value(200)) as f64;
            json!({ "type": "number", "minimum": min, "maximum": min + span })
        }
        3 => json!({ "type": "boolean" }),
        4 => {
            let opts = tc.draw(
                gs::vecs(gs::text().min_size(1).max_size(10))
                    .min_size(1)
                    .max_size(5),
            );
            json!({ "type": "string", "enum": opts })
        }
        _ => {
            let min_items = tc.draw(gs::integers::<u64>().min_value(0).max_value(3));
            let max_items = tc.draw(
                gs::integers::<u64>()
                    .min_value(min_items)
                    .max_value(min_items + 3),
            );
            json!({
                "type": "array",
                "minItems": min_items,
                "maxItems": max_items,
                "items": { "type": "integer", "minimum": 0, "maximum": 50 }
            })
        }
    }
}

#[hegel::test]
fn valid_generated_args_always_satisfy_schema(tc: TestCase) {
    let field_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    let mut props = Map::new();
    let mut required = Vec::new();

    for i in 0..field_count {
        let key = format!("field_{i}");
        props.insert(key.clone(), draw_leaf_schema(&tc));
        if tc.draw(gs::booleans()) {
            required.push(Value::String(key));
        }
    }

    let schema = json!({
        "type": "object",
        "properties": props,
        "required": required,
    });

    let mut pool = ValuePool::default();
    let seed_strs = tc.draw(gs::vecs(gs::text().max_size(12)).max_size(4));
    pool.seed_resource_uris(&seed_strs);

    let generated = generate_tool_args(&schema, &pool, &tc, InputKind::Valid);
    let errors = validate_against_schema(&generated, &schema, "$");
    assert!(
        errors.is_empty(),
        "generated Valid args violated schema!\nschema: {schema}\nvalue: {generated}\nerrors: {errors:?}"
    );
}

#[hegel::test]
fn sse_parser_extracts_matching_id_amid_noise(tc: TestCase) {
    let target_id = tc.draw(gs::integers::<u64>().min_value(1).max_value(10_000));
    let payload_val = tc.draw(hegel_json::values());
    let target_frame = json!({
        "jsonrpc": "2.0",
        "id": target_id,
        "result": payload_val,
    });

    let before_noise = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let after_noise = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));

    let mut sse = String::new();
    for i in 0..before_noise {
        sse.push_str(": keepalive comment\n");
        let other_id = target_id.wrapping_add((i as u64) + 1);
        if tc.draw(gs::booleans()) {
            // Server notification (no id)
            sse.push_str(&format!(
                "event: message\ndata: {}\n\n",
                json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": { "n": i } })
            ));
        } else {
            // Unrelated response id
            sse.push_str(&format!(
                "event: message\ndata: {}\n\n",
                json!({ "jsonrpc": "2.0", "id": other_id, "result": {} })
            ));
        }
    }

    // Target event (optionally pretty-printed across multiple `data:` lines!)
    if tc.draw(gs::booleans()) {
        sse.push_str("event: message\n");
        for line in serde_json::to_string_pretty(&target_frame).unwrap().lines() {
            sse.push_str("data: ");
            sse.push_str(line);
            sse.push('\n');
        }
        sse.push('\n');
    } else {
        sse.push_str(&format!("data: {target_frame}\n\n"));
    }

    for i in 0..after_noise {
        let other_id = target_id.wrapping_add((i as u64) + 100);
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({ "jsonrpc": "2.0", "id": other_id, "result": null })
        ));
    }

    let expected: Value = serde_json::from_str(&target_frame.to_string()).unwrap();
    let parsed = parse_sse_response(&sse, target_id).expect("must extract matching SSE response");
    assert_eq!(parsed, expected);
}

#[hegel::test]
fn substitute_values_remaps_nested_scalars_without_corrupting_others(tc: TestCase) {
    let orig = tc.draw(gs::text().min_size(1).max_size(16));
    let live = tc.draw(gs::text().min_size(1).max_size(16));
    let keep = tc.draw(gs::text().min_size(1).max_size(16));
    tc.assume(orig != keep && orig != live);

    let input = json!({
        "direct": orig,
        "untouched": keep,
        "nested": {
            "arr": [orig, keep, 42, true, null]
        }
    });

    let subs = vec![(Value::String(orig.clone()), Value::String(live.clone()))];
    let out = substitute_values(&input, &subs);

    assert_eq!(out["direct"], Value::String(live.clone()));
    assert_eq!(out["untouched"], Value::String(keep.clone()));
    assert_eq!(out["nested"]["arr"][0], Value::String(live));
    assert_eq!(out["nested"]["arr"][1], Value::String(keep));
    assert_eq!(out["nested"]["arr"][2], json!(42));
    assert_eq!(out["nested"]["arr"][3], json!(true));
    assert_eq!(out["nested"]["arr"][4], Value::Null);

    // Empty substitution list is always an exact identity transform on arbitrary JSON trees
    let arbitrary = tc.draw(hegel_json::values());
    assert_eq!(substitute_values(&arbitrary, &[]), arbitrary);
}

#[hegel::test]
fn invalid_jsonrpc_envelopes_are_always_flagged(tc: TestCase) {
    let req_id = tc.draw(gs::integers::<u64>().min_value(1).max_value(10_000));
    let bad_envelope = match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
        0 => {
            let bad_ver = tc.draw(gs::text().max_size(8));
            tc.assume(bad_ver != "2.0");
            json!({ "jsonrpc": bad_ver, "id": req_id, "result": {} })
        }
        1 => {
            let wrong_id = tc.draw(gs::integers::<u64>().min_value(1).max_value(10_000));
            tc.assume(wrong_id != req_id);
            json!({ "jsonrpc": "2.0", "id": wrong_id, "result": {} })
        }
        2 => json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {},
            "error": { "code": -32600, "message": "both present" }
        }),
        3 => json!({ "jsonrpc": "2.0", "id": req_id }),
        4 => json!({ "jsonrpc": "2.0", "id": req_id, "error": "not-an-object" }),
        _ => json!({ "jsonrpc": "2.0", "id": req_id, "error": { "code": "not-an-int" } }),
    };

    let outcome = RpcOutcome {
        request_id: req_id,
        latency: Duration::from_millis(1),
        http_status: Some(200),
        www_authenticate: None,
        raw_response: Some(bad_envelope),
        transport_error: None,
        stdout_pollution: vec![],
    };

    let violations = check_step(
        &Action::Ping,
        &outcome,
        &Discovery::default(),
        &RuleSet::default(),
    );
    assert!(
        violations
            .iter()
            .any(|v| v.property == "valid_jsonrpc_envelope"),
        "expected valid_jsonrpc_envelope violation, got {violations:?}"
    );
}

#[hegel::test]
fn value_pool_never_harvests_from_error_outcomes(tc: TestCase) {
    let msg = tc.draw(gs::text().min_size(1).max_size(40));
    let raw = if tc.draw(gs::booleans()) {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "isError": true,
                "content": [{ "type": "text", "text": msg }]
            }
        })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32602, "message": msg }
        })
    };

    let outcome = RpcOutcome {
        request_id: 1,
        latency: Duration::from_millis(1),
        http_status: Some(200),
        www_authenticate: None,
        raw_response: Some(raw),
        transport_error: None,
        stdout_pollution: vec![],
    };

    let mut pool = ValuePool::default();
    pool.harvest_outcome(&outcome);
    assert!(
        pool.is_empty(),
        "error outcome polluted ValuePool: {pool:?}"
    );
}

#[hegel::test]
fn response_bindings_extract_nested_ids_from_text_and_structured_content(tc: TestCase) {
    let id_val = tc.draw(
        gs::text()
            .min_size(1)
            .max_size(32)
            .min_codepoint(0x21)
            .max_codepoint(0x7E),
    );
    let outcome = RpcOutcome {
        request_id: 1,
        latency: Duration::from_millis(1),
        http_status: Some(200),
        www_authenticate: None,
        raw_response: Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{ "type": "text", "text": json!({ "businesses": [{ "slug": id_val }] }).to_string() }],
                "structuredContent": { "id": id_val }
            }
        })),
        transport_error: None,
        stdout_pollution: vec![],
    };

    let bindings = extract_response_bindings(&outcome);
    assert!(
        bindings
            .iter()
            .any(|(p, v)| p == "sc.id" && v == &Value::String(id_val.clone()))
    );
    assert!(
        bindings
            .iter()
            .any(|(p, v)| p == "text0.businesses[0].slug" && v == &Value::String(id_val.clone()))
    );

    // Also verify ValuePool binds `businesses[].slug` under singular `business`
    let mut pool = ValuePool::default();
    pool.harvest_outcome(&outcome);
    assert!(!pool.is_empty());
}

#[hegel::test]
fn parse_token_supports_raw_and_json_auth_files(tc: TestCase) {
    let tok = tc.draw(
        gs::text()
            .min_size(1)
            .max_size(64)
            .min_codepoint(0x21)
            .max_codepoint(0x7E),
    );
    tc.assume(!tok.starts_with('{'));

    // Raw text token with surrounding whitespace
    assert_eq!(parse_token_str(&format!("  {tok}\n")).unwrap(), tok);

    // JSON auth files with accessToken / access_token / token
    let keys = ["accessToken", "access_token", "token"];
    let key = keys[tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let json_doc = json!({ key: tok, "expiresIn": 3600 }).to_string();
    assert_eq!(parse_token_str(&json_doc).unwrap(), tok);
}

#[hegel::test]
fn target_redacted_strips_sensitive_headers_and_keeps_safe_ones(tc: TestCase) {
    let secret = tc.draw(gs::text().min_size(4).max_size(32));
    let safe_val = tc.draw(gs::text().min_size(1).max_size(24));

    let target = Target::Http {
        url: "https://example.com/mcp".into(),
        headers: vec![
            ("Authorization".into(), format!("Bearer {secret}")),
            ("X-API-Key".into(), secret.clone()),
            ("Cookie".into(), format!("session={secret}")),
            ("X-Custom-Token".into(), secret.clone()),
            ("X-Request-Id".into(), safe_val.clone()),
        ],
        insecure: false,
    };

    let Target::Http { headers, .. } = target.redacted() else {
        unreachable!()
    };
    for (k, v) in &headers {
        if k == "X-Request-Id" {
            assert_eq!(v, &safe_val);
        } else {
            assert_eq!(v, "<REDACTED>", "header {k} was not redacted");
        }
    }
}

#[hegel::test]
fn parse_duration_handles_all_units_and_rejects_invalid(tc: TestCase) {
    let units: &[(&str, u64)] = &[
        ("ms", 1),
        ("s", 1_000),
        ("m", 60_000),
        ("h", 3_600_000),
        ("d", 86_400_000),
    ];
    let idx = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(units.len() - 1),
    );
    let (suffix, mult_ms) = units[idx];
    let n = tc.draw(gs::integers::<u64>().min_value(1).max_value(1_000));

    let parsed = parse_duration(&format!("{n}{suffix}")).unwrap();
    assert_eq!(parsed, Duration::from_millis(n * mult_ms));

    assert!(parse_duration("").is_err());
    assert!(parse_duration("0s").is_err());
    assert!(parse_duration("42").is_err());
    assert!(parse_duration("10w").is_err());
}
