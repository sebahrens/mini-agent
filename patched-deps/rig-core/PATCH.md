# mini-agent Rig 0.40 bounded application backports

This tree is the crates.io `rig-core` 0.40.0 archive, SHA-256
`d8731dd5532b3a12ce1613af73073fb2051ef750f50c504778c21d55ae933cac`, with `src/http_client/mod.rs` and `src/providers/openrouter/completion.rs` changed in the upstream source. The normalized
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
