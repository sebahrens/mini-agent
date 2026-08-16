---
description: "How mini-agent releases are published: GitHub, Homebrew, AUR, Conda, and the release workflow."
---

# Publishing Releases

This guide covers the full release workflow: bumping the version, tagging, publishing GPL-compliant
GitHub assets, and updating downstream package managers.

## Canonical executable and archive layout

Cargo and every package channel install the public executable as `mini-agent`. Full archives are
named `mini-agent-<target>.tar.gz`; lite archives are named
`mini-agent-lite-<target>.tar.gz`. Every binary archive contains exactly four top-level files:
`mini-agent` (or `mini-agent.exe`), `LICENSE`, `NOTICE`, and `SOURCE.md`. The release workflow checks
that exact payload, extracts it into a clean directory, and runs the executable with `--version`
before upload. Full archives use the supported default Cargo feature set; lite archives
use `--no-default-features`. Opt-in native features such as `skills-embed` are not silently bundled
into cross-platform archives and keep their platform-specific installation requirements.

Every release also includes five platform VSIX candidates, the dual-purpose
`mini-agent-windows-x64.msi`, their checksum manifests, and
`mini-agent-vX.Y.Z-source.tar.gz`. This Corresponding Source archive is
made from the exact tagged commit and adds the complete locked Cargo dependency graph under
`vendor/` plus a generated `.cargo/config.toml`. CI validates that Cargo can resolve the bundle with
`--locked --offline`, includes the source archive in `SHA256SUMS`, and publishes it in the same
GitHub release as the binaries. Never delete a source asset while any matching binary asset remains
available.

## Product identity matrix

| Category | Canonical value | Compatibility policy |
|---|---|---|
| Cargo package, CLI/UI, provider identity, ACP agent, MCP OAuth, LSP client | `mini-agent` | Public identity; do not report `zerostack` to new integrations. |
| Source repository and release origin | `sebahrens/mini-agent` | All active download, homepage, source, and checksum URLs use this repository. |
| Release assets | binary archives plus `mini-agent-vX.Y.Z-source.tar.gz` | Binary archives carry the executable, GPL text, modification notice, and source directions; the same release carries vendored Corresponding Source. |
| AUR, Conda, and Homebrew recipe names | `zerostack-bin`, `zerostack`, and `zerostack.rb` | Retained only as package-channel compatibility names; each installs `mini-agent`. |
| Persisted data, project policy, and hook environment | `zerostack`, `.zerostack`, and `ZEROSTACK_*` | Stable user-data compatibility contract; release-coordinate changes must not migrate or rename it. |

## Supported distribution surfaces

Supported package channels are source/Cargo, AUR, Conda, and Homebrew. Their status is
deliberately explicit:

| Surface | Support status |
|---|---|
| Source/Cargo | Install only from this repository checkout. The crates.io `mini-agent` package is unrelated and must never be advertised or published by this project. |
| GitHub release archives and shell installer | Supported only after the exact-version archive and `SHA256SUMS` smoke passes against the public canonical repository. |
| Windows MSI | Supported x86-64 dual-purpose installer. Defaults to a no-admin per-user install, supports `ALLUSERS=1` enterprise deployment, and side-loads the bundled VSIX when VS Code exists in the installing account. |
| Native VS Code VSIX | Supported local-install candidates for five release targets; Marketplace/Open VSX publication remains a separate deferred step. |
| AUR and Conda | Repository-maintained recipes; publication remains the manual downstream step described below. |
| Homebrew | Compatibility formula retained, but no end-user install command is supported until a canonical tap exists and its archive smoke passes. |

Nix packaging is intentionally unsupported. The former impure, unpinned package, overlay, and
development-shell entry points were removed rather than presented as a working install channel.
Restoring Nix support requires pinned inputs, Linux and macOS CI, default-feature parity, and a
smoke test of the exact store output before any install claim returns.

## Prerequisites

- [just](https://github.com/casey/just) command runner
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

Static musl builds also pin the `cross` CLI version and both cross-rs container images by digest in
`Cross.toml`. Update those digests only after reviewing the upstream image definition and resolving
the intended published tag to its platform-specific immutable digest.

The package metadata policy enforces both the release pins and the Dependabot configuration:

```bash
python3 -m unittest scripts.tests.test_check_package_metadata
python3 scripts/check-package-metadata.py
python3 scripts/smoke-package-compliance.py \
  --channel aur --channel conda-bin --channel conda-source --channel homebrew
```

The package-compliance smoke is offline. It first verifies the exact canonical GPL-3.0-only
`LICENSE` digest, then executes the checked-in AUR and Conda install scripts
with controlled command shims, executes the Homebrew formula's `install` method through a minimal
Ruby DSL harness, and compares the staged `LICENSE`, `NOTICE`, and `SOURCE.md` bytes with the
repository originals. The Conda source smoke also executes the recipe's declared binary checks and
proves the generated third-party license inventory reaches `${PREFIX}/THIRDPARTY.yml`. CI runs
Linux-only recipes on Ubuntu and the Homebrew formula on macOS.

## Quick start

```bash
just release patch   # 1.7.1 -> 1.7.2
just release minor   # 1.7.1 -> 1.8.0
just release major   # 1.7.1 -> 2.0.0
```

This single command creates and pushes the release tag. After CI finishes building the release
binaries and Corresponding Source, run `just post-release` to update packaging checksums.

## What `just release` does

1. Verifies the working tree is clean
2. Bumps the version in `Cargo.toml`
3. Syncs the new version to `Cargo.lock` and all packaging files (AUR, conda, Homebrew)
4. Commits as `bump to vX.Y.Z` and pushes the current branch
5. Validates that the tag is exactly `vX.Y.Z` (or `vX.Y.Z-prerelease`) and matches the Cargo package version
6. Creates and pushes an annotated tag — this triggers the [GitHub Actions release workflow](../../.github/workflows/release.yml), which builds binaries for all targets
7. Leaves crates.io untouched because its `mini-agent` package belongs to an unrelated project

Both local tag commands require all tracked working-tree and staged changes to be committed, so
the metadata they validate is the metadata in the commit they tag.

The release workflow accepts only pushed `v*` tags. Its first job rejects a non-tag ref,
a malformed tag, or a tag whose version differs from the root Cargo package version before any
release binary is built. Manual branch dispatch is intentionally disabled, so a branch name can
never become a public release identity. Tags containing a prerelease suffix (for example,
`v2.0.0-rc.1`) remain GitHub prereleases.

If a tagged run needs recovery, use **Re-run jobs** on that tag's existing Actions run. Do not
start the release workflow from a branch. Publication still happens only after every expected full
and lite archive, the tag-matched Corresponding Source archive, five platform VSIX candidates, the
Windows MSI, and all three checksum manifests have been assembled and checked. The MSI job uses
pinned WiX 6.0.2, performs a quiet per-user install, runs the installed binary, and uninstalls it
before upload.

## GPL release checklist

Before treating a release as complete, verify that:

- every binary archive has only the executable, `LICENSE`, `NOTICE`, and `SOURCE.md`;
- `NOTICE` identifies the imported ZeroStack commit and the date mini-agent modifications began;
- the same release contains the source asset named by `SOURCE.md`;
- the source asset contains the tagged tree, locked vendored dependencies, and offline Cargo config;
- `SHA256SUMS` covers all binary and source archives;
- `VSIX_SHA256SUMS` and `MSI_SHA256SUMS` cover the editor and Windows installer artifacts;
- the MSI installs `LICENSE.txt`, `NOTICE.txt`, and `SOURCE.md` beside its binary and VSIX; and
- the shell installer and downstream recipes install `NOTICE` and `SOURCE.md` alongside the GPL text.

For an older noncompliant release, attach its exact vendored source bundle, standalone compliance
documents, and a prominent release-note correction before leaving its binary assets available.

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
| `just add-tag` | Validate, tag, and push the current Cargo version (no version bump) |
| `just remove-tag [VERSION]` | Delete a local + remote tag (interactive picker if omitted) |
| `just aur-checksums` | Update AUR checksums only |
| `just conda-source-sha256` | Update conda source tarball checksum only |
| `just conda-bin-checksums` | Update conda binary checksums only |
| `just homebrew-checksums` | Update Homebrew checksums only |
| `just release-checksums` | Download all exact-version inputs, then update every package checksum |
| `just aur-regen-srcinfo` | Regenerate `.SRCINFO` from `PKGBUILD` |
