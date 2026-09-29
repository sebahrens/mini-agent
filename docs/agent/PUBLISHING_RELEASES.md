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
before upload. A second clean-runner gate downloads each exact private archive on its native
platform. Full archives must pass the offline `--js-runtime-check` (`1 + 1` evaluates to `2`) on a
validated containment host. The Intel archive runs on GitHub's macOS 15 runner, where the normative
contract deliberately keeps containment unavailable, so that row instead proves the JS diagnostic
exists and fails closed with the expected unavailable diagnostic; the artifact can execute JS on a
validated macOS 26 Intel host. Lite archives must reject the feature-specific diagnostic entirely.
Full archives use the supported default Cargo feature set; lite archives use
`--no-default-features`. Opt-in native features such as `skills-embed` are not silently bundled into
cross-platform archives and keep their platform-specific installation requirements. Linux
archive-smoke runners install Bubblewrap and enable the hosted runner's
unprivileged-user-namespace boundary before executing the production runtime check; omitting either
prerequisite must fail the release metadata policy check.

The manually dispatched `Windows release archive smoke` workflow is the non-publishing audit path
for the Windows default-feature archive. It builds the documented target, transfers the exact
private archive to a clean Windows runner, verifies its version and offline JS evaluation, produces
and verifies `SHA256SUMS`, and retains the smoke-verified candidate for inspection. It has read-only
repository permission and contains no release-publication job. Before extraction, the smoke gate
removes inherited access from its per-user install directory and grants inheritable control only to
the runner identity, SYSTEM, and Administrators. The candidate process uses that private directory
as its `LOCALAPPDATA` root, giving the downloaded executable the same trusted-ancestor shape
required by the production AppContainer launcher without changing the shared hosted-runner profile
or weakening the launcher for CI.

Every release also includes five platform VSIX candidates, the dual-purpose
`mini-agent-windows-x64.msi`, their checksum manifests, and
`mini-agent-vX.Y.Z-source.tar.gz`. This Corresponding Source archive is
made from the exact tagged commit and adds the complete locked Cargo dependency graph under
`vendor/` plus a generated `.cargo/config.toml`, and every npm tarball pinned by
`editors/vscode/package-lock.json` under `vendor-npm/` with a `npm-sources.json` manifest of their
lockfile integrity hashes (the VSIX's `dist/extension.js` bundles npm dependencies). The packager
needs the pinned Node and npm (`editors/vscode/.nvmrc`, `packageManager`) and fails if npm is
missing. CI validates that Cargo can resolve the bundle with `--locked --offline`, verifies every
npm tarball against the lockfile and rebuilds `dist/extension.js` with `npm ci --offline`, writes
the archive reproducibly (sorted entries, the tagged commit's timestamp), includes the source
archive in `SHA256SUMS`, and publishes it in the same GitHub release as the binaries. Never delete a source asset while any matching binary asset remains
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

Linux sandbox CI, packaged-runtime CI, and native release smoke tests install bubblewrap through
`scripts/install-ci-bubblewrap.sh`. It scopes both APT refresh and installation to the hosted
Ubuntu image's `/etc/apt/sources.list.d/ubuntu.sources`, excluding unrelated vendor repositories
without changing package authentication. A missing source file, refresh/install failure, or failed
`bwrap --version` stops the job. Custom CI images can pass an absolute Ubuntu source-file path to
the helper. The hosted image's source-file layout is documented in its
[APT configuration script](https://github.com/actions/runner-images/blob/main/images/ubuntu/scripts/build/configure-apt.sh).

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

Before running the release command, update the matching version section in `CHANGELOG.md`. Confirm
that it calls out breaking changes, security exceptions, manual follow-up, and any benchmark state
that still lacks evidence. The release workflow extracts that section and uses it as the GitHub
release body, so the heading must match the Cargo version.

1. Verifies the working tree is clean
2. Bumps the version in `Cargo.toml`
3. Syncs the new version to `Cargo.lock`, the packaging recipes (AUR, conda, Homebrew), the VS Code
   extension manifest and lockfile, `editors/vscode/SOURCE.md`, `packaging/windows/README.md`, and
   `docs/acp-registry.json`. Because a version change invalidates every previously recorded release
   digest, the sync also replaces each recipe's artifact `sha256` with the placeholder
   `0000…0000` (64 zeros); only the version-independent GPL `LICENSE` digest is preserved.
4. Validates the package metadata and runs `cargo test --locked` on the bumped tree
5. Commits as `bump to vX.Y.Z` and pushes the current branch
6. Validates that the tag is exactly `vX.Y.Z` (or `vX.Y.Z-prerelease`) and matches the Cargo package version
7. Creates and pushes an annotated tag — this triggers the [GitHub Actions release workflow](../../.github/workflows/release.yml), which builds binaries for all targets once CI has passed for the tagged commit
8. Leaves crates.io untouched because its `mini-agent` package belongs to an unrelated project

Both local tag commands require all tracked working-tree and staged changes to be committed, so
the metadata they validate is the metadata in the commit they tag. Both also run
`cargo test --locked` before anything is pushed or tagged; `just add-tag` re-checks that the tree
is still clean after the tests.

## CI gate before release

A release is built and published only from a commit CI has passed:

- `.github/workflows/ci.yml` ends with the `ci-success` aggregate job. It needs every other CI job,
  runs even when one of them failed, and passes only when each dependency succeeded or was skipped
  by design: a documentation-only change skips the build, lint, and test jobs. When code changed,
  every other job must succeed on every event (including `harness-regression`, which runs on pushes
  as well as pull requests); `failure` and `cancelled` are never accepted. `scripts/ci_success.py` owns that decision.
  Adding a CI job without adding it to `ci-success` fails the workflow policy tests.
- The release workflow's first job, `verify-ci`, runs before any build. It fails unless the tagged
  commit is an ancestor of `origin/main`, then waits (up to 150 minutes, polling every 30 seconds)
  for the CI run that the same tag push started and requires that run's `ci-success` check run
  to conclude `success` (`scripts/verify_release_ci.py`). A `ci-success` from a `main` push is not
  accepted in its place, because that run may have skipped the matrix for a documentation-only
  diff; tag builds always run the full matrix. Every other release job depends on `verify-ci`.
- `publish-release` runs in the `release` deployment environment, so the environment's protection
  rules are the last gate before assets become public.

If `verify-ci` fails because CI failed, fix the cause (or re-run the flaky CI jobs on the tag's CI
run), wait for `ci-success` to pass, and then use **Re-run failed jobs** on the tag's release run.

### Repository settings (manual, owner only)

Two protections live in repository settings rather than in the workflow files. An administrator
applies them once; automated agents are not permitted to change repository rules.

1. **Require `ci-success` on `main`.** Settings → Rules → Rulesets → New branch ruleset: name
   `Require CI`, enforcement *Active*, target *Default branch*, enable *Require status checks to
   pass* and add the check `ci-success` from the *GitHub Actions* app. Equivalently:

   ```bash
   gh api -X POST repos/sebahrens/mini-agent/rulesets --input - <<'EOF'
   {
     "name": "Require CI",
     "target": "branch",
     "enforcement": "active",
     "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
     "bypass_actors": [{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}],
     "rules": [{
       "type": "required_status_checks",
       "parameters": {
         "strict_required_status_checks_policy": false,
         "do_not_enforce_on_create": false,
         "required_status_checks": [{"context": "ci-success", "integration_id": 15368}]
       }
     }]
   }
   EOF
   ```

   `integration_id` 15368 is the GitHub Actions app. The repository-admin bypass keeps the
   existing direct-push release flow (`just release` pushes the version bump to `main`) working;
   the check then binds pull requests from everyone else, and the release workflow's `verify-ci`
   job still refuses to publish an admin-pushed commit that CI has not passed. Remove the bypass
   actor only when every change, including the release bump, lands through a pull request.
   Confirm with `gh api repos/sebahrens/mini-agent/rules/branches/main`, which must list a
   `required_status_checks` rule naming `ci-success`. The existing `Blocking` ruleset (id
   20623676) applies to every branch, so do not add the check there.
2. **Protect the `release` environment.** Settings → Environments → `release` (GitHub creates it
   on the first release run if it does not exist): under *Deployment branches and tags* choose
   *Selected branches and tags* and add the tag rule `v*`; optionally add *Required reviewers* to
   approve each publication. Equivalently:

   ```bash
   gh api -X PUT repos/sebahrens/mini-agent/environments/release \
     -F 'deployment_branch_policy[protected_branches]=false' \
     -F 'deployment_branch_policy[custom_branch_policies]=true'
   gh api -X POST repos/sebahrens/mini-agent/environments/release/deployment-branch-policies \
     -f name='v*' -f type=tag
   ```

The release workflow accepts only pushed `v*` tags. After `verify-ci`, its `package-metadata` job rejects a non-tag ref,
a malformed tag, or a tag whose version differs from the root Cargo package version before any
release binary is built. The same job reads the version once from `Cargo.toml` (and requires
`editors/vscode/package.json` to agree) and exports it as a job output; every VSIX and SBOM file
name downstream is derived from that output, never from a literal in the workflow. All release
builds run with `--locked` so the committed `Cargo.lock` is authoritative. Manual branch dispatch is intentionally disabled, so a branch name can
never become a public release identity. Tags containing a prerelease suffix (for example,
`v2.0.0-rc.1`) remain GitHub prereleases.

If a tagged run needs recovery, use **Re-run jobs** on that tag's existing Actions run. Do not
start the release workflow from a branch. Publication still happens only after every expected full
and lite archive, the tag-matched Corresponding Source archive, five platform VSIX candidates, the
Windows MSI, and all three checksum manifests have been assembled and checked. The MSI job uses
pinned WiX 6.0.2, performs a quiet per-user install, runs the installed binary, and uninstalls it
before upload.

Private archive artifacts remain in separate download directories until checksum assembly so two
jobs cannot silently overwrite the same basename. `scripts/release_artifacts.py` rejects missing,
extra, duplicate, unsafe, symlinked, or non-regular candidates and writes the LF-terminated
`SHA256SUMS` in deterministic filename order. The final publication job reconstructs the complete
candidate set and verifies every archive byte against that manifest before creating the public
release. The final job generates repository-linked SLSA build-provenance attestations for every
candidate, uploads the complete set to a draft, compares the remote asset names with the validated
local set, and only then publishes the draft. A rerun removes its stale draft but refuses to replace
an already-published release for the tag. Consumers can verify a downloaded asset with
`gh attestation verify <asset> --repo sebahrens/mini-agent`.

## GPL release checklist

Before treating a release as complete, verify that:

- every binary archive has only the executable, `LICENSE`, `NOTICE`, and `SOURCE.md`;
- `NOTICE` identifies the imported ZeroStack commit and the date mini-agent modifications began;
- the same release contains the source asset named by `SOURCE.md`;
- the source asset contains the tagged tree, locked vendored dependencies, and offline Cargo config;
- `SHA256SUMS` covers all binary and source archives;
- `VSIX_SHA256SUMS` and `MSI_SHA256SUMS` cover the editor and Windows installer artifacts;
- every published asset has repository-linked SLSA build provenance that `gh attestation verify`
  accepts;
- the MSI installs `LICENSE.txt`, `NOTICE.txt`, and `SOURCE.md` beside its binary and VSIX; and
- the shell installer and downstream recipes install `NOTICE` and `SOURCE.md` alongside the GPL text.

For an older noncompliant release, attach its exact vendored source bundle, standalone compliance
documents, and a prominent release-note correction before leaving its binary assets available.

## Code signing status

Release binaries are currently **unsigned**. The repository holds no Authenticode or Apple
Developer ID certificate, so the release workflow runs no `signtool`, `codesign`, or `notarytool`
step:

- `mini-agent-windows-x64.msi` and the `mini-agent.exe` it installs have no Authenticode
  signature. SmartScreen warns on first open (users choose **More info**, then **Run anyway**), and
  AppLocker/WDAC publisher rules cannot pin a mini-agent signer; managed fleets must use file-hash
  rules and refresh them each release.
- The macOS archives' binaries are not Developer ID signed or notarized. Gatekeeper blocks a
  browser-downloaded binary until the user runs `xattr -d com.apple.quarantine ./mini-agent` or
  Control-clicks it and chooses **Open**; the `curl`-based shell installer is not quarantined.

User-facing integrity verification is the release's `SHA256SUMS`/`MSI_SHA256SUMS` plus
`gh attestation verify <asset> --repo sebahrens/mini-agent`. The README, Get Started guide, and
`packaging/windows/README.md` document these steps; keep them in sync with this section.

TODO (once a signing certificate exists; tracked as a separate deferred bead): sign the MSI and
`mini-agent.exe` in the `windows-msi` job, then add a release step after the quiet-install smoke that
runs `Get-AuthenticodeSignature` on both files and fails unless `Status` is `Valid` and the signer
subject matches the pinned publisher. Add the equivalent `codesign --verify --strict` and
`spctl --assess` gate for notarized macOS binaries. Do not add these steps before signing exists:
they would fail every release.

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

### Recipe digest lifecycle

Package recipes are refreshed only by `just post-release`; never copy a digest from an older
release or compute one by hand.

| State | Recipe `sha256` values | `check-package-metadata.py` result |
|---|---|---|
| After `just sync-version` / `just release`, before the GitHub release exists | placeholder `0000…0000` under the new `vX.Y.Z` URLs | Passes in default and `--ref-type tag` modes (the pre-tag gate). |
| After `just post-release` | digests of the published `vX.Y.Z` artifacts | Passes; `just post-release` ends by running the checker with `--require-release-digests`, which rejects any remaining placeholder. |
| Any state | a digest identical to one recorded at the previous release tag under the new version's URLs | Fails: the recipe is stale and must be refreshed by `just post-release`. |

The stale-digest comparison reads the previous `v*` tag's recipes from Git history, so it needs a
checkout that has tags (`git fetch --tags`); a shallow, tag-less clone skips that comparison but
still enforces the placeholder rule.

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
| `just sync-version` | Sync `Cargo.toml` version to packaging, VS Code, Windows, and ACP registry files; resets recipe digests to the placeholder when the version changes (no commit) |
| `just pre-release` | Same as `sync-version` (alias used by `release`) |
| `just add-tag` | Run `cargo test --locked`, validate, tag, and push the current Cargo version (no version bump) |
| `just remove-tag [VERSION]` | Delete a local + remote tag (interactive picker if omitted) |
| `just aur-checksums` | Update AUR checksums only |
| `just conda-source-sha256` | Update conda source tarball checksum only |
| `just conda-bin-checksums` | Update conda binary checksums only |
| `just homebrew-checksums` | Update Homebrew checksums only |
| `just release-checksums` | Download all exact-version inputs, then update every package checksum |
| `just aur-regen-srcinfo` | Regenerate `.SRCINFO` from `PKGBUILD` |
