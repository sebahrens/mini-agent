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
bounded, one-pass literal substitution. A missing variable leaves that server
inactive and reports the variable name, without its value. Raw placeholders
remain on disk and in trust fingerprints; expanded credentials are not copied
to notices or origin metadata. Literal secrets explicitly placed in a project
file remain file content; review redaction does not make those bytes inaccessible
to another reader already authorized to read that file.

URL servers require public addresses by default. Explicit `allow_local=true`
also permits loopback-only endpoints: IPv4 `127/8`, IPv6 `::1`, normalized mapped
loopback and DNS names resolving exclusively to loopback. DNS results remain
pinned. Mixed public/loopback sets, LAN/private addresses, link-local/metadata
addresses and wildcard listeners remain forbidden. Local clients bypass proxies;
MCP redirects remain forbidden. OAuth may contact the exact captured local server
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

A connection keeps its first successful `tools/list` result, so agent rebuilds do
not page `tools/list` again; a reconnect is a
new connection, failures are not cached, and mini-agent does not subscribe to
`tools/list_changed`. Shutdown closes all servers concurrently, and a worktree
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
the complete bounded catalogue. Existing 4 KiB description/16 KiB schema limits
remain unchanged.

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
