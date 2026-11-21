# Windows ARM64 release binary

Status: native build and smoke workflow only; no ARM64 archive is published yet.

`.github/workflows/windows-arm64-smoke.yml` (manual `workflow_dispatch`, non-publishing) builds
`aarch64-pc-windows-msvc` on a `windows-11-arm` runner and runs `--version`, `--js-runtime-check`
(the JS worker LPAC self-check) and `--git-runtime-check`.

Remaining before the release matrix can include the target:

- The guarded Git runtime (`scripts/build-git-runtime-windows.py`, `.github/actions/prepare-git-runtime`)
  builds only `x86_64-pc-windows-msvc` from MSYS2; an ARM64 runtime bundle and its source proof are needed.
- `scripts/check-package-metadata.py`, `scripts/release_artifacts.py` and the `release.yml` checksum and
  MSI steps list exactly the current archive set; adding the ARM64 zip means extending those lists together.
- `mini-agent update` selects the asset by install channel, not by a target table, so no `src/update.rs`
  change is needed until the archive exists in `SHA256SUMS`.
