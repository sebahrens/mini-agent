# Review: Security — mini-agent

You are conducting a focused security review of the mini-agent Rust workspace.

## Setup

1. Read `CLAUDE.md`, `ARCHITECTURE.md`, and `SPEC.md §Phase 2` (sandbox hardening).
2. Check existing beads: `bd list --limit 0 --status open && bd search "SEC:"`.
3. Survey the attack surface with narsil-mcp:
   ```
   mcp__narsil-mcp__get_project_structure()
   mcp__narsil-mcp__scan_security()                # automated security scan
   mcp__narsil-mcp__find_injection_vulnerabilities()
   mcp__narsil-mcp__check_owasp_top10()
   mcp__narsil-mcp__get_taint_sources()            # where untrusted data enters
   mcp__narsil-mcp__trace_taint("user_input")      # follow taint to sensitive sinks
   mcp__narsil-mcp__get_typed_taint_flow()
   ```

## Bead filing protocol

```bash
bd create --title="SEC: <short summary>" --type=bug --priority=<0-2> \
  --description="Location: <file:line from narsil-mcp>
Attack vector: <how an attacker exploits this>
Evidence: <code snippet>
Impact: <data loss, sandbox escape, privilege escalation, etc.>
Fix: <concrete mitigation>
Verification: <how to confirm the fix>"
```

Priority: 0=critical (sandbox escape), 1=high (data exposure), 2=medium.

## Security vectors to investigate

### 1. JS sandbox escape

The critical security boundary: JS code must not escape the sandbox.

```
mcp__narsil-mcp__find_call_path("eval", "Sandbox")    # does eval reach sandbox?
mcp__narsil-mcp__trace_taint("js_code")               # follow JS code to execution
mcp__narsil-mcp__find_symbols("require")              # must NOT be exposed
mcp__narsil-mcp__find_symbols("import")               # must NOT be exposed
mcp__narsil-mcp__find_symbols("fetch")                # must NOT be exposed without Phase 2
```

- Can JS code call `require()` or `import()` to load arbitrary modules?
- Can JS code access the filesystem directly (other than via `read_file`/`write_file` host globals)?
- Can JS code escape the memory limit via creative allocation patterns?
- Can a deeply recursive JS function bypass the stack limit and segfault the process?
- Can JS code access the `JsTool`'s `mpsc::Sender` or other Rust state directly?

### 2. Command injection

```
mcp__narsil-mcp__find_injection_vulnerabilities()
mcp__narsil-mcp__find_call_path("spawn", "std::process::Command")
mcp__narsil-mcp__trace_taint("cmd_string")
```

- Is the `spawn()` host global's `cmd` argument validated before execution?
- Can `args[]` contain shell metacharacters that are interpreted by a shell wrapper?
- Is `Sandbox::wrap_command` called for every subprocess spawned from JS?
- Is there any path from LLM-generated content to `std::process::Command` that bypasses `Sandbox`?

### 3. Path traversal

```
mcp__narsil-mcp__trace_taint("file_path")
mcp__narsil-mcp__find_call_path("read_file", "std::fs")
```

- Does `read_file(path)` allow reading outside the workspace (e.g. `../../etc/passwd`)?
- Does `write_file(path, content)` allow writing outside the workspace?
- Are symlinks resolved before the workspace boundary check?

### 4. Permission system integrity

```
mcp__narsil-mcp__find_callers("check_perm")
mcp__narsil-mcp__find_references("PermCheck")
```

- Is there any tool execution path that skips `check_perm`?
- Can the permission state be corrupted between check and use (TOCTOU)?
- Are deny rules checked before allow rules?

### 5. Dependency supply chain

```
mcp__narsil-mcp__check_dependencies()
mcp__narsil-mcp__check_licenses()
mcp__narsil-mcp__generate_sbom()
```

- Any known-vulnerable crate versions in Cargo.lock?
- Any crates with unexpected build scripts that could execute code at compile time?

## Deduplication protocol

Before filing: `bd search "<keyword>"`. Add comments to existing beads for duplicates.

## After completing

```bash
bd dolt push
```

Report: attack surface summary, top 3 risks by severity, any sandbox escape vectors found.
