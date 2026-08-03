---
description: "How mini-agent releases are published: crates.io, Homebrew, AUR, Conda, and the release workflow."
---

# Publishing Releases

This guide covers the full release workflow: bumping the version, tagging, publishing to crates.io, and updating downstream package managers.

## Canonical executable and archive layout

Cargo and every package channel install the public executable as `mini-agent`. Full archives are
named `mini-agent-<target>.tar.gz`; lite archives are named
`mini-agent-lite-<target>.tar.gz`. Every archive contains exactly one top-level executable named
`mini-agent`, which the release workflow extracts into a clean directory and runs with
`--version` before upload.

## Product identity matrix

| Category | Canonical value | Compatibility policy |
|---|---|---|
| Cargo package, CLI/UI, provider identity, ACP agent, MCP OAuth, LSP client | `mini-agent` | Public identity; do not report `zerostack` to new integrations. |
| Source repository and release origin | `sebahrens/mini-agent` | All active download, homepage, source, and checksum URLs use this repository. |
| Release assets | `mini-agent-<target>.tar.gz` and `mini-agent-lite-<target>.tar.gz` | Archive contents contain the `mini-agent` executable. |
| AUR, Conda, and Homebrew recipe names | `zerostack-bin`, `zerostack`, and `zerostack.rb` | Retained only as package-channel compatibility names; each installs `mini-agent`. |
| Persisted data, project policy, and hook environment | `zerostack`, `.zerostack`, and `ZEROSTACK_*` | Stable user-data compatibility contract; release-coordinate changes must not migrate or rename it. |

## Supported distribution surfaces

Supported package channels are Cargo/crates.io, AUR, Conda, and Homebrew. Their status is
deliberately explicit:

| Surface | Support status |
|---|---|
| Cargo/crates.io | Published package and canonical `mini-agent` executable. |
| GitHub release archives and shell installer | Supported only after the exact-version archive and `SHA256SUMS` smoke passes against the public canonical repository. |
| AUR and Conda | Repository-maintained recipes; publication remains the manual downstream step described below. |
| Homebrew | Compatibility formula retained, but no end-user install command is supported until a canonical tap exists and its archive smoke passes. |

Nix packaging is intentionally unsupported. The former impure, unpinned package, overlay, and
development-shell entry points were removed rather than presented as a working install channel.
Restoring Nix support requires pinned inputs, Linux and macOS CI, default-feature parity, and a
smoke test of the exact store output before any install claim returns.

## Prerequisites

- [just](https://github.com/casey/just) command runner
- `cargo publish` access — run `cargo login` once to authenticate with crates.io
- `gh` CLI (only needed for `post-release` checksum downloads)
- `makepkg` (only needed for AUR `.SRCINFO` regeneration)

## Quick start

```bash
just release patch   # 1.7.1 -> 1.7.2
just release minor   # 1.7.1 -> 1.8.0
just release major   # 1.7.1 -> 2.0.0
```

This single command handles everything up to crates.io publication. After CI finishes building the release binaries, run `just post-release` to update packaging checksums.

## What `just release` does

1. Verifies the working tree is clean
2. Bumps the version in `Cargo.toml`
3. Syncs the new version to all packaging files (AUR, conda, Homebrew)
4. Commits as `bump to vX.Y.Z` and pushes the current branch
5. Creates and pushes an annotated tag `vX.Y.Z` — this triggers the [GitHub Actions release workflow](../.github/workflows/release.yml), which builds binaries for all targets
6. Runs `cargo publish` to publish the crate to crates.io

## Post-release (after CI completes)

Once the GitHub Actions release workflow has finished and all binary assets are attached to the release:

```bash
just post-release
```

Before publishing coordinate or installer changes, exercise the exact checked-in installer against
the canonical repository release pinned by the current Cargo version:

```bash
bash scripts/smoke-canonical-installer.sh
```

This gate is expected to fail closed when `sebahrens/mini-agent` has no complete release containing
the platform archive and `SHA256SUMS`; do not publish or close the coordinate change in that state.

The smoke and checksum updater fail on HTTP errors before changing package metadata. The updater
downloads all required artifacts for the exact Cargo version before it writes any checksum. It
then updates SHA256 checksums in:

- `packaging/aur/PKGBUILD`
- `packaging/conda/zerostack/meta.yaml` (source tarball)
- `packaging/conda/zerostack-bin/meta.yaml` (prebuilt binaries)
- `packaging/homebrew/zerostack.rb`

It also regenerates `packaging/aur/.SRCINFO`.

### Publishing to package registries

After `post-release`, commit the checksum updates and publish manually:

| Registry | Command |
|----------|---------|
| AUR | `cd packaging/aur && pkgctl aur publish zerostack-bin` |
| conda-forge | Submit a PR to `conda-forge/staged-recipes` |
| Homebrew | Push `packaging/homebrew/zerostack.rb` to the homebrew-tap repo |

Do not publish Homebrew install instructions until a canonical `sebahrens` tap exists and the
formula has passed its archive smoke there.

## Standalone commands

These are useful for partial workflows or recovery:

| Command | Purpose |
|---------|---------|
| `just sync-version` | Sync `Cargo.toml` version to packaging files (no commit) |
| `just pre-release` | Same as `sync-version` (alias used by `release`) |
| `just add-tag` | Tag the current version and push (no version bump) |
| `just remove-tag [VERSION]` | Delete a local + remote tag (interactive picker if omitted) |
| `just aur-checksums` | Update AUR checksums only |
| `just conda-source-sha256` | Update conda source tarball checksum only |
| `just conda-bin-checksums` | Update conda binary checksums only |
| `just homebrew-checksums` | Update Homebrew checksums only |
| `just release-checksums` | Download all exact-version inputs, then update every package checksum |
| `just aur-regen-srcinfo` | Regenerate `.SRCINFO` from `PKGBUILD` |
