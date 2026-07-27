# Review: Code Quality — mini-agent

You are auditing the mini-agent workspace for code quality: naming, documentation,
test coverage, and adherence to project conventions.

## Setup

1. Read `CLAUDE.md` (build rules, comment style, invariants) and `AGENTS.md`.
2. Check existing beads: `bd list --limit 0 --status open && bd search "QUALITY:"`.
3. Survey quality signals with narsil-mcp:
   ```
   mcp__narsil-mcp__get_complexity()               # high-complexity modules
   mcp__narsil-mcp__get_function_hotspots()        # large functions (likely undertested)
   mcp__narsil-mcp__find_dead_stores()             # variables assigned but never read
   mcp__narsil-mcp__check_type_errors("src/")      # latent type errors
   mcp__narsil-mcp__find_semantic_clones()         # duplicated logic
   ```

## Bead filing protocol

```bash
bd create --title="QUALITY: <short summary>" --type=task --priority=<2-4> \
  --description="Location: <file:line from narsil-mcp>
Description: <quality concern>
Evidence: <code snippet or narsil-mcp output>
Impact: <maintainability, readability, test reliability>
Fix: <specific refactor or documentation addition>
Verification: <cargo test passes, or reviewer can confirm by inspection>"
```

## Quality vectors to investigate

### 1. Comment quality

CLAUDE.md rule: "Default to writing no comments. Only add one when the WHY is non-obvious."

```
mcp__narsil-mcp__get_chunks("src/")               # read module-level code blocks
```

- Are there comment blocks explaining WHAT the code does (should be removed)?
- Are there non-obvious invariants that lack a comment (OOM risk, thread-affinity, etc.)?
- Are there `TODO` or `FIXME` comments without a corresponding bead?

### 2. Error handling uniformity

```
mcp__narsil-mcp__find_symbols("anyhow")
mcp__narsil-mcp__find_symbols("thiserror")
mcp__narsil-mcp__find_symbols("unwrap")
```

CLAUDE.md / ARCHITECTURE.md convention:
- `thiserror` for library error types
- `anyhow` only for binary entry-point glue
- No `.unwrap()` or `.expect()` outside `#[test]` blocks in critical paths

File a bead for each violation of these rules.

### 3. Test coverage gaps

```
mcp__narsil-mcp__find_symbols("cfg(test)")        # find existing test modules
mcp__narsil-mcp__find_symbols("#[test]")
mcp__narsil-mcp__find_dead_code()                 # untested paths often show as dead
```

CLAUDE.md requires: "write tests for new non-TUI code". Check:
- Every module in `src/extras/js/` (if it exists) should have a `#[cfg(test)]` block.
- The `JsTool` channel types should have at minimum a type-assertion test.
- Host functions (`read_file`, `write_file`, `spawn`) should have integration tests.

### 4. Naming conventions

```
mcp__narsil-mcp__workspace_symbol_search("Js")    # check JS type naming
mcp__narsil-mcp__workspace_symbol_search("js_")   # check function naming
```

- Are `JsRequest`, `JsResponse`, `JsOutcome` named exactly per SPEC.md?
- Are function names imperative verbs (e.g. `run_step`, `register_host_globals`)?
- Are constants `SCREAMING_SNAKE_CASE` (e.g. `STEP_TIMEOUT`, `MEMORY_LIMIT`)?

### 5. Module structure

```
mcp__narsil-mcp__get_project_structure()
mcp__narsil-mcp__get_import_graph()
```

- Does the actual file placement match the table in AGENTS.md?
- Are there modules that should be `pub(crate)` but are `pub`?
- Are there `use` wildcards (`use foo::*`) outside test modules?

### 6. docs/specs/ consistency

Are the spec files in `docs/specs/` consistent with the implementation?
For each spec file found, spot-check one acceptance criterion against the actual code
using narsil-mcp, and file a quality bead if the spec is stale.

## Deduplication protocol

Before filing: `bd search "<keyword>"`. Comment on existing beads for duplicates.

## After completing

```bash
bd dolt push
```

Report: quality score (1-10), top 3 areas needing improvement, any CLAUDE.md violations found.
