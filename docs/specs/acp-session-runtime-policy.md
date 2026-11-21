# ACP session runtime policy

ACP mode changes and session approval choices belong to the loaded session's
runtime history owner. The global CLI/configuration, sandbox, tool eligibility,
captured spend profile and other sessions remain independent. Mode changes are
accepted through the typed `session/set_mode` request only while that session
has no active generation; prompt text cannot select this policy.

Each turn rebuilds current configured permissions, applies its selected mode,
then loads only this runtime owner's bounded choices. AllowAlways retains the
exact advertised scopes. RejectAlways records the exact permission input as an
ordinary deny rule, with literal wildcard characters, normal path normalization
and existing case-folding deny semantics. Deny precedence applies to tools,
walkers, aliases and hooks. Choices are limited to 128 rules and 64 KiB per
session, with an 8 KiB individual rule bound. Capacity refusal denies the
current request explicitly. A cancelled or undeliverable reply creates no
retained approval. These choices are not serialized or imported from sessions.

The same session owns canonical persistence, bounded list/load, available commands,
multimodal input and editor resume. Native history imports use the shared bounded
native replay contract, without restoring instruction receipts, grants or authority
from provider labels.

ACP relays the session's real `TodoStore` as `Plan` updates when it changes,
including an empty replacement. It checks after tool results and runner
settlement, including the final owned drain on cancellation, so rejected writes never install a plan from their arguments or
output. Priority and supported status values map to ACP v1. A cancelled item
remains pending with `mini-agent-status: cancelled` metadata because ACP v1
has no cancelled status. Restored legacy stores are bounded to the ordinary
50-item / 500-character tool limits in client updates. Other sessions retain
their own stores.

Each ACP state owns one bounded session-identity lease set. Reservation counts
pending imports and retiring turns against the 64-session capacity without
holding the asynchronous session-map lock across disk work. The history owner
retains its lease through its active turn: removing a session from the protocol
map cannot admit a second writer for that identity until the final owner drops.
Failed admission releases its identity and capacity immediately. This owner is
process-local; ordinary private session storage and cross-process locks remain
the durable write contract.

Native Anthropic turns establish their retained publication barrier before configured
SessionStart hooks, MCP connection or agent preparation. They bind the actual typed
agent and canonical feed, pair the exact paused producer, and commit ordered user
content only after that actor and its effects retire. ACP uses its JSON-RPC event
sink throughout; this lifecycle never writes headless print output to stdout.
Publication failures keep the context refused rather than recreating a fresh owner.
Saved native thinking/signatures remain opaque data and are excluded from editor
replay notifications. Local `/undo`, `/compact` and `/rename` work on a
native-bearing session: the command changes the transcript or name, drops a thinking chain the
new transcript no longer matches, and republishes the session as a new epoch
(`native_publication::republish_idle`: the old value is denied on disk first, then the
healthy payload replaces it); an unfinished native context refuses with its own error and the
saved bytes stay unchanged. Creating or clearing a goal still refuses on such a session
(`/goal status` works), naming the command and the next step. Automatic and manual native
compaction restart the chain the same way.

The native fake-wire fixture checks completed signed block bytes/order and unchanged
system/tools across same-process turns and process restart at a physical 1 MiB stack.
It executes one permitted read and verifies its live lifecycle call/result pair,
the canonical model call/result IDs in saved and replayed history, and the real
provider follow-up containing the read result. Usage accumulates across all four
requests. Live lifecycle IDs and persisted model IDs remain separate identities.
Its inert signatures exercise retention and request construction, not a claim that a
paid provider accepted the signatures. The unchanged generic fixture covers ordered
images, retry identity, private save/load, literal commands and cumulative usage.

## Authentication, rewind, additional directories and shell output (2026-11-21)

- **Auth methods** (mini-agent-nvnkx.109): `initialize` lists a `credentials` agent method and, for a
  client that advertised `auth.terminal`, terminal methods `login-openai` and `login-copilot` whose
  args are `--login <provider>`; the client runs them in its own terminal. The login flows print to
  stdout, the protocol channel here, so the server never runs them in-process. `authenticate` accepts
  only listed method ids and returns success when the default provider's credential resolves, else
  `auth_required`. `session/new` returns `auth_required` (data: reason and method ids) when a client that
  advertised `auth.terminal` has no credential for the default provider; other clients keep the failure at
  the first prompt. This needs the `unstable_auth_methods` feature of `agent-client-protocol`.
- **`/rewind [n]` and `/redo`** (mini-agent-nvnkx.107): `/rewind` lists the user turns newest first;
  `/rewind n` restores the files recorded since the n-th most recent turn from the session's file
  checkpoints, then removes that turn and later ones from the conversation; a refused restore (changed
  file, unrestorable change, imported checkpoints) leaves the conversation untouched. `/redo` restores the
  conversation only. Both follow the single-text-block authority rule and use the `/undo` native-session
  republish path; `n` must be digits 1-9999, anything else is a -32602 error. `/context`, `/mcp`, `/diff`,
  `/init` and `/fork` are not offered over ACP yet.
- **Shell output and client terminals** (mini-agent-nvnkx.110, decision): commands keep running in
  mini-agent's own sandbox; ACP `terminal/create` is not used, because moving execution to the client
  would bypass the sandbox profile and permission policy. The result update of a `shell` call carries
  `rawOutput` `{kind:"shell", status, workdir, output}` so a client can render a console. An opt-in client
  terminal key was considered and not built; do not reopen it as a defect.
- **`additionalDirectories`** (mini-agent-nvnkx.111): `session/new` and `session/load` accept up to 16
  absolute existing directories when `add_dir_tool_access = true`, canonicalised and added to the
  session's `--add-dir` list for every turn (permission rules and denies still decide each access).
  Without the key the request fails with -32602 naming it.
