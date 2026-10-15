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

## Non-owning tool-server handle (mini-agent-i7ro6.59)

`ToolServerHandle::downgrade` and `WeakToolServerHandle::upgrade` expose only a
weak reference to the existing tool-server state. A registered discovery tool
can publish an exact selected tool through an upgraded handle during its call
without creating a server-to-tool ownership cycle. No new registry, retrieval,
lock or dispatch policy is introduced. Existing structured dispatch clones the
tool under a scoped read lock and releases that lock before awaiting the tool,
so publication during a call can acquire the ordinary write lock. Application
tests exercise bounded publication during actual structured dispatch and
confirm the weak reference expires after the owning server is dropped.

`ToolServerHandle::append_toolset_if` extends that same seam with a synchronous
admission predicate under the existing write lock. The predicate runs after
waiting for the lock; rejection publishes no definitions. The accepted batch
is installed without another await or partial publication. The predicate must
not re-enter the server. Application controls hold the real write lock during
actual structured discovery, then cancel, drop or revoke the pending request
and assert zero definitions and activation changes; a current request publishes
normally. The existing prepared-request allowed-name snapshot remains unchanged
and prevents later publication from widening an already received model response.

## Native deferred tool references (mini-agent-i7ro6.59)

`CompletionModel::deferred_tool_loading` defaults to generic dynamic discovery.
Native Anthropic models use their actual requested model identifier to select
supported immutable deferred declarations or eager fallback. A model retains
the host-captured catalogue through `with_deferred_tools`; streaming and blocking
request preparation exclude activated duplicate definitions, append identical
native deferred declarations, and reject conflicting raw prefix controls before
HTTP. This does not register executable tools or preserve unrelated mutations.

A successful executed search attaches bounded `ToolReferences`. The SDK runner
projects their names only after the result hook keeps the result, retaining its
ordinary text and exact tool-use ID. Rewrite/terminate suppress projection.
Reserved generic Text replay metadata bridges those typed names to native
`tool_reference` blocks. The ordinary multimodal output parser creates only
text/images, never this metadata. Replay references outside the captured
catalogue fail before HTTP; no reference installs an executable or a grant.
The other providers' search behavior remains unchanged. Existing completed
native reasoning projection and output-config merge are preserved.

The official [custom search contract](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool)
requires every reference's definition at top level. Application fake-wire tests
verify fixed declarations, normal Ask allow/deny, hidden-call and invalid-query
refusal, result redaction/termination, forged textual references, and pre-HTTP
prefix collisions. Inert signatures are continuity witnesses, not cryptographic
or paid-server acceptance. Full application replay/frontier work remains separate.

## Prepared request observation and canonical tool-result IDs (mini-agent-i7ro6.63)

`StepEvent::CompletionRequestPrepared` borrows the actual typed completion request
after request patches, tool constraints and normal preparation. Its default hook
behavior is no-op. It is observe-only: Continue proceeds to existing model
dispatch; Terminate follows ordinary prompt cancellation. It does not imply the
typed request is final provider JSON when raw additional parameters override
transcript fields. Streaming and blocking calls use the same preparation seam;
no extra request, usage observation or receipt is created.

`PreparedCompletionRequest::build` retains the original public builder API and
validation, exposing a private built model/request bundle to the driver.
Canonical `StepEvent::ToolResult.tool_id` borrows `ToolCall.id` from the exact
invocation. `internal_call_id` and optional provider `tool_call_id` retain their
meanings; no name/argument/order matching or replay/grant authority is introduced.
Concrete, erased and stacked hooks forward these values unchanged.

Native tool invocations receive the existing run-owned HookContext scratchpad
as an additive `ToolCallExtensions` value. Other extensions remain intact;
there is no ambient registry. Instruction receipts and reviewed state remain
inside that runner's scratchpad and reflect only effective post-hook result
bytes. Successful loading acknowledges nothing. Application loopback controls
cover denied mutation -> canonical next request -> accepted response -> effect,
copied sibling histories, redaction, spill, pruning, cancellation and pre-dispatch
authority revocation. Two actual consumer paths use explicit 1 MiB Rust threads.

## Generic cache-write tier carrier (mini-agent-i7ro6.71)

`completion::{CacheCreationUsage, Usage}` additionally carries an optional
reported five-minute/one-hour write split. Checked aggregation/difference keeps
a positive unknown component unknown; a complete zero split does not manufacture
reported usage. Native blocking and streaming `GetTokenUsage` mappings preserve
the already-validated raw split. Other provider/embedding constructors explicitly
use `None`, without inferring TTL information from names or request parameters.
No request TTL, tool execution, hook ordering, transport, or pricing calculation
is changed by this SDK carrier itself. The application's fake native HTTP tests
exercise both mappings and cumulative/zero/missing/invalid metrics.

## Native malformed-event diagnostics

Anthropic's streaming JSON decoder reports a static `ResponseError` for a
nonempty malformed event. Neither raw SSE data nor serde's value-bearing parse
diagnostic is display data: both can include signatures or redacted thinking.
This preserves the existing error-yield and empty-event behavior, and changes
no transport status/source chain, usage observation, or other provider decoder.
The application fixture exercises malformed signature/redacted/type-tag events
through real local HTTP, requires failure and zero tool effects, and checks both
Display and Debug diagnostics without accepting an opaque completed block.

## Retained HTTP backend access for bounded model metadata

The immutable `Client::http_client()` getter lets mini-agent reuse the actual
captured request endpoint/auth/version headers and configured HTTP backend for
bounded listing GETs. It adds no client state and leaves completion request,
streaming, native projection and cache-control semantics unchanged. The host's
listing adapter owns pagination/body/deadline limits and redacted errors.

## Lower native replay inspection (application delivery still separate)

The native Anthropic model exposes its actual endpoint, manual-marker policy,
and the existing streaming request-body factory through `replay_endpoint`,
`replay_manual_cache_markers`, and `replay_request_body`. These helpers perform
no network work and preserve deferred declarations and cache normalization.
The application compares fully prepared bodies against bounded completed native
content; imported compatibility data creates no runtime owner or permission.
Generic provider order, Responses identity, tool dispatch and usage remain unchanged.

### Native zero-visible terminal metadata (mini-agent-i7ro6.46.3)

The native Anthropic streaming response retains a private, nonserialized flag
only when the actual parsed `message_delta.stop_reason` equals `end_turn`.
`GetTokenUsage` defaults this accessor to false for every other provider. The
stream assembler carries it to the existing canonical `ModelTurnFinished`
event even when no display-text final item is forwarded. Blocking completion
conservatively reports false. Usage, tool dispatch and generic empty-answer
continuation remain unchanged.

The application uses this metadata only in an actual native invocation whose
bounded capture has a valid prepared body/domain, completed signed or redacted
content, no tool calls or visible answer, and a current healthy frontier. A new
prepared request resets eligibility. Capture failure, truncation, paused or
missing stop reasons, incomplete signatures, and unbound/other protocols do not
become completion. The actual headless and interactive actor tests retain the
opaque blocks without inventing answer text; a private reload receives an
actor-backed saved boundary separately. The official [stop-reason contract](https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons)
distinguishes natural `end_turn` from token limits and paused tool work.
This carrier is not saved runtime ownership, permissions, or billing metadata.

Image-read tool results preserve literal hybrid response text and use native
OpenAI Responses function-output image content. Text-only tool-role transports
(Chat Completions, OpenRouter, Ollama, Gemini) append image user content only
after the complete correlated tool batch. Original text results and IDs remain
in place; ambiguous/incomplete image batches fail before HTTP. Anthropic's native
image tool-result conversion is unchanged. Host canonical image validation,
permission and native capture limits remain outside this adapter.

## Vertex Claude native rawPredict (mini-agent-i7ro6.81.2)

`anthropic::client::{AnthropicExt, VertexRoute}` carries an optional fixed
project/location route. Existing Anthropic-compatible extensions keep the
original Messages mode. The typed completion and streaming composers share
safe route construction, move the fixed model to rawPredict/streamRawPredict,
set `anthropic_version` to `vertex-2023-10-16`, and omit the body's model.
Model/version overrides fail before transport. `replay_request_body` and
`replay_endpoint` expose the same final composition used by actual streaming
HTTP, rather than a body transformed later by an authentication adapter.
The response decoder, canonical native block ordering, hooks, cancellation and
accounting retain their existing owners. This routing structure grants no
credential or imported publication authority.

Local application controls in `src/provider_vertex_tests.rs` exercise ADC and
supplied bearer wire requests, actual tools/SSE/usage, fixed-route override
rejection and destination isolation. Native domain tests retain the existing
saved schema while deriving Vertex's absent body model from its exact path.
Official contract: https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/use-claude


### Host-owned Copilot authentication

Copilot's `ExternalAuthenticator` is a cloneable caller-owned async callback
returning the existing `AuthContext`; `Authenticator` dispatches it directly,
bypassing the SDK's editor client registration, ambient cache, device flow and
credential HTTP. This retains the existing chat/Responses request converters,
model classifier, tool loop and usage owners. `CompletionModel::uses_responses`
exposes the converter selected by the SDK so host instructions match the actual
wire protocol. The application strips borrowed editor identity headers and
supplies its own identity and bounded credential acquisition in `src/auth/copilot.rs`.
No other provider uses this callback and no new detached task is introduced.

Copilot's own chat decoder validates prompt/total/completion/cache counts before
unsigned subtraction or stream normalization. Malformed SSE usage is an error,
rather than disappearing from accounting; the blocking converter rejects the
same inconsistencies. OpenAI's shared decoder and Responses conversion remain
unchanged. Local actual-SDK controls cover both streaming tool routes, blocking
and streaming malformed usage, redirects, listing and auth destination isolation.
