# Review: Dependencies — mini-agent

You are auditing the mini-agent workspace for dependency hygiene: Cargo.toml correctness,
feature gate cleanliness, version pinning, and license compatibility.

## Setup

1. Read `CLAUDE.md` and the `[dependencies]` section of `Cargo.toml`.
2. Check existing beads: `bd list --limit 0 --status open && bd search "DEPS:"`.
3. Run dependency analysis with narsil-mcp:
   ```
   mcp__narsil-mcp__check_dependencies()           # known vulnerabilities
   mcp__narsil-mcp__check_licenses()               # license compatibility
   mcp__narsil-mcp__generate_sbom()                # full software bill of materials
   mcp__narsil-mcp__find_upgrade_path()            # outdated crate versions
   mcp__narsil-mcp__get_import_graph()             # actual vs declared usage
   mcp__narsil-mcp__find_unused_exports()          # exports from deps no one uses
   ```

## Bead filing protocol

```bash
bd create --title="DEPS: <short summary>" --type=task --priority=<1-3> \
  --description="Crate: <crate name and version>
Description: <dependency concern>
Evidence: <narsil-mcp output or Cargo.toml snippet>
Impact: <security, binary size, license risk, build time>
Fix: <upgrade, remove, or narrow the feature set>
Verification: <cargo test still passes after the change>"
```

## Dependency vectors to investigate

### 1. Optional dependency correctness

The JS engine uses optional dependencies. Verify they are declared correctly:

Read `Cargo.toml` and check:
- `rquickjs = { version = "0.12", features = ["full"], optional = true }` — is `optional = true`?
- `birdcage` (Phase 2) — is it declared as optional under `sandbox` feature?
- `fastembed` + `rusqlite` (Phase 3) — declared optional under `skills` feature?

### 2. Feature flag hygiene

```
mcp__narsil-mcp__find_symbols("cfg(feature")       # all feature-gated code
```

- Are all features in `[features]` reachable through `default`? (non-default features need explicit `--features`)
- Does the `js` feature gate exactly and only `dep:rquickjs`?
- Are there features that activate multiple optional deps without naming them individually?
- Does `cargo build` (default features, no `--features js`) produce a warning-free build?

### 3. Vulnerability scan

From narsil-mcp `check_dependencies()` output:
- Any crates with known CVEs in Cargo.lock?
- Any crates with `RUSTSEC-20XX-XXXX` advisories?

File a priority-0 bead for each critical vulnerability.

### 4. License compatibility

The project is `GPL-3.0-only` (from Cargo.toml). Check:
- Are all runtime dependencies GPL-compatible?
- Is `rquickjs` (MIT) compatible? (Yes, MIT is GPL-compatible.)
- Are there any `LGPL` or `GPL-2-only` deps that could create compatibility issues?

### 5. Unused or over-broad feature sets

```
mcp__narsil-mcp__find_unused_exports()
mcp__narsil-mcp__get_import_graph()
```

- Does any dep pull in features that mini-agent doesn't use?
  (e.g. `tokio = { features = ["full"] }` instead of the minimal set already declared)
- Is `rquickjs = { features = ["full"] }` appropriate, or can it be narrowed?
  (Check which rquickjs features are actually used.)

### 6. Workspace dependency consistency

The production workspace contains only root (`mini-agent`); `spike/` is a standalone research
workspace. Check:
- Does root metadata remain free of the spike package and its research-only dependencies?
- Does `spike/Cargo.toml` remain independently runnable with its own lockfile?
- Has any production source started importing the standalone research crate?

## Deduplication protocol

Before filing: `bd search "DEPS:"`. Comment on existing beads for duplicates.

## After completing

```bash
bd dolt push
```

Report: count of vulnerabilities by severity, license issues, feature bloat opportunities, version staleness.
