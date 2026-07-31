# mini-agent / JS Engine Integration

Research and implementation workspace for a minimal coding agent with a bounded embedded
JavaScript engine.

## Repository layout

| Path | Purpose |
|------|---------|
| `src/` | Production mini-agent source |
| `src/extras/js/` | Phase 1 JS engine integration |
| `spike/` | QuickJS proof-of-concept research |
| `docs/specs/` | Normative phased JS specifications and superseded research |
| `ARCHITECTURE.md` | Non-normative architecture overview |
| `SPEC.md` | Non-normative implementation overview |

The old nested `zerostack/` layout was flattened. Production source and the workspace
`Cargo.toml` are at the repository root.

## Canonical executable

The Cargo package, installed CLI, and every release archive use the executable name
`mini-agent`. Target-specific full and lite archives are named
`mini-agent[-lite]-<target>.tar.gz`, and each archive contains exactly one executable at
`mini-agent`.

## Documentation authority

Start with the [normative specification index](docs/specs/00-index.md). It defines corpus
authority, feature relationships, phase dependencies, and exit semantics.

- [Foundation: paths and persistent storage](docs/specs/platform-paths.md)
- [Phase 1: core JS engine](docs/specs/phase-1-js-engine.md)
- [Phase 2: sandbox hardening](docs/specs/phase-2-sandbox.md)
- [Phase 3: skill library](docs/specs/phase-3-skill-library.md)
- [Phase 4: agent proposals and human-gated admission](docs/specs/phase-4-auto-admission.md)
- [Phase 5: evidence-based self-learning](docs/specs/phase-5-evidence-learning.md)

[ARCHITECTURE.md](ARCHITECTURE.md) and [SPEC.md](SPEC.md) are maintained overviews only. The
[dated JS blueprint](docs/specs/2026-07-27-js-engine-blueprint.md) is a superseded research
artifact retained for history and must not guide implementation.

## Core boundaries

- One dedicated 8 MiB OS thread per `JsTool`; QuickJS state stays on that thread.
- A fresh runtime for every step, with hard heap/stack limits, an interrupt deadline, and bounded
  pending-job drain.
- File globals always use secure target resolution and mandatory permissions.
- Process spawn always uses the existing permission policy and `Sandbox::wrap_command`.
- VM isolation is distinct from child-process isolation; Windows process isolation is not
  delivered by Phases 1 or 2.
- Learned-skill identity covers the full versioned execution/discovery payload, including ordered
  tests; only exact JavaScript boolean `true` passes a test.
- Phase 4 requires human approval into non-retrievable canary state. Phase 5 owns the limited,
  evidence-based automatic lifecycle.

## Linux subprocess sandbox

Sandboxing is opt-in (`--sandbox` or `sandbox = true`) and fail-closed. Without it, subprocesses
inherit the host filesystem, environment, devices, process namespaces, and network. With the
default `bwrap` backend on Linux, mini-agent applies this capability matrix:

| Capability | Enforced Linux `bwrap` policy |
|------------|---------------------------------|
| Filesystem reads | Current workspace, mini-agent's application cache, explicit system runtime roots (`/usr`, `/bin`, `/sbin`, `/lib*`, `/nix`), `/etc/localtime`, `/etc/ld.so.cache`, and kernel/system metadata exposed by the new `/proc` |
| Filesystem writes | Current workspace, mini-agent's application cache, and a private ephemeral `/tmp`; the remaining sandbox root and runtime mounts are read-only |
| Processes | Separate user, PID, IPC, UTS, and cgroup namespaces |
| Devices | A minimal synthetic `/dev`; host devices are not mounted |
| Environment | Cleared and rebuilt from `PATH`, identity, shell, terminal, locale, and color variables; credential, agent-socket, display, and API-key variables are not forwarded |
| Network | Separate IP network namespace with no host or external IP connectivity, including no access to host loopback listeners; filesystem Unix-domain sockets placed in the workspace or application cache remain reachable |

Only the workspace path selected by the process working directory is exposed; running mini-agent
from a broad or sensitive directory broadens that boundary. The sandbox does not provide seccomp,
CPU/memory quotas, filtering of Unix-domain sockets inside writable bind mounts, or confidentiality
from host kernel metadata. Only a root-owned, non-group/world-writable `bwrap` executable beneath
equally protected parent directories is trusted. If it is missing or any namespace or mount setup
fails, the subprocess does not run. The optional `zerobox` backend has
backend-defined read/process/device/environment/network behavior and is not reported as providing
the Linux `bwrap` guarantee.

Use `mini-agent --sandbox --sandbox-backend bwrap --print-config` to see the configured backend,
availability, and effective capability report. Permission-gated in-process HTTP fetches are a
separate boundary; subprocess network isolation does not bypass or replace fetch permissions.

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| Foundation | Typed paths, ownership, migration, platform security | In progress |
| 1 | Core JS engine, host globals, permissions, process wrapper | In progress |
| 2 | `fetch`, narrowing allow-lists, Linux/macOS process isolation | Planned |
| 3 | Immutable learned skills and prompt-time retrieval | Planned |
| 4 | Agent proposals, held-out evaluation, human-gated canary | Planned |
| 5 | Evidence, promotion, quarantine, repair, rollback | Planned |

## Development commands

From the repository root:

```bash
cargo fmt
cargo test --features js
cargo install --path . --debug
```

Do not use `cargo build`, `cargo check`, or development `--release` builds.
