# Review: Debate (Design Challenges) — mini-agent

For JavaScript runtime review, use **Phase 6 security invariants (canonical)** in
`docs/specs/phase-6-brokered-js-runtime.md` as the authority. Check current callers and behavior;
retired Phase 1 symbols are not missing implementation requirements.

You are a Tier 3 reviewer examining design decisions that have competing valid interpretations.
Your role is to surface genuine tradeoffs, flag premature decisions, and challenge assumptions —
not to find bugs or file implementation tasks.

This domain runs after Tiers 1 and 2 so you have the full picture of what's been found.

## Setup

1. Read `CLAUDE.md`, `ARCHITECTURE.md`, `SPEC.md`, and `AGENTS.md` fully.
2. Read all open beads from prior review domains: `bd list --status open --limit 0`.
3. Read the docs/specs/ files to understand design decisions already made.
4. Use narsil-mcp to verify design claims against actual implementation:
   ```
   mcp__narsil-mcp__get_project_structure()
   mcp__narsil-mcp__get_call_graph("JsTool")
   mcp__narsil-mcp__find_callers("Runtime::new")
   mcp__narsil-mcp__get_complexity()
   ```

## Bead filing protocol

```bash
bd create --title="DEBATE: <design question>" --type=task --priority=2 \
  --description="Design question: <the tradeoff or ambiguity>
Option A: <first position and its rationale>
Option B: <second position and its rationale>
Evidence from code: <narsil-mcp output or file:line showing current choice>
Recommendation: <which option is better and why>
Decision needed from: <human | can be resolved in code>
Impact if wrong: <consequences of choosing the inferior option>"
```

## Design debates to investigate

### 1. Fresh-runtime cost

A fresh runtime per request is a resolved containment contract. Measure current worker startup,
trusted-bytecode loading, and request execution separately before proposing an optimization.

- Can trusted preparation be reused without retaining request-local QuickJS state?
- Does the current benchmark represent both cold and reused-process paths?

### 2. Shared worker supervision

The parent shares one serialized worker supervisor across tool rebuilds. Evaluate queueing and
retirement policy within that contract; an in-process thread pool is not the current design.

- Does the reuse policy balance startup cost against resource exhaustion and idle cleanup?
- Do queued requests retain independent cancellation and deadlines?

### 3. Deadline coordination

The total invocation deadline includes parent effects and permission waits. Worker interrupts and
the parent watchdog serve different failure boundaries.

- Can shorter operation limits improve responsiveness without resetting the total budget?
- Are worker-reported resource faults distinguishable from parent deadline expiry?

### 4. Closed host errors

Brokered effect failures carry closed error codes across the ABI. Arbitrary exception text,
source, and stacks must not become model-visible diagnostics.

- Can callers distinguish actionable closed errors without leaking contents or secrets?
- Do synchronous and promise failures preserve the same bounded contract?

### 5. Skill identity and discovery

Identity version 2 uses the full SHA-256 of the canonical execution/discovery payload, ABI,
and structured capability scopes. A source-only or truncated hash is not an alternative contract.

- Are retrieval and repair decisions bound to the exact immutable identity?
- Can discovery metadata change without the required identity and verification updates?

### 6. JS vs Rust for tool implementation

The spike chose JS as the scripting layer for tool logic. Debate:

- For simple filesystem operations, is JS indirection worth the measured worker and runtime cost?
- Should the host globals be richer (exposing more Rust functionality) to reduce JS code complexity?
- Is there a class of tools that should always be pure Rust and never JS?

## Rules

- File beads for genuine design ambiguities, not implementation choices that are already clearly correct.
- Do NOT file beads that simply rehash the conclusions already in ARCHITECTURE.md.
- Do NOT propose changes that violate the resolved decisions in AGENTS.md invariants.
- DO flag cases where the implementation diverges from the documented design decisions.

## After completing

```bash
bd dolt push
```

Report: count of unresolved design debates, which ones require human decision vs can be resolved in code.
