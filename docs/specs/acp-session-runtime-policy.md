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
replay notifications. Local `/undo`, `/compact` and `/rename` refuse changes to a
native-bearing session without mutating its saved bytes; goal mutations also refuse
while goal status remains read-only. Start a new ACP session to change that context. Automatic native compaction also refuses before HTTP.

The native fake-wire fixture checks completed signed block bytes/order and unchanged
system/tools across same-process turns and process restart at a physical 1 MiB stack.
It executes one permitted read and verifies its live lifecycle call/result pair,
the canonical model call/result IDs in saved and replayed history, and the real
provider follow-up containing the read result. Usage accumulates across all four
requests. Live lifecycle IDs and persisted model IDs remain separate identities.
Its inert signatures exercise retention and request construction, not a claim that a
paid provider accepted the signatures. The unchanged generic fixture covers ordered
images, retry identity, private save/load, literal commands and cumulative usage.
