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
- [ ] Default and `js`-only behavior deny JS file access until roots or explicit unrestricted
      opt-ins are configured; non-file JS behavior remains unchanged.
- [x] Linux `bwrap` filesystem, namespace, device, environment, and network policy is explicit,
      capability-reported, fail-closed, and covered by real-backend CI probes.

## Out of scope for Phase 2

- Windows process isolation and Windows child-process lifecycle enforcement
- UI for editing allow-lists
- portable/learned skill libraries (Phase 3)
- proposal/admission or evidence lifecycle (Phases 4–5)
