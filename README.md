# blofeld

Fast, zero-config property-based testing and autonomous state-space exploration for MCP servers—inspired by Antithesis's [Bombadil](https://github.com/antithesishq/bombadil) and built to plug into the Antithesis stack.

## Quick Start

Point `blofeld` at any Streamable HTTP MCP server URL:

```bash
cargo run -- http://localhost:3000/mcp
```

With auth headers or read-only safety:

```bash
blofeld https://api.example.com/mcp -H "Authorization: Bearer $TOKEN" --read-only
```

Or test a local stdio MCP server:

```bash
blofeld --stdio node server.js
```

## What It Checks Out of the Box

With zero configuration, `blofeld` connects, runs MCP discovery (`initialize`, `tools/list`, `resources/list`, `prompts/list`), and autonomously explores the server using a schema-guided fuzzer with a cross-call value pool (so IDs and URIs returned by earlier tool calls are fed into later ones):

- **Protocol & envelope safety**: valid JSON-RPC 2.0 responses, matching IDs, `result` XOR `error`, valid error objects.
- **No crashes / 5xx / hangs**: catches transport drops, EOFs, timeouts, and HTTP 5xx errors across valid, boundary, and malformed inputs.
- **MCP contract conformance**: valid `content` blocks, `outputSchema` validation on `structuredContent`, and `resources/read` shape checks.
- **Stdout hygiene**: flags stray non-JSON-RPC output on stdio servers.
- **No internal stack-trace leaks**: detects raw panics and tracebacks.
- **Automatic trace shrinking**: reduces failing multi-step sessions to the minimal reproducible counterexample (`--save-trace` / `--replay`).

## Optional Declarative Rules (`--rules`)

```yaml
rules:
  - name: no-sql-errors
    never_contains: "SQLITE_ERROR"
  - name: fast-tools
    max_latency_ms: 2000
  - name: search-always-works
    tool: search
    valid_calls_succeed: true
```

## Antithesis Stack Integration

`blofeld` uses `antithesis_sdk` directly (matching Bombadil's architecture):

- Locally / in CI: deterministic seeded exploration (`--seed`), delta-debugging trace shrinking, and optional local assertion logging via `ANTITHESIS_SDK_LOCAL_OUTPUT`.
- Inside Antithesis (`--antithesis` or `ANTITHESIS_OUTPUT_DIR` set): drives decisions through `antithesis_sdk::random::AntithesisRng` and emits `assert_always!`, `assert_sometimes!`, and `lifecycle::setup_complete!` straight to Antithesis triage reports.
