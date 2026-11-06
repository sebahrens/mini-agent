# MCP configuration and live operations

`/mcp` reports configured server origins, inactive definitions, retained
connections, and observed connection or tool-catalogue attempts. Reporting does
not connect, probe, or request a new tool list. A failed reconnect can coexist
with a retained usable connection; the two observations are shown separately.
`/doctor` also reports the last typed connection/catalogue outcomes, capped at
32 server records. These are retained observations, not a new health probe;
removed, cancelled, failed and pending attempts remain distinguishable.

```text
/mcp add local http://localhost:3000/mcp --allow-local
/mcp add runner "C:\Program Files\runner.exe" "literal argument" --project
/mcp remove local
/mcp remove runner --project
```

Arguments use bounded literal quoting, including empty arguments and Unicode.
They are not shell expressions. `--` ends flag parsing. `--allow-local` applies
only to URL servers. Duplicate names are refused. Global mutations update only
the named entry at the global config path captured during startup, preserving
unavailable feature keys, unrelated values, and JSON/TOML/YAML format.
`--project` targets the captured workspace's `.mcp.json`; it does not edit an
inherited project TOML entry. Removal refuses a different selected owning
source. Built-in servers retain their dedicated enable settings.

Project JSON uses the `mcpServers` object. Precedence is global config, trusted
`.mcp.json`, trusted `.mini-agent/config.toml`, then built-in injection. A project
file is bounded to 1 MiB and 128 server entries, read without following symbolic
links or accepting hardlinks, directories, or FIFOs. Malformed or untrusted
definitions remain inactive and visible. Feature-off builds preserve config
keys without reading or launching project JSON servers.

An authored project edit is saved privately before connection. It shows a
bounded review of the entire resulting file, with environment/header values
and recognized secret-bearing argument options redacted. `[Y]` records trust for
that exact raw SHA-256, canonical file and captured workspace; `[N]` leaves the edited content
inactive. An unrelated pre-existing entry receives no implicit approval.
Replacing the reviewed content, changing sessions/workspace bindings, or
retiring the operation prevents a stale approval from connecting a server.
Existing project TOML trust does not authorize `.mcp.json`.

`${VAR}` substitution occurs after raw project content is trusted, on supported
command, argument, working-directory, environment, URL and header values. It is
bounded, one-pass literal substitution. `${VAR:-default}` uses the literal
`default` (up to the first `}`, never expanded itself, same size bounds) when
`VAR` is unset or empty. A missing variable without a default leaves that server
inactive and reports the variable name, without its value. Raw placeholders
remain on disk and in trust fingerprints; expanded credentials are not copied
to notices or origin metadata. Literal secrets explicitly placed in a project
file remain file content; review redaction does not make those bytes inaccessible
to another reader already authorized to read that file.

Project URL servers may use `"type": "http"` or `"streamable-http"`; the legacy
`"type": "sse"` (and `ws`) is rejected with a message naming the `http`
alternative. For auth headers sourced from the environment, `bearerTokenEnvVar`
(`Authorization: Bearer ${VAR}`) and `envHttpHeaders` (`{"Header": "VAR"}`) are
folded into `headers` as ordinary `${VAR}` references, with the same
substitution, bounds and missing-variable behavior. `headersHelper` (also `headers_helper`; a command that mints headers) is
**closed by default**: it runs a command named by configuration, so nothing runs
unless the top-level `mcp_headers_helper = true` is set. Without it the server
does not connect and the notice names the key; the command is never started.
Project config reaches that key only through content-bound project trust, and a
project `.mcp.json` entry additionally needs its own file trust, so an untrusted
clone cannot cause a command to run. Forms: `"headersHelper": "./get-token.sh"`
(one program), an argument array `["./tok", "--audience", "x"]`, or a table
`{"command": [...], "inherit_env": ["TOKEN_SRC"], "sandbox": "<backend>"}`.
There is no shell and no `${VAR}` expansion: the first element is the program (a
path with a directory is taken relative to the workspace; a bare name searches
only absolute `PATH` entries). The helper starts with a cleared environment
(only `inherit_env` names are passed), may use the same workspace-service
`sandbox` backend as stdio servers, is stopped after 10 s, and must print a JSON
object of header names to string values (at most 64 KiB and 32 headers; framing
and hop-by-hop headers such as `Host` and `Content-Length` are refused). Its
headers override static `headers` of the same name. Failure notices carry the
exit status only, never its output. It runs on every (re)connect, not on each
request; command servers cannot use it.

URL servers require public addresses by default. Explicit `allow_local=true`
also permits loopback-only endpoints: IPv4 `127/8`, IPv6 `::1`, normalized mapped
loopback and DNS names resolving exclusively to loopback. DNS results remain
pinned. Mixed public/loopback sets, LAN/private addresses, link-local/metadata
addresses and wildcard listeners remain forbidden. Local clients bypass proxies.
MCP transport requests follow at most five same-origin redirects (same scheme,
host and port, so the pinned, policy-checked destination and the request's
credentials are unchanged): `307`/`308` replay the POST body, and GET streams
also follow `301`/`302`/`303`. Every other redirect (another host, port or
scheme, including private or metadata targets, or a method-changing POST
redirect) is refused, and the connection notice names the target as
`scheme://host:port/path` (no query) so the configured URL can be corrected.
OAuth may contact the exact captured local server
origin, while other authorization hosts retain the public-address policy.

Live edit, trust, reconnect, shutdown and OAuth callback stages are owned by the
App, bound to its session, retained workspace, source definition and operation
generation. Trust writes and service shutdown do not occupy the UI event loop.
`Ctrl-C` cancels and joins the current operation; an already saved edit can
remain configured. Final teardown also cancels and joins the captured operation
before closing its manager services; a prior retirement error still blocks new
admission and does not skip that final ownership attempt. OAuth waiting
owns its listener and accepted socket directly, with bounded request lines and
exact callback-state validation. Explicit login rechecks the captured source
before and after each guarded HTTP request, after DNS, on redirects, and before
returning response bytes; revocation during metadata discovery cannot authorize
a later registration or token exchange. Cancellation does not leave a detached
accept thread. Configuration approval creates no tool permission grant: ordinary MCP
tool invocation still passes its existing Ask, deny, hook and invocation checks.

Implementation ownership:

- [`project.rs`](../../src/extras/mcp/project.rs): bounded raw definitions,
  substitution, redacted review, retained file snapshots and private publication.
- [`health.rs`](../../src/extras/mcp/health.rs): bounded typed attempt generations;
  stale completions cannot relabel a newer connection or catalogue attempt.
- [`mcp_admin.rs`](../../src/ui/mcp_admin.rs): literal command parsing and owned
  operation/approval state, including settlement obstruction.
- [`mcp_admin_app.rs`](../../src/ui/mcp_admin_app.rs): actual App command,
  notification, trust and connection adoption paths.
- [`oauth.rs`](../../src/extras/mcp/oauth.rs) and
  [`egress.rs`](../../src/extras/mcp/egress.rs): owned callback lifecycle and
  captured endpoint policy. This exception does not change brokered JS fetch
  authority or worker containment.

Doctor's configured-server count uses the last retained effective MCP catalogue
after live add/remove, alongside its captured origins and observed attempts.
It does not replace startup provider/budget configuration, reload configuration,
or send an RPC to obtain that count.

Initial startup, eager prebuild and lazy tool registration use the same captured
source proof as live reconnect: the selected raw project digest and current
private trust, or the selected private global definition, must still match
before launch, after endpoint lookup and after the handshake. A changed or
revoked source cannot reuse an earlier loaded catalogue to start a server.
Private publication initializes only a newly created, empty, single-link
temporary file with the explicit current Windows user owner and protected DACL
before writing bytes; existing destinations retain strict validation.
The Windows reader compares the actual binary ACL trustees with the current
token SID and Local System. It requires a protected DACL with exactly two
uninherited full-file-access allow entries and rejects other trustees or rights.
SDDL account aliases in diagnostics do not confer authority; owner, regular-file
and single-link checks remain independent requirements.

On Windows, edits to an existing configuration currently fail closed because
the retained workspace writer does not support atomic compare-and-replace.
Creating a new private configuration is supported. Existing-file add/remove
support remains pending under `mini-agent-i7ro6.57.1`; the native temporary-file
ownership test does not establish a Windows add/remove round trip.

The Windows-only `fs::private::windows::operator_config_lease_probe` tests are
finite falsification experiments on private owned inert files. They measure
retained-relative exclusive opens, surviving mapped views, name/link/private
metadata races and reader-before-marker ordering. An experiment can pass by
observing a bypass and explicitly rejecting the candidate protocol before any
candidate write. These results do not implement configuration transactions,
crash recovery, trust approval or Windows existing-file add/remove support.
The ordinary workspace compare-and-replace restriction remains unchanged.

A further test-only retirement experiment uses a retained DELETE/read Share0
handle and no-replace native rename, with a full surviving read-only view after
both its creator and mapping handles close. It tests private and public inert
sources, preserves every old byte, and separately refuses destination collisions,
new hardlinks and changed private ACLs before any retirement. A specifically
reported mapped-object conflict rejects that attempted operation; other native
errors fail the test. A no-mapping positive rules out invalid helper access.
These controlled checks do not establish an atomic metadata lock or a production
transaction. Different application state overrides, bootstrap/feature-off reader
obstruction, exact journal discoverability and crash recovery remain unresolved.

A separate exact-rights control tests `GENERIC_READ | DELETE | READ_CONTROL`
with Share0, rather than inferring its behavior from the older writable lease.
It covers existing/new read, write and delete handles, actual unlink, and a
full surviving writable view after both creator handles close. If that view
is admitted, it must demonstrate changed leased inert bytes and explicitly
reject stable-content authority. Hardlink and protected-DACL changes before
and during the exact lease remain distinct observed metadata conflicts.
Every object is owned, inert and retired by retained file/view/root owners;
no product configuration, marker, journal, permission or fallback is changed.
The four prior native experiments remain unchanged. The added control requires
its own exact native Windows discovery/execution before any new kernel claim.

## Per-server timeouts

Command and URL server entries accept two optional timeout fields (seconds,
clamped to 1..=3600). Built-in registrations use the defaults.

| Field | Default | Bounds |
| --- | --- | --- |
| `startup_timeout_sec` | 10 s | Connect, OAuth restore/refresh and the `initialize` handshake. It also raises (never lowers) the server's `tools/list` budget, default 30 s. |
| `tool_timeout_sec` | `mcp_tool_timeout_secs` (120 s) | Idle timeout for one `tools/call` on this server; progress notifications restart it, up to an absolute 30 minute ceiling. In the interactive UI, progress is shown as a live preview (`progress p/total: message`, throttled to 10 per second, control characters removed, display-only) under the running call. |

```json
{"mcp_servers": {"slow": {"command": "npx", "args": ["-y", "some-mcp"],
  "startup_timeout_sec": 60, "tool_timeout_sec": 300}}}
```

## Per-server tool selection and enable toggle

Command and URL entries also accept:

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` keeps the entry configured but never launches or connects it (startup and explicit reconnect both refuse). |
| `enabled_tools` | omitted | Only these exact server tool names are registered. |
| `disabled_tools` | `[]` | These exact server tool names are never registered (applied after `enabled_tools`). |
| `parallel_read_only` | `false` | Opt in: this server's tools that advertise `readOnlyHint: true` share the parallel tool lane instead of the exclusive one, so several can run alongside reads and each other. A server's hint is its own claim, so it is ignored unless you set this. |

Selection runs on each `tools/list` page before the per-server tool cap (128
with `defer = false`, 1024 with `true`/`auto`), so the cap counts only admitted
tools and a selected tool past the cap's index is still reached. When the cap
omits tools, the startup notice counts them and names the first 16. `/mcp <server>` shows how many listed tools the selection excluded.
Selection only narrows the registered set; it grants no permission.

## Resource tools (`list_mcp_resources`, `read_mcp_resource`)

Servers that advertise resources get two model tools. The catalogue is
discovered at connection and bounded to 128 entries / 256 KiB of metadata. A
malformed, over-long or duplicate entry is skipped individually (the listing
reports how many were skipped and how many fell beyond the bound) rather than
discarding the whole catalogue. `list_mcp_resources` takes `server`, a
case-insensitive `query` over URI, name and description, and `offset`/`limit`
paging; a truncated listing names the `offset` that continues it.
`read_mcp_resource` returns up to 256 KiB per call for any textual MIME type
(the `web_fetch` set: `text/*`, JSON, XML, YAML, TOML, JavaScript, `+json`,
`+xml`, with parameters ignored). Longer text is cut at a character boundary
with a marker naming the `offset` to pass back (`length` is optional); at most
4 MiB of a resource is retained, and a server that sends more is flagged.
Continuation pages reuse the first read: the fetched text (at most four
resources) is cached for two minutes per session, so paging through a resource
costs one server request and one permission decision. Every page header shows a
`content_hash` (12 hex digits of SHA-256); pass it back with `offset`, and if
the resource changed the read restarts from byte 0 and says so (a fresh read at
offset 0 also reports a change since the previous one). Bytes beyond the 4 MiB
fetch bound stay unreachable and the header says so. Unscoped
`list_mcp_resources` paging is per server, so its `more matches` hint names
`server=<name>` (and the query/limit in use).
Binary parts and non-text MIME types are omitted with a note stating the part
size and MIME type, and a resource with nothing readable is refused with that
reason. Operator `@mcp:` attachments stay strict: text over 256 KiB is refused
with its size and the limit, since it must fit the prompt snapshot.

## Server instructions

The `instructions` text a server returns from `initialize` is added to the
system prompt (section "MCP server instructions", also with deferred tool
loading) as untrusted data: each server is cut at 2 KiB (visible
`[instructions truncated at 2048 bytes]` notice), all servers together at 8 KiB
(a notice counts the omitted ones), control characters are removed, the server
name is reduced to `[A-Za-z0-9._-]`, and a server's own closing tag is
neutralised so it cannot escape its block. The block states that it carries no
system, user or approval authority and cannot change permissions. It is omitted
when `--tools` restricts the tool set.

## MCP server mode (`--mcp-server`)

`mini-agent --mcp-server [flags]` serves mini-agent itself to another agent or
IDE over MCP on stdio (newline-delimited JSON-RPC: `initialize`, `ping`,
`tools/list`, `tools/call`). It is opt-in and off by default. It offers one
tool, `run` (`prompt`, at most 256 KiB), which launches this executable as
`mini-agent [flags] -p -- <prompt>` in the server's working directory and
returns the final answer (stdout, at most 1 MiB, with a truncation note). The
child therefore applies exactly the provider, model, sandbox, permission and
tool flags the server was started with, and the ordinary headless policy:
permission asks cannot be answered over MCP and fail closed unless those flags
allow the action. A non-zero exit returns `isError: true` with stdout and a
bounded stderr tail; a run is stopped after 30 minutes (`--mcp-server-timeout <seconds>`, 1-86400; the effective value is printed in the `run` tool description). On timeout the child gets SIGTERM and up to 12 s to save: the error then carries its stderr tail and, if it saved the session, the session id (also in `structuredContent`) so the work can be continued; a child that ignores SIGTERM is killed and reports no id. A `prompt` whose trimmed text starts with `!` is refused with `-32602`: in a headless child that prefix is the user-typed shell-command shortcut, which skips permission rules, so a remote prompt is never allowed to use it. Requests are read while
a run executes: `ping` is answered, `notifications/cancelled` for the call's
request id kills the child's whole process group and sends no reply (per the MCP
cancellation utility), and a `_meta.progressToken` on the call yields
`notifications/progress` heartbeats every 15 s. Up to 4 runs may overlap
(`--mcp-server-max-runs <N>`, 1-64); the next is refused with error `-32000`. A persisted run reports its session id
(`structuredContent.session_id` and a trailing `[session_id: …]` line); pass it
back as `session_id` to continue that conversation (`--session` on the child). Only exact ids this server returned for this workspace are accepted. The ids persist in a small ledger under the state directory (`mcp-server/sessions-<workspace hash>.json`, at most 4,096 ids, least recently used dropped first, using an id refreshes it), so a restarted server still resumes them; a prefix, a session name, or any other session, including conversations of other workspaces, is refused with `-32602` naming the rule. `initialize` answers the client's `protocolVersion` only when this server implements it (`2025-06-18`, `2025-03-26`, `2024-11-05`); any other version gets `2025-06-18`.
`--mcp-server-allow-model-override` (default off) adds a per-call `model`
argument; without it `model` is rejected. `--mcp-server-allow-cwd-override`
(default off) adds a per-call `cwd`: a directory inside the server's workspace
(relative to its root). The path is canonicalised first, so `..` traversal,
symlinks leaving the workspace, files and missing paths are refused; without
the flag `cwd` is rejected. The child runs in that directory with the server's
sandbox and permission flags unchanged (the sandbox is not narrowed to the
subdirectory; relative paths in forwarded flags then resolve from it). The server owns the child's
`--output`, so a forwarded `--output` is dropped. The server process itself runs no tools and opens no connections.
`--mcp-server-elicit-approvals` (default off) forwards a run's permission asks to the MCP client as `elicitation/create` requests, but only when the client advertised the `elicitation` capability at `initialize`. The child then runs with `--input-format stream-json --output stream-json --stream-json-control` (the server owns those flags) and each `can_use_tool` ask is shown to the client with the complete reviewed input and a boolean `allow` field. Only `action: accept` with `allow: true` approves that single action; decline, cancel, any other answer, an error, a closed connection, an input over 64 KiB or no answer within 300 s deny it. Without the flag, or with a client that does not advertise elicitation, asks fail closed as before and the ordinary one-shot JSON child is used. The flag adds no permission by itself: only actions that would otherwise ask reach the client, and `--mcp-server-elicit-approvals` is not forwarded to the child.
`--mcp-server` cannot be combined with `-p`, a prompt, `--acp` or `--loop`.

```json
{"mcpServers": {"mini-agent": {"command": "mini-agent",
  "args": ["--mcp-server", "--read-only"]}}}
```

## Captured source checks at tool dispatch

Each configured connection retains its captured workspace, selected server and
source proof. Its tools revalidate current source bytes and private trust before
permission evaluation and again after an approval wait, immediately before the
RPC. A changed or revoked source sends no tool call; reconnect captures the new
approved definition. These checks confer no permission grants and do not repair
private files. Explicit manually constructed originless clients retain their
existing behavior.

Each check is metadata-first: an origin file and the private trust store whose
retained identity, length, mtime, ctime, mode, owner and link count are
unchanged since their last successful full verification reuse that result.
Any metadata change, unavailable metadata, or a non-Unix platform forces the
full read, hash and parse, and a failed verification is never cached. Pending
MCP permission and question prompts revalidate their source once per second
instead of on every event-loop pass; prompt creation, key handling and the
reply paths keep the full check.

A connection keeps its last successful `tools/list` result, so agent rebuilds do
not page `tools/list` again; a reconnect is a new connection and failures are not
cached. A server's `notifications/tools/list_changed` marks that connection's
listing stale: the next tool collection lists it again, and the interactive
session rebuilds the agent before the next turn so its first model request
advertises the new set (deferred schemas, source-proof checks, tool selection
and the tool cap apply as for the first listing). If that relist fails, the
previous set is kept and a notice says so. The prompt and resource catalogue
(`/mcp:` commands, `@` resources, `list_mcp_resources`) is discovered after
connecting and again, without reconnecting, after the server's
`notifications/prompts/list_changed` or `notifications/resources/list_changed`:
the notification only marks it stale, and the next use (typing `/` or `@`, running an
`/mcp:` prompt, selecting a resource, or listing resources) re-runs the same bounded
discovery (same item and metadata limits) and replaces the catalogue. Until that refresh
finishes the previous entries are still offered, and picking one the server has removed is
refused. A failed relist keeps the previous entries of the failed kind and marks them
incomplete. Shutdown closes all servers concurrently, and a worktree
switch closes the previous binding's servers while the new binding connects.

## Deferred tool schemas

A server accepts `defer: true`, `defer: false`, or `defer: "auto"` (the default).
Automatic mode defers schemas when the total accepted MCP catalogue exceeds
32 tools. The explicit CLI tool ceiling is applied first. `false` preserves
eager loading; `true` defers even a small catalogue. Built-in servers use
automatic mode and retain their sealed connection identity.

```json
{"mcp_servers":{"example":{"url":"https://example.invalid/mcp","defer":"auto"}}}
```

On generic providers, the first model request receives `mcp_search` and a bounded
JSON names index, rather than every deferred schema. `mcp_search {"query":"tool name or words"}`
uses deterministic name/description matching over the captured catalogue. Each
query word matches independently, so one query may list several exact tool names.
Ranking is: the whole query equals a name; a query word equals a name; every word
is in the name; every word is in the name or description; then partial matches,
ordered by how many words matched, with names breaking ties. A result with no
matches carries a `hint` that names this rule, so an unmatched query is not read
as an unavailable server. It
returns at most eight names, descriptions and schemas, limited to 128 KiB total,
and publishes those exact schemas for subsequent model requests. Queries are
limited to 2 KiB and 32 words; invalid or oversized input fails explicitly. The
names index is limited to 64 KiB and labels omitted names; search still considers
the complete bounded catalogue. The 4 KiB description limit is unchanged; an input
schema over 16 KiB is reduced (examples, then descriptions/titles/defaults, are
removed) or, as a last resort, replaced by a permissive object schema with a
note in the description, instead of the tool being dropped.

Search changes no permissions. Enabled tools retain the original captured
client, ordinary Ask/deny policy, hook invocation identity, cost admission,
concurrency lane and structured result extensions. A model cannot call a hidden
tool in the same response as its search: the SDK validates calls against the
immutable definitions advertised for that response before executing siblings.
The next generic request advertises the newly enabled schema. Omitting `mcp_search`
from an explicit CLI tool ceiling keeps the admitted MCP tools eager; tools
outside that ceiling remain unavailable. Discovery never widens the ceiling.

Activation lasts across agent rebuilds within the same logical session,
workspace binding and unchanged catalogue and search eligibility. An eager,
empty, no-tools or search-ineligible rebuild retires earlier discovery state;
restoring a wider CLI ceiling does not resurrect old enabled schemas. New
sessions, imported/forked identities, changed catalogues and replaced workspaces
start without activation.
No activation or approval authority is serialized into a saved session.
A retired client or revoked selected configuration is refused before ordinary
Ask and again immediately before the actual RPC after an answer.

`/mcp` reports the configured loading mode and cached deferred/enabled counts
from the last retained discovery build without an RPC. `/mcp <server>` includes
that loading state alongside its ordinary tool list. These observations do not
prove current source authority or install grants. Retired build counts are
omitted; the manager keeps only a weak diagnostic reference.

Generic discovery changes a provider's tools prefix and makes no fixed tokenizer
savings claim. For supported native Anthropic Messages models, all captured MCP
definitions instead stay in an immutable top-level tools array with
`defer_loading: true`. Successful search projects bounded native `tool_reference`
blocks from typed host metadata; ordinary tool strings cannot create them.
Deferral reduces model context, not HTTP definition payload. Older or unknown
native model identifiers explicitly retain eager schemas and report that choice.
The current supported native families follow the official
[tool-search model contract](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool).

Native discovery validates references against the captured catalogue, refuses
raw model/system/messages overrides and conflicting raw tool names before HTTP,
and preserves ordinary CLI ceilings, hooks, Ask/deny and source/cost checks.
SDK result redaction or termination cannot force references into the next body.
Native loopback controls compare raw tools/system bytes across actual requests,
including unchanged manual and automatic one-hour cache markers, and complete signed
blocks; they do not verify signatures cryptographically or claim paid-server
acceptance. Broader saved application reasoning replay and unrelated prefix
changes remain under `mini-agent-i7ro6.46`. The deterministic builder estimator
measures reduction only for its controlled generic schema fixture.

[`deferred.rs`](../../src/extras/mcp/deferred.rs) owns bounded runtime activation,
matching and schema publication. Its tool-server binding is weak: a registered
search tool cannot retain its own server after its agent retires. Publication
uses the existing SDK tool-server write lock, with cancellation, source and
client admission checked after acquiring it, immediately before the atomic
batch. A cancelled lock wait or revoked candidate publishes neither schemas
nor activation names. No separate invocation registry, paid embeddings or
semantic retrieval service is introduced.

## Optional GitHub companion

The stdlib-only `tools/ghagent-mcp` companion runs outside the sandbox and exposes
allowlisted branch pushes, CI polling, and opt-in draft release creation. It is not
registered automatically. See [installation, permissions, and trust boundaries](../../tools/ghagent-mcp/README.md)
before configuring its token and repository allowlist.
