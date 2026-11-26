# ![Blofeld — property-based testing for MCP servers](assets/banner.png)

Blofeld explores failures in [Model Context Protocol (MCP)](https://modelcontextprotocol.io) servers autonomously. Point it at an MCP server URL and it discovers the server's tools, resources, and prompts, generates schema-guided valid, boundary, and malformed calls, chains returned IDs across steps, checks protocol and custom invariants, and shrinks any failure down to a minimal replayable trace.

Powered by [Hegel](https://hegel.dev) (`hegeltest` / native `libhegel`) for structured generation and shrinking, inspired by Antithesis's [Bombadil](https://github.com/antithesishq/bombadil), and integrated with `antithesis_sdk`.

We use Blofeld at [Economico](https://economi.co/?utm_source=github&utm_medium=readme&utm_campaign=blofeld) to fuzz and verify our MCP server.

## Try it

Point `blofeld` at any Streamable HTTP MCP server URL:

```sh
cargo run -- http://localhost:3000/mcp
```

With authentication (`--token`, `BLOFELD_TOKEN`, or `--token-file` reading either a raw token or JSON `{ "accessToken" | "access_token" | "token" }`), self-signed local HTTPS (`-k`), or read-only safety:

```sh
blofeld https://localhost:8787/mcp -k --token "$TOKEN" --read-only
blofeld https://localhost:8787/mcp -k --token-file .context/auth.json
BLOFELD_TOKEN="$TOKEN" blofeld https://api.example.com/mcp
```

Saved counterexample traces (`--save-trace`) automatically redact `Authorization`, `Cookie`, and API key/token headers so traces are safe to share or commit.

Or test a local stdio MCP server:

```sh
blofeld --stdio node server.js
```

Save and deterministically replay shrunk counterexamples:

```sh
blofeld http://localhost:3000/mcp --save-trace failure.trace.json
blofeld http://localhost:3000/mcp --replay failure.trace.json
```

## Use it with your coding agent

Blofeld ships an [agent skill](skills/blofeld/SKILL.md) that teaches coding agents such as Claude Code, Codex, and Cursor how to fuzz an MCP server, handle authentication cleanly, write YAML invariant rules, and lock in shrunk regressions.

Install it into your project with the [skills](https://skills.sh) CLI:

```sh
npx skills add economico/blofeld
```

Without installing anything, point your agent at the repo:

> Read `skills/blofeld/SKILL.md` (or https://github.com/economico/blofeld/blob/main/skills/blofeld/SKILL.md) and follow it. Start a local isolated instance of our MCP server, obtain a bearer token into a temp auth file, run a Blofeld campaign of at least 15 runs and 40 steps with `--save-trace`, and report any failing property with its shrunk counterexample and replay command.

## What it checks out of the box

With zero configuration, `blofeld` runs MCP discovery (`initialize`, `tools/list`, `resources/list`, `prompts/list`), bootstraps zero-argument read-only tools (such as `businesses` or `list_*`) to seed live IDs, and explores stateful call sequences:

- **Schema-guided generation & shrinking**: generates `Valid`, `Boundary`, and `Malformed` arguments from each tool's JSON Schema (including `format`s like `uuid`/`email`/`uri`/`date-time` and `pattern` regexes), chained via a cross-call `ValuePool` that binds collection IDs and slugs across steps.
- **Protocol & envelope safety**: enforces valid JSON-RPC 2.0 envelopes on 2xx responses, matching IDs, `result` XOR `error`, and valid error objects, while allowing transport-level HTTP 4xx refusals.
- **No crashes / 5xx / hangs**: catches transport drops, EOFs, timeouts, and HTTP 5xx errors.
- **MCP contract conformance**: validates `content` blocks, `outputSchema` conformance on `structuredContent`, and `resources/read` results.
- **Stdout hygiene**: flags stray non-JSON-RPC bytes printed to stdout by stdio servers.
- **No internal stack-trace leaks**: detects raw panics, tracebacks, and segfault signatures across both 2xx and 4xx responses.
- **Dual shrinking**: Hegel shrinks both the action sequence and individual JSON argument payloads simultaneously, followed by symbolic-ID-remapping delta-debugging (`ddmin`) so stateful bugs shrink cleanly even when the server issues fresh UUIDs or sequential IDs.

## Optional declarative rules (`--rules`)

Add domain-specific invariants in YAML without writing code:

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

## Antithesis & Hegel stack

`blofeld` combines `hegeltest` (native `libhegel`) and `antithesis_sdk`:

- **Locally / in CI**: deterministic seeded exploration (`--seed`), targeted search toward richer harvested state, full Hypothesis + symbolic `ddmin` shrinking, and optional local assertion logging via `ANTITHESIS_SDK_LOCAL_OUTPUT`.
- **Inside Antithesis** (`--antithesis` or `ANTITHESIS_OUTPUT_DIR` set): switches Hegel to `Backend::Urandom` (driving every draw from Antithesis's `/usr/lib/libvoidstar.so` fuzzer) and emits `assert_always!`, `assert_sometimes!`, and `lifecycle::setup_complete!` straight to Antithesis triage reports.
- **Self-tested with Hegel**: `cargo test` runs both end-to-end MCP server integration tests and `#[hegel::test]` property tests of `blofeld`'s own generator, validator, SSE parser, envelope checker, and value pool.

## Built by Economico

[![Economico: the business context layer for founders and their agents](assets/economico.gif)](https://economi.co/?utm_source=github&utm_medium=readme&utm_campaign=blofeld)

Blofeld comes out of [Economico](https://economi.co/?utm_source=github&utm_medium=readme&utm_campaign=blofeld), the business context layer for founders and their agents. Your customers, agreements, activity and money live in one structured record your own agents operate, with real books and evidence behind every answer.

## License

MIT License. See [LICENSE](LICENSE).
