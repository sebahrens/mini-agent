# Phase 2 — Sandbox Hardening

**Status**: Pre-implementation  
**Prerequisite**: Phase 1 complete and passing  
**Delivers**: `fetch()` host global with URL allow-lists, file path allow-lists enforced in `read_file`/`write_file`, and `birdcage` process-level isolation wrapping `spawn()`.

---

## Overview

Phase 1 `spawn()` routes through `Sandbox::wrap_command` but only applies bubblewrap/zerobox on Linux. Phase 2 adds:

1. **`fetch(url, opts?)`** — HTTP from JS, permission-gated by URL glob pattern
2. **File allow-lists** — per-config path restrictions on `read_file`/`write_file`
3. **`birdcage` integration** — Landlock (Linux) + Seatbelt (macOS) abstraction wrapping `spawn()` subprocesses

---

## Cargo.toml additions

```toml
[features]
# Add — keep separate from js; phases are independent feature flags
sandbox = ["dep:birdcage"]

[dependencies]
birdcage  = { version = "0.7", optional = true }
reqwest   = { version = "0.13", features = ["blocking"], optional = true }
```

Note: `reqwest` (non-blocking) is already a mandatory dep at line 67 of `Cargo.toml`. The `blocking` feature should be added as an optional dep or the existing `reqwest` dep should gain `features = ["blocking"]` under the `sandbox` feature. Do not add a second `reqwest` entry.

---

## Feature gate

```rust
// Any new sandbox-specific code
#[cfg(feature = "sandbox")]
```

The `js` feature and the `sandbox` feature are **independent**. An implementor should be able to enable either without the other. `spawn()` sandboxing in Phase 2 requires both `js` and `sandbox`.

---

## Target files

| File | Status | Change |
|------|--------|--------|
| `src/sandbox.rs` | EXISTS (10.0 KB) | Add `birdcage`-backed `wrap_spawn_sandboxed` method |
| `src/extras/js/host.rs` | TO BE CREATED in Phase 1 | Add `make_fetch()` and file allow-list checks |
| `Cargo.toml` | EXISTS | Add `sandbox`, `birdcage`, `reqwest` blocking feature |

---

## Current state of `src/sandbox.rs`

The `Sandbox` struct (defined at `src/sandbox.rs:9`; `#[derive(Debug, Clone)]` is on line 8) currently wraps two Linux backends:
- **bwrap** (`bubblewrap`) — checked via `bwrap_exists()` at line 18
- **zerobox** — checked via `zerobox_exists()` at line 22
- **macOS/Windows** — no sandbox backend; `is_effectively_sandboxed()` returns `false`

`Sandbox::wrap_command` at line 109 applies bubblewrap or zerobox on Linux and falls back to unsandboxed elsewhere. `Sandbox::output_command` (called in `bash.rs:145`) calls `wrap_command` and awaits the output.

---

## fetch() host global

### Cargo

Enable `reqwest/blocking` under the `sandbox` (or `js`) feature gate to avoid tokio-in-tokio issues on the dedicated JS thread.

### Permission routing

```toml
# User config (config.toml)
[js.fetch.allow]
patterns = ["https://api.github.com/**", "https://*.openai.com/**"]
```

Permission check call site pattern (mirrors `check_perm` at `src/agent/tools/mod.rs:199`):

```rust
check_perm(&self.permission, &self.ask_tx, "js/fetch", &url).await?
```

Unknown URLs fall to `Ask` — user approves interactively.

### Implementation sketch

```rust
pub fn make_fetch() -> impl Fn(String, Option<serde_json::Value>) -> rquickjs::Result<FetchResult> {
    move |url: String, _opts: Option<serde_json::Value>| {
        // Permission is checked via sync channel before this executes
        // (same SpawnContext pattern as spawn())
        let client = reqwest::blocking::Client::new();
        let resp = client.get(&url).send()
            .map_err(|e| rquickjs::Error::new_from_js("fetch", &e.to_string()))?;
        Ok(FetchResult {
            status: resp.status().as_u16(),
            text: resp.text().unwrap_or_default(),
        })
    }
}
```

Response visible to JS: `{ status: number, text: string }`.

---

## File allow-list

Config format:

```toml
[js.file.allow]
read  = ["/home/**", "/tmp/**"]
write = ["/tmp/**"]
```

Enforcement in `host.rs` before `std::fs` calls:

```rust
fn check_file_allow(path: &str, allow_patterns: &[String]) -> rquickjs::Result<()> {
    let allowed = allow_patterns.iter().any(|pat| glob_match(pat, path));
    if !allowed {
        return Err(rquickjs::Error::new_from_js("file", "path not in allow-list"));
    }
    Ok(())
}
```

When no allow-list is configured, all paths are permitted (same default-open policy as `BashTool`).

---

## birdcage integration

### Why birdcage

`birdcage` is a single crate that abstracts:
- **Linux**: Landlock + seccomp
- **macOS**: `sandbox-exec` (Seatbelt)

It provides a single swap point if Apple removes `sandbox-exec` in a future macOS release.

### Integration point

`spawn()` in `host.rs` currently calls `Sandbox::wrap_command` (a `tokio::process::Command`). With birdcage, a new method is added:

```rust
// src/sandbox.rs — new method, gated behind #[cfg(feature = "sandbox")]
#[cfg(feature = "sandbox")]
pub fn wrap_spawn_sandboxed(&self, cmd: &str, args: &[String]) -> std::process::Command {
    use birdcage::{Birdcage, Sandbox as BirdcageSandbox};
    // Configure birdcage with read/write access matching Sandbox parameters
    // ...
    // Returns a std::process::Command (blocking — JS thread acceptable)
}
```

### Platform matrix

| Platform | Mechanism | Status |
|----------|-----------|--------|
| Linux | Landlock + seccomp (via birdcage) | Phase 2 |
| macOS | Seatbelt / `sandbox-exec` (via birdcage) | Phase 2 |
| Windows | Job Objects + AppContainer | Out of scope for Phase 2 |
| Windows fallback | Unsandboxed (same as Phase 1) | Phase 2 |

Windows enforcement requires `rappct` (separate crate, Phase 2 stretch or Phase 3). The existing `#[cfg(unix)]` pattern in `src/sandbox.rs` (`kill_process_group`) shows the convention: Windows arms are empty stubs.

---

## Acceptance criteria

All must pass under `cargo test --features js,sandbox`:

- [ ] `fetch("https://example.com")` returns `{ status: 200, text: "..." }` in an integration test
- [ ] `fetch()` with a URL not matching the allow-list returns a JS error, not a panic
- [ ] `read_file("/etc/shadow")` returns a JS error when the allow-list excludes `/etc/**`
- [ ] `spawn("ls", ["/tmp"])` on Linux runs inside a birdcage cage (Landlock-enforced)
- [ ] `cargo test --features js` (without `sandbox`) still passes unchanged — features are independent
- [ ] macOS: `spawn()` runs inside Seatbelt when `sandbox` feature is enabled

---

## Out of scope for Phase 2

- Windows sandbox enforcement (Job Objects / AppContainer)
- UI for viewing/editing the URL allow-list
- Skill library (Phase 3)
- Auto-admission (Phase 4)
