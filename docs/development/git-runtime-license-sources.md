# Embedded Git notices and native dependency sources

The embedded Git executables and their native libraries are not Cargo packages.
Their provenance cannot be inferred from `Cargo.lock`. The existing
`THIRD_PARTY_LICENSES` v1 format and Cargo package count remain unchanged; a
separate `Native components (not Cargo packages)` section contains verbatim
notices with the same deduplicated text references.

Git 2.56.0 notices are read from the SHA-256-pinned source archive, including
`COPYING` and the archive's additional component license files. Git's `COPYING`
identifies GPLv2, rather than converting Git to the application's GPLv3 license.
The policy patch, source archive, and build scripts remain in the tagged source
tree and therefore in `git archive`/Corresponding Source. The source packager
checks their pinned hashes before producing its final archive. Older source
trees without the embedded Git builder retain their previous packaging behavior.

No OpenSSL linking exception was found in the pinned Git source. The earlier
OpenSSL3 static experiment is not a shipping basis. The actual MbedTLS 4.1.1
source used by the selected musl builder includes license texts offering
Apache-2.0 **or GPL-2.0-or-later** for MbedTLS, its framework and TF-PSA-Crypto.
The inventory reproduces upstream declarations and complete notices; it does
not decide license compatibility merely from package labels. The existing
publisher must retain its reviewed composition choice.

## Target-native record

`scripts/collect-git-native-sources.py` emits `native-components.json` and its
actual source/notice files. It is publisher tooling, not runtime authorization.
The record is outside the strict executable `bundle.manifest` and contains:

```json
{
  "format": "mini-agent-native-components/1",
  "target": "x86_64-pc-windows-msvc",
  "bundle_manifest_sha256": "<actual bundle manifest SHA-256>",
  "components": [{
    "name": "actual-distribution-package",
    "version": "exact-installed-version",
    "license": "upstream license declaration",
    "source_url": "https://official/source/origin",
    "bundled_files": [{"name": "actual.dll", "sha256": "<SHA-256>"}],
    "license_files": [{"path": "relative/COPYING", "sha256": "<SHA-256>"}],
    "source_files": [{"path": "relative/source.tar.xz", "sha256": "<SHA-256>"}]
  }]
}
```

All referenced files must be present, bounded regular files below the record's
directory, without symlink traversal. Hashes are checked before use. A supplied
runtime bundle must match the record's manifest digest and its complete actual
DLL inventory. Static components have no separate DLL entries. This declaration
is a trusted build input; no JSON record can authorize runtime Git bytes or
establish legal completeness independently of the source/build pipeline.

Native GNU/macOS dynamic system libraries are not copied into these payloads;
their platform prerequisite policy remains separate. Windows redistributed DLLs
and musl static components require their actual records. Inventory generation
fails if the record is missing on either target family, rather than claiming the
Cargo resolution covers those dependencies.

## Alpine static closure

Run the collector against the **actual builder's installed package database** and
the verified linker/package ownership list, not a guessed TLS backend list:

```sh
python3 scripts/collect-git-native-sources.py \
  --distro alpine --target aarch64-unknown-linux-musl \
  --bundle target/actual-musl-bundle --output target/native-musl-sources \
  --curl-source scripts/git-runtime/curl-8.23.0.tar.xz \
  --static-curl-closure
```

The collector resolves the actual `pkg-config --libs --static libcurl` archives
and compiler-default `libc`, `libgcc` and `libgcc_eh`, hashes each one and matches
its exact installed APK owner. The custom curl archive must be the pinned source
build. Unsupported linker flags or unowned inputs fail closed. The measured
closure and hashes travel in `static-link-inputs.json`; an external read-only
metadata capture can instead pass its exact `--package` owner list. The collector derives the distfiles series from `/etc/alpine-release`.
An external collector can pass an exact copied `--alpine-db` and explicit
`--alpine-series`. Installed APK metadata supplies origin, package version and
the aports commit. The collector fetches that commit's recipe and every literal
SHA512 source input, verifies bytes, and includes recipe-local patches. It does
not execute APKBUILD. Unsupported/dynamic checksum declarations, missing source
archives and absent verbatim notices fail rather than generating placeholders.
Repeated packages from one origin share the same captured source files.

Custom curl 8.23.0 is bound to the official archive SHA-256
`8dadd7d2536a314f982ad05a651b713f426bf7d02cc2c76cffb9796f2218e6c5`;
it is not mislabeled as Alpine's separately installed curl version. The build
recipe controlling that custom compile is the tagged Dockerfile/launcher.

## Windows DLL closure

Inside the provisioned UCRT64 environment, retain the exact pacman package cache
and provide the `zstd` host utility:

```sh
/usr/bin/python3 scripts/collect-git-native-sources.py \
  --distro msys2 --target x86_64-pc-windows-msvc \
  --bundle target/actual-windows-bundle --output target/native-windows-sources
```

Every actual bundled DLL is matched to the identical installed file, then to its
owning package. The cached binary package signature is checked with the existing
MSYS2 keyring, and its package identity must match the installed version. Its
`.BUILDINFO` anchors the exact `PKGBUILD` hash in the official full-source package.
Checksum-pinned source inputs must all be present and valid. Notices are copied
from those source archives. Source package/version availability and a retained
binary cache are requirements; the collector refuses an unavailable historical
source or a mismatched recipe. It does not substitute a current recipe or execute
PKGBUILD to fill a missing checksum declaration. Actual Windows collection and
execution remain part of the hosted acceptance row.

## Binary and Corresponding Source integration

After the target's source capture, generate full/lite notices with the same record:

```sh
python3 scripts/third_party_licenses.py generate --target "$TARGET" \
  --native-record "$RECORD" --git-runtime-bundle "$BUNDLE" \
  --output target/THIRD_PARTY_LICENSES
```

Pass every target's actual record to the existing source assembler:

```sh
bash scripts/package-corresponding-source.sh "$TAG" . HEAD \
  --native-record path/to/windows/native-components.json \
  --native-record path/to/musl/native-components.json
```

The assembler copies the verified source/notice inputs into
`vendor-native/<target>/`, preserves relative paths and revalidates the copied
record. Missing/tampered/duplicate-target data removes only its exclusively
created partial tree. It never repairs or replaces an existing source tree.
Source files travel with the record; a sidecar without its assets is insufficient.
Release orchestration must transfer these source artifacts from native build jobs
to the Corresponding Source job. The earlier Cargo/npm-only archive is not a
complete claim for the new redistributed native components.

The release musl launcher accepts `--native-sources-output` and performs this
collection in a separately named, owned, unprivileged read-only container with
public network access. The source/bundle mounts are read-only; host credentials
and host authentication directories are not mounted. Git compilation itself
retains its network-disabled container. Source collection failure stops release
packaging and never labels an incomplete sidecar complete.

The Windows composite action's `collect-sources` input defaults to false for
ordinary compilation jobs. Release builds set it to true and publish
`MINI_AGENT_GIT_NATIVE_RECORD` plus `MINI_AGENT_GIT_NATIVE_SOURCE_ROOT` only after
collection succeeds. Both full and lite inventories use that exact record.
The musl and Windows jobs upload their verified source directories; Corresponding
Source waits for both jobs, requires all three target records, and copies them
with their assets. Normal GNU/macOS release inventories automatically include
the pinned Git archive's notices without a separate redistributed-library record.
