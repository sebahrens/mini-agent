# Built-in persona prompt review (2026-10-01)

## Scope

This review covers the nine compiled-in specialist personas in `data/agents/`.
They are usable as main-agent personas, but their bundled contract remains a
read-only, source-backed investigation contract so selecting one cannot imply
authority that the runtime did not grant.

## Research basis

Exa searches were constrained to primary vendor and standards sources. The
resulting guidance consistently favors:

- outcome- and constraint-oriented instructions, with explicit evidence and
  deliverable requirements, rather than a long prescribed reasoning script
  ([OpenAI prompt guidance](https://developers.openai.com/api/docs/guides/prompt-guidance));
- clear roles, scoped tool behavior, and explicit output structure
  ([Anthropic prompting best practices](https://docs.anthropic.com/en/docs/build-with-claude/prompt-engineering/system-prompts));
- small, composable agent patterns and observable stopping conditions
  ([Anthropic, Building effective agents](https://www.anthropic.com/engineering/building-effective-agents));
- deliberate curation of finite context instead of indiscriminate checklists
  ([Anthropic, Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents));
- source-to-sink reasoning and separation of manual findings from automated
  leads in security review
  ([OWASP Secure Code Review Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Secure_Code_Review_Cheat_Sheet.html)); and
- constraint, trade-off, and verification framing for cloud decisions
  ([Azure Well-Architected security checklist](https://learn.microsoft.com/en-us/azure/well-architected/security/checklist)).

## Findings and changes

All nine prompts already had strong caveats-first ordering, task-scoped
checklists, specialist boundaries, and concrete return contracts. The common
gap was epistemic labeling: most prompts asked for evidence but did not
consistently require the report to separate observations, inferences, and
unverified claims.

The prompts now add short, domain-specific evidence disciplines:

| Persona | Added emphasis |
| --- | --- |
| `azure-cloud-architect` | Constraint table, requirement-to-component traceability, current-documentation verification |
| `informatica-mapplet-to-fabric-sql` | Source-to-target traceability and explicit treatment of ambiguous engine semantics |
| `node-typescript-maintainer` | Narrow evidence path and repository-native verification commands |
| `python-maintainer` | Narrow evidence path and interpreter/platform qualification |
| `rust-async-concurrency` | Ownership/cancellation evidence and separation of timing hypotheses |
| `rust-maintainer` | Narrow evidence path and compilation/target qualification |
| `rust-security-review` | Baseline-versus-diff scope, attacker model, reachability-based severity |
| `rust-unsafe-code-audit` | Per-operation proof obligations and reachable-precondition standard |
| `vscode-extension-developer` | Manifest-to-handler-to-artifact trace and host-dependent qualification |

Each return contract now asks for confidence or remaining verification while
preserving the existing caveats-first structure. The prompts deliberately do
not add chain-of-thought requests, universal whole-repository checklists, or
commands that bypass repository instructions.
