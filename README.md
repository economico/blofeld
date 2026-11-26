# blofeld

Fast, zero-config property-based testing and autonomous state-space exploration for MCP servers—inspired by Antithesis's [Bombadil](https://github.com/antithesishq/bombadil), powered by [Hegel](https://hegel.dev) (`hegeltest` / native `libhegel`), and integrated with `antithesis_sdk`.

## Quick Start

Point `blofeld` at any Streamable HTTP MCP server URL:

```bash
cargo run -- http://localhost:3000/mcp
```

With auth headers, self-signed local HTTPS (`-k`), or read-only safety:

```bash
blofeld https://localhost:8787/mcp -k -H "Authorization: Bearer $TOKEN" --read-only
```

Or test a local stdio MCP server:

```bash
blofeld --stdio node server.js
```

## What It Checks Out of the Box

With zero configuration, `blofeld` connects, runs MCP discovery (`initialize`, `tools/list`, `resources/list`, `prompts/list`), bootstraps zero-arg read-only tools (such as `businesses` or `list_*`), and explores the server using Hegel's native Hypothesis engine (`libhegel`):

- **Schema-guided generation & shrinking**: generates `Valid`, `Boundary`, and `Malformed` arguments from each tool's JSON Schema (including `format`s like `uuid`/`email`/`uri`/`date-time` and `pattern` regexes), chained via a cross-call `ValuePool`.
- **Protocol & envelope safety**: valid JSON-RPC 2.0 responses on 2xx, matching IDs, `result` XOR `error`, valid error objects.
- **No crashes / 5xx / hangs**: catches transport drops, EOFs, timeouts, and HTTP 5xx errors.
- **MCP contract conformance**: valid `content` blocks, `outputSchema` validation on `structuredContent`, and `resources/read` shape checks.
- **Stdout hygiene**: flags stray non-JSON-RPC output on stdio servers.
- **No internal stack-trace leaks**: detects raw panics and tracebacks across both 2xx and 4xx responses.
- **Dual shrinking**: Hegel shrinks both the action trace and individual JSON argument payloads simultaneously, followed by symbolic-ID-remapping delta-debugging (`--save-trace` / `--replay`).

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

`blofeld` combines `hegeltest` (native `libhegel`) and `antithesis_sdk`:

- **Locally / in CI**: deterministic seeded exploration (`--seed`), targeted search toward richer harvested state, full Hypothesis + `ddmin` shrinking, and optional local assertion logging via `ANTITHESIS_SDK_LOCAL_OUTPUT`.
- **Inside Antithesis** (`--antithesis` or `ANTITHESIS_OUTPUT_DIR` set): switches Hegel to `Backend::Urandom` (driving every draw from Antithesis's `/usr/lib/libvoidstar.so` fuzzer) and emits `assert_always!`, `assert_sometimes!`, and `lifecycle::setup_complete!` straight to Antithesis triage reports.
