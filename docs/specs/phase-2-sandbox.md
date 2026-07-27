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
birdcage = { version = "0.7", optional = true }
```

**reqwest note**: `reqwest = "0.13"` is already a mandatory dep at `Cargo.toml:67`. To enable `reqwest/blocking` (needed for the JS-thread `fetch()` implementation), add `features = ["blocking"]` to the existing entry rather than adding a second `reqwest` entry. Do NOT add a duplicate dep.

The `sandbox` and `js` features are **independent**. Enabling `sandbox` without `js` must compile (it only extends `src/sandbox.rs`). Enabling `js` without `sandbox` must compile (Phase 1 behavior, unsandboxed spawn).

---

## Target files

| File | Status | Change |
|------|--------|--------|
| `src/sandbox.rs` | EXISTS (10.0 KB) | Add `birdcage`-backed `wrap_spawn_sandboxed` method |
| `src/extras/js/host.rs` | TO BE CREATED in Phase 1 | Add `make_fetch()` and file allow-list checks |
| `Cargo.toml` | EXISTS | Add `sandbox` feature, `birdcage` optional dep, `reqwest/blocking` feature |

---

## Current state of `src/sandbox.rs`

The `Sandbox` struct (defined at `src/sandbox.rs:9`; `#[derive(Debug, Clone)]`) currently wraps two Linux backends:

- **bwrap** (`bubblewrap`) — checked via `bwrap_exists()` at line 18
- **zerobox** — checked via `zerobox_exists()` at line 24
- `Sandbox::wrap_command` at line 109 returns `tokio::process::Command`; applies bubblewrap or zerobox on Linux, falls back to unsandboxed on macOS/Windows
- `Sandbox::output_command` at line 205 (async) — calls `wrap_command` and awaits the output
- `kill_process_group` at line 294 is `#[cfg(unix)]` with empty Windows arm — keep this pattern

**macOS/Windows**: No sandbox backend in Phase 1. `is_effectively_sandboxed()` returns `false` on both.

---

## fetch() host global

### Permission routing

```toml
# User config (config.toml)
[js.fetch.allow]
patterns = ["https://api.github.com/**", "https://*.openai.com/**"]
```

Permission check call pattern (mirrors `check_perm` at `src/agent/tools/mod.rs:199`):

```rust
check_perm(&self.permission, &self.ask_tx, "js/fetch", &url).await?
```

Unknown URLs fall to `Ask` — user approves interactively.

### Implementation sketch

```rust
pub fn make_fetch() -> impl Fn(String, Option<serde_json::Value>) -> rquickjs::Result<FetchResult> {
    move |url: String, _opts: Option<serde_json::Value>| {
        // Permission is checked via sync channel before this executes
        // (same SpawnContext pattern as spawn() in Phase 1)
        let client = reqwest::blocking::Client::new();
        let resp = client.get(&url).send()
            .map_err(|e| rquickjs::Error::new_from_js("fetch", &e.to_string()))?;
        Ok(FetchResult {
            status: resp.status().as_u16(),
            text: resp.text().unwrap_or_default(),
        })
    }
}

pub struct FetchResult {
    pub status: u16,
    pub text: String,
}
```

Response visible to JS: `{ status: number, text: string }`.

`reqwest::blocking` is used because `fetch()` runs on the dedicated JS thread, which has no tokio runtime. Using `reqwest::blocking` avoids tokio-inside-tokio issues.

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

`spawn()` in `host.rs` currently calls `Sandbox::wrap_command` (at `src/sandbox.rs:109`). With birdcage, a new method is added to `Sandbox`:

```rust
// src/sandbox.rs — new method, gated behind #[cfg(feature = "sandbox")]
#[cfg(feature = "sandbox")]
pub fn wrap_spawn_sandboxed(&self, cmd: &str, args: &[String]) -> std::process::Command {
    use birdcage::{Birdcage, Sandbox as BirdcageSandbox};
    // Configure birdcage with read/write access matching Sandbox parameters
    // Returns a std::process::Command (blocking — JS thread acceptable)
}
```

`Sandbox::wrap_command` (existing, at line 109) returns `tokio::process::Command`. `wrap_spawn_sandboxed` returns `std::process::Command` for use on the JS thread. The two are parallel paths.

### Platform matrix

| Platform | Mechanism | Status |
|----------|-----------|--------|
| Linux | Landlock + seccomp (via birdcage) | Phase 2 |
| macOS | Seatbelt / `sandbox-exec` (via birdcage) | Phase 2 |
| Windows | Unsandboxed (same as Phase 1) | Phase 2 fallback |
| Windows enforcement | Job Objects + AppContainer (`rappct`) | Out of scope for Phase 2 |

Follow the `#[cfg(unix)]` pattern from `src/sandbox.rs:294` (`kill_process_group`): Windows arms are empty stubs, not absent.

---

## Acceptance criteria

All must pass under `cargo test --features js,sandbox`:

- [ ] `fetch("https://example.com")` returns `{ status: 200, text: "..." }` in an integration test
- [ ] `fetch()` with a URL not matching the allow-list returns a JS error string, not a panic
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
