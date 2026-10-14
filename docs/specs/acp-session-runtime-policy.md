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

This runtime policy is one part of the ACP session parity work. Persistence,
bounded list/load, available commands, multimodal input and
editor resume remain separate required parts of that bead. Native history
imports must use the shared bounded native replay contract, without restoring
instruction receipts, grants or authority from provider labels.

ACP relays the session's real `TodoStore` as `Plan` updates when it changes,
including an empty replacement. It checks after tool results and runner
settlement, including the final owned drain on cancellation, so rejected writes never install a plan from their arguments or
output. Priority and supported status values map to ACP v1. A cancelled item
remains pending with `mini-agent-status: cancelled` metadata because ACP v1
has no cancelled status. Restored legacy stores are bounded to the ordinary
50-item / 500-character tool limits in client updates. Other sessions retain
their own stores.
