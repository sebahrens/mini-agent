# Corresponding Source

Every mini-agent GitHub release provides the exact source used to build its binaries in the same
release and at no additional charge. For a binary reporting version `<VERSION>`, download:

```text
https://github.com/sebahrens/mini-agent/releases/download/v<VERSION>/mini-agent-v<VERSION>-source.tar.gz
```

The source archive is covered by the `SHA256SUMS` file in that release. It contains the tagged
mini-agent source tree, build and release scripts, the GPL license and modification notice, all
Cargo dependency sources vendored with the locked dependency graph, and the VS Code extension's
locked npm package tarballs. Its generated
`.cargo/config.toml` makes Cargo use those vendored sources.

After installing the Rust toolchain named in `rust-toolchain.toml`, a native full or lite binary can
be rebuilt from the extracted source archive without downloading Cargo dependencies:

```bash
cargo build --release --locked --offline
cargo build --release --locked --offline --no-default-features
```

The VS Code extension bundles npm dependencies into `editors/vscode/dist/extension.js`, so the
archive also carries every tarball pinned by `editors/vscode/package-lock.json` under `vendor-npm/`,
listed with its lockfile integrity hash in `vendor-npm/npm-sources.json`. With the Node version in
`editors/vscode/.nvmrc` and the npm named by `packageManager` in `editors/vscode/package.json`, the
bundle can be rebuilt from the extracted archive without downloading npm packages:

```bash
npm cache add --cache "$PWD/.npm-offline-cache" vendor-npm/*.tgz
cd editors/vscode
npm ci --offline --ignore-scripts --cache "$PWD/../../.npm-offline-cache"
npm run build
```

The archive entries are sorted and share the tagged commit's timestamp, so rebuilding the archive
from the same tag with the same locked dependencies reproduces it byte for byte.

Cross-target release settings and pinned build-container identities are in `Cross.toml` and
`.github/workflows/release.yml`. See `docs/agent/PUBLISHING_RELEASES.md` for the complete release
procedure.

The project will retain each Corresponding Source asset for as long as it distributes the matching
binary asset. If a matching source asset is unavailable, report a compliance issue at
https://github.com/sebahrens/mini-agent/issues and identify the release tag and binary archive.
