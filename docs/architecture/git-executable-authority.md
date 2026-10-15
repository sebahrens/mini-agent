# Guarded host Git authority

`src/git/runtime.rs` owns the publisher-approved Git 2.56.0 executable payload,
its HTTP/HTTPS helpers, extraction, retained physical identities, and deny-only
model mutation protections. Build-generated constants bind every payload byte to
the application's build fingerprint and the pinned policy patch. A runtime
inventory file or PATH entry never grants host Git authority. Missing approval
refuses Git capabilities; it does not choose system Git for model operations.
The bundle/build interfaces are described in `../development/git-runtime-build.md`
and `../development/git-runtime-bootstrap.md`.

`src/git/global_snapshot.rs` captures operator global Git configuration before
configuration-driven project hooks, MCP, or model execution. It parses each
source using a fixed, read-only Git configuration command, follows the entire
bounded include graph regardless of current conditional selection, and retains
its bytes as parsed ordered entries. Missing sources stay absent. Limits are 128
source paths, 64 KiB per source, and 1 MiB over the captured graph. Changes by the
operator after startup require a restart. This ordering also prevents a later
model-held descriptor to an unlinked alias from changing trusted source bytes.
Hostile independent same-UID processes are outside this boundary.

When a Git command needs an empty global source, it explicitly selects
`GIT_CONFIG_GLOBAL=/dev/null`, including on Windows. The pinned MinGW Git
recognizes that spelling in both its access and file-open adapters. Uppercase
`NUL` is not interchangeable during those checks. The selector remains set so
HOME and XDG cannot become fallback global sources; a selected private mirror
still replaces it when evaluating captured conditional includes.

For each host operation, a private short-lived mirror contains only those
captured sources. Git evaluates its native `gitdir`, branch, and remote URL
conditions against the current repository. Relative include paths retain their
original source directory; relative `gitdir:./` conditions retain their original
canonical directory. Only GLOBAL entries originating from that mirror can enter
the ordered snapshot; LOCAL data never gains GLOBAL authority. The mirror is
used only by the bounded read-only parser query. Its removal on cancellation can
fail that query; the existing Sandbox process worker still owns child retirement.
No mutation command is launched following a failed query.

The actual Command captures the serialized configuration in its environment.
The patched parser loads these bytes as GLOBAL before LOCAL/WORKTREE, preserving
normal local data precedence. It does not reopen HOME, XDG, system configuration,
or includes during an operation. Its synthetic source marker cannot be supplied
by a repository file or config blob. Executable local/worktree key families are
rejected when consumed, including keys first added after command preparation.
Hooks and custom transport helpers remain disabled under the existing host-runner
baseline. Standard HTTP/HTTPS helpers resolve to the approved payload.

Simple credential-helper shorthand is resolved at startup from the original
Git exec-path and then the captured absolute PATH. The fixed `git --exec-path`
query is the sole use of the original Git program; it cannot dispatch a model
operation. Resolved helper executable names and target identities are retained,
including standalone aliases. `store`/`cache` use the approved Git builtins.
Explicit global `!` commands and absolute helper commands retain their operator
semantics. Complex quoted shorthand must be expressed as an absolute command or
an explicit `!` command; an unsupported selected shorthand fails visibly.

On Windows, selected shell-dependent global services retain the original
Git-for-Windows installation's `usr/bin/sh.exe` identity and its physical
`usr/bin` directory, including adjacent runtime DLLs. That entire directory is
outside all model write grants and protected from direct file writes and later
grants. The exact validated directory precedes ambient PATH for those services;
the guarded UCRT64 child disables Git's implicit HOME/bin PATH insertion while
preserving actual HOME. No shell is recovered through a later PATH search.
Missing shell authority refuses the selected service; ordinary local operations
and plain native SSH/askpass executable paths do not require a shell.
`src/git/windows_runtime_tests.rs` owns native loopback HTTP acceptance for
explicit `!` and rewritten `store` helpers with Git/cmd-only PATH and spaces in
repository, runtime, and credential paths, plus service-authority refusal tests.

Existing Sandbox clones retain the actual workspace, cache, and Windows extra
write-root capabilities through existing process retirement. A weak registry
checks their current physical identities and rejects grants overlapping retained
runtime directories or resolved shorthand services. Built-in Write/Edit/Notebook
and JS parent file authorization also reject protected executable targets,
including permission-free or unrestricted file modes. Ordinary file writes
remain available. Registration failures, poisoned state, resource limits, stale
identities, and excessive serialized environments fail closed; configuration is
never truncated and diagnostics do not include configuration values.

The parser policy is pinned to the audited Git source and the fixed host
GitRunner domain. Explicit user-trusted shell bypasses and arbitrary Git invoked
inside its authorized process sandbox retain their existing semantics. Implicit
Git child processes inherit the parent operation's selected snapshot; existing
network recursion is disabled. Each later explicit host GitRunner operation,
including a nested repository or worktree, evaluates conditions anew.

## Acceptance status

The implementation remains private until the runtime bundle is available on all
supported release/developer targets and the original owning native Linux
Sandbox-to-host-Git absent/late/nested tests pass. A default unprivileged local
Linux container currently refuses user-namespace creation; no privileged or
sysctl workaround is part of this change. Passing parser or macOS controls does
not substitute for that Linux acceptance requirement.

Windows C Git paths use an equivalent ordinary DOS/UNC spelling only at the
parser/serialized-include boundary. Rust keeps its original canonical paths and
identity checks. Conversion checks the existing object (or the same existing
parent for a missing final include), and refuses device namespaces, reserved
names, trailing dots/spaces and other ambiguous Win32 spellings. NTFS protection
remains enabled. Native origin records must match the exact translated mirror
paths supplied to Git; arbitrary returned paths are never canonicalized into
source authority.

## Scoped native Windows acceptance (2026-10-15)

The enforced Git boundary now satisfies its remaining Windows runtime controls.
The final production path correction is `a66c2947`; `d0c66726` changes only the
HTTP fixture's credential-store filename to the checked equivalent spelling.

| Control | Native evidence | Result |
| --- | --- | --- |
| Real HTTP credentials, shell and store-shorthand modes, cmd-only PATH and spaces | [run 38728057227, job 116398700712](https://github.com/sebahrens/mini-agent/actions/runs/38728057227/job/116398700712) | Complete owning loop: 1 passed, 0 failed, 0 ignored; 0.95 s |
| Same-object Windows path conversion and ambiguous/device-name refusal; missing-shell refusal only for selected services | [run 38725998347, job 116391890600](https://github.com/sebahrens/mini-agent/actions/runs/38725998347/job/116391890600) | Two unchanged passing controls reused |
| Exclusive private-directory creation; shell overlap, later grants and replacement refusal | [run 38714148392, job 116352605112](https://github.com/sebahrens/mini-agent/actions/runs/38714148392/job/116352605112) | Two unchanged passing controls reused |

These are five controls across scoped runs. The earlier three-test run's HTTP
store fixture failed on an unsupported verbatim filename; the corrected full
HTTP loop passes above. The production path conversion retains canonical Rust
authority, checks file identity, and keeps NTFS protection enabled. Its separate
[20-observation diagnosis](../reviews/2026-10-15-windows-git-parser-path-diagnosis.json)
records the preceding direct C experiments and their limits.

Root verified the final native output and full log hashes, exact named test,
zero-ignore summary, successful job steps and source blobs. Final output SHA-256:
`650da585a4d7376e29d39a1f1349472ba7bfb3e20247ee8d946501e08405c07b`.
The native observer was a workflow-only child of `d0c66726`; its exact head was
`4c55a98131fab8dc3b69335550a2818c32e20190`. The matching earlier path-identity
and missing-shell output SHA-256 was
`59b36179a955ede574a4fa3e61e9195237e762578b9c860d52912d1055a0a5a8`.

This acceptance reuses the prior Linux attack-refusal and credential-service
evidence. It does not claim a fresh all-platform suite or green aggregate CI.
