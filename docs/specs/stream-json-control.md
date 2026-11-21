# stream-json control channel (`mini-agent-control/1`)

- **Document role**: wire contract for `-p --input-format stream-json --output stream-json --stream-json-control`
- **Protocol version**: `mini-agent-control/1` (reported by `initialize`)
- **Code**: `src/headless_control.rs`, `src/print.rs` (`spawn_control_reader`), `src/startup.rs`
  (`dispatch_print_stream_input`, `apply_headless_model`), `src/agent/tools/concurrency.rs`

The channel is a CLI flag only. Without it every `control_*` line is an ordinary `input_error` and
headless permission asks stay denied. Unknown fields in any record must be ignored by both sides.

## Records the driver sends

Every request is acknowledged by one line:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{...}}}
{"type":"control_response","response":{"subtype":"error","request_id":"r1","error":"reason"}}
```

`response.response` is present only for requests that return data (`initialize`).

| `request.subtype` | Fields | Effect |
|---|---|---|
| `initialize` | any (hooks, agents, MCP servers, prompts are ignored) | Success payload: `protocol`, `supported_requests`, `session_id`, `commands` (always `[]`; headless has no slash commands), `models` (configured `[quick_models]` aliases) and `can_use_tool_reply`. May be sent at any time and more than once. |
| `interrupt` | none | Cancels the running turn like the first SIGINT; an error when no turn is running or queued. |
| `set_permission_mode` | `mode` | Narrows the active mode only; raising privilege is refused. |
| `set_model` | `model` | Selects a `[quick_models]` alias for the next turn. An unknown alias is an `error` ack. The switch is applied at the next turn boundary with the checks `/model` makes: a session that holds provider-native state refuses it (an `input_error` record names the reason and the old model stays), and an unusable credential leaves the old route in place. There is no reset to the startup model; send its alias. |

## Permission asks (`can_use_tool`)

The runner sends, for a tool `Ask`:

```json
{"type":"control_request","request_id":"perm_1","request":{"subtype":"can_use_tool","tool_name":"shell","input":"<reviewed text>","input_bytes":12,"truncated":false,"tool_input":{"command":"ls"}}}
```

`input` is the complete text being approved (never clipped, deceptive format characters shown as
`<U+XXXX>`). `tool_input` is the call's JSON arguments, present for tool calls whose arguments are an
object of at most 1 MiB; it is what `updatedInput` edits. The driver answers with a `control_response`
whose `response.response` is one of:

```json
{"behavior":"allow"}
{"behavior":"allow","scope":"run"}
{"behavior":"allow","updatedInput":{"command":"ls -la"}}
{"behavior":"deny","message":"why"}
```

Only an explicit allow approves. A timeout, an `error` reply, a closed stdin or an interrupted turn
deny. `scope:"run"` matters only to `sandbox_network` asks.

### `updatedInput`

`updatedInput` (also accepted as `updated_input`) must be a JSON object. The tool then runs with those
arguments instead of the model's. It is:

- ignored (a plain allow) when it equals the offered `tool_input`, or is the offered `input` text, so
  a client that echoes what it received behaves like a plain allow;
- ignored on `sandbox_escalation` and `sandbox_network` asks, which have no arguments to edit;
- re-checked: the call restarts with the edited arguments, goes through the tool's own permission check
  again (deny rules and modes apply, and the driver is asked again if the edited call still needs
  approval), and takes the concurrency lane those arguments need. At most three edits are accepted per
  call; a further one fails the call.

The UI event stream shows the effective arguments (`ToolInputRewritten`), as for a PreToolUse hook
rewrite.

## Compatibility

`--stream-json-schema claude` output plus this channel lets an Agent SDK style driver open a session
with `initialize`, answer `can_use_tool` asks with `allow` / `deny` / `updatedInput` and change the
model with `set_model`. Differences from Claude Code: `can_use_tool.input` is review text (the object is
`tool_input`), `initialize` returns no account or output-style data, and permission rule suggestions are
not sent.
