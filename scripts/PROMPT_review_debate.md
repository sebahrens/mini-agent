# Review: Debate (Design Challenges) — mini-agent

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

### 1. Runtime lifecycle: per-step vs per-request

ARCHITECTURE.md mandates fresh `Runtime` per step (~500μs overhead). But:
- What if the agent runs thousands of steps in one session — does 500μs × 1000 = 500ms matter?
- Is there a safe way to pool Runtimes without the OOM risk? (QuickJS issue: allocator state after OOM)
- Does the current spec document WHY reuse is forbidden, or just say it is?

Use narsil-mcp to check if there's any comment or doc explaining the OOM invariant in code.

### 2. Thread-per-JsTool vs thread pool

Current design: one OS thread per `JsTool` instance. Alternative: a pool of N JS threads
shared across all JsTool instances.

- Does one-thread-per-tool scale if the agent has many tool invocations in parallel?
- Would a thread pool require Runtime reuse (breaking invariant 3)?
- The `!Send` constraint eliminates shared state — is the per-tool thread the only safe model?

### 3. Interrupt handler vs tokio::time::timeout

ARCHITECTURE.md: interrupt handler fires only during JS bytecode; blocking host calls
need `tokio::time::timeout`. This creates two timeout mechanisms for one timeout budget.

- Should the total step timeout be `STEP_TIMEOUT` for everything, or separate budgets?
- What happens if JS runs 25s of pure bytecode and then calls `spawn()` which takes 10s?
  (Total = 35s, but STEP_TIMEOUT = 30s — does the interrupt catch it or not?)
- Is the current design correct, or does it have an exploitable window?

### 4. Host global API: error model

`read_file` returns `Result<string, string>` and `write_file` returns `Result<null, string>`.
Alternative: throw JavaScript exceptions instead of returning Result.

- Does returning strings-as-errors give LLMs better feedback than JS exceptions?
- Does the current model interact correctly with the microtask queue drain?
- Is there a risk of error messages being confused with successful string output?

### 5. Skill library content-addressing

Skills are addressed by `sha256(source)[..16]` — a 64-bit prefix. Debate:

- Is 64 bits of collision resistance sufficient for a skill library that could grow to thousands?
  (Birthday bound: 50% collision at ~4 billion skills — overkill or not?)
- Should the ID be the full 256-bit hash, or is the 16-hex truncation in SPEC.md intentional?
- Does truncating the ID change behavior if two skills have the same prefix?

### 6. JS vs Rust for tool implementation

The spike chose JS as the scripting layer for tool logic. Debate:

- For simple filesystem operations, is JS indirection worth the ~500μs Runtime creation cost?
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
