# Windows ARM64 release binary

Status: the release workflow builds `aarch64-pc-windows-msvc` natively on a `windows-11-arm` runner
and publishes `mini-agent-aarch64-pc-windows-msvc.tar.gz` and the lite equivalent (zip-style archives).
The ARM64 build ships as an archive, an MSI (`mini-agent-windows-arm64.msi`) and a `win32-arm64` VSIX; there
is still no npm platform package.
Running the x64 binary under emulation on ARM64 Windows is not supported: the shell, git and
js tools are unavailable there, so use the native archive.

How it works:

- `scripts/build-git-runtime-windows.py --target aarch64-pc-windows-msvc` builds the pinned Git source with
  clang in the MSYS2 CLANGARM64 environment (packages from the official MSYS2 clangarm64 repository, the same
  signed package source the x64 build uses from UCRT64). Every bundled executable and DLL must be a native
  ARM64 PE32+ image. The first PowerShell start of the emulated MSYS2 userland is slow, so the cache DACL
  step allows 300 s on this target instead of 30 s.
- `.github/actions/prepare-git-runtime` takes a `target` input (default x64) and is used by the release
  workflow for both Windows rows.
- `.github/workflows/windows-arm64-smoke.yml` (manual) builds the target and runs `--version`,
  `--js-runtime-check` (the JS worker LPAC self-check) and `--git-runtime-check`.
- `mini-agent update` maps the host to an asset name with a pure function (`release_archive_for` in
  `src/self_update.rs`; aarch64 Windows gives `mini-agent-aarch64-pc-windows-msvc.tar.gz`, unit-tested on any
  host). Windows installs stay on the manual path because a running `.exe` cannot be replaced in place, so the
  update check only compares versions.
- `ci.yml` has a compile-only job, `windows-arm64-compile` (ARM64 Git runtime, then `cargo test --locked
  --no-run` on `windows-11-arm`), under the same scope rules as `windows-hooks-compile` and listed in
  `ci-success`. The runtime checks run on the release archive in `archive-smoke`.
- The Scoop manifest carries an `arm64` architecture entry (and `autoupdate`) for the ARM64 archive.

ARM64 MSI and VSIX: the release builds the `win32-arm64` VSIX on `windows-11-arm` (the same test, package,
SBOM and clean VS Code install steps as the other five targets; `package:win32-arm64` and the `win32-arm64`
entry in `editors/vscode/scripts/platform.mjs`), then builds the MSI per architecture with the existing WiX
project (`MsiArch` property, one `windows-msi` job per architecture, the ARM64 one on `windows-11-arm`).
`windows-installer` assembles both MSIs and a single `MSI_SHA256SUMS`. The winget installer template carries an
`arm64` entry for the ARM64 MSI. `ci.yml`'s `windows-msi` job stays x64-only (fixture build and install); the ARM64 MSI is built, installed and uninstalled by the release job.
