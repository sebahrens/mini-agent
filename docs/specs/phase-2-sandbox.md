# Phase 2 — Sandbox Hardening

- **Document role**: normative phase specification
- **Specification version**: 1.0.0
- **Delivery status**: planned
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-07-29
- **Entry dependency**: Phase 1 complete
- **Exit dependency**: every acceptance criterion below and every Phase 2 blocker

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). Phase 2 extends Phase 1; it does not weaken Phase 1 permissions,
resource bounds, secure file resolution, or `Sandbox::wrap_command` routing.

## Overview

Phase 2 adds:

1. permission-gated `fetch(url, opts?)` with URL allow-lists and bounded responses;
2. read/write path allow-lists that can only narrow Phase 1 file authorization; and
3. effective child-process isolation on Linux and macOS through the shared `Sandbox` abstraction.

Phase 2 does not deliver Windows process isolation. JavaScript VM limits are not a substitute for
child-process isolation on any platform.

## Cargo.toml additions

The features are independent. The checked-in Linux `bwrap` backend uses the system executable and
therefore currently needs no Rust dependency:

```toml
[features]
sandbox = []
```

A later macOS/birdcage implementation may extend that feature without changing the relationship:

```toml
[features]
sandbox = ["dep:birdcage"]

[dependencies]
birdcage = { version = "0.7", optional = true }
```

- `sandbox` without `js` extends the shared process sandbox and must compile.
- `js` without `sandbox` retains the Phase 1 wrapper and permission behavior.
- `js,sandbox` adds the Phase 2 JS integrations.
- `skills` does not implicitly enable `sandbox`.

Cargo features express compiled capabilities, not proof that an OS backend is installed or
effective. Runtime diagnostics continue to report the actual backend state.

### reqwest note

The repository has one `reqwest` dependency. Enable its blocking client feature on that existing
entry; do not add another version. `fetch()` runs from the dedicated JS thread but all waits still
have finite deadlines and cancellation.

## Target files

| Concern | Location |
|---------|----------|
| Shared process isolation | `src/sandbox.rs` |
| Fetch and file allow-lists | `src/extras/js/host.rs` |
| Phase 2 feature/dependencies | `Cargo.toml` |
| Configuration schema | existing typed config modules |
| Integration tests | colocated tests and `src/extras/js/tests/` |

## `fetch()` host global

`fetch(url, opts?)` is registered only for the Phase 2 feature combination. It:

1. parses and normalizes an absolute HTTP(S) URL;
2. checks a configured URL allow-list as a narrowing policy;
3. always obtains `js/fetch` permission for the normalized URL;
4. applies a finite request deadline and cancellation; and
5. returns `{status, text}` or a typed JS error.

When an allow-list exists, an unmatched URL is rejected before I/O. Without an additional
allow-list restriction, an unknown URL follows the normal permission policy: interactive `Ask`
when available and fail-closed in non-interactive mode. Redirects are disabled or each target
repeats normalization, allow-list, and permission checks before redirected I/O.

The host never exposes ambient `fetch`, a general socket API, or an authorization path independent
of the existing permission service.

## File allow-list

Configuration supplies separate read and write patterns. Matching occurs against the same
canonical/resolved UTF-8 target used by Phase 1 permission checks, using component-aware semantics.
It never matches the caller’s unresolved spelling.

The decision order is:

1. securely resolve the target without reading content or mutating;
2. reject when the applicable allow-list is configured and does not match;
3. obtain the mandatory Phase 1 permission for the exact resolved target; and
4. perform the Phase 1 stable read or atomic no-follow write.

An absent allow-list means “no additional Phase 2 restriction,” not “no permission required.”
Allow-list failure, permission denial, races, timeout, and I/O errors have no read/write effect.

## birdcage integration

Phase 2 extends the existing `Sandbox` implementation rather than creating a JS-only subprocess
path. `spawn()` continues to use `Sandbox::wrap_command`; the wrapper selects and configures the
effective backend.

| Platform | Phase 2 process guarantee |
|----------|---------------------------|
| Linux | Effective configured isolation using the supported Linux backend, verified by escape/denial tests |
| macOS | Effective Seatbelt isolation while the supported backend is available, verified by escape/denial tests |
| Windows | No Phase 2 process-isolation guarantee; execution is disabled or explicitly reported as non-isolated according to product policy |

Backend absence or setup failure never masquerades as isolation. Whether fallback execution is
allowed is an explicit user/product policy decision and remains visible to the caller. Phase 2
does not claim Job Objects, AppContainer, `rappct`, or Windows child termination.

The implementation must not add a parallel raw `std::process::Command` path for JS. Any blocking
adapter remains behind the shared wrapper and preserves Phase 1 permission, argument, timeout,
cancellation, and output bounds.

### Linux `bwrap` capability matrix

The default Linux subprocess policy is opt-in (`--sandbox` or `sandbox = true`). When disabled,
the wrapper intentionally inherits host capabilities and reports that state. When enabled with
the default `bwrap` backend, the following matrix is normative:

| Capability | Enforced policy |
|------------|-----------------|
| Filesystem reads | The canonical current workspace, mini-agent's canonical application cache, explicit read-only runtime roots (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`, `/lib64`, `/nix` when present), `/etc/localtime`, `/etc/ld.so.cache`, and kernel/system metadata exposed by the new `/proc` |
| Filesystem writes | The workspace and application cache bind mounts plus a private ephemeral `/tmp`; the remaining sandbox root and runtime mounts are read-only |
| Process namespace | Separate user, PID, IPC, UTS, and cgroup namespaces |
| Devices | A synthetic minimal `/dev`; host device trees are not mounted |
| Environment | `--clearenv`, followed only by `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TERM`, `LANG`, `LC_ALL`, `COLORTERM`, and `NO_COLOR`; `TMPDIR` is fixed to `/tmp` |
| Network | A separate IP network namespace; host interfaces, host loopback listeners, and external IP networks are unreachable. Filesystem Unix-domain sockets inside the workspace or application-cache bind remain reachable |

The working directory is the workspace boundary. The wrapper refuses filesystem root as a
workspace or application-cache root, but a user who starts mini-agent in another broad directory
has intentionally selected that directory as visible and writable. Runtime mounts remain readable
and therefore are not confidentiality boundaries for their contents.

The wrapper does not claim seccomp syscall filtering, CPU/memory quotas, Unix-domain socket
filtering inside writable binds, confidentiality for kernel/system metadata exposed through
`/proc`, protection from the host kernel, or Windows isolation. The `zerobox` backend currently
requests workspace writes but its read/process/device/environment/network behavior is
backend-defined; mini-agent must not report it as satisfying the Linux `bwrap` matrix.

The selected `bwrap` executable and every parent directory must be root-owned and not
group/world-writable; a workspace- or user-controlled PATH entry is never trusted as the security
backend. Backend absence and every current-directory, cache-directory, namespace, mount, device,
or child setup error fail closed. No command is retried outside the backend. Capability reporting
must distinguish disabled, requested-and-available, and requested-but-unavailable states and must
list the actual bwrap flags/mount policy above.

Subprocess networking and the in-process `fetch()` global are separate capabilities. The bwrap
subprocess policy always denies networking. A permission-approved `fetch()` runs in the parent
host implementation and remains subject to its URL validation, allow-list, redirect, deadline,
and response bounds; it does not grant network access to spawned processes.

## Windows behavior

The in-process QuickJS engine may compile and run on Windows, but Phase 2 does not make the full
action primitive secure or release-ready there. No document may describe Windows spawn as
sandboxed until a later normative specification defines the backend, lifecycle/termination
semantics, ACL interactions, CI, and release gate.

## Acceptance criteria

- [ ] `sandbox`, `js`, and `js,sandbox` feature combinations compile and retain their documented
      relationships.
- [ ] `fetch()` validates URLs, rechecks redirects, enforces bounds/deadlines/cancellation, applies
      the narrowing allow-list, and always obtains `js/fetch` permission.
- [ ] File allow-lists match resolved targets and never bypass Phase 1 permissions or secure I/O.
- [ ] Linux and macOS process escape/denial tests prove an effective backend before Phase 2 closes.
- [ ] Backend absence/failure and Windows non-isolation are visible and never reported as
      sandboxed.
- [ ] JS process spawn still uses the one shared `Sandbox::wrap_command` path.
- [ ] Default and `js`-only behavior remain unchanged except for explicitly fixed Phase 1 defects.
- [x] Linux `bwrap` filesystem, namespace, device, environment, and network policy is explicit,
      capability-reported, fail-closed, and covered by real-backend CI probes.

## Out of scope for Phase 2

- Windows process isolation and Windows child-process lifecycle enforcement
- UI for editing allow-lists
- portable/learned skill libraries (Phase 3)
- proposal/admission or evidence lifecycle (Phases 4–5)
