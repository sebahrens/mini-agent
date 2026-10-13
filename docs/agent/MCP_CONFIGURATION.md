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
`.mcp.json`, trusted `.zerostack/config.toml`, then built-in injection. A project
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

## Captured source checks at tool dispatch

Each configured connection retains its captured workspace, selected server and
source proof. Its tools revalidate current source bytes and private trust before
permission evaluation and again after an approval wait, immediately before the
RPC. A changed or revoked source sends no tool call; reconnect captures the new
approved definition. These checks confer no permission grants and do not repair
private files. Explicit manually constructed originless clients retain their
existing behavior.
