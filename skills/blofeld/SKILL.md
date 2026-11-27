---
name: blofeld
description: Fuzz and property-test Model Context Protocol (MCP) servers (Streamable HTTP or stdio) with blofeld. Use when testing an MCP server for stateful crashes, 5xx errors, schema mismatches, or JSON-RPC protocol violations; when shrinking or replaying a failing MCP trace; or when writing YAML invariant rules for MCP tools.
---

# Blofeld

Blofeld is a Rust property-based testing CLI for MCP servers powered by Hegel (`libhegel`) and designed to work like Bombadil. Give it an MCP endpoint URL (or `--stdio <cmd>`), and it discovers tools, resources, and prompts, chains returned IDs across calls, checks built-in and custom invariants, and shrinks any violation to a minimal replayable trace.

Run `blofeld --help` for the complete flag list.

## Workflow

1. **Ensure `blofeld` is available.** Check `which blofeld`; if missing, install from crates.io (`cargo install blofeld`) or from a local checkout (`cargo build --release`). Done when `blofeld --help` exits 0.
2. **Prepare an isolated target and auth.**
   - Prefer an isolated local server with ephemeral state so write/mutating tools can be fuzzed freely. Against staging or shared environments, pass `--read-only` (restricts calls to tools with `readOnlyHint: true`) or `--only <tool>`.
   - For local self-signed HTTPS (such as `wrangler dev --local-protocol https`), pass `-k`.
   - If `initialize` returns HTTP `401`, read the printed `WWW-Authenticate` / `resource_metadata` hint, obtain a bearer token using the project's own login helper or OAuth flow, and pass it via `--token-file <path>` or `BLOFELD_TOKEN`.
   - Done when a 1-step smoke check (`--runs 1 --steps 5`) connects and prints discovered tools.
3. **Run an exploration campaign.**
   ```sh
   blofeld https://localhost:8787/mcp -k --token-file auth.json \
     --time-limit 2m --save-trace blofeld.trace.json
   ```
   Or budget by run count (`--runs 15 --steps 40`). Pass `-v` when inspecting live tool calls and latencies, or `--rules rules.yaml` / `--spec spec.yaml` to enforce project-specific invariants and stateful specs. Done when the campaign either exits `0` (`PASS`) or exits `1` (`FAIL`) with a shrunk counterexample and saved trace.
4. **Confirm the failure replays red.**
   ```sh
   blofeld https://localhost:8787/mcp -k --token-file auth.json \
     --replay blofeld.trace.json
   ```
   Always verify the saved trace reproduces the same property failure before editing server code.
5. **Fix and verify.** Fix the server bug, re-run `--replay blofeld.trace.json` until it exits `0`, and then run a fresh campaign (`--runs 20 --steps 50`) with a new seed to verify no follow-on failures remain.
6. **Report results.** State the target URL/command, tool count discovered, runs and steps executed, seed, and for any failure: the violated property, shrunk step count (vs original), minimal call sequence, and replay command.

## Reference

### Authentication & trace safety

- `--token-file <path>` accepts either a raw text token or a JSON auth file containing `accessToken`, `access_token`, or `token` (such as OAuth login script outputs).
- `BLOFELD_TOKEN` sets `Authorization: Bearer <token>` without placing secrets in CLI arguments or shell history.
- `--token <token>` and `-H "Key: Value"` are also supported for direct invocation and custom headers.
- `--save-trace` automatically replaces `Authorization`, `Cookie`, `X-Api-Key`, and `*token*`/`*secret*` header values with `<REDACTED>`. Supply live auth flags again when running `--replay`.

### How exploration chains state

- At the start of each session, Blofeld bootstraps zero-argument read-only tools (for example `businesses` or `list_*`) to seed its `ValuePool` with live IDs and slugs.
- Responses with `isError: true` or JSON-RPC `error` are excluded from `ValuePool` so error text never pollutes later arguments.
- Object `id`/`slug` fields inside named arrays (such as `businesses[].slug`) are bound under both the field name and the singular parent key (`business`), so tools accepting `business` reuse live slugs automatically.
- On failure, Hegel shrinks both the action sequence and individual JSON argument payloads simultaneously, followed by symbolic-ID-remapping delta-debugging so traces shrink cleanly even when the server assigns fresh UUIDs or sequential IDs per session.

### Built-in invariants (always active)

| Property | Fails when |
| :--- | :--- |
| `no_transport_error` | Connection drop, EOF/crash, timeout, or unparseable body |
| `no_http_5xx` | HTTP `500–599` status on any call |
| `valid_jsonrpc_envelope` | HTTP `2xx` (or stdio) response lacks `jsonrpc: "2.0"`, matching `id`, or `result` XOR `error` |
| `valid_tool_result` | Successful `tools/call` result lacks a valid `content` array |
| `output_schema_conformance` | Tool declares `outputSchema`, `isError` is false, and `structuredContent` is missing or violates the schema |
| `stdout_hygiene` | Stdio server writes non-JSON-RPC bytes to stdout |
| `no_internal_error_leak` | Any `2xx` or `4xx` response contains raw runtime panic/traceback signatures |

Note: HTTP `4xx` transport-level refusals (such as `401`/`403`) without a JSON-RPC envelope are tolerated by default while still scanned for stack-trace leaks.

### Custom YAML rules & stateful specs (`--rules` / `--spec`)

```yaml
state:
  items: {}
  count: 0

init:
  - name: seed-count
    call: { tool: list_items, arguments: {} }
    update:
      count: "size(res.items)"

transitions:
  - name: item-created
    tool: create_item
    when: "ok"
    assert: "has(res.id) && !(res.id in state.items)"
    update:
      "items[res.id]": "'open'"
      count: "state.count + 1"
  - name: item-locked
    tool: lock_item
    when: "ok"
    update:
      "items[args.id]": "'locked'"
  - name: item-deleted
    tool: delete_item
    when: "ok"
    delete: ["items[args.id]"]
    update:
      count: "state.count - 1"

probes:
  - name: list-matches-model
    after: mutating # or every_step, never, tool_name, or [tool_a, tool_b]
    call: { tool: list_items, arguments: {} }
    assert: "ok && size(res.items) == state.count"

actions:
  - tool: lock_item
    when: "'open' in values(state.items)"
    args:
      id: "pick_key_where(state.items, 'open')"
  - tool: delete_item
    when: "size(state.items) > 0"
    args:
      id: "pick_key(state.items)"

rules:
  - name: no-sql-errors
    never_contains: "SQLITE_ERROR"
  - name: fast-reads
    max_latency_ms: 1500
  - name: list-always-succeeds
    tool: businesses
    valid_calls_succeed: true
  - name: locked-item-cannot-be-deleted
    tool: delete_item
    when: "args.id in state.items && state.items[args.id] == 'locked'"
    assert: "!ok"
```

All top-level sections are optional—start with just `rules:` for simple assertions, or combine `state:`, `init:`, `transitions:`, `probes:`, and `actions:` to model a stateful workflow. CEL expressions have access to `state`, `tool`, `args`, `ok`, normalized `res` (`structuredContent` → parsed JSON from `content[0].text` → raw `result`), `text`, `error`, `latency_ms`, `http_status`, `step` (plus `trigger` in `probes.assert`), pure helpers (`keys`, `values`, `get`, `set_entry`, `remove_key`, `sum`), and shrinkable Hegel generators inside `actions.args` (`pick_key`, `pick_value`, `pick_key_where`, `pick_item`/`pick`, `gen_int`, `gen_float`, `gen_string`, `gen_bool`). Episode shadow state resets cleanly on every run, during shrinking, and during `--replay`.

Authoring notes:
- String values inside `update:`, `call.arguments:`, and `actions.args:` are evaluated as CEL expressions (while numbers, booleans, and `null` are literal JSON values), so literal strings require inner quotes (e.g. `"'open'"`).
- By default, Blofeld mixes `actions:` with general schema/boundary/malformed exploration; set `actions_only: true` at the top level to restrict generation strictly to `actions:`.
- Pass `--adversarial` (or `--adversarial-rate <0.0..=1.0>` / YAML `adversarial_rate: 0.2`) to make Blofeld actively try to break state-machine specs via cross-state/precondition violations, nested argument mutations, and verbatim or mutated step replays (still respecting `--read-only`).

### Simulation & structured assertions

- Blofeld emits structured `always` / `sometimes` property events and supports external entropy (`Backend::Urandom`) when running inside deterministic simulation environments.
