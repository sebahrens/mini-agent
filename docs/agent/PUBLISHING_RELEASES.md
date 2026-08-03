---
description: "How zerostack releases are published: crates.io, Homebrew, AUR, Conda, Nix packages, and the release workflow."
---

# Publishing Releases

This guide covers the full release workflow: bumping the version, tagging, publishing to crates.io, and updating downstream package managers.

## Canonical executable and archive layout

Cargo and every package channel install the public executable as `mini-agent`. Full archives are
named `mini-agent-<target>.tar.gz`; lite archives are named
`mini-agent-lite-<target>.tar.gz`. Every archive contains exactly one top-level executable named
`mini-agent`, which the release workflow extracts into a clean directory and runs with
`--version` before upload.

## Prerequisites

- [just](https://github.com/casey/just) command runner
- `cargo publish` access — run `cargo login` once to authenticate with crates.io
- `gh` CLI (only needed for `post-release` checksum downloads)
- `makepkg` (only needed for AUR `.SRCINFO` regeneration)
- Ruby with its standard Psych YAML parser (used by the release workflow policy check)

## Release workflow dependency pins

Every remote action in `.github/workflows/release.yml` is pinned to an immutable, reviewed
40-character commit SHA and carries its human-readable version in a trailing comment. Dependabot's
root `github-actions` updater advances the SHA and version comment together. The offline
`APPROVED_RELEASE_ACTIONS` map in `scripts/check-package-metadata.py` then requires a maintainer to
record the reviewed action, version, and SHA triple before the update can pass CI. Before changing
an action manually, verify that the proposed version tag resolves to the pinned commit and review
the upstream commit; never copy an unverified SHA from an issue or pull request.

The package metadata policy enforces both the release pins and the Dependabot configuration:

```bash
python3 -m unittest scripts.tests.test_check_package_metadata
python3 scripts/check-package-metadata.py
```

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

This downloads the release artifacts and updates SHA256 checksums in:

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
| `just aur-regen-srcinfo` | Regenerate `.SRCINFO` from `PKGBUILD` |
