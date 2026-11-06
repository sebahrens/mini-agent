# Subagent return channel: validated text by value, evidence by handle

Beads: mini-agent-ef0m2 (epic), mini-agent-26mj0 (evidence store), mini-agent-ruvs6 (citations and
Evidence section), mini-agent-i2t78 (`task_evidence`), mini-agent-nrsow (this record). Follow-ups:
mini-agent-g442v (harness evaluation with handles on and off), mini-agent-6m0om (opt-in inline image
lane). Decision: **keep the narrow, quoted, host-validated text report as the only thing a child returns
by value, and add host-minted evidence handles that the parent dereferences on demand.** Raw tool
results and images never travel inline in `task` output; an inline image lane, if added, is opt-in
with a default of zero.

## Contract

- A child starts with a fresh context, read-only by default, at depth 1 (`TaskTool` is never in a
  child tool set), under the parent's permission checker. Permissions are inherited and never widened.
- Its final text must satisfy the host-owned Findings / Unverified / Coverage contract. One tool-free
  repair turn is allowed. A report that still fails is withheld from the parent model and replaced by a
  fixed host report; the raw text is kept for the user only (`/tasks result <id>`).
- Accepted text is quoted line by line between `[subagent output begins]` and `[subagent output
  ends]`, and capped per child and per call.
- After each child run the host walks the child's own conversation and records every tool result in
  tool-call order as `E1..En`, with text, validated image parts, tool name, a sanitized argument
  summary, size, MIME type, dimensions and estimated tokens. Content goes to the session's private
  tool-output directory. Only bounded metadata stays in memory: 64 records per child, 256 KiB of text
  per record, 16 MiB of content per child, and 64 children per session, with the oldest evicted and
  deleted. The store is never deserialized, so a restarted session has no handles.
- A Findings entry may end with `[evidence: E3, E7]`. The validator rejects unknown or malformed IDs
  as a contract violation, which triggers the existing repair turn. An uncited high-confidence finding
  only gets a soft note. Host normalization never passes an unverifiable citation through.
- After the closing fence the host appends `## Evidence`: the per-child `task_id` handle plus metadata
  rows for cited records only (id, tool, target, size, MIME, estimated tokens), never content. The
  target is child-chosen argument text, so it is stripped of Markdown, table and marker syntax and
  bounded to 60 bytes. A withheld report still gets the first 16 index rows, because those are
  host-recorded tool output, not child prose. The section is dropped whole if it would use more than
  half of the remaining output budget.
- The parent-only `task_evidence(task_id, id, offset?, limit?, max_px?)` returns one record as an
  ordinary tool result. Text is paged like `read`, and the tool is self-bounded. Images use the
  envelope that `read` and MCP already use, so existing provider bridges, persistence and spill
  exemptions apply. Images are admitted by the parent model's own image support. Each call passes the
  parent's permission checker, and a recorded `read` is re-judged by the parent's read rule for its path.
- `task_evidence = false` restores the text-only report: nothing is recorded, children are not told
  about citations, no Evidence section is added and the tool is not registered.

## Rationale

**Context rot.** Performance degrades with input length even on simple tasks. On LongMemEval,
focused prompts of about 300 tokens beat full prompts of about 113k tokens for every model tested
([Chroma](https://www.trychroma.com/research/context-rot)). Anthropic recommends that each subagent
return "a condensed, distilled summary of its work (often 1,000-2,000 tokens)"
([context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)).
The field has converged on returning only the child's final text or a schema-typed summary. Examples
are [Claude Code](https://code.claude.com/docs/en/sub-agents),
[Codex](https://learn.chatgpt.com/docs/agent-configuration/subagents), OpenAI's `Agent.as_tool()`
([agent.py](https://github.com/openai/openai-agents-python/blob/main/src/agents/agent.py)) and Google
ADK's `AgentTool`, which turns non-text parts into empty strings
([agent_tool.py](https://github.com/google/adk-python/blob/main/src/google/adk/tools/agent_tool.py)).
Every inline image is billed again on each later parent turn, at about 1.5k tokens at Claude's
standard tier and up to 4,784 at the high-resolution tier
([Anthropic vision](https://platform.claude.com/docs/en/build-with-claude/vision)). Images also
invalidate prompt caches
([prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)).

**Injection quarantine.** A quarantine holds only when the quarantined component's output is
constrained ([design patterns, arXiv 2506.08837](https://arxiv.org/html/2506.08837);
[CaMeL, arXiv 2503.18813](https://arxiv.org/abs/2503.18813)). Raw tool results or images passed
inline would carry fetched pages, MCP output and text embedded in images straight into the privileged
parent, framed as the output of a trusted delegation. When the parent dereferences a handle instead,
it gets the same bytes it would get by calling the tool itself. Those bytes arrive as a tool result,
where untrusted content belongs
([Anthropic tool use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)),
and only when the parent decides to pay for them. Because the host records the content, the child
cannot forge it.

**Verification gap.** In MAST, about 24% of multi-agent failures were verification failures
([arXiv 2503.13657](https://arxiv.org/html/2503.13657v3)). Handoff summaries keep facts but drop the
metadata that governs their use ([arXiv 2608.29028](https://arxiv.org/abs/2608.29028)). In one study,
verbatim, checkable "signed manifest" handoffs cut unsafe commits from 69.4% to 9.0%, and a
deterministic verifier beat an LLM checker
([arXiv 2611.07483](https://arxiv.org/abs/2611.07483)). Anthropic's multi-agent system passes
"lightweight references back to the coordinator"
([multi-agent research](https://www.anthropic.com/engineering/multi-agent-research-system)), and ADK
keeps large data in an artifact store
([Google Developers Blog](https://developers.googleblog.com/architecting-efficient-context-aware-multi-agent-framework-for-production/)).
Host-minted IDs turn a `[confidence: high]` claim into one that can be checked deterministically
without repeating the child's work. The preprint magnitudes come from synthetic tasks, so treat them
as directional.

## Alternatives rejected

- **Raw passthrough by value** (tool results or images inline in `task` output). It would dissolve the
  quarantine and spend the parent's context on the child's say-so.
- **Free-text file references only.** The parent can already re-`read` a path a child names. That
  covers neither MCP, `web_fetch`, `js` or shell results nor provenance, and a child can name a path it
  never read.
- **Resizing images in `task_evidence`.** The build intentionally links no image decoder
  (`docs/agent/IMAGE_READ.md`), so `max_px` is a ceiling that omits oversized images rather than a
  resampler. Downscaling belongs with the inline image lane (mini-agent-6m0om) if that lane is adopted.

## Open points

- No published benchmark compares summary-only, summary with references, and raw returns on the same
  tasks. mini-agent-g442v measures parent accuracy and token cost with `task_evidence` on and off
  before any inline image lane is enabled.
- Image support on the Copilot, Azure, Bedrock and Vertex routes is unverified, so images stay gated on
  the same `image_read_support` capture that `read` uses.
- The host-authored `## Isolated worktree` section of write children is still appended inside the
  child's quoted output, not after the fence. This record does not change that.
