# Crossterm 0.29.0 snapshot

This directory contains the MIT-licensed crates.io `crossterm` 0.29.0 archive,
including its original LICENSE, authors, Cargo.toml.orig and Cargo.lock.

- Upstream: https://github.com/crossterm-rs/crossterm
- Archive: https://static.crates.io/crates/crossterm/crossterm-0.29.0.crate
- Archive SHA-256: `d8b9f2e4c67f833b660cdb0a3523065869fb35570177239812ed4c905aeff87b`
- Packaged upstream revision: `36d95b26a26e64b0f8c12edfe11f410a6d56a812`

The local changes for mini-agent-i7ro6.118 are limited to:

1. The Unix mio terminal read loop returns `UnexpectedEof` on a zero-byte read,
   and propagates errors other than `WouldBlock` and `Interrupted`. Previously
   both cases could loop forever without returning to the application's owned
   input reader. `WouldBlock` and `Interrupted` retain their existing behavior.
2. Three focused tests in the same module cover EOF, ordinary keys/bracketed
   paste before EOF, and a fatal read error (the libc backend).
3. An empty `[workspace]` in the normalized Cargo.toml allows isolated dependency
   tests. The root excludes this dependency from its production workspace.

The current upstream master file was checked on 2026-10-15 and still contained
this read loop: https://github.com/crossterm-rs/crossterm/blob/master/src/event/source/unix/mio.rs
No other upstream implementation files are changed. Application reader error
backoff, terminal ownership, cancellation and joining are unchanged.

From the mini-agent root, using a checkout-specific CARGO_TARGET_DIR:

```sh
cargo test --manifest-path patched-deps/crossterm/Cargo.toml --lib terminal_hangup118_ -- --nocapture
cargo test --manifest-path patched-deps/crossterm/Cargo.toml --lib --features libc terminal_hangup118_fatal_read_error_is_propagated -- --nocapture
```
