# Corresponding Source

Every mini-agent GitHub release provides the exact source used to build its binaries in the same
release and at no additional charge. For a binary reporting version `<VERSION>`, download:

```text
https://github.com/sebahrens/mini-agent/releases/download/v<VERSION>/mini-agent-v<VERSION>-source.tar.gz
```

The source archive is covered by the `SHA256SUMS` file in that release. It contains the tagged
mini-agent source tree, build and release scripts, the GPL license and modification notice, and all
Cargo dependency sources vendored with the locked dependency graph. Its generated
`.cargo/config.toml` makes Cargo use those vendored sources.

After installing the Rust toolchain named in `rust-toolchain.toml`, a native full or lite binary can
be rebuilt from the extracted source archive without downloading Cargo dependencies:

```bash
cargo build --release --locked --offline
cargo build --release --locked --offline --no-default-features
```

Cross-target release settings and pinned build-container identities are in `Cross.toml` and
`.github/workflows/release.yml`. See `docs/agent/PUBLISHING_RELEASES.md` for the complete release
procedure.

The project will retain each Corresponding Source asset for as long as it distributes the matching
binary asset. If a matching source asset is unavailable, report a compliance issue at
https://github.com/sebahrens/mini-agent/issues and identify the release tag and binary archive.
