# Phase 2 — Sandbox Hardening

- **Document role**: normative phase specification
- **Specification version**: 1.2.0
- **Delivery status**: delivered
- **Owner**: mini-agent maintainers
- **Last reconciled**: 2026-08-02
- **Entry dependency**: Phase 1 complete
- **Exit dependency**: every acceptance criterion below and every Phase 2 blocker

The corpus authority and conflict rules are defined in
[`00-index.md`](00-index.md). Phase 2 extends Phase 1; it does not weaken Phase 1 permissions,
resource bounds, secure file resolution, or `Sandbox::wrap_command` routing.

[`phase-6-brokered-js-runtime.md`](phase-6-brokered-js-runtime.md) supersedes Phase 2 only for the
native JavaScript worker and the placement of JS host effects. This phase remains authoritative for
the general subprocess path, parent-brokered command containment, `fetch()` validation, and
file/URL narrowing. The workspace-visible profiles below are forbidden for the JS worker.

## Overview

Phase 2 adds:

1. permission-gated `fetch(url, opts?)` with URL allow-lists and bounded responses;
2. read/write path allow-lists that can only narrow Phase 1 file authorization; and
3. effective general child-process isolation on Linux and macOS through the shared `Sandbox`
   abstraction.

Phase 2 does not deliver Windows isolation for the general subprocess path. JavaScript VM limits
are not a substitute for child-process isolation on any platform.

## Cargo.toml additions

The features are independent. The checked-in Linux `bwrap` and macOS `seatbelt` backends use
trusted system executables and therefore need no Rust dependency:

```toml
[features]
sandbox = []
```

- `sandbox` without `js` extends the shared process sandbox and must compile.
- `js` without `sandbox` retains the Phase 1 wrapper and permission behavior.
- `js,sandbox` adds the Phase 2 JS integrations.
- `skills` does not implicitly enable `sandbox`.

Cargo features express compiled capabilities, not proof that an OS backend is installed or
effective. Runtime diagnostics continue to report the actual backend state.

### reqwest note

The repository has one `reqwest` dependency. Enable its blocking client feature on that existing
entry; do not add another version. Historically, `fetch()` ran from the Phase 1 JS thread; Phase 6
supersedes that placement with parent-broker execution. All waits retain finite deadlines and
cancellation.

## Target files

| Concern | Location |
|---------|----------|
| Shared general-process isolation (not the Phase 6 JS worker) | `src/sandbox.rs` |
| Fetch and file allow-lists | `src/extras/js/host.rs` |
| Phase 2 feature/dependencies | `Cargo.toml` |
| Configuration schema | existing typed config modules |
| Integration tests | colocated tests and `src/extras/js/tests/` |

## `fetch()` host global

`fetch(url, opts?)` is registered only for the Phase 2 feature combination. It:

1. parses and normalizes one absolute URL with the same parser used by the transport;
2. rejects credentials, fragments, ambiguous hosts, and schemes other than HTTPS (or HTTP when
   `js-fetch-allow-http = true`);
3. checks `js-fetch-origins` as an exact-origin narrowing policy when configured;
4. resolves the host under a three-second deadline and rejects the whole answer set if any IPv4,
   IPv6, or mapped address is loopback, private, link-local, multicast, unspecified, reserved,
   documentation-only, transition-only, or metadata-capable;
5. obtains `js/fetch` permission for the normalized URL and sorted exact socket-address set;
6. binds the connection to that authorized resolution result; and
7. returns `{status, text}` or a typed JS error.

`js-fetch-origins` entries contain only a scheme, host, and optional non-default port, for example
`["https://docs.rs", "https://api.example.com:8443"]`. An empty or malformed configured list denies
all fetches. When the setting is absent, the origin narrowing layer is unrestricted but the
mandatory permission check remains. An unmatched `js/fetch` permission is `Ask` in interactive
standard/guarded modes and fails closed without an approval channel. Read-only and plan-write
modes deny it by default. Explicit `permission`, `permission-allow`, `permission-ask`, and
`permission-deny` rules can target `js/fetch`.

Options support `method` (`GET` or `POST`), a string-to-string `headers` object, and a UTF-8
`body` for POST. Host, framing, proxy, credential, forwarding, and content-encoding headers are
controlled by the host and cannot be supplied from JavaScript.

Automatic redirects are disabled. Up to five redirects are followed manually, and every target
repeats scheme, credential, origin, DNS/address, permission, and connection-binding checks before
redirected I/O. This also prevents DNS answer changes between authorization and connect from
rebinding an approved hostname to a denied address. Non-GET requests are never replayed after a
redirect. A redirect cannot carry caller-supplied headers to a different origin.

The blocking transport disables automatic redirects, ambient proxy discovery, connection pooling,
and transparent response decompression. Its default connect and per-read timeouts are three
seconds, with a separate ten-second wall-clock deadline across the full streamed response. Request
headers are limited to 64 fields/16 KiB and request bodies to 256 KiB; response headers are limited
to 128 fields/64 KiB and response bodies to 1 MiB. Header and body limits are checked incrementally
with overflow-safe accounting. Bodies must be UTF-8. Any `Content-Encoding` other than `identity`
is rejected, so compressed bytes are never expanded into an unbounded hidden representation.
Cancellation is checked before the request, after response headers, and between streamed reads.

The host never exposes ambient `fetch`, a general socket API, or an authorization path independent
of the existing permission service.

System DNS resolution has a process-wide four-lookup concurrency budget. A timed-out or cancelled
caller returns promptly, but its lookup retains that budget slot until the operating-system
resolver actually finishes. Waiting callers share the same three-second deadline and cannot start
additional operating-system resolver work. A late DNS result is discarded and can never advance
to permission or destination I/O. The blocking resolver operation itself owns the slot, so Tokio
runtime shutdown cannot release capacity early. Capacity becomes reusable only when the underlying
lookup completes.

## File allow-list

Configuration supplies separate directory roots through `js-read-roots` and `js-write-roots`.
Relative roots resolve against `js-file-base-dir`; when that setting is absent, the base is the
startup workspace directory captured while the agent is built. A relative `js-file-base-dir`
itself resolves against that same startup directory. The base and every root are normalized once
to absolute canonical directories, so later process-CWD changes cannot alter the policy.

An absent, empty, malformed, nonexistent, or internally ambiguous root list denies that access
class. Unrestricted access is available only through the explicit `js-read-unrestricted = true`
or `js-write-unrestricted = true` opt-in. Supplying roots and the corresponding unrestricted
setting together is ambiguous and denies access. Read and write policy remain independent, and
neither form bypasses the mandatory Phase 1 permission check.

Containment uses `Path` components against the same canonical/resolved UTF-8 target used by Phase 1
permission checks. It never uses raw string prefixes or glob matching, so a `/safe` root does not
match `/safe-evil`. Reads follow a final symlink only when its canonical target remains within an
allowed root; dangling links are invalid. Writes reject final symlinks. For a nonexistent write
target, authorization canonicalizes the nearest existing directory and validates every remaining
normal path component. A symlinked parent is therefore judged by its canonical destination.

The decision order is:

1. securely resolve the target without reading content or mutating;
2. obtain a typed allow-list authorization decision for the resolved target and reject denials;
3. obtain the mandatory Phase 1 permission for the exact resolved target; and
4. perform the Phase 1 stable read or atomic no-follow write.

The allow-list helper authorizes only; it does not perform file I/O. Final reads and writes
separately revalidate the target or parent identity immediately before operating. Allow-list
failure, permission denial, races, timeout, and I/O errors have no read/write effect.

## General subprocess integration

Phase 2 extends the existing `Sandbox` implementation for general commands rather than creating a
JS-only command path. Model-authored `spawn()` reaches `Sandbox::wrap_command` through the parent
capability broker. The wrapper selects and
configures the effective general-process backend. It must not be used to launch the Phase 6 worker,
whose broker-only containment has no workspace/cache visibility and fails closed independently.

| Platform | Phase 2 general-process guarantee |
|----------|---------------------------|
| Linux | Effective configured isolation using the supported Linux backend, verified by escape/denial tests |
| macOS | Seatbelt denies network and writes outside the workspace/cache/temp boundary; host-readable files, devices, and process namespaces are explicitly not claimed as isolated |
| Windows | No Phase 2 process-isolation guarantee; execution is disabled or explicitly reported as non-isolated according to product policy |

Backend absence or setup failure never masquerades as isolation. Whether fallback execution is
allowed is an explicit user/product policy decision for the general subprocess path and remains
visible to the caller. It is never permission for an uncontained JS worker fallback. Phase 2 does
not claim Job Objects, AppContainer, `rappct`, or Windows child termination.

The implementation must not add a parallel raw `std::process::Command` path for JS. Any blocking
adapter remains behind the shared wrapper and preserves Phase 1 permission, argument, timeout,
cancellation, and output bounds.

### Linux general-process `bwrap` capability matrix

The default Linux general-subprocess policy is opt-in (`--sandbox` or `sandbox = true`). When disabled,
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

General-subprocess networking and Phase 2's historically in-process `fetch()` global are separate
capabilities. Phase 6 supersedes that placement by keeping `fetch()` in the parent capability
broker. The bwrap general-subprocess policy always denies networking. A permission-approved
`fetch()` remains subject to its URL validation, allow-list, redirect, deadline, and response
bounds; it does not grant network access to spawned processes or to the JS worker.

### macOS general-process `seatbelt` capability matrix

macOS defaults to the fixed `/usr/bin/sandbox-exec` backend. The executable and every parent
directory must be root-owned and not group/world-writable. The generated profile denies by
default, allows child processes, allows host-readable files, permits writes only below the
canonical workspace, canonical application cache, `/private/tmp`, and `/dev/null`, and denies all
Seatbelt network operations. The child starts through `/usr/bin/env -i`; only the same
non-credential environment allow-list as Linux is restored and `TMPDIR` is fixed to
`/private/tmp`.

Seatbelt does not provide a filesystem or process namespace. Accordingly, Phase 2 does not claim
read confidentiality, device isolation, a private temporary directory, or process-namespace
isolation on macOS. It does enforce the stated write and network boundaries, and all descendants
inherit the profile. Backend absence, profile application failure, or child setup failure starts
no requested child and is never retried unsandboxed.

## Windows behavior

Phase 2's in-process QuickJS placement is historical and superseded by Phase 6; Phase 2 did not
make the full action primitive secure or release-ready on Windows. Phase 6 separately implements a
zero-capability LPAC worker with a creation-time Job Object. That worker boundary contains only
QuickJS evaluation and does not confer authority on a brokered child command. Parent-brokered JS
`spawn` therefore remains disabled on Windows until the general command path has its own normative
backend, complete descendant-lifetime ownership, CI, and release gate.

## Acceptance criteria

- [x] `sandbox`, `js`, and `js,sandbox` feature combinations compile and retain their documented
      relationships.
- [x] `fetch()` validates URLs, rechecks redirects, enforces bounds/deadlines/cancellation, applies
      the narrowing allow-list, and always obtains `js/fetch` permission.
- [x] File allow-lists match resolved targets and never bypass Phase 1 permissions or secure I/O.
- [x] Linux and macOS process escape/denial tests prove their documented backend guarantees.
- [x] Backend absence/failure and Windows non-isolation are visible and never reported as
      sandboxed.
- [x] The general command created for JS `spawn` still uses the one shared
      `Sandbox::wrap_command` path; this is not the Phase 6 worker-launch path.
- [x] Default and `js`-only behavior deny JS file access until roots or explicit unrestricted
      opt-ins are configured; non-file JS behavior remains unchanged.
- [x] Linux `bwrap` filesystem, namespace, device, environment, and network policy is explicit,
      capability-reported, fail-closed, and covered by real-backend CI probes.

## Out of scope for Phase 2

- Windows general-process isolation and Windows child-process lifecycle enforcement
- broker-only JS worker containment (Phase 6)
- UI for editing allow-lists
- portable/learned skill libraries (Phase 3)
- proposal/admission or evidence lifecycle (Phases 4–5)
