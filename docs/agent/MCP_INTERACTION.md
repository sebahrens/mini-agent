# MCP prompts, resources and forms

The `mcp` feature discovers each server's advertised prompts and resources when
its accepted connection is established. Each catalogue has at most128 entries,
32 pages and256KiB of serialized metadata, with a three-second budget per kind.
A failed or bounded auxiliary catalogue is marked incomplete and does not remove
an otherwise connected tool server. `/mcp` reports these cached counts without
another RPC. Command and file pickers also use the cached, current-source catalogue.

Use `/mcp:<server>:<prompt>` to select a prompt. An optional JSON object of string
arguments follows the command, for example
`/mcp:owned:review {"topic":"public API"}`. Only declared arguments are accepted;
required arguments must be supplied. The selected `prompts/get` request goes
through ordinary MCP permission and cost admission. The returned text messages
retain their user/assistant role labels inside literal JSON user-turn data. They
cannot become slash commands, shell input, system directives, plan approval or
permission grants. The ordinary provider request then uses the current model,
mode, budget and session policy.

Use `@mcp:<server>:<uri>` in an ordinary message, or `/add mcp:<server>:<uri>` to
retain resource text for later turns in this running session. `/drop
mcp:<server>:<uri>` and `/drop-all` remove these runtime snapshots. Dropping while
a resource selection is pending cancels and settles that exact read before
removal; its late completion cannot restore the attachment. Picker operands
percent-encode names and URIs into a single token, including literal percent
characters. Use the picker spelling for URIs containing escapes or spaces.
An MCP resource URI is an opaque server operand; it is never opened as a host
path or fetched by a separate HTTP client.

`resources/read` uses the same source, ordinary permission and cost checks as
MCP tool calls. Auxiliary approval identities are distinct from remote tool
names and include the exact selected prompt/resource. Existing MCP operation
policy and deny rules still apply. An auxiliary session approval cannot authorize
a same-named remote tool or a different selected resource; native tool grants
cannot authorize auxiliary selections. A selected result must contain only bounded text: at most32
contents and256KiB total, with absent MIME or text, JSON or XML MIME types.
Binary contents are refused. At most20 attachments and1MiB of escaped snapshot
data are retained. Each has the captured session, workspace, source and live
connection generation. Retired snapshots are excluded before the next request.
Selected text is appended as a new literal user message and persists exactly in
that turn's history; older signed native messages and the system/tool prefix are
not rewritten or reread. Runtime attachment selections are not imported or
restored from saved sessions. Resource text is server-supplied data, not verified
host-file content.

## Server form requests

The client advertises form elicitation. An `elicitation/create` callback must
match one raw transport request and exactly one live client RPC on that
connection. Its frontend, Question turn, Run and cancellation scope are captured
from that RPC, not inferred from a tool name, callback text or a last-running
request. Ambiguous concurrent RPCs, reused request IDs and retired sources are
refused. The callback is an owned part of the original work scope and stops with
that work. URL elicitation and server sampling are unsupported. Headless declines forms; ACP cancels unsupported forms; they never supply a permission answer instead.

The interactive form overlay uses the ordinary typed question editor, while
agent, side-question and other UI channels continue to drain. Esc or Ctrl-C
cancels the form and stops its exact work; queued main inputs are discarded by
the ordinary stop policy. A completed form is only typed data returned to the
already authorized server request. It does not claim an agent question, persist
a pending question or create a permission grant. Source, frontend ownership and
actual cost admission are checked again before returning an accepted answer.

Raw JSON is validated before rmcp's typed schema decoder, which may otherwise
drop unknown constraints. The bounded UI supports one to four flat string,
number, integer or boolean fields, string enums (including titled and legacy
enums), and unique multi-select string-enum arrays. It preserves declared
required/optional fields, supported defaults, inclusive numeric bounds,
length/item limits and supported formats. Integer values and bounds use exact
signed/unsigned64-bit arithmetic; fractional integer bounds are unsupported.
An empty string has an explicit choice when allowed; skipping an optional field
omits it. Defaults are validated but never silently submitted.

Unsupported or malformed constraints, nesting, password formats, more than four
fields, incompatible option labels or UI byte limits decline the whole form.
The raw form cap is16KiB, enum count128, field/enum-value bound128bytes, and answer
bound4096bytes. Supported string formats are URI, RFC3339 date-time, padded
YYYY-MM-DD date, and a conservative unquoted email address with a DNS-style
domain. Unknown schema constraints are never accepted under weaker rules. Form
questions are untrusted server text; do not submit credentials through them.

## Ownership and implementation

`src/extras/mcp/catalogue.rs` owns bounded metadata, selected RPCs and literal
text projections. `forms.rs` validates raw schemas and typed answers;
`elicitation.rs` owns per-connection weak RPC leases and form callbacks. Both
stdio and bounded HTTP/SSE transports observe raw requests before decoding.
`ui/mcp_admin_app.rs` uses the existing owned MCP actor for operator selections;
`permission/ask.rs` keeps auxiliary permission and form request types distinct.
Retirement is joined before candidate adoption or new admission. These paths
preserve the configured model-tool ceiling, existing tool wrappers and public/
explicit-loopback transport policy; they grant no new MCP server connection.
