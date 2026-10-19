# Pinned Git source bootstrap

`scripts/build-git-runtime.py` builds Git 2.56.0 and its HTTP/HTTPS helper from
the vendored official source archive. It verifies SHA-256
`26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3`
and the repository policy patch SHA-256
`3138c02584ea00a7234eef01366b0ca5757c0b5e4f28b4878381544b0b0e97b2`
before extraction or compilation. The archive's upstream signature was checked
when acquiring the pinned source; this offline script verifies the pinned bytes.
It never downloads dependencies, changes HOME, installs system packages, or
invokes Cargo.

From the repository root on a native supported host:

```sh
python3 scripts/build-git-runtime.py \
  --target aarch64-apple-darwin \
  --output target/my-git-runtime-bundle
```

The output directory must be absent. Stdout contains its absolute path; compiler
diagnostics go to stderr. `--source-archive` and `--policy-patch` accept explicit
paths but still require the pinned hashes. `--jobs` accepts 1–64. The default
private build cache is `target/git-runtime-bootstrap/<target>/<input-key>/`;
`--cache-root` changes its parent. The key includes both pinned input hashes and
the bootstrap script digest. An exclusive target lock serializes builders.
Cache hits recheck the target, manifest and every executable hash/architecture.
Invalid cache entries fail without being repaired or adopted. Removing an owned
cache explicitly is an operator action, not an automatic error fallback.

Extraction uses exclusive staging with bounded member counts and expanded size,
rejects traversal, hardlinks, duplicate paths and symlink ancestors, and permits
only internal symlinks. Failed builds retire their owned process group before
removing their own staging directory. Completed cache entries retain the bundle,
not the unpacked source tree. Inherited `GIT_*` and mini-agent Git snapshot policy
variables are removed from build/version commands; actual HOME is preserved.

The C build requires a C compiler, make, patch, curl development headers/library
and zlib. Git's Rust, gettext, Tcl/Tk, Perl, Python and gitweb components are
disabled explicitly. This is an external C source build; it does not change the
production Rust development build rules. macOS payloads must reference only
system dylibs. GNU/Linux uses the native host glibc/curl/zlib toolchain and its
system library prerequisites; this does not assert compatibility with every
older glibc distribution or provide a static-musl payload.

Packages: Debian/Ubuntu `apt install build-essential libcurl4-openssl-dev zlib1g-dev python3`
(`curl-config` comes from `libcurl4-openssl-dev`); Fedora/RHEL
`dnf install gcc make patch libcurl-devel zlib-devel python3`; macOS
`xcode-select --install`. Musl targets additionally need `pkg-config`.

The resulting files are passed to `prepare-git-runtime.py`: three independent
executables (`git`, `git-remote-http`, `git-remote-https`) and the bounded
`bundle.manifest`. The Rust embedding validator captures and validates the bundle
again. The build cache is trusted operator/build input; its self-described
manifest is not independent authentication of a compiler or untrusted runtime
bytes. Runtime approval comes from the compiled payload and policy contract.

## Target pipeline status

| Target | Bootstrap path |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Native x86-64 glibc host; C/curl/zlib prerequisites |
| `aarch64-unknown-linux-gnu` | Native ARM64 glibc host; C/curl/zlib prerequisites |
| `x86_64-apple-darwin` | Native Intel macOS; system curl/zlib |
| `aarch64-apple-darwin` | Native ARM64 macOS; system curl/zlib |
| `x86_64-unknown-linux-musl` | Explicit Docker launcher and pinned static MbedTLS/curl closure; see `git-runtime-musl.md` |
| `aarch64-unknown-linux-musl` | Explicit Docker launcher and pinned static MbedTLS/curl closure; see `git-runtime-musl.md` |
| `x86_64-pc-windows-msvc` | Explicit MSYS2 UCRT64 native executable/DLL bundle and Schannel curl; see below |

The generic native bootstrap refuses cross-target requests; the musl and Windows
paths use their explicit provisioning helpers. No host payload is labeled as
another target, and bootstrap failure does not silently select unguarded Git.
A prepared payload still requires its owning platform acceptance checks before
shipping. These helpers neither dispatch CI nor publish a release.

Focused policy tests:

```sh
python3 -m unittest scripts.tests.test_build_git_runtime
```

These cover pinned inputs, native architecture enforcement, safe extraction,
verified cache reuse/tampering, exclusive output, failure cleanup and owned child
retirement. A real native C build is separate from the synthetic archive tests.

## Windows native x64 provisioning

`scripts/build-git-runtime-windows.py` provides a separate MSYS2 UCRT64 source
build path for `x86_64-pc-windows-msvc`. This describes the **child executable's
Windows x64 architecture**, not a Rust static-library ABI: the payload is native
AMD64 PE32+ produced by MinGW GCC and does not link into the Rust program.
The original generic bootstrap continues to refuse Windows; CI must build this
bundle explicitly and pass it to the existing embedding producer/consumer.
The helper and deterministic policy tests exist; an actual hosted Windows build,
DLL-isolated execution and guarded Git acceptance are still required before
describing this target as delivered.

Runtime Git configuration mirrors use an exclusively created scratch directory.
On Windows its current-user owner and protected user/SYSTEM DACL are installed
at creation through the existing private-directory descriptor. A pre-existing
leaf is refused without repair or adoption; ordinary strict owner, reparse and
identity validation and the snapshot owner's cleanup remain in place. This
avoids inheriting an elevated token's Administrators owner before validation.

Git's pinned `config.mak.uname` has a MINGW compatibility branch. Its CMake path
defaults to obtaining vcpkg dependencies during configuration, so this helper
uses the existing Makefile with explicitly preinstalled dependencies instead.
The helper never runs pacman, downloads a toolchain, modifies privileged settings,
or changes HOME. It must run under **MSYS POSIX `/usr/bin/python3`**, with
`MSYSTEM=UCRT64`; native Windows/MinGW Python is deliberately rejected. Compiler
identity must be `x86_64-w64-mingw32` from the exact `/ucrt64/bin` prefix.

Provision these signed MSYS2 packages in CI before invoking the helper:

```yaml
- uses: msys2/setup-msys2@v2 # Pin this action to the publisher's reviewed commit in CI.
  with:
    msystem: UCRT64
    update: true
    install: >-
      make patch python diffutils
      mingw-w64-ucrt-x86_64-gcc
      mingw-w64-ucrt-x86_64-curl-winssl
      mingw-w64-ucrt-x86_64-pcre2
      mingw-w64-ucrt-x86_64-zlib
      mingw-w64-ucrt-x86_64-expat
      mingw-w64-ucrt-x86_64-libiconv
- name: Build pinned guarded Windows Git
  shell: msys2 {0}
  run: |
    set -euo pipefail
    /usr/bin/python3 scripts/build-git-runtime-windows.py \
      --target x86_64-pc-windows-msvc \
      --output target/git-runtime-windows-bundle
    bundle=$(cygpath -am target/git-runtime-windows-bundle)
    printf 'MINI_AGENT_GIT_RUNTIME_BUNDLE=%s\n' "$bundle" >> "$GITHUB_ENV"
```

Dependency acquisition belongs to the explicit publisher/CI step, not the source
bootstrap. CI should retain its package/version inventory with build artifacts.
The preparation action disables the MSYS2 package cache when `collect-sources`
is true: the pinned setup action otherwise ends with `pacman -Scc`, removing the
exact signed installed archives that source collection verifies. Ordinary CI jobs
keep package caching enabled.
Detached signatures are verified by MSYS2 GPG against the exact configured
`pacman-conf GPGDir`, requiring cryptographic success, `VALIDSIG` and fully or
ultimately trusted status for that signature. Read-only verification disables
automatic key retrieval/import, trustdb refresh and random-state writes; it does
not alter the package keyring or require pacman-key's unrelated `lock-never`
configuration precheck. Failure stops collection and reports exact package/signature
basenames, exit status and at most 4 KiB of escaped GPG diagnostics. No missing
key is imported and no unverified package is accepted.
Upstream recipe `SKIP` entries are accepted only for detached `.sig`/`.asc`
files paired with an already checksum-verified payload. The recipe's literal
`validpgpkeys` fingerprints select HTTPS-acquired public keys (OpenPGP directory,
with Ubuntu keyserver fallback); each primary fingerprint is checked before
dearmoring into a separate owned temporary keyring. `gpgv` must verify the exact
pair and report an allowed primary signing fingerprint. No keys are imported,
no recipe code runs, and every build-source/patch checksum remains mandatory.
`--pgp-key-cache` reuses public keys only after the same fingerprint checks.
The pinned MinGW configuration enables PCRE2; provision its UCRT64 headers and
libraries rather than disabling that feature. Before allocating build staging,
the helper checks the required header with the exact native compiler using a
fixed syntax-only input and a ten-second bound. This probe produces no executable.
Do not install the OpenSSL/gnutls curl variants alongside `curl-winssl`: the
helper requires `curl-config --ssl-backends` to report Schannel only. The official
[MSYS2 curl package recipe](https://github.com/msys2/MINGW-packages/blob/master/mingw-w64-curl/PKGBUILD)
configures this variant for Windows certificate trust without an external CA
bundle. Git's own OpenSSL paths are disabled; HTTP/HTTPS remain enabled through
curl/Schannel. See the official
[UCRT64 environment contract](https://www.msys2.org/docs/environments/) and
[CI provisioning guide](https://www.msys2.org/docs/ci/).

The output contains `git.exe`, independent HTTP and HTTPS helper files, and the
entire non-system DLL closure. A bounded parser checks AMD64 PE32+ headers,
normal imports, RVA-based delayed imports and export forwarders using
[Microsoft's PE format](https://learn.microsoft.com/en-us/windows/win32/debug/pe-format).
Each non-system dependency must resolve inside the exact provisioned UCRT64 `bin`
directory, including transitive dependencies; ambient PATH and installed Git are
never searched for missing DLLs. Case collisions, missing DLLs, incorrect
architectures, and any MSYS/Cygwin runtime dependency refuse publication. Only
the helper's explicit Windows 10+ system DLL/API-set contracts are omitted from
the bundle; this is not a fallback to an external Git installation.

The producer inventories every captured DLL under the existing 128-file/256-MiB
bundle limits. A final `git.exe --version` runs from the captured output with
PATH limited to that output and Windows system directories, so the toolchain
directory cannot mask a missing immediate Git dependency. This check alone does
not prove a live HTTPS operation; target-native helper/guarded-runtime acceptance
must execute the HTTP path as well. Failed builds and failed captured-version
checks remove only their exclusive staging/output. Windows builds are fresh
rather than reusing a cache across dependency-package updates; their owned
staging directory is under the existing target-specific cache root.

Focused host-independent tests:

```sh
python3 -m unittest scripts.tests.test_build_git_runtime_windows
```

These exercise PE import/delay/forwarder parsing, recursive/cyclic DLL closure,
absent DLLs despite ambient PATH, emulation-runtime refusal, wrong architecture,
native compiler/Schannel requirements, whole-bundle publication and failure
cleanup. They do not substitute for the pending native Windows execution row.
