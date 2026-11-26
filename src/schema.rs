use crate::mcp::{InputKind, RpcOutcome};
use rand::Rng;
use serde_json::{Map, Number, Value, json};
use std::collections::HashMap;

const MAX_POOL_PER_BUCKET: usize = 32;

/// Harvests concrete values (IDs, URIs, strings, numbers) from server responses
/// so later tool calls can chain onto state created by earlier calls.
#[derive(Debug, Default, Clone)]
pub struct ValuePool {
    by_field: HashMap<String, Vec<Value>>,
    strings: Vec<String>,
    integers: Vec<i64>,
    numbers: Vec<f64>,
}

impl ValuePool {
    pub fn seed_resource_uris(&mut self, uris: &[String]) {
        for uri in uris {
            self.push_field("uri", Value::String(uri.clone()));
            self.push_string(uri.clone());
        }
    }

    pub fn harvest_outcome(&mut self, outcome: &RpcOutcome) {
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
                    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
                    {
                        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                            self.harvest_value(None, &parsed, 0);
                        }
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

    fn pick_for_field<R: Rng>(
        &self,
        field: &str,
        expected_type: &str,
        rng: &mut R,
    ) -> Option<Value> {
        let norm = normalize_key(field);
        // Exact normalized match first, then suffix match (e.g., item_id -> id)
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
                .filter(|v| value_matches_type(v, expected_type))
                .collect();
            if !matching.is_empty() {
                return Some(matching[rng.random_range(0..matching.len())].clone());
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

/// Generate arguments for an MCP tool's `inputSchema`.
pub fn generate_tool_args<R: Rng>(
    schema: &Value,
    pool: &ValuePool,
    rng: &mut R,
    kind: InputKind,
) -> Value {
    match kind {
        InputKind::Valid => gen_value(schema, None, pool, rng, false, 0),
        InputKind::Boundary => gen_value(schema, None, pool, rng, true, 0),
        InputKind::Malformed => gen_malformed_object(schema, rng),
    }
}

fn gen_malformed_object<R: Rng>(schema: &Value, rng: &mut R) -> Value {
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

    match rng.random_range(0..5) {
        // Empty object (often violates required fields)
        0 if !required.is_empty() => json!({}),
        // Wrong type on a declared property
        1 if props.is_some_and(|p| !p.is_empty()) => {
            let props = props.unwrap();
            let mut obj = Map::new();
            for (k, prop_schema) in props {
                let wrong = match primary_type(prop_schema) {
                    "string" => json!(12345),
                    "integer" | "number" => json!("not-a-number"),
                    "boolean" => json!("definitely-not-a-bool"),
                    "array" => json!({"not": "an-array"}),
                    "object" => json!(["not", "an", "object"]),
                    _ => Value::Null,
                };
                obj.insert(k.clone(), wrong);
            }
            Value::Object(obj)
        }
        // Unexpected extra fields + null for required fields
        2 => {
            let mut obj = Map::new();
            for k in &required {
                obj.insert(k.clone(), Value::Null);
            }
            obj.insert(
                "__blofeld_unexpected_key__".into(),
                json!({"nested": [null, true]}),
            );
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
                let s = adversarial[rng.random_range(0..adversarial.len())];
                obj.insert(k.clone(), json!(s));
            }
            Value::Object(obj)
        }
        // Oversized or deeply nested unexpected structure
        _ => json!({
            "__invalid__": "x".repeat(4096),
            "nested": [[[[null]]]]
        }),
    }
}

fn gen_value<R: Rng>(
    schema: &Value,
    field_name: Option<&str>,
    pool: &ValuePool,
    rng: &mut R,
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
        return variants[rng.random_range(0..variants.len())].clone();
    }

    if let Some(any_of) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
        && !any_of.is_empty()
    {
        let chosen = &any_of[rng.random_range(0..any_of.len())];
        return gen_value(chosen, field_name, pool, rng, boundary, depth + 1);
    }

    let ty = primary_type(schema);

    // Try reusing a harvested value from prior calls (stateful exploration!)
    if !boundary {
        if let Some(fname) = field_name
            && rng.random_bool(0.55)
            && let Some(reused) = pool.pick_for_field(fname, ty, rng)
        {
            return reused;
        }
        if ty == "string" && !pool.strings.is_empty() && rng.random_bool(0.25) {
            let s = &pool.strings[rng.random_range(0..pool.strings.len())];
            if string_fits_constraints(s, schema) {
                return Value::String(s.clone());
            }
        }
        if ty == "integer" && !pool.integers.is_empty() && rng.random_bool(0.25) {
            let i = pool.integers[rng.random_range(0..pool.integers.len())];
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
                for (k, prop_schema) in props {
                    let is_req = required.contains(&k.as_str());
                    if is_req || rng.random_bool(0.65) {
                        let v = gen_value(
                            prop_schema,
                            Some(k.as_str()),
                            pool,
                            rng,
                            boundary,
                            depth + 1,
                        );
                        obj.insert(k.clone(), v);
                    }
                }
            }
            Value::Object(obj)
        }
        "array" => {
            let min_items = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            let max_items = schema
                .get("maxItems")
                .and_then(Value::as_u64)
                .unwrap_or(4)
                .min(8) as usize;
            let len = if boundary {
                if rng.random_bool(0.5) {
                    min_items
                } else {
                    max_items.max(min_items)
                }
            } else {
                rng.random_range(min_items..=max_items.max(min_items))
            };
            let item_schema = schema.get("items").unwrap_or(&Value::Null);
            let mut arr = Vec::with_capacity(len);
            for _ in 0..len {
                arr.push(gen_value(item_schema, None, pool, rng, boundary, depth + 1));
            }
            Value::Array(arr)
        }
        "integer" => {
            let min = schema
                .get("minimum")
                .and_then(Value::as_i64)
                .unwrap_or(if boundary { -1000 } else { 0 });
            let max = schema
                .get("maximum")
                .and_then(Value::as_i64)
                .unwrap_or(if boundary { 1_000_000 } else { 100 });
            let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
            let val = if boundary {
                let choices = [lo, hi, 0.clamp(lo, hi), (-1).clamp(lo, hi)];
                choices[rng.random_range(0..choices.len())]
            } else {
                rng.random_range(lo..=hi)
            };
            json!(val)
        }
        "number" => {
            let min = schema
                .get("minimum")
                .and_then(Value::as_f64)
                .unwrap_or(if boundary { -1000.0 } else { 0.0 });
            let max = schema
                .get("maximum")
                .and_then(Value::as_f64)
                .unwrap_or(if boundary { 10000.0 } else { 100.0 });
            let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
            let f = if boundary {
                if rng.random_bool(0.5) { lo } else { hi }
            } else {
                let raw = rng.random_range(lo..=hi);
                (raw * 100.0).round() / 100.0
            };
            Number::from_f64(f).map(Value::Number).unwrap_or(json!(0))
        }
        "boolean" => json!(rng.random_bool(0.5)),
        "null" => Value::Null,
        _ => Value::String(gen_string(schema, field_name, rng, boundary)),
    }
}

fn gen_string<R: Rng>(
    schema: &Value,
    field_name: Option<&str>,
    rng: &mut R,
    boundary: bool,
) -> String {
    let min_len = schema.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
    let max_len = schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .unwrap_or(64)
        .min(256) as usize;

    if let Some(fmt) = schema.get("format").and_then(Value::as_str) {
        match fmt {
            "uuid" => {
                return format!(
                    "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
                    rng.random::<u32>(),
                    rng.random::<u16>(),
                    rng.random::<u16>() & 0x0fff,
                    rng.random::<u16>() & 0x0fff,
                    rng.random::<u64>() & 0x0000_ffff_ffff_ffff
                );
            }
            "email" => return format!("user{}@example.com", rng.random_range(1..1000)),
            "uri" | "url" => {
                return format!("https://example.com/item/{}", rng.random_range(1..100));
            }
            "date" => {
                return format!(
                    "2026-{:02}-{:02}",
                    rng.random_range(1..=12),
                    rng.random_range(1..=28)
                );
            }
            "date-time" => return "2026-01-15T12:00:00Z".to_string(),
            _ => {}
        }
    }

    if boundary {
        let candidates = [
            "".to_string(),
            " ".to_string(),
            "a".repeat(min_len.max(1)),
            "x".repeat(max_len.max(min_len)),
            "héllo-🚀-日本語".to_string(),
            "line1\nline2\t\"quoted\"\\slash".to_string(),
        ];
        for _ in 0..4 {
            let s = &candidates[rng.random_range(0..candidates.len())];
            if s.chars().count() >= min_len && s.chars().count() <= max_len {
                return s.clone();
            }
        }
        return "a".repeat(min_len.max(1).min(max_len.max(min_len)));
    }

    let sample = match field_name.map(normalize_key).as_deref() {
        Some("email") => format!("test{}@example.com", rng.random_range(1..500)),
        Some("uri") | Some("url") => format!("https://example.com/{}", rng.random_range(1..50)),
        Some(k) if k.ends_with("id") => format!("id-{}", rng.random_range(1..20)),
        _ => {
            let words = [
                "alpha", "beta", "gamma", "delta", "invoice", "ledger", "test", "item",
            ];
            format!(
                "{}-{}",
                words[rng.random_range(0..words.len())],
                rng.random_range(1..100)
            )
        }
    };

    let mut out = sample;
    while out.len() < min_len {
        out.push('a');
    }
    if out.len() > max_len && max_len >= min_len {
        out.truncate(max_len);
    }
    out
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

/// Lightweight recursive JSON Schema check used to verify `outputSchema` conformance.
pub fn validate_against_schema(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(obj) = schema.as_object() else {
        return errors;
    };

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

    if let (Some(item_schema), Some(arr)) = (obj.get("items"), value.as_array()) {
        for (idx, item) in arr.iter().enumerate() {
            let sub_path = format!("{path}[{idx}]");
            errors.extend(validate_against_schema(item, item_schema, &sub_path));
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
