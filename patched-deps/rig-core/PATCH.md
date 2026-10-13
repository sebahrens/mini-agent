# mini-agent Rig 0.40 bounded application backports

This tree is the crates.io `rig-core` 0.40.0 archive, SHA-256
`d8731dd5532b3a12ce1613af73073fb2051ef750f50c504778c21d55ae933cac`, with `src/http_client/mod.rs`, `src/providers/openrouter/completion.rs`, and the native Anthropic `completion.rs`/`streaming.rs` changed in the upstream source. The normalized
`Cargo.toml` additionally declares an empty `[workspace]` so direct SDK tooling stays
independent of ancestor workspaces; `Cargo.toml.orig` remains byte-for-byte upstream.
The upstream license, authors, original manifest, and
`.cargo_vcs_info.json` remain intact.

The changed symbols are `Error::non_success_status`, `Error::non_success_body`,
the private `nested_http_response` helper, and the public compile-time contract marker
`MINI_AGENT_HTTP_RESPONSE_SOURCE_CHAIN`. A boxed transport error can carry
request-owned metadata and retain a canonical Rig HTTP error as its source.
The helper searches at most 32 source links for that canonical non-success
response. Finding it lets the existing `CompletionError::from_stream_transport`
retain the complete error instead of flattening it to a message. Errors without
such a source retain upstream behavior. The HTTP metadata patch leaves provider, hook, budget, tool dispatch,
response parsing, and native transport behavior unchanged.

Upstream Rig 0.42 preserves HTTP response headers directly in
[PR #2333](https://github.com/0xPlaygrounds/rig/pull/2333). A full upgrade currently
conflicts with mini-agent's ONNX dependencies and replaces the unified hook API;
this bounded backport permits Retry-After support without that migration.

The application regressions use real HTTP 429/529 responses, streaming retry
recovery, concurrent requests and absent/malformed hints. The SDK tests cover
source-chain status/body preservation, non-HTTP fallthrough, and cycle bounds.
Keep this tree outside the generated `vendor/` directory so Corresponding Source
assembly retains the patch while vendoring registry dependencies.

The application workspace leaves `members` empty (the root package remains an implicit member)
and excludes this SDK path. An explicit `members = ["."]` overrides nested exclusions in Cargo
([upstream issue #6745](https://github.com/rust-lang/cargo/issues/6745)); avoiding that declaration
keeps this dependency out of workspace membership, its dev graph out of the application lockfile,
and its license included in the third-party inventory.

## OpenRouter output-token limit correction (mini-agent-i7ro6.96)

`OpenrouterCompletionRequest::try_from(OpenRouterRequestParams)` now copies the
host-provided `CompletionRequest.max_tokens` instead of discarding it as `None`.
Absent caps remain omitted, and the existing explicit additional-parameter
precedence is unchanged. The application filters raw token-cap overrides on
passive title/formatting requests, keeping their typed cap authoritative.
This correction applies to primary and compaction requests too. The SDK test
covers absent, 64-token, and 1,000-token conversion; the application fake HTTP
fixture verifies primary and format-repair requests carry the exact requested
64/1,000 caps. No dependency version, original archive checksum, license, or
source-input hashing policy changes.
The [OpenRouter parameter contract](https://openrouter.ai/docs/api_reference/parameters)
identifies `max_tokens` as the optional generated-token limit, including reasoning
on most routes; the backport preserves that documented request field.

## Native Anthropic output configuration and omitted thinking (mini-agent-i7ro6.46)

`AnthropicCompletionRequest::try_from` now extracts raw `output_config` into one
private `OutputConfig` through `resolve_output_config`. The caller's typed
`CompletionRequest.output_schema` owns `format`, while validated native effort
coexists in that object. This avoids duplicate serialized keys silently replacing
the typed schema in streaming and blocking requests. Other explicit native
output-config members remain caller parameters; the application's separate
passive-request sanitizer still controls auxiliary capabilities and formatting.
Malformed objects/effort produce static request errors without exposing values.
Absent output-config controls remain absent.

Native `streaming::handle_event` emits a completed thinking block when its
signature is nonempty even if its text is empty. Empty display deltas are not
emitted as unsigned placeholder blocks; an interrupted signature is never
completed without the block-stop event. These changes preserve native omitted
thinking rather than forcing summarized display. Generic reasoning aggregation,
Responses identity, HTTP source errors, tools, hooks and caps are unchanged.

The application's `provider::effort_tests` uses real loopback native SDK HTTP
for schema/effort composition on both request paths, effort-only/absent controls,
static pre-HTTP failures, and completed versus interrupted empty signed blocks.
The official [effort](https://platform.claude.com/docs/en/build-with-claude/effort)
and [thinking](https://platform.claude.com/docs/en/build-with-claude/thinking)
contracts require these shapes. This backport alone does not establish complete
application history replay, prefix preservation or live effort control.
### Native cache usage snapshot preservation (mini-agent-i7ro6.71)

`anthropic::completion::{Usage, CacheCreation, validate_cache_creation,
validate_usage_counts}` and `anthropic::streaming::PartialUsage::{from_start,
apply_delta}` retain reported cache reads/writes from `message_start` when
optional cumulative `message_delta` fields are absent. Explicit zero overwrites;
a changed total without a replacement tier split clears the old split. The
optional native `cache_creation` counts remain distinct from a requested TTL.
Blocking and streaming responses reject inconsistent split sums and overflowing
usage with bounded static errors. This stage preserves the native raw split;
the application generic usage/pricing/ledger tier extension is separate and
no one-hour configuration is enabled by this backport alone.

Primary semantics: [Anthropic SDK accumulation](https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/lib/streaming/_messages.py)
and [cache usage fields](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).
Production consumer tests in `src/provider_cache_tests.rs` cover initial cache
counts with output-only delta, cumulative updates/explicit zero/missing split,
invalid and overflowing metrics, and blocking split preservation.

### Native completed assistant projection (mini-agent-i7ro6.46)

`streaming::StreamingCompletionResponse::native_anthropic_stream` selects a
private native policy: reasoning deltas remain display events but do not create
unsigned replay placeholders. Completed signed/text and redacted blocks retain
their arrival order. `anthropic::streaming::GenericCompletionModel::stream`
selects this policy; other provider constructors retain existing aggregation.
`agent::run::streamed::StreamedTurnAssembler::finish_native_anthropic` keeps the
existing tool validation/identity bookkeeping and commits the normalized native
choice without generic reasoning/text/tool regrouping. The streaming driver
uses it only for unrecovered native turns; repaired calls keep their existing
canonical behavior and cannot claim the original provider sequence.

Actual loopback probes in `src/provider_effort_replay_probe.rs` compare the
next SDK HTTP body against canonical thinking-first/redacted and adversarial
interleaved source blocks. They also prove nonempty thinking is not duplicated,
interrupted signatures do not complete, and generic delta-only aggregation is
unchanged. This backport does not deliver application Session replay/frontier,
mutable memory placement, or effort/session controls by itself.

## Canonical tool-call hook identity (partial mini-agent-i7ro6.46)

`StepEvent::ToolCall` additionally borrows `tool_id` directly from the assistant's
`ToolCall.id` in `run_single_tool`. The leaf, erased, and nested `HookStack`
resolution paths forward it unchanged, including argument rewrites and skipped
calls. The optional `tool_call_id` still means `ToolCall.call_id`; the internal
Rig event ID is unchanged. Native Anthropic calls can legitimately have a
canonical tool-use ID and no optional provider call ID. This additive event
field provides exact correlation without guessing from tool names or callback
order. No tool execution, Flow, result, billing, or transport behavior changes.
The application native capture fixture verifies both real canonical tool-use
IDs, absent optional IDs, and distinct internal IDs on actual SDK calls.
