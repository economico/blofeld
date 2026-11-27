// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use crate::mcp::{InputKind, RpcOutcome};
use hegel::TestCase;
use hegel::extras::serde_json as hegel_json;
use hegel::generators as gs;
use serde_json::{Map, Number, Value, json};
use std::collections::HashMap;

const MAX_POOL_PER_BUCKET: usize = 32;

/// Harvests concrete values (IDs, URIs, strings, numbers) from successful server responses
/// so later tool calls can chain onto state created by earlier calls.
#[derive(Debug, Default, Clone)]
pub struct ValuePool {
    by_field: HashMap<String, Vec<Value>>,
    strings: Vec<String>,
    integers: Vec<i64>,
    numbers: Vec<f64>,
}

impl ValuePool {
    pub fn total_values(&self) -> usize {
        self.strings.len()
            + self.integers.len()
            + self.numbers.len()
            + self.by_field.values().map(Vec::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.total_values() == 0
    }

    pub fn strings_slice(&self) -> &[String] {
        &self.strings
    }

    pub fn seed_resource_uris(&mut self, uris: &[String]) {
        for uri in uris {
            self.push_field("uri", Value::String(uri.clone()));
            self.push_string(uri.clone());
        }
    }

    pub fn harvest_outcome(&mut self, outcome: &RpcOutcome) {
        if outcome.is_tool_error() || outcome.error().is_some() {
            return;
        }
        let Some(result) = outcome.result() else {
            return;
        };
        if let Some(structured) = result.get("structuredContent") {
            self.harvest_value(None, structured, 0);
        }
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            for item in content {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    let trimmed = text.trim();
                    if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                        || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                        && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
                    {
                        self.harvest_value(None, &parsed, 0);
                    } else if !trimmed.is_empty() && trimmed.len() <= 128 {
                        self.push_string(trimmed.to_string());
                    }
                }
            }
        }
    }

    fn harvest_value(&mut self, field: Option<&str>, val: &Value, depth: usize) {
        if depth > 6 {
            return;
        }
        match val {
            Value::String(s) if !s.is_empty() && s.len() <= 256 => {
                if let Some(k) = field {
                    self.push_field(k, Value::String(s.clone()));
                }
                self.push_string(s.clone());
            }
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if let Some(k) = field {
                        self.push_field(k, Value::Number(n.clone()));
                    }
                    push_unique(&mut self.integers, i);
                } else if let Some(f) = n.as_f64() {
                    if let Some(k) = field {
                        self.push_field(k, Value::Number(n.clone()));
                    }
                    if !self.numbers.contains(&f) && self.numbers.len() < MAX_POOL_PER_BUCKET {
                        self.numbers.push(f);
                    }
                }
            }
            Value::Object(map) => {
                // If an object inside a named collection (e.g. `businesses: [{ slug: "..." }]`)
                // has an identifier field (`id` or `slug`), also bind it under the singular
                // parent name (`business`) so tools taking `business` pick it up automatically.
                if let Some(parent) = field.and_then(singularize) {
                    for id_key in ["slug", "id"] {
                        if let Some(v) = map.get(id_key)
                            && (v.is_string() || v.is_number())
                        {
                            self.push_field(&parent, v.clone());
                        }
                    }
                }
                for (k, v) in map {
                    self.harvest_value(Some(k), v, depth + 1);
                }
            }
            Value::Array(arr) => {
                for item in arr.iter().take(16) {
                    self.harvest_value(field, item, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn push_field(&mut self, key: &str, val: Value) {
        let norm = normalize_key(key);
        let bucket = self.by_field.entry(norm).or_default();
        if !bucket.contains(&val) {
            if bucket.len() >= MAX_POOL_PER_BUCKET {
                bucket.remove(0);
            }
            bucket.push(val);
        }
    }

    fn push_string(&mut self, s: String) {
        push_unique(&mut self.strings, s);
    }

    fn pick_for_field(
        &self,
        field: &str,
        expected_type: &str,
        schema: &Value,
        tc: &TestCase,
    ) -> Option<Value> {
        let norm = normalize_key(field);
        let candidates = self.by_field.get(&norm).or_else(|| {
            if norm.ends_with("id") {
                self.by_field.get("id")
            } else {
                None
            }
        });
        if let Some(vals) = candidates {
            let matching: Vec<&Value> = vals
                .iter()
                .filter(|v| {
                    value_matches_type(v, expected_type)
                        && validate_against_schema(v, schema, "$").is_empty()
                })
                .collect();
            if !matching.is_empty() {
                let idx = draw_index(tc, matching.len());
                return Some(matching[idx].clone());
            }
        }
        None
    }
}

fn push_unique<T: PartialEq>(vec: &mut Vec<T>, item: T) {
    if !vec.contains(&item) {
        if vec.len() >= MAX_POOL_PER_BUCKET {
            vec.remove(0);
        }
        vec.push(item);
    }
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn singularize(key: &str) -> Option<String> {
    let norm = normalize_key(key);
    if let Some(stem) = norm.strip_suffix("ies")
        && stem.len() >= 2
    {
        return Some(format!("{stem}y"));
    }
    if let Some(stem) = norm.strip_suffix("es")
        && (stem.ends_with('s')
            || stem.ends_with('x')
            || stem.ends_with("ch")
            || stem.ends_with("sh"))
    {
        return Some(stem.to_string());
    }
    if let Some(stem) = norm.strip_suffix('s')
        && stem.len() >= 2
    {
        return Some(stem.to_string());
    }
    None
}

fn value_matches_type(v: &Value, expected: &str) -> bool {
    match expected {
        "string" => v.is_string(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn draw_index(tc: &TestCase, len: usize) -> usize {
    debug_assert!(len > 0);
    if len <= 1 {
        0
    } else {
        tc.draw_silent(gs::integers::<usize>().min_value(0).max_value(len - 1))
    }
}

/// Generate arguments for an MCP tool's `inputSchema` using Hegel's shrinking generators.
pub fn generate_tool_args(
    schema: &Value,
    pool: &ValuePool,
    tc: &TestCase,
    kind: InputKind,
) -> Value {
    match kind {
        InputKind::Valid => gen_value(schema, None, pool, tc, false, 0),
        InputKind::Boundary => gen_value(schema, None, pool, tc, true, 0),
        InputKind::Malformed => gen_malformed_object(schema, tc),
    }
}

/// Fill any missing `required` properties in `obj` using valid schema-guided generation.
/// Does not draw from `tc` if all required properties are already present.
pub fn fill_missing_required_args(
    obj: &mut Map<String, Value>,
    schema: &Value,
    pool: &ValuePool,
    tc: &TestCase,
) {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return;
    };
    let props = schema.get("properties").and_then(Value::as_object);
    for req in required.iter().filter_map(Value::as_str) {
        if obj.contains_key(req) {
            continue;
        }
        let prop_schema = props
            .and_then(|p| p.get(req))
            .cloned()
            .unwrap_or_else(|| json!({ "type": "string" }));
        let val = gen_value(&prop_schema, Some(req), pool, tc, false, 1);
        obj.insert(req.to_string(), val);
    }
}

fn gen_malformed_object(schema: &Value, tc: &TestCase) -> Value {
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let props = schema.get("properties").and_then(Value::as_object);

    let strategy: u8 = tc.draw_silent(gs::integers::<u8>().min_value(0).max_value(5));
    match strategy {
        // Empty object (often violates required fields)
        0 if !required.is_empty() => json!({}),
        // Wrong type on declared properties
        1 if props.is_some_and(|p| !p.is_empty()) => {
            let props = props.unwrap();
            let mut obj = Map::new();
            for (k, prop_schema) in props {
                let wrong = match primary_type(prop_schema) {
                    "string" => json!(tc.draw_silent(gs::integers::<i64>())),
                    "integer" | "number" => json!(tc.draw_silent(gs::text().max_size(24))),
                    "boolean" => json!("not-a-bool"),
                    "array" => json!({"not": "an-array"}),
                    "object" => json!(["not", "an", "object"]),
                    _ => Value::Null,
                };
                obj.insert(k.clone(), wrong);
            }
            Value::Object(obj)
        }
        // Unexpected extra keys + null for required fields
        2 => {
            let mut obj = Map::new();
            for k in &required {
                obj.insert(k.clone(), Value::Null);
            }
            let extra = tc.draw_silent(hegel_json::values());
            obj.insert("__blofeld_unexpected_key__".into(), extra);
            Value::Object(obj)
        }
        // Adversarial strings in properties
        3 if props.is_some_and(|p| !p.is_empty()) => {
            let adversarial = [
                "",
                "\0",
                "../../etc/passwd",
                "' OR '1'='1",
                "<script>alert(1)</script>",
                "🔥\u{202E}rtl",
            ];
            let mut obj = Map::new();
            for k in props.unwrap().keys() {
                let idx = draw_index(tc, adversarial.len());
                obj.insert(k.clone(), json!(adversarial[idx]));
            }
            Value::Object(obj)
        }
        // Arbitrary shrunk JSON object from Hegel's serde_json generator
        4 => {
            let val = tc.draw_silent(hegel_json::values());
            match val {
                Value::Object(m) => Value::Object(m),
                other => json!({ "__blofeld__": other }),
            }
        }
        // Oversized string payload
        _ => {
            let len = tc.draw_silent(gs::integers::<usize>().min_value(64).max_value(4096));
            json!({ "__invalid__": "x".repeat(len) })
        }
    }
}

fn gen_value(
    schema: &Value,
    field_name: Option<&str>,
    pool: &ValuePool,
    tc: &TestCase,
    boundary: bool,
    depth: usize,
) -> Value {
    if depth > 4 {
        return Value::Null;
    }

    if let Some(c) = schema.get("const") {
        return c.clone();
    }

    if let Some(variants) = schema.get("enum").and_then(Value::as_array)
        && !variants.is_empty()
    {
        return variants[draw_index(tc, variants.len())].clone();
    }

    if let Some(any_of) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
        && !any_of.is_empty()
    {
        let chosen = &any_of[draw_index(tc, any_of.len())];
        return gen_value(chosen, field_name, pool, tc, boundary, depth + 1);
    }

    let ty = primary_type(schema);

    // Try reusing a harvested value from prior calls (stateful exploration with shrinkable indices!)
    if !boundary {
        if let Some(fname) = field_name
            && tc.draw_silent(gs::weighted_booleans(0.75))
            && let Some(reused) = pool.pick_for_field(fname, ty, schema, tc)
        {
            return reused;
        }
        if ty == "string"
            && schema.get("format").is_none()
            && schema.get("pattern").is_none()
            && !pool.strings.is_empty()
            && tc.draw_silent(gs::weighted_booleans(0.25))
        {
            let s = &pool.strings[draw_index(tc, pool.strings.len())];
            if string_fits_constraints(s, schema) {
                return Value::String(s.clone());
            }
        }
        if ty == "integer"
            && !pool.integers.is_empty()
            && tc.draw_silent(gs::weighted_booleans(0.25))
        {
            let i = pool.integers[draw_index(tc, pool.integers.len())];
            if int_fits_constraints(i, schema) {
                return json!(i);
            }
        }
    }

    match ty {
        "object" => {
            let mut obj = Map::new();
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                // Vary optional-field density per object so minimal calls (required fields only)
                // succeed against strict action-discriminated APIs and harvest live IDs, while
                // sparse/dense calls still exercise filter and parameter combinations.
                let opt_prob = if boundary {
                    0.75
                } else {
                    match tc.draw_silent(gs::integers::<u8>().min_value(0).max_value(2)) {
                        0 => 0.0,
                        1 => 0.2,
                        _ => 0.65,
                    }
                };
                for (k, prop_schema) in props {
                    let is_req = required.contains(&k.as_str());
                    if is_req || (opt_prob > 0.0 && tc.draw_silent(gs::weighted_booleans(opt_prob)))
                    {
                        let v =
                            gen_value(prop_schema, Some(k.as_str()), pool, tc, boundary, depth + 1);
                        obj.insert(k.clone(), v);
                    }
                }
            }
            Value::Object(obj)
        }
        "array" => {
            let min_items = schema
                .get("minItems")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(16) as usize;
            let max_items = schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .unwrap_or(4)
                .min(16) as usize;
            let (lo, hi) = if min_items <= max_items {
                (min_items, max_items)
            } else {
                (min_items, min_items)
            };
            let len = if boundary {
                if tc.draw_silent(gs::booleans()) {
                    lo
                } else {
                    hi
                }
            } else if lo == hi {
                lo
            } else {
                tc.draw_silent(gs::integers::<usize>().min_value(lo).max_value(hi))
            };
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            let mut arr = Vec::with_capacity(len);
            for _ in 0..len {
                arr.push(gen_value(item_schema, None, pool, tc, boundary, depth + 1));
            }
            Value::Array(arr)
        }
        "integer" => {
            let min = schema
                .get("minimum")
                .and_then(Value::as_i64)
                .unwrap_or(if boundary { -100_000 } else { 0 });
            let max = schema
                .get("maximum")
                .and_then(Value::as_i64)
                .unwrap_or(if boundary { 1_000_000 } else { 100 });
            let (lo, hi) = if min <= max { (min, max) } else { (min, min) };
            let val = if boundary {
                let choices = [lo, hi, 0.clamp(lo, hi), (-1).clamp(lo, hi)];
                choices[draw_index(tc, choices.len())]
            } else if lo == hi {
                lo
            } else {
                tc.draw_silent(gs::integers::<i64>().min_value(lo).max_value(hi))
            };
            json!(val)
        }
        "number" => {
            let min = schema
                .get("minimum")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                .unwrap_or(if boundary { -10_000.0 } else { 0.0 });
            let max = schema
                .get("maximum")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                .unwrap_or(if boundary { 10_000.0 } else { 100.0 });
            let (lo, hi) = if min <= max { (min, max) } else { (min, min) };
            let f = if boundary {
                if tc.draw_silent(gs::booleans()) {
                    lo
                } else {
                    hi
                }
            } else if (lo - hi).abs() < f64::EPSILON {
                lo
            } else {
                tc.draw_silent(
                    gs::floats::<f64>()
                        .min_value(lo)
                        .max_value(hi)
                        .allow_nan(false)
                        .allow_infinity(false),
                )
            };
            Number::from_f64(f).map(Value::Number).unwrap_or(json!(0))
        }
        "boolean" => json!(tc.draw_silent(gs::booleans())),
        "null" => Value::Null,
        _ => Value::String(gen_string(schema, tc, boundary)),
    }
}

fn gen_string(schema: &Value, tc: &TestCase, boundary: bool) -> String {
    let min_len = schema
        .get("minLength")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(128) as usize;
    let raw_max = schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .unwrap_or(32)
        .min(128) as usize;
    let max_len = raw_max.max(min_len);

    if let Some(fmt) = schema.get("format").and_then(Value::as_str) {
        let formatted = match fmt {
            "uuid" => Some(tc.draw_silent(gs::uuids())),
            "email" => Some(tc.draw_silent(gs::emails())),
            "uri" | "url" => Some(tc.draw_silent(gs::urls())),
            "hostname" => Some(tc.draw_silent(gs::domains())),
            "ipv4" => Some(tc.draw_silent(gs::ip_addresses().v4()).to_string()),
            "ipv6" => Some(tc.draw_silent(gs::ip_addresses().v6()).to_string()),
            "date" => Some(tc.draw_silent(gs::date_strings())),
            "time" => Some(tc.draw_silent(gs::time_strings())),
            "date-time" => Some(tc.draw_silent(gs::datetime_strings())),
            _ => None,
        };
        if let Some(s) = formatted
            && string_fits_constraints(&s, schema)
        {
            return s;
        }
    }

    if !boundary
        && let Some(pat) = schema.get("pattern").and_then(Value::as_str)
        && is_supported_regex(pat)
    {
        let generated = tc.draw_silent(gs::from_regex(pat).fullmatch(true));
        if string_fits_constraints(&generated, schema) {
            return generated;
        }
    }

    if boundary {
        let candidates = [
            "".to_string(),
            " ".to_string(),
            "a".repeat(min_len.max(1)),
            "x".repeat(max_len),
            "héllo-🚀-日本語".to_string(),
            "line1\nline2\t\"quoted\"\\slash".to_string(),
        ];
        let valid: Vec<&String> = candidates
            .iter()
            .filter(|s| {
                let c = s.chars().count();
                c >= min_len && c <= max_len
            })
            .collect();
        if !valid.is_empty() {
            return valid[draw_index(tc, valid.len())].clone();
        }
        return "a".repeat(min_len);
    }

    // Standard Hegel text generator: produces clean ASCII/Unicode strings and shrinks naturally
    // toward short, simple strings (e.g. "", "a", "0").
    tc.draw_silent(
        gs::text()
            .min_size(min_len)
            .max_size(max_len)
            .min_codepoint(0x20)
            .max_codepoint(0x7E),
    )
}

fn is_supported_regex(pat: &str) -> bool {
    !pat.is_empty()
        && pat.len() <= 128
        && !pat.contains("(?=")
        && !pat.contains("(?!")
        && !pat.contains("(?<=")
        && !pat.contains("(?<!")
        && !pat.contains("\\1")
        && !pat.contains("\\2")
}

fn primary_type(schema: &Value) -> &str {
    match schema.get("type") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("null"),
        _ => {
            if schema.get("properties").is_some() {
                "object"
            } else if schema.get("items").is_some() {
                "array"
            } else {
                "string"
            }
        }
    }
}

fn string_fits_constraints(s: &str, schema: &Value) -> bool {
    let len = s.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
        && len < min
    {
        return false;
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
        && len > max
    {
        return false;
    }
    true
}

fn int_fits_constraints(i: i64, schema: &Value) -> bool {
    if let Some(min) = schema.get("minimum").and_then(Value::as_i64)
        && i < min
    {
        return false;
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_i64)
        && i > max
    {
        return false;
    }
    true
}

/// Lightweight recursive JSON Schema check used to verify `outputSchema` conformance
/// and generator soundness.
pub fn validate_against_schema(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(obj) = schema.as_object() else {
        return errors;
    };

    if let Some(c) = obj.get("const")
        && value != c
    {
        errors.push(format!("{path}: expected const {c}, got {value}"));
        return errors;
    }

    if let Some(ty_val) = obj.get("type") {
        let allowed: Vec<&str> = match ty_val {
            Value::String(s) => vec![s.as_str()],
            Value::Array(arr) => arr.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !allowed.is_empty() && !allowed.iter().any(|t| value_matches_type(value, t)) {
            errors.push(format!(
                "{path}: expected type {:?}, got {}",
                allowed,
                json_type_name(value)
            ));
            return errors;
        }
    }

    if let Some(variants) = obj.get("enum").and_then(Value::as_array)
        && !variants.contains(value)
    {
        errors.push(format!("{path}: value {value} not in enum"));
    }

    if let Some(s) = value.as_str() {
        let len = s.chars().count() as u64;
        if let Some(min) = obj.get("minLength").and_then(Value::as_u64)
            && len < min
        {
            errors.push(format!("{path}: string len {len} < minLength {min}"));
        }
        if let Some(max) = obj.get("maxLength").and_then(Value::as_u64)
            && len > max
        {
            errors.push(format!("{path}: string len {len} > maxLength {max}"));
        }
    }

    if let Some(n) = value.as_f64() {
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            errors.push(format!("{path}: number {n} < minimum {min}"));
        }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            errors.push(format!("{path}: number {n} > maximum {max}"));
        }
    }

    if let Some(req) = obj.get("required").and_then(Value::as_array)
        && let Some(map) = value.as_object()
    {
        for r in req.iter().filter_map(Value::as_str) {
            if !map.contains_key(r) {
                errors.push(format!("{path}: missing required field '{r}'"));
            }
        }
    }

    if let (Some(props), Some(map)) = (
        obj.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (k, sub_schema) in props {
            if let Some(sub_val) = map.get(k) {
                let sub_path = format!("{path}.{k}");
                errors.extend(validate_against_schema(sub_val, sub_schema, &sub_path));
            }
        }
    }

    if let Some(arr) = value.as_array() {
        let len = arr.len() as u64;
        if let Some(min) = obj.get("minItems").and_then(Value::as_u64)
            && len < min
        {
            errors.push(format!("{path}: array len {len} < minItems {min}"));
        }
        if let Some(max) = obj.get("maxItems").and_then(Value::as_u64)
            && len > max
        {
            errors.push(format!("{path}: array len {len} > maxItems {max}"));
        }
        if let Some(item_schema) = obj.get("items") {
            for (idx, item) in arr.iter().enumerate() {
                let sub_path = format!("{path}[{idx}]");
                errors.extend(validate_against_schema(item, item_schema, &sub_path));
            }
        }
    }

    errors
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Extract `(path, scalar_value)` bindings from an MCP response so trace shrinking
/// can remap session-specific IDs (UUIDs, auto-increment IDs) across fresh sessions.
pub fn extract_response_bindings(outcome: &RpcOutcome) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let Some(result) = outcome.result() else {
        return out;
    };
    if let Some(sc) = result.get("structuredContent") {
        collect_bindings("sc", sc, 0, &mut out);
    }
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for (idx, item) in content.iter().enumerate() {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                let trimmed = text.trim();
                if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
                    || (trimmed.starts_with('[') && trimmed.ends_with(']')))
                    && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
                {
                    collect_bindings(&format!("text{idx}"), &parsed, 0, &mut out);
                }
            }
        }
    }
    out
}

fn collect_bindings(prefix: &str, val: &Value, depth: usize, out: &mut Vec<(String, Value)>) {
    if depth > 5 || out.len() >= 64 {
        return;
    }
    match val {
        Value::String(s) if !s.is_empty() && s.len() <= 256 => {
            out.push((prefix.to_string(), Value::String(s.clone())));
        }
        Value::Number(n) => {
            out.push((prefix.to_string(), Value::Number(n.clone())));
        }
        Value::Object(map) => {
            for (k, v) in map {
                collect_bindings(&format!("{prefix}.{k}"), v, depth + 1, out);
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().take(8).enumerate() {
                collect_bindings(&format!("{prefix}[{i}]"), v, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// Recursively replace recorded scalar values with their fresh session equivalents.
pub fn substitute_values(val: &Value, subs: &[(Value, Value)]) -> Value {
    if subs.is_empty() {
        return val.clone();
    }
    for (orig, live) in subs.iter().rev() {
        if val == orig {
            return live.clone();
        }
    }
    match val {
        Value::Object(map) => {
            let mut next = Map::with_capacity(map.len());
            for (k, v) in map {
                next.insert(k.clone(), substitute_values(v, subs));
            }
            Value::Object(next)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| substitute_values(v, subs)).collect()),
        _ => val.clone(),
    }
}
