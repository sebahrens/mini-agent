# ADR: Tauri as a Product Surface for Mini Agent

**Date**: 2026-08-15
**Status**: Proposed
**Deciders**: Engineering lead

---

## Context

Mini Agent currently ships as a CLI/TUI tool with archive distribution and is integrating into VS Code via ACP. Adding a Tauri desktop shell is an optional scope extension. This ADR records the go/no-go decision, the supported OS/architecture/installer matrix if approved, and the consequences for dependent implementation work.

## Alternatives Considered

1. **Keep CLI/TUI + package-manager channels only** — no GUI, no additional signing or WebView dependency. Simplest path. Editor integration (ACP/VS Code) covers the discovery surface.
2. **Prioritize editor integrations (ACP/VS Code) without a desktop GUI** — VS Code extension delivers AI assistance where developers already work. Avoids WebView dependency, update policy, and second signing identity.
3. **Tauri v2 shell with ACP sidecar** — Tauri renders a web UI; mini-agent runs as a child process connected via ACP stdio. Separates UI lifecycle from core crate.
4. **Native GUI by linking agent internals directly** — rejected: couples UI release cadence to core crate, duplicates lifecycle and security authority, requires rewriting containment boundary in a second language.

## Decision

**No-go at this time.**

### Rationale

The VS Code extension (ny65 epic) already delivers the highest-value user experience: mini-agent runs where developers write code, inside the editor that has file context. A standalone desktop GUI duplicates this without adding workflow value for the current user base.

The concrete costs of a Tauri surface:

| Cost | Detail |
|---|---|
| WebView dependency | WKWebView (macOS), WebView2 (Windows), WebKitGTK (Linux) — each has known quirks and version fragility |
| Second signing identity | macOS notarization + Windows EV certificate, separate from CLI archives |
| Update policy | Must define an update mechanism (Tauri updater or package-manager) orthogonal to the CLI update story |
| CI matrix expansion | Per-OS installer jobs on top of existing cross-platform binary jobs |
| Security authority | A desktop GUI with file-system access is a second trust surface; containment rules must be re-validated |

None of these costs produce measurable user value until:
- CLI/TUI UX has stabilized
- ACP/VS Code integration is shipped and validated
- Evidence exists that users need a standalone GUI (e.g., non-developer users, onboarding workflows, or offline batch jobs)

### Success Criteria (to revisit this decision)

A Tauri surface should be reconsidered when **all three** are true:

1. VS Code integration is GA and has active users
2. A concrete user workflow is identified that requires a desktop GUI (not addressable by CLI or editor integration)
3. A maintainer volunteers to own the WebView dependency, signing identities, and update policy

### Consequences

- `mini-agent-o78r.2` (Scaffold Tauri vertical slice) and its children are closed with this rationale.
- Distribution focus remains: CLI archives, Homebrew, Cargo install, Windows installer via cargo-wix.
- The ACP sidecar architecture described in alternative 3 remains valid if the decision is revisited; no implementation work is lost.

## Supported Installer Matrix (for reference if decision is reversed)

| Platform | Installer | Signable | Priority |
|---|---|---|---|
| macOS arm64 | `.dmg` | Yes (notarize) | P1 if go |
| macOS x86_64 | `.dmg` | Yes (notarize) | P2 |
| Linux x86_64 | `.deb` + AppImage | deb: no; AppImage: no | P1 if go |
| Linux arm64 | AppImage | No | P3 |
| Windows x86_64 | NSIS `.exe` | Yes (EV cert) | P1 if go |
| Windows arm64 | NSIS `.exe` | Yes (EV cert) | P3 — requires explicit demand |
| Windows x86_64 | `.msi` | Yes (EV cert) | P3 — requires explicit demand |

MSI and Windows ARM require explicit user demand before being added to the matrix.
