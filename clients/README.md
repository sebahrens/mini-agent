# mini-agent clients

Thin clients over the stream-json control protocol (`docs/specs/stream-json-control.md`,
protocol `mini-agent-control/1`). Each spawns
`mini-agent -p --input-format stream-json --output stream-json --stream-json-control`, and exposes a
`query()` helper, a `Session` (send / turns / interrupt / set_model / set_permission_mode / initialize)
and a permission callback wired to `can_use_tool` (absent callback, an exception or a timeout deny).

| | TypeScript (`clients/typescript`) | Python (`clients/python`) |
|---|---|---|
| Tests | `cd clients/typescript && npm test` (Node 18+) | `cd clients/python && python3 -m unittest discover -s tests` |
| Entry | `import { query, Session } from '@mini-agent/client'` | `from mini_agent_client import query, Session` |

Tests run against a protocol stand-in (`fake-agent`) so they need neither the binary nor a provider.
Set `argv` to bypass the generated flags when wrapping the binary.

## Agent SDK option mapping

| Claude Agent SDK option | mini-agent client option | Flag |
|---|---|---|
| `prompt` | `prompt` | positional prompt after `-p` |
| `canUseTool` | `canUseTool` / `can_use_tool` | `can_use_tool` control request |
| `interrupt()` | `interrupt()` | control `interrupt` |
| `model` / `setModel` | `model` / `setModel()` | `--model` / control `set_model` (a `[quick_models]` alias) |
| `permissionMode` | `permissionMode` | `--permission-mode` / control `set_permission_mode` (narrowing only) |
| `includePartialMessages` | `includePartialMessages` | `--include-partial-messages` |
| `continue` | `continueSession` | `--continue` |
| message schema | `schema` (default `claude`) | `--stream-json-schema` |

Not provided: hooks, subagent definitions, MCP server injection and account data through `initialize`
(ignored by the agent); `can_use_tool.input` is review text and the call's object arrives as `tool_input`.

## Not done yet

A `--stream-json-schema-dump` flag generating JSON Schema from the Rust record types, and a CI job
running these suites against the debug binary with a stub provider.
