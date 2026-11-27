// Copyright 2026 Economico, LLC
// SPDX-License-Identifier: MIT

use crate::mcp::{Action, Discovery, InputKind, RpcOutcome, ToolDef};
use crate::properties::Violation;
use crate::schema::{ValuePool, fill_missing_required_args, generate_tool_args};
use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use cel::objects::{Key as CelKey, Map as CelMap};
use cel::{Context as CelContext, Env, ExecutionError, Program, ResolveResult, Value as CelValue};
use hegel::TestCase;
use hegel::generators as gs;
use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

thread_local! {
    static ACTIVE_TC: RefCell<Option<TestCase>> = const { RefCell::new(None) };
    static LAST_PICK_EMPTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static RELAX_PICK_WHERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn mark_empty_pick() {
    LAST_PICK_EMPTY.with(|c| c.set(true));
}

fn clear_empty_pick() {
    LAST_PICK_EMPTY.with(|c| c.set(false));
}

fn take_empty_pick() -> bool {
    LAST_PICK_EMPTY.with(|c| c.replace(false))
}

fn is_relax_pick_where() -> bool {
    RELAX_PICK_WHERE.with(|c| c.get())
}

struct RelaxPickGuard {
    prev: bool,
}

impl RelaxPickGuard {
    fn enter() -> Self {
        let prev = RELAX_PICK_WHERE.with(|c| c.replace(true));
        Self { prev }
    }
}

impl Drop for RelaxPickGuard {
    fn drop(&mut self) {
        RELAX_PICK_WHERE.with(|c| c.set(self.prev));
    }
}

struct TcGuard {
    prev: Option<TestCase>,
}

impl TcGuard {
    fn enter(tc: &TestCase) -> Self {
        let prev = ACTIVE_TC.with(|cell| cell.borrow_mut().replace(tc.clone()));
        Self { prev }
    }
}

impl Drop for TcGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        ACTIVE_TC.with(|cell| {
            *cell.borrow_mut() = prev;
        });
    }
}

fn with_active_tc<T>(
    fn_name: &str,
    f: impl FnOnce(&TestCase) -> Result<T, ExecutionError>,
) -> Result<T, ExecutionError> {
    ACTIVE_TC.with(|cell| {
        let borrow = cell.borrow();
        let tc = borrow.as_ref().ok_or_else(|| {
            ExecutionError::function_error(
                fn_name,
                "generator functions can only be called inside `actions.args`",
            )
        })?;
        f(tc)
    })
}

/// Shared CEL environment with standard library + CEL extensions.
fn cel_env() -> Arc<Env> {
    static ENV: OnceLock<Arc<Env>> = OnceLock::new();
    Arc::clone(ENV.get_or_init(|| {
        let mut env = Env::stdlib();
        env.add_extension(cel::extensions::strings)
            .expect("register cel strings extension");
        env.add_extension(cel::extensions::lists)
            .expect("register cel lists extension");
        env.add_extension(cel::extensions::math)
            .expect("register cel math extension");
        env.add_extension(cel::extensions::sets)
            .expect("register cel sets extension");
        env.add_extension(cel::extensions::bindings)
            .expect("register cel bindings extension");
        env.add_extension(cel::extensions::two_var_comprehensions)
            .expect("register cel two_var_comprehensions extension");
        Arc::new(env)
    }))
}

fn compile_cel(source: &str) -> Result<CompiledExpr> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("CEL expression cannot be empty");
    }
    let program = cel_env()
        .compile(trimmed)
        .map_err(|e| anyhow!("invalid CEL expression `{trimmed}`: {e}"))?;
    Ok(CompiledExpr {
        source: trimmed.to_string(),
        program: Arc::new(program),
    })
}

/// Ordered key-value list that deserializes from / serializes to a YAML/JSON mapping
/// while preserving declaration order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderedMap(pub Vec<(String, Value)>);

impl OrderedMap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, Value)> {
        self.0.iter()
    }
}

impl Serialize for OrderedMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for OrderedMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct OrderedMapVisitor;

        impl<'de> Visitor<'de> for OrderedMapVisitor {
            type Value = OrderedMap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a YAML/JSON mapping")
            }

            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(OrderedMap::default())
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    entries.push((k, v));
                }
                Ok(OrderedMap(entries))
            }
        }

        deserializer.deserialize_any(OrderedMapVisitor)
    }
}

/// Accepts either a single string or a list of strings in YAML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl Default for StringOrVec {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl StringOrVec {
    pub fn as_vec(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::One(s) => s.is_empty(),
            Self::Many(v) => v.is_empty(),
        }
    }
}

impl From<&str> for StringOrVec {
    fn from(value: &str) -> Self {
        Self::One(value.to_string())
    }
}

impl From<String> for StringOrVec {
    fn from(value: String) -> Self {
        Self::One(value)
    }
}

impl From<Vec<String>> for StringOrVec {
    fn from(value: Vec<String>) -> Self {
        Self::Many(value)
    }
}

/// Tool call descriptor used by `init:` and `probes:`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSpec {
    pub tool: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// Initial probe call run at the start of every episode to seed `state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitStep {
    #[serde(default)]
    pub name: Option<String>,
    pub call: ToolCallSpec,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub assert: Option<StringOrVec>,
    #[serde(default)]
    pub update: OrderedMap,
    #[serde(default)]
    pub delete: Vec<String>,
}

/// State-machine transition matching tool calls and mutating shadow `state`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransitionSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool: Option<StringOrVec>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub assert: Option<StringOrVec>,
    #[serde(default)]
    pub update: OrderedMap,
    #[serde(default)]
    pub delete: Vec<String>,
}

fn default_probe_after() -> StringOrVec {
    StringOrVec::One("mutating".to_string())
}

/// Active read-tool probe executed after steps to cross-check live server state against shadow `state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSpec {
    pub name: String,
    #[serde(default = "default_probe_after")]
    pub after: StringOrVec,
    #[serde(default)]
    pub when: Option<String>,
    pub call: ToolCallSpec,
    #[serde(default)]
    pub assert: Option<StringOrVec>,
    #[serde(default)]
    pub update: OrderedMap,
    #[serde(default)]
    pub delete: Vec<String>,
}

fn default_action_weight() -> usize {
    1
}

/// State-guided action generator entry in `actions:`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSpec {
    #[serde(default)]
    pub name: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default = "default_action_weight")]
    pub weight: usize,
    #[serde(default)]
    pub args: Option<Value>,
}

/// Compiled CEL expression with its original source text for error diagnostics.
#[derive(Debug, Clone)]
pub struct CompiledExpr {
    pub source: String,
    program: Arc<Program>,
}

impl CompiledExpr {
    pub fn eval(&self, ctx: &CelContext<'_, '_>) -> ResolveResult {
        self.program.execute(ctx)
    }
}

/// Compiled JSON/YAML template where string leaves are CEL expressions and
/// non-string scalars are literal JSON values.
#[derive(Debug, Clone)]
pub enum CompiledValue {
    Literal(Value),
    Expr(CompiledExpr),
    Array(Vec<CompiledValue>),
    Object(Vec<(String, CompiledValue)>),
}

impl CompiledValue {
    pub fn compile(val: &Value) -> Result<Self> {
        match val {
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(Self::Literal(val.clone())),
            Value::String(s) => Ok(Self::Expr(compile_cel(s)?)),
            Value::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    out.push(Self::compile(item)?);
                }
                Ok(Self::Array(out))
            }
            Value::Object(map) => {
                let mut out = Vec::with_capacity(map.len());
                for (k, v) in map {
                    out.push((k.clone(), Self::compile(v)?));
                }
                Ok(Self::Object(out))
            }
        }
    }

    pub fn eval(&self, ctx: &CelContext<'_, '_>) -> Result<Value> {
        match self {
            Self::Literal(v) => Ok(v.clone()),
            Self::Expr(expr) => {
                let cel_val = expr
                    .eval(ctx)
                    .map_err(|e| anyhow!("CEL error in `{}`: {e}", expr.source))?;
                cel_to_json(&cel_val).with_context(|| {
                    format!("failed converting result of `{}` to JSON", expr.source)
                })
            }
            Self::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(item.eval(ctx)?);
                }
                Ok(Value::Array(out))
            }
            Self::Object(entries) => {
                let mut obj = Map::with_capacity(entries.len());
                for (k, v) in entries {
                    obj.insert(k.clone(), v.eval(ctx)?);
                }
                Ok(Value::Object(obj))
            }
        }
    }
}

/// Parsed target path for `update:` and `delete:` (e.g. `items[res.id].status`).
#[derive(Debug, Clone)]
pub struct StatePath {
    pub raw: String,
    segments: Vec<PathSegment>,
}

#[derive(Debug, Clone)]
enum PathSegment {
    Field(String),
    Index(CompiledExpr),
}

#[derive(Debug, Clone, PartialEq)]
enum ResolvedSegment {
    Field(String),
    Index(Value),
}

impl StatePath {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let body = if let Some(rest) = trimmed.strip_prefix("state.") {
            rest
        } else if trimmed.starts_with("state[") {
            &trimmed["state".len()..]
        } else {
            trimmed
        };
        if body.is_empty() {
            bail!("state path `{raw}` cannot be empty");
        }

        let bytes = body.as_bytes();
        let mut i = 0usize;
        let mut segments = Vec::new();

        while i < bytes.len() {
            match bytes[i] {
                b'.' => {
                    if segments.is_empty() {
                        bail!("invalid state path `{raw}`: unexpected leading `.`");
                    }
                    i += 1;
                    if i >= bytes.len() || bytes[i] == b'.' || bytes[i] == b'[' || bytes[i] == b']'
                    {
                        bail!("invalid state path `{raw}`: expected field name after `.`");
                    }
                }
                b'[' => {
                    let start = i + 1;
                    i += 1;
                    let mut depth = 1usize;
                    let mut quote: Option<u8> = None;
                    let mut escaped = false;
                    while i < bytes.len() && depth > 0 {
                        let b = bytes[i];
                        if let Some(q) = quote {
                            if escaped {
                                escaped = false;
                            } else if b == b'\\' {
                                escaped = true;
                            } else if b == q {
                                quote = None;
                            }
                        } else {
                            match b {
                                b'\'' | b'"' => quote = Some(b),
                                b'[' => depth += 1,
                                b']' => depth -= 1,
                                _ => {}
                            }
                        }
                        i += 1;
                    }
                    if depth != 0 {
                        bail!("invalid state path `{raw}`: unclosed `[`");
                    }
                    let inner = &body[start..i - 1];
                    let compiled = compile_cel(inner).with_context(|| {
                        format!("invalid index expression in state path `{raw}`")
                    })?;
                    segments.push(PathSegment::Index(compiled));
                    if i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        bail!("invalid state path `{raw}`: expected `.` or `[` after `]`");
                    }
                }
                b']' => {
                    bail!("invalid state path `{raw}`: unexpected `]`");
                }
                _ => {
                    let start = i;
                    while i < bytes.len()
                        && bytes[i] != b'.'
                        && bytes[i] != b'['
                        && bytes[i] != b']'
                    {
                        i += 1;
                    }
                    let ident = body[start..i].trim();
                    if ident.is_empty() || ident.chars().any(char::is_whitespace) {
                        bail!("invalid state path `{raw}`: invalid field segment `{ident}`");
                    }
                    segments.push(PathSegment::Field(ident.to_string()));
                }
            }
        }

        if segments.is_empty() {
            bail!("state path `{raw}` must have at least one segment");
        }

        Ok(Self {
            raw: raw.to_string(),
            segments,
        })
    }

    fn resolve(&self, ctx: &CelContext<'_, '_>) -> Result<Vec<ResolvedSegment>> {
        let mut out = Vec::with_capacity(self.segments.len());
        for seg in &self.segments {
            match seg {
                PathSegment::Field(name) => out.push(ResolvedSegment::Field(name.clone())),
                PathSegment::Index(expr) => {
                    let val = expr.eval(ctx).map_err(|e| {
                        anyhow!(
                            "CEL error evaluating index `{}` in path `{}`: {e}",
                            expr.source,
                            self.raw
                        )
                    })?;
                    let json_val = cel_to_json(&val)?;
                    out.push(ResolvedSegment::Index(json_val));
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct CompiledUpdate {
    pub path: StatePath,
    pub value: CompiledValue,
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub name: String,
    pub tool: Option<String>,
    pub never_contains: Option<String>,
    pub max_latency_ms: Option<u64>,
    pub valid_calls_succeed: bool,
    pub when: Option<CompiledExpr>,
    pub asserts: Vec<CompiledExpr>,
}

#[derive(Debug, Clone)]
pub struct CompiledInit {
    pub name: String,
    pub tool: String,
    pub arguments: Option<CompiledValue>,
    pub when: Option<CompiledExpr>,
    pub asserts: Vec<CompiledExpr>,
    pub updates: Vec<CompiledUpdate>,
    pub deletes: Vec<StatePath>,
}

#[derive(Debug, Clone)]
pub struct CompiledTransition {
    pub name: String,
    pub tools: Option<Vec<String>>,
    pub when: Option<CompiledExpr>,
    pub asserts: Vec<CompiledExpr>,
    pub updates: Vec<CompiledUpdate>,
    pub deletes: Vec<StatePath>,
}

#[derive(Debug, Clone)]
pub enum CompiledProbeAfter {
    Mutating,
    EveryStep,
    Never,
    Tools(Vec<String>),
}

impl CompiledProbeAfter {
    fn matches(&self, action: &Action, discovery: &Discovery) -> bool {
        match self {
            Self::Never => false,
            Self::EveryStep => true,
            Self::Mutating => match action {
                Action::CallTool { name, .. } => {
                    let is_read_only = discovery
                        .tools
                        .iter()
                        .find(|t| t.name == *name)
                        .and_then(|t| t.annotations.as_ref())
                        .and_then(|a| a.read_only_hint)
                        .unwrap_or(false);
                    !is_read_only
                }
                _ => false,
            },
            Self::Tools(list) => match action {
                Action::CallTool { name, .. } => list.iter().any(|t| t == name),
                _ => false,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompiledProbe {
    pub name: String,
    pub after: CompiledProbeAfter,
    pub when: Option<CompiledExpr>,
    pub tool: String,
    pub arguments: Option<CompiledValue>,
    pub asserts: Vec<CompiledExpr>,
    pub updates: Vec<CompiledUpdate>,
    pub deletes: Vec<StatePath>,
}

#[derive(Debug, Clone)]
pub struct CompiledAction {
    pub name: String,
    pub tool: String,
    pub when: Option<CompiledExpr>,
    pub weight: usize,
    pub args: Option<CompiledValue>,
}

/// Pre-compiled representation of all CEL expressions, state paths, probes, and actions
/// from a `RuleSet`. Created once before exploration/replay and cheaply cloned across runs.
#[derive(Debug, Clone, Default)]
pub struct CompiledSpec {
    pub initial_state: Map<String, Value>,
    pub actions_only: bool,
    pub adversarial_rate: f64,
    pub rules: Vec<CompiledRule>,
    pub init: Vec<CompiledInit>,
    pub transitions: Vec<CompiledTransition>,
    pub probes: Vec<CompiledProbe>,
    pub actions: Vec<CompiledAction>,
}

impl CompiledSpec {
    pub fn compile(rules: &crate::properties::RuleSet) -> Result<Self> {
        let mut compiled_rules = Vec::with_capacity(rules.rules.len());
        for rule in &rules.rules {
            let when = match &rule.when {
                Some(w) => Some(
                    compile_cel(w).with_context(|| format!("in rule '{}' `when`", rule.name))?,
                ),
                None => None,
            };
            let asserts = compile_asserts(&rule.assert, &format!("rule '{}'", rule.name))?;
            compiled_rules.push(CompiledRule {
                name: rule.name.clone(),
                tool: rule.tool.clone(),
                never_contains: rule.never_contains.clone(),
                max_latency_ms: rule.max_latency_ms,
                valid_calls_succeed: rule.valid_calls_succeed,
                when,
                asserts,
            });
        }

        let mut init = Vec::with_capacity(rules.init.len());
        for (idx, step) in rules.init.iter().enumerate() {
            if step.call.tool.trim().is_empty() {
                bail!("init[{idx}]: `call.tool` cannot be empty");
            }
            let name = step
                .name
                .clone()
                .unwrap_or_else(|| format!("{}[{idx}]", step.call.tool));
            let label = format!("init '{name}'");
            let when = compile_opt_expr(&step.when, &format!("{label} `when`"))?;
            let asserts = compile_asserts(&step.assert, &label)?;
            let arguments =
                compile_opt_args_object(&step.call.arguments, &format!("{label} `arguments`"))?;
            let updates = compile_updates(&step.update, &label)?;
            let deletes = compile_deletes(&step.delete, &label)?;
            init.push(CompiledInit {
                name,
                tool: step.call.tool.clone(),
                arguments,
                when,
                asserts,
                updates,
                deletes,
            });
        }

        let mut transitions = Vec::with_capacity(rules.transitions.len());
        for (idx, tr) in rules.transitions.iter().enumerate() {
            let name = tr
                .name
                .clone()
                .unwrap_or_else(|| format!("transition_{idx}"));
            let label = format!("transition '{name}'");
            let tools = match &tr.tool {
                Some(t) => {
                    let list: Vec<String> = t.as_vec().into_iter().map(String::from).collect();
                    if list.is_empty() || list.iter().any(|s| s.trim().is_empty()) {
                        bail!("{label}: `tool` filter cannot be empty");
                    }
                    Some(list)
                }
                None => None,
            };
            let when = compile_opt_expr(&tr.when, &format!("{label} `when`"))?;
            let asserts = compile_asserts(&tr.assert, &label)?;
            let updates = compile_updates(&tr.update, &label)?;
            let deletes = compile_deletes(&tr.delete, &label)?;
            transitions.push(CompiledTransition {
                name,
                tools,
                when,
                asserts,
                updates,
                deletes,
            });
        }

        let mut probes = Vec::with_capacity(rules.probes.len());
        for probe in &rules.probes {
            let label = format!("probe '{}'", probe.name);
            if probe.call.tool.trim().is_empty() {
                bail!("{label}: `call.tool` cannot be empty");
            }
            let after = parse_probe_after(&probe.after, &label)?;
            let when = compile_opt_expr(&probe.when, &format!("{label} `when`"))?;
            let arguments =
                compile_opt_args_object(&probe.call.arguments, &format!("{label} `arguments`"))?;
            let asserts = compile_asserts(&probe.assert, &label)?;
            let updates = compile_updates(&probe.update, &label)?;
            let deletes = compile_deletes(&probe.delete, &label)?;
            probes.push(CompiledProbe {
                name: probe.name.clone(),
                after,
                when,
                tool: probe.call.tool.clone(),
                arguments,
                asserts,
                updates,
                deletes,
            });
        }

        let mut actions = Vec::with_capacity(rules.actions.len());
        for (idx, act) in rules.actions.iter().enumerate() {
            if act.tool.trim().is_empty() {
                bail!("action[{idx}]: `tool` cannot be empty");
            }
            let name = act
                .name
                .clone()
                .unwrap_or_else(|| format!("{}[{idx}]", act.tool));
            let label = format!("action '{name}'");
            if act.weight == 0 {
                bail!("{label}: `weight` must be >= 1");
            }
            let when = compile_opt_expr(&act.when, &format!("{label} `when`"))?;
            let args = compile_opt_args_object(&act.args, &format!("{label} `args`"))?;
            actions.push(CompiledAction {
                name,
                tool: act.tool.clone(),
                when,
                weight: act.weight,
                args,
            });
        }

        let adversarial_rate = rules.adversarial_rate.unwrap_or(0.0);
        if !adversarial_rate.is_finite() || !(0.0..=1.0).contains(&adversarial_rate) {
            bail!("`adversarial_rate` must be between 0.0 and 1.0, got {adversarial_rate}");
        }

        Ok(Self {
            initial_state: rules.state.clone(),
            actions_only: rules.actions_only,
            adversarial_rate,
            rules: compiled_rules,
            init,
            transitions,
            probes,
            actions,
        })
    }

    pub fn has_stateful_features(&self) -> bool {
        !self.initial_state.is_empty()
            || self
                .rules
                .iter()
                .any(|r| r.when.is_some() || !r.asserts.is_empty())
            || !self.init.is_empty()
            || !self.transitions.is_empty()
            || !self.probes.is_empty()
            || !self.actions.is_empty()
    }
}

fn compile_opt_expr(expr: &Option<String>, label: &str) -> Result<Option<CompiledExpr>> {
    match expr {
        Some(s) => Ok(Some(compile_cel(s).with_context(|| format!("in {label}"))?)),
        None => Ok(None),
    }
}

fn compile_opt_args_object(val: &Option<Value>, label: &str) -> Result<Option<CompiledValue>> {
    match val {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(v @ (Value::Object(_) | Value::String(_))) => Ok(Some(
            CompiledValue::compile(v).with_context(|| format!("in {label}"))?,
        )),
        Some(other) => bail!(
            "in {label}: tool arguments must be a mapping or a CEL expression returning an object, got {other}"
        ),
    }
}

fn compile_asserts(assert: &Option<StringOrVec>, label: &str) -> Result<Vec<CompiledExpr>> {
    let mut out = Vec::new();
    if let Some(list) = assert {
        for raw in list.as_vec() {
            out.push(compile_cel(raw).with_context(|| format!("in {label} `assert`"))?);
        }
    }
    Ok(out)
}

fn compile_updates(map: &OrderedMap, label: &str) -> Result<Vec<CompiledUpdate>> {
    let mut out = Vec::with_capacity(map.0.len());
    for (path_raw, val) in &map.0 {
        let path = StatePath::parse(path_raw)
            .with_context(|| format!("in {label} `update` key `{path_raw}`"))?;
        let value = CompiledValue::compile(val)
            .with_context(|| format!("in {label} `update.{path_raw}`"))?;
        out.push(CompiledUpdate { path, value });
    }
    Ok(out)
}

fn compile_deletes(paths: &[String], label: &str) -> Result<Vec<StatePath>> {
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        out.push(
            StatePath::parse(raw).with_context(|| format!("in {label} `delete` entry `{raw}`"))?,
        );
    }
    Ok(out)
}

fn parse_probe_after(after: &StringOrVec, label: &str) -> Result<CompiledProbeAfter> {
    let items = after.as_vec();
    if items.is_empty() || items.iter().any(|s| s.trim().is_empty()) {
        bail!("{label}: `after` cannot be empty");
    }
    if items.len() == 1 {
        match items[0] {
            "mutating" => return Ok(CompiledProbeAfter::Mutating),
            "every_step" => return Ok(CompiledProbeAfter::EveryStep),
            "never" => return Ok(CompiledProbeAfter::Never),
            _ => {}
        }
    }
    Ok(CompiledProbeAfter::Tools(
        items.into_iter().map(String::from).collect(),
    ))
}

/// Normalize an MCP `RpcOutcome` into the `res` value exposed to CEL expressions:
/// 1. Prefer `result.structuredContent` (if present and non-null).
/// 2. Fall back to parsed JSON from `result.content[0].text` (if valid JSON).
/// 3. Else raw `result` (or `null` if no `result`).
pub fn normalize_outcome_res(outcome: &RpcOutcome) -> Value {
    let Some(result) = outcome.result() else {
        return Value::Null;
    };
    if let Some(sc) = result.get("structuredContent")
        && !sc.is_null()
    {
        return sc.clone();
    }
    if let Some(text) = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find_map(|item| item.get("text").and_then(Value::as_str))
        })
    {
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
        {
            return parsed;
        }
    }
    result.clone()
}

/// Returns `true` iff the MCP call completed cleanly without transport error, HTTP >= 400,
/// JSON-RPC `error`, or MCP `isError: true`.
pub fn outcome_is_ok(outcome: &RpcOutcome) -> bool {
    outcome.transport_error.is_none()
        && outcome.http_status.is_none_or(|s| s < 400)
        && outcome.error().is_none()
        && outcome.result().is_some()
        && !outcome.is_tool_error()
}

fn outcome_text(outcome: &RpcOutcome) -> String {
    let Some(arr) = outcome
        .result()
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
    else {
        return String::new();
    };
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect();
    parts.join("\n")
}

/// Convert `serde_json::Value` into `cel::Value`, keeping JSON integers in `cel::Value::Int`
/// whenever they fit in `i64` (avoiding CEL `uint` vs `int` overload mismatches).
pub fn json_to_cel(value: &Value) -> CelValue {
    match value {
        Value::Null => CelValue::Null,
        Value::Bool(b) => CelValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CelValue::Int(i)
            } else if let Some(u) = n.as_u64() {
                CelValue::UInt(u)
            } else if let Some(f) = n.as_f64() {
                CelValue::Float(f)
            } else {
                CelValue::Null
            }
        }
        Value::String(s) => CelValue::String(Arc::new(s.clone())),
        Value::Array(arr) => CelValue::List(Arc::new(arr.iter().map(json_to_cel).collect())),
        Value::Object(obj) => {
            let mut map = HashMap::with_capacity(obj.len());
            for (k, v) in obj {
                map.insert(CelKey::String(Arc::new(k.clone())), json_to_cel(v));
            }
            CelValue::Map(CelMap { map: Arc::new(map) })
        }
    }
}

pub fn cel_to_json(val: &CelValue) -> Result<Value> {
    val.json().map_err(|e| anyhow!("{e}"))
}

fn cel_key_rank(k: &CelKey) -> u8 {
    match k {
        CelKey::String(_) => 0,
        CelKey::Int(_) => 1,
        CelKey::Uint(_) => 2,
        CelKey::Bool(_) => 3,
    }
}

fn cmp_cel_key(a: &CelKey, b: &CelKey) -> std::cmp::Ordering {
    match (a, b) {
        (CelKey::String(x), CelKey::String(y)) => x.cmp(y),
        (CelKey::Int(x), CelKey::Int(y)) => x.cmp(y),
        (CelKey::Uint(x), CelKey::Uint(y)) => x.cmp(y),
        (CelKey::Bool(x), CelKey::Bool(y)) => x.cmp(y),
        _ => cel_key_rank(a).cmp(&cel_key_rank(b)),
    }
}

fn cel_val_to_f64(val: CelValue) -> Result<f64, ExecutionError> {
    match val {
        CelValue::Float(f) => Ok(f),
        CelValue::Int(i) => Ok(i as f64),
        CelValue::UInt(u) => Ok(u as f64),
        other => Err(ExecutionError::UnexpectedType {
            got: format!("{other:?}"),
            want: "float or int".to_string(),
        }),
    }
}

fn cel_key_to_value(k: &CelKey) -> CelValue {
    match k {
        CelKey::String(s) => CelValue::String(Arc::clone(s)),
        CelKey::Int(i) => CelValue::Int(*i),
        CelKey::Uint(u) => CelValue::UInt(*u),
        CelKey::Bool(b) => CelValue::Bool(*b),
    }
}

fn value_to_cel_key(v: CelValue) -> Result<CelKey, ExecutionError> {
    match v {
        CelValue::String(s) => Ok(CelKey::String(s)),
        CelValue::Int(i) => Ok(CelKey::Int(i)),
        CelValue::UInt(u) => Ok(CelKey::Uint(u)),
        CelValue::Bool(b) => Ok(CelKey::Bool(b)),
        other => Err(ExecutionError::unsupported_key_type(other)),
    }
}

fn sorted_map_entries(map: &CelMap) -> Vec<(&CelKey, &CelValue)> {
    let mut entries: Vec<(&CelKey, &CelValue)> = map.map.iter().collect();
    entries.sort_by(|(a, _), (b, _)| cmp_cel_key(a, b));
    entries
}

fn register_pure_helpers(ctx: &mut CelContext<'_, '_>) {
    ctx.add_function("keys", |val: CelValue| -> ResolveResult {
        match val {
            CelValue::Map(map) => {
                let entries = sorted_map_entries(&map);
                let list: Vec<CelValue> = entries
                    .into_iter()
                    .map(|(k, _)| cel_key_to_value(k))
                    .collect();
                Ok(CelValue::List(Arc::new(list)))
            }
            other => Err(ExecutionError::UnexpectedType {
                got: format!("{other:?}"),
                want: "map".to_string(),
            }),
        }
    })
    .expect("register CEL function `keys`");

    ctx.add_function("values", |val: CelValue| -> ResolveResult {
        match val {
            CelValue::Map(map) => {
                let entries = sorted_map_entries(&map);
                let list: Vec<CelValue> = entries.into_iter().map(|(_, v)| v.clone()).collect();
                Ok(CelValue::List(Arc::new(list)))
            }
            other => Err(ExecutionError::UnexpectedType {
                got: format!("{other:?}"),
                want: "map".to_string(),
            }),
        }
    })
    .expect("register CEL function `values`");

    ctx.add_function(
        "get",
        |container: CelValue, key_val: CelValue, default_val: CelValue| -> ResolveResult {
            match container {
                CelValue::Map(map) => {
                    let key = value_to_cel_key(key_val)?;
                    Ok(map.map.get(&key).cloned().unwrap_or(default_val))
                }
                CelValue::List(items) => {
                    let idx = match key_val {
                        CelValue::Int(i) if i >= 0 => i as usize,
                        CelValue::UInt(u) => u as usize,
                        _ => return Ok(default_val),
                    };
                    Ok(items.get(idx).cloned().unwrap_or(default_val))
                }
                CelValue::Null => Ok(default_val),
                other => Err(ExecutionError::UnexpectedType {
                    got: format!("{other:?}"),
                    want: "map or list".to_string(),
                }),
            }
        },
    )
    .expect("register CEL function `get`");

    ctx.add_function(
        "set_entry",
        |map_val: CelValue, key_val: CelValue, new_val: CelValue| -> ResolveResult {
            match map_val {
                CelValue::Map(map) => {
                    let key = value_to_cel_key(key_val)?;
                    let mut next = (*map.map).clone();
                    next.insert(key, new_val);
                    Ok(CelValue::Map(CelMap {
                        map: Arc::new(next),
                    }))
                }
                other => Err(ExecutionError::UnexpectedType {
                    got: format!("{other:?}"),
                    want: "map".to_string(),
                }),
            }
        },
    )
    .expect("register CEL function `set_entry`");

    ctx.add_function(
        "remove_key",
        |map_val: CelValue, key_val: CelValue| -> ResolveResult {
            match map_val {
                CelValue::Map(map) => {
                    let key = value_to_cel_key(key_val)?;
                    let mut next = (*map.map).clone();
                    next.remove(&key);
                    Ok(CelValue::Map(CelMap {
                        map: Arc::new(next),
                    }))
                }
                other => Err(ExecutionError::UnexpectedType {
                    got: format!("{other:?}"),
                    want: "map".to_string(),
                }),
            }
        },
    )
    .expect("register CEL function `remove_key`");

    ctx.add_function("sum", |list_val: CelValue| -> ResolveResult {
        match list_val {
            CelValue::List(items) => {
                let mut int_sum: i64 = 0;
                let mut float_sum: f64 = 0.0;
                let mut is_float = false;
                for item in items.iter() {
                    match item {
                        CelValue::Int(i) => {
                            if is_float {
                                float_sum += *i as f64;
                            } else {
                                int_sum = int_sum.saturating_add(*i);
                            }
                        }
                        CelValue::UInt(u) => {
                            if is_float {
                                float_sum += *u as f64;
                            } else if let Ok(i) = i64::try_from(*u) {
                                int_sum = int_sum.saturating_add(i);
                            } else {
                                is_float = true;
                                float_sum = (int_sum as f64) + (*u as f64);
                            }
                        }
                        CelValue::Float(f) => {
                            if !is_float {
                                is_float = true;
                                float_sum = int_sum as f64;
                            }
                            float_sum += *f;
                        }
                        other => {
                            return Err(ExecutionError::UnexpectedType {
                                got: format!("{other:?}"),
                                want: "int or float".to_string(),
                            });
                        }
                    }
                }
                if is_float {
                    Ok(CelValue::Float(float_sum))
                } else {
                    Ok(CelValue::Int(int_sum))
                }
            }
            other => Err(ExecutionError::UnexpectedType {
                got: format!("{other:?}"),
                want: "list".to_string(),
            }),
        }
    })
    .expect("register CEL function `sum`");
}

fn draw_shrinking_index(tc: &TestCase, len: usize) -> usize {
    if len <= 1 {
        0
    } else {
        tc.draw_silent(gs::integers::<usize>().min_value(0).max_value(len - 1))
    }
}

fn register_generator_helpers(ctx: &mut CelContext<'_, '_>) {
    ctx.add_function("pick_key", |val: CelValue| -> ResolveResult {
        match val {
            CelValue::Map(map) => {
                let entries = sorted_map_entries(&map);
                if entries.is_empty() {
                    if is_relax_pick_where() {
                        return Ok(CelValue::String(Arc::new(
                            "__blofeld_unknown_id__".to_string(),
                        )));
                    }
                    mark_empty_pick();
                    return Err(ExecutionError::function_error(
                        "pick_key",
                        "cannot pick key from empty map",
                    ));
                }
                let idx =
                    with_active_tc("pick_key", |tc| Ok(draw_shrinking_index(tc, entries.len())))?;
                Ok(cel_key_to_value(entries[idx].0))
            }
            other => Err(ExecutionError::UnexpectedType {
                got: format!("{other:?}"),
                want: "map".to_string(),
            }),
        }
    })
    .expect("register CEL function `pick_key`");

    ctx.add_function("pick_value", |val: CelValue| -> ResolveResult {
        match val {
            CelValue::Map(map) => {
                let entries = sorted_map_entries(&map);
                if entries.is_empty() {
                    mark_empty_pick();
                    return Err(ExecutionError::function_error(
                        "pick_value",
                        "cannot pick value from empty map",
                    ));
                }
                let idx = with_active_tc("pick_value", |tc| {
                    Ok(draw_shrinking_index(tc, entries.len()))
                })?;
                Ok(entries[idx].1.clone())
            }
            other => Err(ExecutionError::UnexpectedType {
                got: format!("{other:?}"),
                want: "map".to_string(),
            }),
        }
    })
    .expect("register CEL function `pick_value`");

    ctx.add_function(
        "pick_key_where",
        |val: CelValue, expected: CelValue| -> ResolveResult {
            match val {
                CelValue::Map(map) => {
                    let entries = sorted_map_entries(&map);
                    if is_relax_pick_where() {
                        let mismatched: Vec<&CelKey> = entries
                            .iter()
                            .filter(|(_, v)| **v != expected)
                            .map(|(k, _)| *k)
                            .collect();
                        if !mismatched.is_empty() {
                            let idx = with_active_tc("pick_key_where", |tc| {
                                Ok(draw_shrinking_index(tc, mismatched.len()))
                            })?;
                            return Ok(cel_key_to_value(mismatched[idx]));
                        }
                        return Ok(CelValue::String(Arc::new(
                            "__blofeld_unknown_id__".to_string(),
                        )));
                    }
                    let matching: Vec<&CelKey> = entries
                        .into_iter()
                        .filter(|(_, v)| **v == expected)
                        .map(|(k, _)| k)
                        .collect();
                    if matching.is_empty() {
                        mark_empty_pick();
                        return Err(ExecutionError::function_error(
                            "pick_key_where",
                            "no map entries matched expected value",
                        ));
                    }
                    let idx = with_active_tc("pick_key_where", |tc| {
                        Ok(draw_shrinking_index(tc, matching.len()))
                    })?;
                    Ok(cel_key_to_value(matching[idx]))
                }
                other => Err(ExecutionError::UnexpectedType {
                    got: format!("{other:?}"),
                    want: "map".to_string(),
                }),
            }
        },
    )
    .expect("register CEL function `pick_key_where`");

    let pick_from_list = |fn_name: &'static str| {
        move |val: CelValue| -> ResolveResult {
            match val {
                CelValue::List(items) => {
                    if items.is_empty() {
                        mark_empty_pick();
                        return Err(ExecutionError::function_error(
                            fn_name,
                            "cannot pick element from empty list",
                        ));
                    }
                    let idx =
                        with_active_tc(fn_name, |tc| Ok(draw_shrinking_index(tc, items.len())))?;
                    Ok(items[idx].clone())
                }
                other => Err(ExecutionError::UnexpectedType {
                    got: format!("{other:?}"),
                    want: "list".to_string(),
                }),
            }
        }
    };
    ctx.add_function("pick_item", pick_from_list("pick_item"))
        .expect("register CEL function `pick_item`");
    ctx.add_function("pick", pick_from_list("pick"))
        .expect("register CEL function `pick`");

    ctx.add_function("gen_int", |min: i64, max: i64| -> ResolveResult {
        if min > max {
            return Err(ExecutionError::function_error(
                "gen_int",
                format!("min ({min}) > max ({max})"),
            ));
        }
        if min == max {
            return Ok(CelValue::Int(min));
        }
        let v = with_active_tc("gen_int", |tc| {
            Ok(tc.draw_silent(gs::integers::<i64>().min_value(min).max_value(max)))
        })?;
        Ok(CelValue::Int(v))
    })
    .expect("register CEL function `gen_int`");

    ctx.add_function(
        "gen_float",
        |min_val: CelValue, max_val: CelValue| -> ResolveResult {
            let min = cel_val_to_f64(min_val)?;
            let max = cel_val_to_f64(max_val)?;
            if !min.is_finite() || !max.is_finite() || min > max {
                return Err(ExecutionError::function_error(
                    "gen_float",
                    format!("invalid float range [{min}, {max}]"),
                ));
            }
            if min == max {
                return Ok(CelValue::Float(min));
            }
            let v = with_active_tc("gen_float", |tc| {
                Ok(tc.draw_silent(
                    gs::floats::<f64>()
                        .min_value(min)
                        .max_value(max)
                        .allow_nan(false)
                        .allow_infinity(false),
                ))
            })?;
            Ok(CelValue::Float(v))
        },
    )
    .expect("register CEL function `gen_float`");

    ctx.add_function(
        "gen_string",
        |min_len: i64, max_len: i64| -> ResolveResult {
            if min_len < 0 || max_len < min_len {
                return Err(ExecutionError::function_error(
                    "gen_string",
                    format!("invalid length bounds [{min_len}, {max_len}]"),
                ));
            }
            let lo = (min_len as usize).min(1024);
            let hi = (max_len as usize).min(1024).max(lo);
            let s = with_active_tc("gen_string", |tc| {
                Ok(tc.draw_silent(
                    gs::text()
                        .min_size(lo)
                        .max_size(hi)
                        .min_codepoint(0x20)
                        .max_codepoint(0x7E),
                ))
            })?;
            Ok(CelValue::String(Arc::new(s)))
        },
    )
    .expect("register CEL function `gen_string`");

    ctx.add_function("gen_bool", || -> ResolveResult {
        let b = with_active_tc("gen_bool", |tc| Ok(tc.draw_silent(gs::booleans())))?;
        Ok(CelValue::Bool(b))
    })
    .expect("register CEL function `gen_bool`");
}

fn base_context<'a>(
    state: &Map<String, Value>,
    step: usize,
    include_generators: bool,
) -> CelContext<'a, 'a> {
    let mut ctx = CelContext::with_env(cel_env());
    register_pure_helpers(&mut ctx);
    if include_generators {
        register_generator_helpers(&mut ctx);
    }
    ctx.add_variable_from_value("state", json_to_cel(&Value::Object(state.clone())));
    ctx.add_variable_from_value("step", CelValue::Int(step as i64));
    ctx
}

fn step_context<'a>(
    state: &Map<String, Value>,
    action: &Action,
    outcome: &RpcOutcome,
    step: usize,
) -> CelContext<'a, 'a> {
    let mut ctx = base_context(state, step, false);

    let (tool_name, args_val, input_kind_str) = match action {
        Action::CallTool {
            name,
            arguments,
            input_kind,
        } => {
            let kind = match input_kind {
                InputKind::Valid => "valid",
                InputKind::Boundary => "boundary",
                InputKind::Malformed => "malformed",
            };
            (name.as_str(), arguments.clone(), kind)
        }
        Action::GetPrompt { arguments, .. } => ("", arguments.clone(), ""),
        Action::ReadResource { uri } => ("", json!({ "uri": uri }), ""),
        Action::ListTools | Action::Ping => ("", json!({}), ""),
    };

    let ok = outcome_is_ok(outcome);
    let res = normalize_outcome_res(outcome);
    let raw = outcome.raw_response.clone().unwrap_or(Value::Null);
    let result_val = outcome.result().cloned().unwrap_or(Value::Null);
    let error_val = outcome.error().cloned().unwrap_or(Value::Null);
    let text = outcome_text(outcome);
    let http_status = outcome
        .http_status
        .map(|s| CelValue::Int(s as i64))
        .unwrap_or(CelValue::Null);

    ctx.add_variable_from_value("tool", CelValue::String(Arc::new(tool_name.to_string())));
    ctx.add_variable_from_value(
        "action",
        CelValue::String(Arc::new(action.method().to_string())),
    );
    ctx.add_variable_from_value("args", json_to_cel(&args_val));
    ctx.add_variable_from_value(
        "input_kind",
        CelValue::String(Arc::new(input_kind_str.to_string())),
    );
    ctx.add_variable_from_value("ok", CelValue::Bool(ok));
    ctx.add_variable_from_value("is_error", CelValue::Bool(!ok));
    ctx.add_variable_from_value("res", json_to_cel(&res));
    ctx.add_variable_from_value("raw", json_to_cel(&raw));
    ctx.add_variable_from_value("result", json_to_cel(&result_val));
    ctx.add_variable_from_value("error", json_to_cel(&error_val));
    ctx.add_variable_from_value("text", CelValue::String(Arc::new(text)));
    ctx.add_variable_from_value(
        "latency_ms",
        CelValue::Int(outcome.latency.as_millis() as i64),
    );
    ctx.add_variable_from_value("http_status", http_status);
    ctx
}

fn eval_guard(
    expr: &Option<CompiledExpr>,
    ctx: &CelContext<'_, '_>,
    verbose: bool,
    label: &str,
) -> bool {
    let Some(expr) = expr else {
        return true;
    };
    match expr.eval(ctx) {
        Ok(CelValue::Bool(b)) => b,
        Ok(other) => {
            if verbose {
                eprintln!(
                    "  [spec] {label} `when` ({}) evaluated to non-bool {other:?}, treating as false",
                    expr.source
                );
            }
            false
        }
        Err(err) => {
            if verbose {
                eprintln!(
                    "  [spec] {label} `when` ({}) eval error ({err}), treating as false",
                    expr.source
                );
            }
            false
        }
    }
}

fn eval_asserts(
    asserts: &[CompiledExpr],
    ctx: &CelContext<'_, '_>,
    property: &str,
    snapshot: &str,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    for expr in asserts {
        match expr.eval(ctx) {
            Ok(CelValue::Bool(true)) => {
                antithesis_sdk::assert_always!(
                    true,
                    "mcp_spec_assert",
                    &json!({ "property": property, "expr": expr.source })
                );
            }
            Ok(CelValue::Bool(false)) => {
                antithesis_sdk::assert_always!(
                    false,
                    "mcp_spec_assert",
                    &json!({ "property": property, "expr": expr.source })
                );
                violations.push(Violation {
                    property: property.to_string(),
                    message: format!(
                        "assertion `{}` evaluated to false ({snapshot})",
                        expr.source
                    ),
                });
            }
            Ok(other) => {
                antithesis_sdk::assert_always!(
                    false,
                    "mcp_spec_assert",
                    &json!({ "property": property, "expr": expr.source })
                );
                violations.push(Violation {
                    property: property.to_string(),
                    message: format!(
                        "assertion `{}` evaluated to non-boolean value {other:?} ({snapshot})",
                        expr.source
                    ),
                });
            }
            Err(err) => {
                antithesis_sdk::assert_always!(
                    false,
                    "mcp_spec_assert",
                    &json!({ "property": property, "expr": expr.source })
                );
                violations.push(Violation {
                    property: property.to_string(),
                    message: format!(
                        "assertion `{}` failed to evaluate: {err} ({snapshot})",
                        expr.source
                    ),
                });
            }
        }
    }
    violations
}

fn json_key_string(val: &Value) -> Result<String> {
    match val {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        other => bail!("unsupported map key value `{other}`"),
    }
}

fn json_array_index(val: &Value) -> Result<usize> {
    if let Some(u) = val.as_u64() {
        Ok(u as usize)
    } else if let Some(i) = val.as_i64()
        && i >= 0
    {
        Ok(i as usize)
    } else {
        bail!("expected non-negative integer array index, got `{val}`")
    }
}

fn apply_resolved_delete(state: &mut Map<String, Value>, segs: &[ResolvedSegment]) {
    let Some((first, rest)) = segs.split_first() else {
        return;
    };
    let root_key = match first {
        ResolvedSegment::Field(k) => k.clone(),
        ResolvedSegment::Index(v) => match json_key_string(v) {
            Ok(k) => k,
            Err(_) => return,
        },
    };
    if rest.is_empty() {
        state.remove(&root_key);
        return;
    }
    let Some(mut cur) = state.get_mut(&root_key) else {
        return;
    };
    for seg in &rest[..rest.len() - 1] {
        cur = match (cur, seg) {
            (Value::Object(map), ResolvedSegment::Field(k)) => match map.get_mut(k) {
                Some(v) => v,
                None => return,
            },
            (Value::Object(map), ResolvedSegment::Index(kval)) => {
                let Ok(k) = json_key_string(kval) else {
                    return;
                };
                match map.get_mut(&k) {
                    Some(v) => v,
                    None => return,
                }
            }
            (Value::Array(arr), ResolvedSegment::Index(ival)) => {
                let Ok(idx) = json_array_index(ival) else {
                    return;
                };
                match arr.get_mut(idx) {
                    Some(v) => v,
                    None => return,
                }
            }
            _ => return,
        };
    }
    let last = &rest[rest.len() - 1];
    match (cur, last) {
        (Value::Object(map), ResolvedSegment::Field(k)) => {
            map.remove(k);
        }
        (Value::Object(map), ResolvedSegment::Index(kval)) => {
            if let Ok(k) = json_key_string(kval) {
                map.remove(&k);
            }
        }
        (Value::Array(arr), ResolvedSegment::Index(ival)) => {
            if let Ok(idx) = json_array_index(ival)
                && idx < arr.len()
            {
                arr.remove(idx);
            }
        }
        _ => {}
    }
}

fn apply_resolved_set(
    state: &mut Map<String, Value>,
    segs: &[ResolvedSegment],
    new_val: Value,
) -> Result<()> {
    let Some((first, rest)) = segs.split_first() else {
        bail!("cannot set empty state path");
    };
    let root_key = match first {
        ResolvedSegment::Field(k) => k.clone(),
        ResolvedSegment::Index(v) => json_key_string(v)?,
    };
    if rest.is_empty() {
        state.insert(root_key, new_val);
        return Ok(());
    }

    if !state
        .get(&root_key)
        .is_some_and(|v| v.is_object() || v.is_array())
    {
        state.insert(root_key.clone(), Value::Object(Map::new()));
    }
    let mut cur = state.get_mut(&root_key).unwrap();

    for seg in &rest[..rest.len() - 1] {
        cur = match seg {
            ResolvedSegment::Field(k) => {
                if !cur.is_object() {
                    *cur = Value::Object(Map::new());
                }
                let map = cur.as_object_mut().unwrap();
                if !map.get(k).is_some_and(|v| v.is_object() || v.is_array()) {
                    map.insert(k.clone(), Value::Object(Map::new()));
                }
                map.get_mut(k).unwrap()
            }
            ResolvedSegment::Index(kval) => {
                if cur.is_array() {
                    let arr = cur.as_array_mut().unwrap();
                    let idx = json_array_index(kval)?;
                    if idx >= arr.len() {
                        bail!("array index {idx} out of bounds (len={})", arr.len());
                    }
                    &mut arr[idx]
                } else {
                    if !cur.is_object() {
                        *cur = Value::Object(Map::new());
                    }
                    let k = json_key_string(kval)?;
                    let map = cur.as_object_mut().unwrap();
                    if !map.get(&k).is_some_and(|v| v.is_object() || v.is_array()) {
                        map.insert(k.clone(), Value::Object(Map::new()));
                    }
                    map.get_mut(&k).unwrap()
                }
            }
        };
    }

    let last = &rest[rest.len() - 1];
    match last {
        ResolvedSegment::Field(k) => {
            if !cur.is_object() {
                *cur = Value::Object(Map::new());
            }
            cur.as_object_mut().unwrap().insert(k.clone(), new_val);
        }
        ResolvedSegment::Index(kval) => {
            if cur.is_array() {
                let arr = cur.as_array_mut().unwrap();
                let idx = json_array_index(kval)?;
                if idx >= arr.len() {
                    bail!("array index {idx} out of bounds (len={})", arr.len());
                }
                arr[idx] = new_val;
            } else {
                if !cur.is_object() {
                    *cur = Value::Object(Map::new());
                }
                let k = json_key_string(kval)?;
                cur.as_object_mut().unwrap().insert(k, new_val);
            }
        }
    }
    Ok(())
}

fn trailing_array_index(segs: &[ResolvedSegment]) -> Option<usize> {
    match segs.last()? {
        ResolvedSegment::Index(v) => json_array_index(v).ok(),
        ResolvedSegment::Field(_) => None,
    }
}

fn apply_mutations(
    state: &mut Map<String, Value>,
    deletes: &[StatePath],
    updates: &[CompiledUpdate],
    ctx: &CelContext<'_, '_>,
) -> Result<()> {
    // Evaluate all dynamic path indices and RHS expressions against the pre-mutation context
    // first so `delete` and `update` see a consistent snapshot regardless of YAML key ordering.
    let mut resolved_deletes: Vec<Vec<ResolvedSegment>> = Vec::with_capacity(deletes.len());
    for del in deletes {
        let segs = del.resolve(ctx)?;
        if !resolved_deletes.contains(&segs) {
            resolved_deletes.push(segs);
        }
    }
    // Apply deeper deletes before shallower ones and higher array indices before lower ones so
    // deleting multiple array elements (e.g. `arr[0]`, `arr[1]`) removes the original indices
    // without shifting earlier removals into later paths.
    resolved_deletes.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| trailing_array_index(b).cmp(&trailing_array_index(a)))
    });

    let mut resolved_updates = Vec::with_capacity(updates.len());
    for upd in updates {
        let segs = upd.path.resolve(ctx)?;
        let val = upd.value.eval(ctx)?;
        resolved_updates.push((segs, val));
    }

    for segs in resolved_deletes {
        apply_resolved_delete(state, &segs);
    }
    for (segs, val) in resolved_updates {
        apply_resolved_set(state, &segs, val)?;
    }
    Ok(())
}

fn eval_call_arguments(
    template: &Option<CompiledValue>,
    ctx: &CelContext<'_, '_>,
    label: &str,
) -> Result<Value> {
    let Some(tmpl) = template else {
        return Ok(json!({}));
    };
    let val = tmpl
        .eval(ctx)
        .with_context(|| format!("failed evaluating arguments for {label}"))?;
    match val {
        Value::Object(_) => Ok(val),
        other => bail!("{label} arguments must evaluate to a JSON object, got {other}"),
    }
}

/// Prepared probe invocation ready to be sent to the MCP server.
#[derive(Debug, Clone)]
pub struct PreparedProbe {
    pub probe_index: usize,
    pub name: String,
    pub action: Action,
    trigger_summary: Value,
}

/// Episode-scoped runtime state machine engine.
/// Reset cleanly at the start of every episode, shrink attempt, and replay.
#[derive(Debug, Clone)]
pub struct SpecEngine {
    spec: CompiledSpec,
    state: Map<String, Value>,
}

impl SpecEngine {
    pub fn new(spec: CompiledSpec) -> Self {
        let state = spec.initial_state.clone();
        Self { spec, state }
    }

    pub fn spec(&self) -> &CompiledSpec {
        &self.spec
    }

    pub fn state(&self) -> &Map<String, Value> {
        &self.state
    }

    pub fn state_value(&self) -> Value {
        Value::Object(self.state.clone())
    }

    /// Evaluate all declarative `rules:` (legacy fields + CEL `when`/`assert`) against the current
    /// (pre-transition) shadow state.
    pub fn check_rules(
        &self,
        action: &Action,
        outcome: &RpcOutcome,
        step: usize,
        verbose: bool,
    ) -> Vec<Violation> {
        if self.spec.rules.is_empty() {
            return Vec::new();
        }
        let needs_cel = self
            .spec
            .rules
            .iter()
            .any(|r| r.when.is_some() || !r.asserts.is_empty());
        let ctx = needs_cel.then(|| step_context(&self.state, action, outcome, step));
        let snapshot = needs_cel.then(|| format_step_snapshot(&self.state, action, outcome));
        let serialized = outcome.raw_response.as_ref().map(Value::to_string);
        let mut violations = Vec::new();

        for rule in &self.spec.rules {
            if let Some(target_tool) = &rule.tool {
                match action {
                    Action::CallTool { name, .. } if name == target_tool => {}
                    _ => continue,
                }
            }
            let label = format!("rule:{}", rule.name);
            if let Some(ctx) = ctx.as_ref()
                && !eval_guard(&rule.when, ctx, verbose, &label)
            {
                continue;
            }

            if let Some(forbidden) = &rule.never_contains
                && let Some(ser) = serialized.as_deref()
                && ser.contains(forbidden.as_str())
            {
                violations.push(Violation {
                    property: label.clone(),
                    message: format!("response contained forbidden substring '{forbidden}'"),
                });
            }

            if let Some(max_ms) = rule.max_latency_ms {
                let actual = outcome.latency.as_millis() as u64;
                if actual > max_ms {
                    violations.push(Violation {
                        property: label.clone(),
                        message: format!("latency {actual}ms exceeded max_latency_ms {max_ms}ms"),
                    });
                }
            }

            if rule.valid_calls_succeed
                && let Action::CallTool {
                    input_kind: InputKind::Valid,
                    ..
                } = action
                && (outcome.error().is_some() || outcome.is_tool_error())
            {
                violations.push(Violation {
                    property: label.clone(),
                    message: format!(
                        "valid call {} failed: {}",
                        action.short_label(),
                        serialized.as_deref().unwrap_or("<no response>")
                    ),
                });
            }

            if !rule.asserts.is_empty()
                && let (Some(ctx), Some(snap)) = (ctx.as_ref(), snapshot.as_deref())
            {
                violations.extend(eval_asserts(&rule.asserts, ctx, &label, snap));
            }
        }

        violations
    }

    /// Apply all matching `transitions:` in declaration order, evaluating their `assert`s and
    /// updating shadow `state`.
    pub fn apply_transitions(
        &mut self,
        action: &Action,
        outcome: &RpcOutcome,
        step: usize,
        verbose: bool,
    ) -> Vec<Violation> {
        if self.spec.transitions.is_empty() {
            return Vec::new();
        }
        let mut violations = Vec::new();

        for tr in self.spec.transitions.clone() {
            if let Some(allowed_tools) = &tr.tools {
                match action {
                    Action::CallTool { name, .. } if allowed_tools.iter().any(|t| t == name) => {}
                    _ => continue,
                }
            } else if !matches!(action, Action::CallTool { .. }) {
                continue;
            }

            let ctx = step_context(&self.state, action, outcome, step);
            let label = format!("transition:{}", tr.name);
            if !eval_guard(&tr.when, &ctx, verbose, &label) {
                continue;
            }

            let snapshot = format_step_snapshot(&self.state, action, outcome);
            let assert_violations = eval_asserts(&tr.asserts, &ctx, &label, &snapshot);
            if !assert_violations.is_empty() {
                violations.extend(assert_violations);
                continue;
            }

            if let Err(err) = apply_mutations(&mut self.state, &tr.deletes, &tr.updates, &ctx) {
                violations.push(Violation {
                    property: label,
                    message: format!("state update failed: {err} ({snapshot})"),
                });
            } else if verbose {
                eprintln!(
                    "  [spec] transition '{}' -> state={}",
                    tr.name,
                    self.state_value()
                );
            }
        }

        violations
    }

    /// Prepare any `init:` hook at `idx` (evaluating `when` and `call.arguments`).
    pub fn prepare_init_step(&self, idx: usize, verbose: bool) -> Result<Option<Action>> {
        let step = &self.spec.init[idx];
        let ctx = base_context(&self.state, 0, false);
        let label = format!("init:{}", step.name);
        if !eval_guard(&step.when, &ctx, verbose, &label) {
            return Ok(None);
        }
        let arguments = eval_call_arguments(&step.arguments, &ctx, &label)?;
        Ok(Some(Action::CallTool {
            name: step.tool.clone(),
            arguments,
            input_kind: InputKind::Valid,
        }))
    }

    /// Check `init:` assertions and apply `init:` state updates after executing the init call.
    pub fn finish_init_step(
        &mut self,
        idx: usize,
        action: &Action,
        outcome: &RpcOutcome,
        verbose: bool,
    ) -> Vec<Violation> {
        let step = self.spec.init[idx].clone();
        let ctx = step_context(&self.state, action, outcome, 0);
        let label = format!("init:{}", step.name);
        let snapshot = format_step_snapshot(&self.state, action, outcome);

        let mut violations = eval_asserts(&step.asserts, &ctx, &label, &snapshot);
        if !violations.is_empty() {
            return violations;
        }
        if let Err(err) = apply_mutations(&mut self.state, &step.deletes, &step.updates, &ctx) {
            violations.push(Violation {
                property: label,
                message: format!("init state update failed: {err} ({snapshot})"),
            });
        } else if verbose {
            eprintln!(
                "  [spec] init '{}' -> state={}",
                step.name,
                self.state_value()
            );
        }
        violations
    }

    /// Prepare probe at `probe_idx` if it matches `action` and its `when` guard holds against
    /// the current shadow state (which reflects transitions and any earlier probes in this step).
    pub fn prepare_probe(
        &self,
        probe_idx: usize,
        action: &Action,
        outcome: &RpcOutcome,
        discovery: &Discovery,
        step: usize,
        verbose: bool,
    ) -> Result<Option<PreparedProbe>, Violation> {
        let Some(probe) = self.spec.probes.get(probe_idx) else {
            return Ok(None);
        };
        if !probe.after.matches(action, discovery) {
            return Ok(None);
        }
        let ctx = step_context(&self.state, action, outcome, step);
        let label = format!("probe:{}", probe.name);
        if !eval_guard(&probe.when, &ctx, verbose, &label) {
            return Ok(None);
        }
        let arguments =
            eval_call_arguments(&probe.arguments, &ctx, &label).map_err(|err| Violation {
                property: label.clone(),
                message: format!("failed building probe arguments: {err}"),
            })?;
        let trigger_summary = json!({
            "tool": match action {
                Action::CallTool { name, .. } => name.as_str(),
                _ => "",
            },
            "action": action.method(),
            "args": match action {
                Action::CallTool { arguments, .. } => arguments.clone(),
                Action::GetPrompt { arguments, .. } => arguments.clone(),
                Action::ReadResource { uri } => json!({ "uri": uri }),
                Action::ListTools | Action::Ping => json!({}),
            },
            "res": normalize_outcome_res(outcome),
            "ok": outcome_is_ok(outcome),
        });
        Ok(Some(PreparedProbe {
            probe_index: probe_idx,
            name: probe.name.clone(),
            action: Action::CallTool {
                name: probe.tool.clone(),
                arguments,
                input_kind: InputKind::Valid,
            },
            trigger_summary,
        }))
    }

    /// Evaluate a probe's assertions (and optional updates) against the probe response and shadow state.
    pub fn finish_probe(
        &mut self,
        prepared: &PreparedProbe,
        outcome: &RpcOutcome,
        step: usize,
        verbose: bool,
    ) -> Vec<Violation> {
        let probe = self.spec.probes[prepared.probe_index].clone();
        let mut ctx = step_context(&self.state, &prepared.action, outcome, step);
        ctx.add_variable_from_value("trigger", json_to_cel(&prepared.trigger_summary));

        let label = format!("probe:{}", probe.name);
        let snapshot = format_step_snapshot(&self.state, &prepared.action, outcome);

        let mut violations = eval_asserts(&probe.asserts, &ctx, &label, &snapshot);
        if !violations.is_empty() {
            return violations;
        }
        if !probe.deletes.is_empty() || !probe.updates.is_empty() {
            if let Err(err) = apply_mutations(&mut self.state, &probe.deletes, &probe.updates, &ctx)
            {
                violations.push(Violation {
                    property: label,
                    message: format!("probe state update failed: {err} ({snapshot})"),
                });
            } else if verbose {
                eprintln!(
                    "  [spec] probe '{}' -> state={}",
                    probe.name,
                    self.state_value()
                );
            }
        }
        violations
    }

    /// Try to pick and materialize a state-guided action from `actions:`.
    /// Returns `Ok(None)` if no actions are configured, none are enabled (or all enabled actions
    /// attempt to pick from an empty collection), or Hegel chooses schema-guided exploration
    /// (when `actions_only` is false). Returns `Err(Violation)` on genuine CEL spec errors in
    /// an enabled action's `args:` template.
    pub fn try_pick_action(
        &self,
        active_tools: &[ToolDef],
        pool: &ValuePool,
        tc: &TestCase,
        step: usize,
        verbose: bool,
    ) -> Result<Option<Action>, Violation> {
        if self.spec.actions.is_empty() {
            return Ok(None);
        }

        let guard_ctx = base_context(&self.state, step, false);
        let mut enabled: Vec<(&CompiledAction, &ToolDef)> = Vec::new();

        for act in &self.spec.actions {
            let Some(tool_def) = active_tools.iter().find(|t| t.name == act.tool) else {
                continue;
            };
            let label = format!("action:{}", act.name);
            if eval_guard(&act.when, &guard_ctx, verbose, &label) {
                enabled.push((act, tool_def));
            }
        }

        if enabled.is_empty() {
            return Ok(None);
        }

        // Unless `actions_only: true`, mix 80% state-guided spec actions with 20% general
        // schema/boundary/malformed exploration using a shrinkable boolean.
        if !self.spec.actions_only && !tc.draw_silent(gs::weighted_booleans(0.80)) {
            return Ok(None);
        }

        while !enabled.is_empty() {
            // Weighted shrinkable pick (smaller index / earlier action preferred during shrinking).
            let chosen_idx = if enabled.len() == 1 {
                0
            } else {
                let total_weight: usize = enabled
                    .iter()
                    .fold(0usize, |acc, (a, _)| acc.saturating_add(a.weight.max(1)));
                let roll = draw_shrinking_index(tc, total_weight);
                let mut acc = 0usize;
                let mut picked_idx = 0usize;
                for (i, entry) in enabled.iter().enumerate() {
                    acc = acc.saturating_add(entry.0.weight.max(1));
                    if roll < acc {
                        picked_idx = i;
                        break;
                    }
                }
                picked_idx
            };

            let (act, tool_def) = enabled[chosen_idx];
            let arguments = match &act.args {
                None => generate_tool_args(&tool_def.input_schema, pool, tc, InputKind::Valid),
                Some(tmpl) => {
                    clear_empty_pick();
                    let _tc_guard = TcGuard::enter(tc);
                    let arg_ctx = base_context(&self.state, step, true);
                    match tmpl.eval(&arg_ctx) {
                        Ok(Value::Object(mut map)) => {
                            let _ = take_empty_pick();
                            fill_missing_required_args(&mut map, &tool_def.input_schema, pool, tc);
                            Value::Object(map)
                        }
                        Ok(other) => {
                            let _ = take_empty_pick();
                            return Err(Violation {
                                property: format!("action:{}", act.name),
                                message: format!(
                                    "action args must evaluate to a JSON object, got {other}"
                                ),
                            });
                        }
                        Err(err) => {
                            if take_empty_pick() {
                                if verbose {
                                    eprintln!(
                                        "  [spec] action '{}' skipped (empty collection pick: {err})",
                                        act.name
                                    );
                                }
                                enabled.remove(chosen_idx);
                                continue;
                            }
                            return Err(Violation {
                                property: format!("action:{}", act.name),
                                message: format!("action args evaluation failed: {err}"),
                            });
                        }
                    }
                }
            };

            return Ok(Some(Action::CallTool {
                name: act.tool.clone(),
                arguments,
                input_kind: InputKind::Valid,
            }));
        }

        Ok(None)
    }

    /// Generate an adversarial action attempting to break the state machine via one of:
    /// 1. Cross-state / precondition violation (relaxing `when:` and `pick_key_where`),
    /// 2. Deep payload mutation of a valid materialized spec action (swapped IDs, boundary values,
    ///    wrong types, dropped keys, unexpected extra fields), or
    /// 3. Replaying an earlier tool call verbatim or with a mutated payload (same idempotency key).
    pub fn try_pick_adversarial_action(
        &self,
        active_tools: &[ToolDef],
        pool: &ValuePool,
        history: &[Action],
        tc: &TestCase,
        step: usize,
        verbose: bool,
    ) -> Option<Action> {
        if self.spec.actions.is_empty() {
            return None;
        }

        let guard_ctx = base_context(&self.state, step, false);
        let mut enabled: Vec<(&CompiledAction, &ToolDef)> = Vec::new();
        let mut disabled: Vec<(&CompiledAction, &ToolDef)> = Vec::new();
        for act in &self.spec.actions {
            let Some(tool_def) = active_tools.iter().find(|t| t.name == act.tool) else {
                continue;
            };
            let label = format!("action:{}", act.name);
            if eval_guard(&act.when, &guard_ctx, false, &label) {
                enabled.push((act, tool_def));
            } else {
                disabled.push((act, tool_def));
            }
        }
        let all_active: Vec<(&CompiledAction, &ToolDef)> =
            enabled.iter().chain(disabled.iter()).copied().collect();
        if all_active.is_empty() {
            return None;
        }

        let replayable: Vec<&Action> = history
            .iter()
            .filter(|a| match a {
                Action::CallTool { name, .. } => active_tools.iter().any(|t| t.name == *name),
                _ => false,
            })
            .collect();

        // Tactics: 0 = CrossState, 1 = MutateArgs, 2 = ReplayStep
        let has_cross_state_target = !disabled.is_empty()
            || (!self.state.is_empty()
                && all_active
                    .iter()
                    .any(|(a, _)| a.args.as_ref().is_some_and(compiled_value_uses_state_pick)));
        let mut tactics: Vec<u8> = Vec::with_capacity(3);
        if has_cross_state_target {
            tactics.push(0);
        }
        if !enabled.is_empty() {
            tactics.push(1);
        }
        if !replayable.is_empty() {
            tactics.push(2);
        }
        if tactics.is_empty() {
            return None;
        }
        let tactic = tactics[draw_shrinking_index(tc, tactics.len())];

        match tactic {
            0 => {
                // Prefer actions whose `when` guard is currently false (illegal transition),
                // otherwise pick any active action and relax `pick_key_where` to select a
                // mismatched-state or unknown entity ID.
                let pool_actions =
                    if !disabled.is_empty() && tc.draw_silent(gs::weighted_booleans(0.7)) {
                        &disabled
                    } else {
                        &all_active
                    };
                let (act, tool_def) = pool_actions[draw_shrinking_index(tc, pool_actions.len())];
                let arguments = match &act.args {
                    None => {
                        generate_tool_args(&tool_def.input_schema, pool, tc, InputKind::Boundary)
                    }
                    Some(tmpl) => {
                        clear_empty_pick();
                        let _tc_guard = TcGuard::enter(tc);
                        let _relax = RelaxPickGuard::enter();
                        let arg_ctx = base_context(&self.state, step, true);
                        match tmpl.eval(&arg_ctx) {
                            Ok(Value::Object(mut map)) => {
                                let _ = take_empty_pick();
                                fill_missing_required_args(
                                    &mut map,
                                    &tool_def.input_schema,
                                    pool,
                                    tc,
                                );
                                Value::Object(map)
                            }
                            _ => {
                                let _ = take_empty_pick();
                                return None;
                            }
                        }
                    }
                };
                if verbose {
                    eprintln!(
                        "  [adversarial:cross-state] action '{}' -> {}",
                        act.name, arguments
                    );
                }
                Some(Action::CallTool {
                    name: act.tool.clone(),
                    arguments,
                    input_kind: InputKind::Boundary,
                })
            }
            1 => {
                let (act, tool_def) = enabled[draw_shrinking_index(tc, enabled.len())];
                let base_args = match &act.args {
                    None => generate_tool_args(&tool_def.input_schema, pool, tc, InputKind::Valid),
                    Some(tmpl) => {
                        clear_empty_pick();
                        let _tc_guard = TcGuard::enter(tc);
                        let arg_ctx = base_context(&self.state, step, true);
                        match tmpl.eval(&arg_ctx) {
                            Ok(Value::Object(mut map)) => {
                                let _ = take_empty_pick();
                                fill_missing_required_args(
                                    &mut map,
                                    &tool_def.input_schema,
                                    pool,
                                    tc,
                                );
                                Value::Object(map)
                            }
                            _ => {
                                let _ = take_empty_pick();
                                return None;
                            }
                        }
                    }
                };
                let (mutated, kind) = mutate_action_args(&base_args, &self.state, pool, tc, false)?;
                if verbose {
                    eprintln!(
                        "  [adversarial:mutate] action '{}' -> {}",
                        act.name, mutated
                    );
                }
                Some(Action::CallTool {
                    name: act.tool.clone(),
                    arguments: mutated,
                    input_kind: kind,
                })
            }
            _ => {
                let orig = replayable[draw_shrinking_index(tc, replayable.len())];
                let Action::CallTool {
                    name, arguments, ..
                } = orig
                else {
                    return None;
                };
                if tc.draw_silent(gs::booleans()) {
                    if verbose {
                        eprintln!("  [adversarial:replay-verbatim] {name}({arguments})");
                    }
                    Some(Action::CallTool {
                        name: name.clone(),
                        arguments: arguments.clone(),
                        input_kind: InputKind::Valid,
                    })
                } else {
                    let (mutated, kind) =
                        mutate_action_args(arguments, &self.state, pool, tc, true)?;
                    if verbose {
                        eprintln!("  [adversarial:replay-mutated] {name}({mutated})");
                    }
                    Some(Action::CallTool {
                        name: name.clone(),
                        arguments: mutated,
                        input_kind: kind,
                    })
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum JsonPathStep {
    Key(String),
    Index(usize),
}

fn compiled_value_uses_state_pick(val: &CompiledValue) -> bool {
    match val {
        CompiledValue::Literal(_) => false,
        CompiledValue::Expr(expr) => {
            expr.source.contains("pick_key_where") || expr.source.contains("pick_key")
        }
        CompiledValue::Array(items) => items.iter().any(compiled_value_uses_state_pick),
        CompiledValue::Object(entries) => entries
            .iter()
            .any(|(_, v)| compiled_value_uses_state_pick(v)),
    }
}

fn collect_mutation_targets(
    val: &Value,
    prefix: &mut Vec<JsonPathStep>,
    preserve_idempotency_key: bool,
    out: &mut Vec<Vec<JsonPathStep>>,
) {
    match val {
        Value::Object(map) => {
            for (k, v) in map {
                if preserve_idempotency_key
                    && (k == "idempotency_key" || k == "idempotencyKey" || k == "business")
                {
                    continue;
                }
                prefix.push(JsonPathStep::Key(k.clone()));
                match v {
                    Value::Object(inner) if !inner.is_empty() => {
                        collect_mutation_targets(v, prefix, preserve_idempotency_key, out);
                    }
                    Value::Array(arr) if !arr.is_empty() => {
                        out.push(prefix.clone());
                        collect_mutation_targets(v, prefix, preserve_idempotency_key, out);
                    }
                    _ => out.push(prefix.clone()),
                }
                prefix.pop();
            }
        }
        Value::Array(arr) => {
            for (idx, item) in arr.iter().enumerate() {
                prefix.push(JsonPathStep::Index(idx));
                match item {
                    Value::Object(inner) if !inner.is_empty() => {
                        collect_mutation_targets(item, prefix, preserve_idempotency_key, out);
                    }
                    _ => out.push(prefix.clone()),
                }
                prefix.pop();
            }
        }
        _ => {}
    }
}

fn collect_state_scalars(val: &Value, depth: usize, out: &mut Vec<Value>) {
    if depth > 4 || out.len() >= 64 {
        return;
    }
    match val {
        Value::String(s) if !s.is_empty() && s.len() <= 128 => {
            if !out.contains(val) {
                out.push(val.clone());
            }
        }
        Value::Number(_) => {
            if !out.contains(val) {
                out.push(val.clone());
            }
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                let kv = Value::String(k.clone());
                if !k.is_empty() && !out.contains(&kv) {
                    out.push(kv);
                }
                collect_state_scalars(&map[k], depth + 1, out);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter().take(16) {
                collect_state_scalars(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn read_json_path<'a>(root: &'a Value, path: &[JsonPathStep]) -> Option<&'a Value> {
    let mut cur = root;
    for step in path {
        cur = match (cur, step) {
            (Value::Object(m), JsonPathStep::Key(k)) => m.get(k)?,
            (Value::Array(a), JsonPathStep::Index(i)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn set_json_path(root: &mut Value, path: &[JsonPathStep], new_val: Value) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut cur = root;
    for step in parents {
        cur = match (cur, step) {
            (Value::Object(m), JsonPathStep::Key(k)) => match m.get_mut(k) {
                Some(v) => v,
                None => return false,
            },
            (Value::Array(a), JsonPathStep::Index(i)) => match a.get_mut(*i) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    match (cur, last) {
        (Value::Object(m), JsonPathStep::Key(k)) => {
            m.insert(k.clone(), new_val);
            true
        }
        (Value::Array(a), JsonPathStep::Index(i)) if *i < a.len() => {
            a[*i] = new_val;
            true
        }
        _ => false,
    }
}

fn delete_json_path(root: &mut Value, path: &[JsonPathStep]) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut cur = root;
    for step in parents {
        cur = match (cur, step) {
            (Value::Object(m), JsonPathStep::Key(k)) => match m.get_mut(k) {
                Some(v) => v,
                None => return false,
            },
            (Value::Array(a), JsonPathStep::Index(i)) => match a.get_mut(*i) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    match (cur, last) {
        (Value::Object(m), JsonPathStep::Key(k)) => m.remove(k).is_some(),
        (Value::Array(a), JsonPathStep::Index(i)) if *i < a.len() => {
            a.remove(*i);
            true
        }
        _ => false,
    }
}

fn inject_extra_key(root: &mut Value, parent_path: &[JsonPathStep]) -> bool {
    let mut cur = root;
    for step in parent_path {
        cur = match (cur, step) {
            (Value::Object(m), JsonPathStep::Key(k)) => match m.get_mut(k) {
                Some(v) => v,
                None => return false,
            },
            (Value::Array(a), JsonPathStep::Index(i)) => match a.get_mut(*i) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    if let Value::Object(m) = cur {
        m.insert("__blofeld_unexpected__".into(), json!("adversarial"));
        true
    } else {
        false
    }
}

fn mutate_action_args(
    args: &Value,
    state: &Map<String, Value>,
    pool: &ValuePool,
    tc: &TestCase,
    preserve_idempotency_key: bool,
) -> Option<(Value, InputKind)> {
    let mut targets = Vec::new();
    let mut prefix = Vec::new();
    collect_mutation_targets(args, &mut prefix, preserve_idempotency_key, &mut targets);
    if targets.is_empty() {
        return None;
    }

    // Prefer mutating inner payload fields (e.g. `input.*`) over top-level routing keys (`business`)
    // so adversarial calls actually reach the command handler underneath.
    let non_business: Vec<&Vec<JsonPathStep>> = targets
        .iter()
        .filter(|p| !matches!(p.first(), Some(JsonPathStep::Key(k)) if k == "business"))
        .collect();
    let pool_targets = if !non_business.is_empty() {
        non_business
    } else {
        targets.iter().collect()
    };

    let target_path = pool_targets[draw_shrinking_index(tc, pool_targets.len())];
    let orig_val = read_json_path(args, target_path)?.clone();
    let mut mutated = args.clone();

    let mode: u8 = tc.draw_silent(gs::integers::<u8>().min_value(0).max_value(4));
    match mode {
        // 0: Swap with another scalar from state/pool (cross-entity / stale-ID confusion)
        0 => {
            let mut candidates = Vec::new();
            collect_state_scalars(&Value::Object(state.clone()), 0, &mut candidates);
            for s in pool.strings_slice() {
                let v = Value::String(s.clone());
                if !candidates.contains(&v) {
                    candidates.push(v);
                }
            }
            let different: Vec<&Value> = candidates.iter().filter(|v| **v != orig_val).collect();
            let replacement = if !different.is_empty() {
                different[draw_shrinking_index(tc, different.len())].clone()
            } else {
                json!("__blofeld_swapped__")
            };
            if set_json_path(&mut mutated, target_path, replacement) {
                Some((mutated, InputKind::Boundary))
            } else {
                None
            }
        }
        // 1: Boundary / adversarial scalar value
        1 => {
            let boundary_vals = [
                json!(""),
                json!("-1"),
                json!("0"),
                json!("99999999999999999999"),
                json!("-999999999999999999"),
                json!("__blofeld_unknown__"),
                json!("2099-99-99T99:99:99Z"),
                json!("../../etc/passwd"),
                json!("\0"),
                json!(-1),
                json!(0),
            ];
            let replacement = boundary_vals[draw_shrinking_index(tc, boundary_vals.len())].clone();
            if set_json_path(&mut mutated, target_path, replacement) {
                Some((mutated, InputKind::Boundary))
            } else {
                None
            }
        }
        // 2: Wrong JSON type
        2 => {
            let wrong = match orig_val {
                Value::String(_) => json!(-42),
                Value::Number(_) => json!("not-a-number"),
                Value::Bool(_) => json!("not-a-bool"),
                Value::Array(_) => json!({ "not": "array" }),
                Value::Object(_) => json!(["not", "object"]),
                Value::Null => json!(123),
            };
            if set_json_path(&mut mutated, target_path, wrong) {
                Some((mutated, InputKind::Malformed))
            } else {
                None
            }
        }
        // 3: Drop field
        3 => {
            if delete_json_path(&mut mutated, target_path) {
                Some((mutated, InputKind::Malformed))
            } else {
                None
            }
        }
        // 4: Inject unexpected extra key into target's parent object
        _ => {
            let parent = &target_path[..target_path.len() - 1];
            if inject_extra_key(&mut mutated, parent) {
                Some((mutated, InputKind::Malformed))
            } else {
                None
            }
        }
    }
}

fn format_step_snapshot(
    state: &Map<String, Value>,
    action: &Action,
    outcome: &RpcOutcome,
) -> String {
    let ok = outcome_is_ok(outcome);
    let res = normalize_outcome_res(outcome);
    let state_json = Value::Object(state.clone());
    format!(
        "action={}, ok={ok}, res={}, state={}",
        action.short_label(),
        truncate_json(&res, 240),
        truncate_json(&state_json, 240),
    )
}

fn truncate_json(v: &Value, max_chars: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max_chars {
        s
    } else {
        let prefix: String = s.chars().take(max_chars).collect();
        format!("{prefix}…")
    }
}
