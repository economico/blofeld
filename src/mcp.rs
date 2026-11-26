use anyhow::{Context, Result, anyhow};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub const PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    Stdio {
        command: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default = "default_object_schema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema", default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: Option<ToolAnnotations>,
}

fn default_object_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolAnnotations {
    #[serde(rename = "readOnlyHint", default)]
    pub read_only_hint: Option<bool>,
    #[serde(rename = "destructiveHint", default)]
    pub destructive_hint: Option<bool>,
    #[serde(rename = "idempotentHint", default)]
    pub idempotent_hint: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceDef {
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDef {
    pub name: String,
    #[serde(default)]
    pub arguments: Vec<PromptArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptArg {
    pub name: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub server_info: Value,
    pub capabilities: Value,
    pub tools: Vec<ToolDef>,
    pub resources: Vec<ResourceDef>,
    pub prompts: Vec<PromptDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Valid,
    Boundary,
    Malformed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    CallTool {
        name: String,
        arguments: Value,
        input_kind: InputKind,
    },
    ReadResource {
        uri: String,
    },
    GetPrompt {
        name: String,
        arguments: Value,
    },
    ListTools,
    Ping,
}

impl Action {
    pub fn method(&self) -> &'static str {
        match self {
            Self::CallTool { .. } => "tools/call",
            Self::ReadResource { .. } => "resources/read",
            Self::GetPrompt { .. } => "prompts/get",
            Self::ListTools => "tools/list",
            Self::Ping => "ping",
        }
    }

    pub fn params(&self) -> Value {
        match self {
            Self::CallTool {
                name, arguments, ..
            } => json!({ "name": name, "arguments": arguments }),
            Self::ReadResource { uri } => json!({ "uri": uri }),
            Self::GetPrompt { name, arguments } => json!({ "name": name, "arguments": arguments }),
            Self::ListTools => json!({}),
            Self::Ping => json!({}),
        }
    }

    pub fn short_label(&self) -> String {
        match self {
            Self::CallTool {
                name,
                arguments,
                input_kind,
            } => format!("tools/call({name}, {input_kind:?}, {arguments})"),
            Self::ReadResource { uri } => format!("resources/read({uri})"),
            Self::GetPrompt { name, .. } => format!("prompts/get({name})"),
            Self::ListTools => "tools/list".to_string(),
            Self::Ping => "ping".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RpcOutcome {
    pub request_id: u64,
    pub latency: Duration,
    pub http_status: Option<u16>,
    pub raw_response: Option<Value>,
    pub transport_error: Option<String>,
    pub stdout_pollution: Vec<String>,
}

impl RpcOutcome {
    pub fn result(&self) -> Option<&Value> {
        self.raw_response.as_ref()?.get("result")
    }

    pub fn error(&self) -> Option<&Value> {
        self.raw_response.as_ref()?.get("error")
    }

    pub fn is_tool_error(&self) -> bool {
        self.result()
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

enum Transport {
    Http {
        client: reqwest::Client,
        url: String,
        headers: HeaderMap,
        session_id: Option<String>,
    },
    Stdio {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
}

pub struct McpClient {
    transport: Transport,
    next_id: u64,
    timeout: Duration,
}

impl McpClient {
    pub async fn connect(target: &Target, timeout: Duration) -> Result<Self> {
        let transport = match target {
            Target::Http { url, headers } => {
                let client = reqwest::Client::builder()
                    .timeout(timeout)
                    .build()
                    .context("failed to build HTTP client")?;
                let mut map = HeaderMap::new();
                for (k, v) in headers {
                    let name = HeaderName::from_bytes(k.as_bytes())
                        .with_context(|| format!("invalid header name: {k}"))?;
                    let val = HeaderValue::from_str(v)
                        .with_context(|| format!("invalid header value for {k}"))?;
                    map.insert(name, val);
                }
                Transport::Http {
                    client,
                    url: url.clone(),
                    headers: map,
                    session_id: None,
                }
            }
            Target::Stdio { command } => {
                let (program, args) = command
                    .split_first()
                    .ok_or_else(|| anyhow!("stdio command cannot be empty"))?;
                let mut child = Command::new(program)
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("failed to spawn stdio MCP server: {program}"))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| anyhow!("failed to open child stdin"))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| anyhow!("failed to open child stdout"))?;
                Transport::Stdio {
                    child,
                    stdin,
                    stdout: BufReader::new(stdout),
                }
            }
        };

        Ok(Self {
            transport,
            next_id: 1,
            timeout,
        })
    }

    pub async fn initialize_and_discover(&mut self) -> Result<(Discovery, Vec<RpcOutcome>)> {
        let mut outcomes = Vec::new();
        let init = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "blofeld",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            )
            .await;
        outcomes.push(init.clone());

        if let Some(err) = &init.transport_error {
            return Err(anyhow!("MCP initialize failed: {err}"));
        }
        if let Some(err) = init.error() {
            return Err(anyhow!("MCP initialize returned JSON-RPC error: {err}"));
        }
        let init_result = init
            .result()
            .cloned()
            .ok_or_else(|| anyhow!("MCP initialize response missing 'result'"))?;

        // Best-effort initialized notification per MCP spec
        let _ = self.notify("notifications/initialized", json!({})).await;

        let mut discovery = Discovery {
            server_info: init_result
                .get("serverInfo")
                .cloned()
                .unwrap_or(Value::Null),
            capabilities: init_result
                .get("capabilities")
                .cloned()
                .unwrap_or(Value::Null),
            ..Default::default()
        };

        // Discover tools
        let tools_res = self.call("tools/list", json!({})).await;
        outcomes.push(tools_res.clone());
        if let Some(arr) = tools_res
            .result()
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_array)
        {
            for item in arr {
                if let Ok(def) = serde_json::from_value::<ToolDef>(item.clone()) {
                    discovery.tools.push(def);
                }
            }
        }

        // Discover resources (optional capability)
        let res_out = self.call("resources/list", json!({})).await;
        if let Some(arr) = res_out
            .result()
            .and_then(|r| r.get("resources"))
            .and_then(Value::as_array)
        {
            for item in arr {
                if let Ok(def) = serde_json::from_value::<ResourceDef>(item.clone()) {
                    discovery.resources.push(def);
                }
            }
        }

        // Discover prompts (optional capability)
        let prompt_out = self.call("prompts/list", json!({})).await;
        if let Some(arr) = prompt_out
            .result()
            .and_then(|r| r.get("prompts"))
            .and_then(Value::as_array)
        {
            for item in arr {
                if let Ok(def) = serde_json::from_value::<PromptDef>(item.clone()) {
                    discovery.prompts.push(def);
                }
            }
        }

        Ok((discovery, outcomes))
    }

    pub async fn execute(&mut self, action: &Action) -> RpcOutcome {
        self.call(action.method(), action.params()).await
    }

    pub async fn call(&mut self, method: &str, params: Value) -> RpcOutcome {
        let id = self.next_id;
        self.next_id += 1;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let started = Instant::now();

        match &mut self.transport {
            Transport::Http {
                client,
                url,
                headers,
                session_id,
            } => {
                let mut req = client
                    .post(url.as_str())
                    .headers(headers.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .header(ACCEPT, "application/json, text/event-stream");
                if let Some(sid) = session_id.as_deref() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                match req.json(&payload).send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        if let Some(sid) = resp
                            .headers()
                            .get("mcp-session-id")
                            .and_then(|v| v.to_str().ok())
                        {
                            *session_id = Some(sid.to_string());
                        }
                        let content_type = resp
                            .headers()
                            .get(CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let body = match resp.text().await {
                            Ok(t) => t,
                            Err(e) => {
                                return RpcOutcome {
                                    request_id: id,
                                    latency: started.elapsed(),
                                    http_status: Some(status),
                                    raw_response: None,
                                    transport_error: Some(format!("failed reading HTTP body: {e}")),
                                    stdout_pollution: vec![],
                                };
                            }
                        };

                        let parsed = if content_type.contains("text/event-stream") {
                            parse_sse_response(&body, id)
                        } else if body.trim().is_empty() {
                            Err(format!("empty HTTP {status} body"))
                        } else {
                            serde_json::from_str::<Value>(&body).map_err(|e| {
                                format!(
                                    "invalid JSON response (HTTP {status}): {e} — body: {}",
                                    truncate(&body, 200)
                                )
                            })
                        };

                        match parsed {
                            Ok(val) => RpcOutcome {
                                request_id: id,
                                latency: started.elapsed(),
                                http_status: Some(status),
                                raw_response: Some(val),
                                transport_error: None,
                                stdout_pollution: vec![],
                            },
                            Err(err) => RpcOutcome {
                                request_id: id,
                                latency: started.elapsed(),
                                http_status: Some(status),
                                raw_response: None,
                                transport_error: Some(err),
                                stdout_pollution: vec![],
                            },
                        }
                    }
                    Err(e) => RpcOutcome {
                        request_id: id,
                        latency: started.elapsed(),
                        http_status: None,
                        raw_response: None,
                        transport_error: Some(format!("HTTP request failed: {e}")),
                        stdout_pollution: vec![],
                    },
                }
            }
            Transport::Stdio { stdin, stdout, .. } => {
                let mut line = match serde_json::to_string(&payload) {
                    Ok(s) => s,
                    Err(e) => {
                        return RpcOutcome {
                            request_id: id,
                            latency: started.elapsed(),
                            http_status: None,
                            raw_response: None,
                            transport_error: Some(format!("serialize error: {e}")),
                            stdout_pollution: vec![],
                        };
                    }
                };
                line.push('\n');
                if let Err(e) = stdin.write_all(line.as_bytes()).await {
                    return RpcOutcome {
                        request_id: id,
                        latency: started.elapsed(),
                        http_status: None,
                        raw_response: None,
                        transport_error: Some(format!("stdin write failed (server exited?): {e}")),
                        stdout_pollution: vec![],
                    };
                }
                let _ = stdin.flush().await;

                let mut pollution = Vec::new();
                let deadline = Instant::now() + self.timeout;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return RpcOutcome {
                            request_id: id,
                            latency: started.elapsed(),
                            http_status: None,
                            raw_response: None,
                            transport_error: Some(format!(
                                "timed out after {}ms waiting for JSON-RPC response id={id}",
                                self.timeout.as_millis()
                            )),
                            stdout_pollution: pollution,
                        };
                    }
                    let mut buf = String::new();
                    match tokio::time::timeout(remaining, stdout.read_line(&mut buf)).await {
                        Ok(Ok(0)) => {
                            return RpcOutcome {
                                request_id: id,
                                latency: started.elapsed(),
                                http_status: None,
                                raw_response: None,
                                transport_error: Some(
                                    "stdio server closed stdout unexpectedly (EOF / crash)"
                                        .to_string(),
                                ),
                                stdout_pollution: pollution,
                            };
                        }
                        Ok(Ok(_)) => {
                            let trimmed = buf.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<Value>(trimmed) {
                                Ok(val) => {
                                    // Skip server notifications (messages without an id)
                                    if val.get("id").is_none() && val.get("method").is_some() {
                                        continue;
                                    }
                                    return RpcOutcome {
                                        request_id: id,
                                        latency: started.elapsed(),
                                        http_status: None,
                                        raw_response: Some(val),
                                        transport_error: None,
                                        stdout_pollution: pollution,
                                    };
                                }
                                Err(_) => {
                                    pollution.push(truncate(trimmed, 200));
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            return RpcOutcome {
                                request_id: id,
                                latency: started.elapsed(),
                                http_status: None,
                                raw_response: None,
                                transport_error: Some(format!("stdout read error: {e}")),
                                stdout_pollution: pollution,
                            };
                        }
                        Err(_) => {
                            return RpcOutcome {
                                request_id: id,
                                latency: started.elapsed(),
                                http_status: None,
                                raw_response: None,
                                transport_error: Some(format!(
                                    "timed out after {}ms waiting for JSON-RPC response id={id}",
                                    self.timeout.as_millis()
                                )),
                                stdout_pollution: pollution,
                            };
                        }
                    }
                }
            }
        }
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match &mut self.transport {
            Transport::Http {
                client,
                url,
                headers,
                session_id,
            } => {
                let mut req = client
                    .post(url.as_str())
                    .headers(headers.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .header(ACCEPT, "application/json, text/event-stream");
                if let Some(sid) = session_id.as_deref() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                let _ = req.json(&payload).send().await;
            }
            Transport::Stdio { stdin, .. } => {
                let mut line = serde_json::to_string(&payload)?;
                line.push('\n');
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
        }
        Ok(())
    }

    pub async fn close(mut self) {
        match &mut self.transport {
            Transport::Http {
                client,
                url,
                headers,
                session_id,
            } => {
                if let Some(sid) = session_id.as_deref() {
                    let _ = client
                        .delete(url.as_str())
                        .headers(headers.clone())
                        .header("Mcp-Session-Id", sid)
                        .send()
                        .await;
                }
            }
            Transport::Stdio { child, .. } => {
                let _ = child.kill().await;
            }
        }
    }
}

/// Extract the JSON-RPC response matching `expected_id` from a Streamable HTTP SSE body.
pub fn parse_sse_response(body: &str, expected_id: u64) -> std::result::Result<Value, String> {
    let mut last_json: Option<Value> = None;
    let mut data_buf = String::new();

    for raw_line in body.lines().chain(std::iter::once("")) {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            if !data_buf.is_empty() {
                if let Ok(val) = serde_json::from_str::<Value>(data_buf.trim()) {
                    if val.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        return Ok(val);
                    }
                    if val.get("id").is_some() {
                        last_json = Some(val);
                    }
                }
                data_buf.clear();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.trim_start());
        }
    }

    last_json.ok_or_else(|| {
        format!(
            "SSE stream contained no JSON-RPC response for id={expected_id}: {}",
            truncate(body, 200)
        )
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
