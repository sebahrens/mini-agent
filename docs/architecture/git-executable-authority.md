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
