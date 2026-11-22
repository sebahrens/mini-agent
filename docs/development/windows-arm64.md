# Windows ARM64 release binary

Status: the release workflow builds `aarch64-pc-windows-msvc` natively on a `windows-11-arm` runner
and publishes `mini-agent-aarch64-pc-windows-msvc.tar.gz` and the lite equivalent (zip-style archives,
no MSI yet). Running the x64 binary under emulation on ARM64 Windows is not supported: the shell, git and
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
- `mini-agent update` selects the asset by install channel, not by a target table.

CI note: a compile-only ARM64 job (prepare the runtime with the ARM64 target, then run the test build on
`windows-11-arm`) belongs in `ci.yml`; it is not added here.
