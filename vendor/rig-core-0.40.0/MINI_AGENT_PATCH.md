# mini-agent Rig patch

This is the crates.io `rig-core` 0.40.0 source pinned by the workspace
`[patch.crates-io]` entry.

mini-agent changes `completion::Usage` addition and addition-assignment to use
field-wise `saturating_add`. Rig aggregates multi-turn run usage internally
before it yields `CompletionCall` events; unchecked arithmetic there can panic
in debug builds or wrap in optimized builds before mini-agent's own accounting
ledger can observe the usage.

Remove this local patch only after the selected upstream Rig release provides
the same saturating guarantee and the integrated near-`u64::MAX` multi-turn
regression continues to pass against it.
