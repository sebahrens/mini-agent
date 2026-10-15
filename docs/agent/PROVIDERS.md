---
description: "Configure LLM providers in mini-agent: OpenRouter, OpenAI-compatible endpoints, Azure OpenAI, AWS Bedrock, Vertex AI Claude, GitHub Copilot, Anthropic, Gemini, Ollama, custom headers, and prompt caching."
---

# Providers

mini-agent supports nine built-in providers and allows custom provider
definitions for OpenAI-compatible endpoints.

## Built-in Providers

| Provider   | Config name         | Default env var for API key |
| ---------- | ------------------- | --------------------------- |
| OpenRouter | `openrouter`        | `OPENROUTER_API_KEY`        |
| OpenAI     | `openai`            | `OPENAI_API_KEY`            |
| Azure OpenAI | `azure` / `azure-openai` | `AZURE_OPENAI_API_KEY` |
| AWS Bedrock (Claude) | `bedrock` / `aws-bedrock` | AWS SDK chain or `AWS_BEARER_TOKEN_BEDROCK` |
| Vertex AI (Claude) | `vertex` / `vertex-ai` | Google ADC or `VERTEX_ACCESS_TOKEN` |
| GitHub Copilot | `copilot` / `github-copilot` | saved login, `GITHUB_COPILOT_API_TOKEN` or `COPILOT_GITHUB_TOKEN` |
| Anthropic  | `anthropic`         | `ANTHROPIC_API_KEY`         |
| Gemini     | `gemini` / `google` | `GEMINI_API_KEY`            |
| Ollama     | `ollama`            | (no key required)           |

Select a provider via the config file, the `--provider` CLI flag, or the
`ZS_PROVIDER` environment variable:

```
mini-agent --provider anthropic
```

## Sign in with ChatGPT

OpenAI documents [ChatGPT plan usage for open-source and locally hosted apps](https://developers.openai.com/siwc/token-sharing-open-source).
Sign in before starting an agent, without an API key:

```sh
mini-agent --login openai
mini-agent --provider openai
```

Inside the TUI, use `/login openai`; `/login openai new` registers another
account or workspace, and `/login openai <number>` reauthorizes a registration
listed by `/provider`. The CLI equivalents are `--login-account new` or
`--login-account <number>`. The browser asks for consent. Ctrl-C cancels while
waiting for the callback; the bounded token exchange then finishes under its
owned cleanup scope. Identity and grant validation finish before the active account can change.

The selected saved login takes precedence over environment/config API keys on
the built-in `openai` route. An explicit `--api-key` still overrides it. Custom
providers and alternate base URLs never inherit subscription credentials.
Changing accounts replaces the client on the next explicit provider/login
operation; an already captured subscription client cannot borrow another
registration's tokens. `/logout openai` (or `--logout openai`) attempts to
revoke the renewable session and clears local tokens. If remote revocation
cannot be confirmed, the message points to ChatGPT Settings. API-key fallback
is available after explicit logout if a key is configured.

`src/auth/oauth.rs` implements OpenAI's [public-client registration](https://developers.openai.com/siwc/token-sharing-open-source/sign-in),
with fresh state, nonce and PKCE, a stable opaque host UUID, and an issued
client ID belonging to this app/account. It validates the ID-token RSA
signature against OpenAI's published keys, issuer, audience, expiry, nonce,
account identity and plan scopes before replacing credentials. No other
application's client ID or credential file is used. Records live below
`credentials_dir/provider-oauth`, with owner-only files, atomic replacement,
bounded reads and a cross-process lock covering rotating refresh tokens.
Revoked/expired authorization requires sign-in; it does not silently switch
to a billed API key. Signing out retains the public host/account registration
for later reauthorization.

Doctor and the setup provider list inspect a bounded, private local snapshot
under the caller's captured `credentials_dir`. They distinguish a stored
ChatGPT/Copilot login, a required sign-in, and an unavailable record; a Copilot
login marker alone is not reported as saved. The same official-route, custom
provider and explicit CLI-key eligibility used by the production factory controls
these labels. An active ChatGPT registration requiring reauthorization remains
visible instead of being mislabeled as an API-key fallback. Inspection creates no
files, locks, directories or identities, changes no permissions, and performs no
refresh or provider call. These are local presence labels, not account-health or
entitlement checks; no token or account identity is included.

The dedicated `src/auth/openai_plan.rs` request policy follows the documented
[Responses limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations):
requests use the official `/v1/responses` endpoint with `store: false`,
`stream: true`, full input history, encrypted reasoning, and local function
definitions in a developer `additional_tools` input item. Ordinary SDK
non-streaming helpers consume the actual completed SSE response, including its
reported usage, through the same route. Server-stored item references,
unsupported hosted tools and unsupported continuation parameters fail before
HTTP. Global `max_tokens` and `temperature` settings are not sent on this
subscription route; ordinary local run/tool budgets still apply. Histories
that depend on server-stored API data require a new session.

Plan allowance and optional credit use depend on the account and workspace;
manage them in ChatGPT Settings. Local `/cost` dollar estimates use captured
model rates and are not a ChatGPT subscription invoice.

### Anthropic subscription availability

There is no independently documented third-party Pro/Max client registration
flow for mini-agent. Anthropic's [developer guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account)
directs third-party apps to Claude Console API keys or supported cloud
providers. Its [ant CLI OAuth login](https://platform.claude.com/docs/en/cli-sdks-libraries/cli/authentication)
is workspace-scoped API authentication and does not establish a Pro/Max
integration contract for another client. `/login anthropic` explains this
limitation; mini-agent does not impersonate Claude Code or import its tokens.

The model is set with `--model` or `ZS_MODEL`:

```
mini-agent --provider openai --model gpt-4o
```

When a built-in provider is selected without an explicit model, mini-agent uses
these catalogued defaults: `claude-sonnet-5-5` for Anthropic, `gpt-5.5` for
OpenAI, `gemini-3.7-flash` for Gemini/Google, `deepseek/deepseek-v4-pro` for OpenRouter,
and `llama3.1` for Ollama. Direct-provider entries in the
embedded catalog always include positive input and output prices; the refresh
script omits entries whose upstream pricing is absent so the status line never
silently treats a paid model as free. OpenRouter continues to refresh its
marketplace pricing at runtime.

Startup and `/provider <name>` use the same default resolver: the configured
`model` when it belongs to that provider, a custom provider's own `model`, the
`deepseek-v4-pro` quick model when it targets that provider, another matching
quick model in alphabetical alias order, then the built-in defaults above.
An explicit `--model` or `/model` selection still overrides these defaults.

## Azure OpenAI

Select `provider = "azure"` and set `AZURE_OPENAI_ENDPOINT` to your resource URL
(for example `https://example.openai.azure.com`). A URL already ending in
`/openai/v1` also works. Set the model to your **deployment name**, rather than
assuming it is the underlying model's public ID. Azure has no shared deployment
default; configure one explicitly:

```toml
provider = "azure"
model = "my-gpt-deployment"
```

Authenticate with `AZURE_OPENAI_API_KEY`, `--api-key`, or `api_keys.azure`. These
send the `api-key` header. Alternatively supply an existing Microsoft Entra access
token in `AZURE_OPENAI_AD_TOKEN`; it sends only `Authorization: Bearer ...`.
CLI keys override environment credentials, the API-key environment variable
precedes the bearer-token variable, and environment credentials precede config
keys. `OPENAI_API_KEY` is never borrowed. Obtain/refresh Entra tokens through your
normal identity tooling; mini-agent does not log in or modify your cloud account.

Azure defaults to its v1 **Responses API**. A named route may select v1 Chat
Completions explicitly:

```toml
[custom_providers.azure-chat]
provider_type = "azure"
base_url = "https://example.openai.azure.com/openai/v1"
api_key_env = "MY_AZURE_RESOURCE_KEY"
api_style = "completions"
model = "my-chat-deployment"
```

Custom routes keep the existing credential isolation policy. Their own key
environment variable/config entry is required unless `inherit_builtin_key = true`
explicitly grants use of Azure credentials. Typed Azure auth replaces competing
custom auth headers. Both APIs preserve streaming tools, usage accounting,
reasoning controls and the existing connection/inactivity deadlines. The legacy
`/openai/deployments/...` API is a different endpoint and is rejected by this v1
kind. Existing `provider_type = "openai"` recipes retain their behavior.

The implementation in `src/provider_azure.rs` supplies tenant routing and typed
auth while reusing the SDK's OpenAI v1 converters and `ProviderHttpClient`.
[Microsoft's v1 reference](https://learn.microsoft.com/en-us/rest/api/aifoundry/azureopenai/responses)
documents the endpoint and credential headers; its
[starter guide](https://learn.microsoft.com/en-us/azure/developer/ai/get-started-azure-openai-starter-kit)
explains deployment names and Entra token scope.

## Provider Recipes

- [MiniMax](providers/Minimax.md)

## AWS Bedrock Claude Messages

Select `provider = "bedrock"`, set `AWS_REGION` (or `AWS_DEFAULT_REGION`), and
configure the exact Bedrock Claude model or inference-profile ID:

```toml
provider = "bedrock"
model = "us.anthropic.claude-sonnet-5"
```

This backend uses the current native Anthropic Messages endpoint,
`https://bedrock-runtime.REGION.amazonaws.com/anthropic/v1/messages`, with
streaming tools and usage. It supports Claude models exposed by that API; it
does not translate other Bedrock model families into Claude requests. No common
model/deployment default is assumed.

Without an API key, the official AWS SDK resolves credentials lazily and signs
each request with SigV4 for service `bedrock`. Its environment, shared-key profile,
SSO, assume-role, web-identity, ECS, and EC2 credential providers remain available.
Use your existing AWS identity setup; mini-agent does not initiate an AWS login.
Credential HTTP uses strict TLS, bounded response bytes and deadlines, and does
not inherit model/gateway headers or disabled TLS verification. Credential
acquisition is cancelled with the owning request and bounded to 30 seconds.

The optional AWS `credential_process` method is **disabled**. When using SDK
credentials, a selected profile or `source_profile` chain containing it is
explicitly rejected before SDK
credential fallback or model dispatch, even if environment keys are available.
The upstream process launcher lacks the bounded output/cancellation ownership
required here. Use one of the supported credential methods instead. This is an
AWS-specific limitation, unrelated to Anthropic/OpenAI OAuth login.

Alternatively, `AWS_BEARER_TOKEN_BEDROCK`, `--api-key`, or `api_keys.bedrock`
selects the native Messages API's `x-api-key` authentication. CLI, environment,
and config precedence matches the other providers. This source always uses exclusive `x-api-key` auth
and never inherits `ANTHROPIC_API_KEY`. Azure/OpenAI credentials are also never
borrowed.

A custom gateway must use its own configured key unless
`inherit_builtin_key = true` explicitly grants AWS credential inheritance:

```toml
[custom_providers.bedrock-gateway]
provider_type = "bedrock"
base_url = "https://gateway.example/anthropic"
api_key_env = "MY_BEDROCK_GATEWAY_KEY"
model = "my-bedrock-model-id"
```

The SDK's Anthropic request/SSE converters are reused without rewriting model,
system, tools, messages, or canonical replay data. `src/provider_bedrock.rs`
owns endpoint/auth selection, signing and the AWS credential HTTP adapter;
`src/provider_bedrock_tests.rs` covers the external contract. The existing
`ProviderHttpClient` owns dispatch, deadlines, redirect policy, request-specific
retry metadata, cancellation, and accounting.

See [AWS native Messages](https://docs.aws.amazon.com/en_en/bedrock/latest/userguide/inference-messages-api.html)
and the [AWS Rust credential chain](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credproviders.html).

## Custom Providers

Custom providers let you point mini-agent at any OpenAI-compatible API (vLLM,
LiteLLM, Ollama, local models, enterprise gateways, etc.). Define them under
the `custom_providers` key in the config file:

```json
{
  "custom_providers": {
    "local-vllm": {
      "provider_type": "openai",
      "base_url": "http://localhost:8000/v1",
      "api_key_env": "VLLM_API_KEY",
      "model": "gemma4"
    },
    "company-gateway": {
      "provider_type": "openai",
      "base_url": "https://gateway.example.com/v1",
      "model": "glm"
    }
  }
}
```

| Field                         | Type    | Description                                                                                                                                                                   |
| ----------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `provider_type`               | string  | Must be one of the built-in provider types (`openrouter`, `openai`, `azure`, `anthropic`, `gemini`, `ollama`).                                                                         |
| `base_url`                    | string  | The API base URL.                                                                                                                                                             |
| `api_key_env`                 | string  | Optional. Name of an environment variable holding the API key. Falls back to the provider-kind default if not set.                                                            |
| `api_style`                   | string  | Optional. For OpenAI-based providers: `"responses"` (Responses API) or `"completions"` (Chat Completions). Custom providers always have a `base_url`, so this defaults to `"completions"`; set it explicitly for a Responses-only endpoint. |
| `headers`                     | object  | Optional. HTTP headers to include in every request. Values support `${ENV_VAR}` expansion.                                                                                    |
| `danger_accept_invalid_certs` | boolean | Optional. Disables TLS certificate verification (MITM risk — use with care).                                                                                                  |
| `timeout_secs`                | integer | Optional. Total deadline for a request, from connect until the response body finishes. Unset by default, because a long streamed turn has no meaningful total bound.           |
| `connect_timeout_secs`        | integer | Optional. Deadline for establishing the connection. Defaults to 30.                                                                                                           |
| `stream_idle_timeout_secs`    | integer | Optional. Deadline between successive reads of a response, covering the wait for headers and the wait between streamed events. It resets on every read, so a long healthy stream is never interrupted. Defaults to 120. |
| `model`                       | string  | Optional. Default model name for this provider. Used when no model is specified via `--model` or `ZS_MODEL`.                                                                  |

### Connection and stream deadlines

Every provider client — built-in and custom alike — is built with a connect
deadline and a stream-inactivity deadline. Without them a peer that accepts a
request and then stalls, before headers or between streamed events, keeps a turn
alive indefinitely and never produces the error that would trigger a retry. Both
bounds are overridable per provider with the `connect_timeout_secs` and
`stream_idle_timeout_secs` entries above; a value of `0` is clamped to one
second rather than disabling the bound.

### Header variable expansion

Header values can reference environment variables with `${VAR}` syntax:

```json
{
  "custom_providers": {
    "company-gateway": {
      "provider_type": "openai",
      "base_url": "https://gateway.example.com/v1",
      "headers": {
        "cf-access-client-id": "${CF_ACCESS_CLIENT_ID}",
        "cf-access-client-secret": "${CF_ACCESS_CLIENT_SECRET}"
      }
    }
  }
}
```

## API Key Resolution

The API key is resolved in this priority order:

1. **CLI flag** `--api-key` (visible in process listings — use with care)
2. **Environment variable** — either the custom one from `api_key_env`, or the
   default env var for the provider kind
3. **Config file** `api_keys` map — keyed by provider slug or custom provider name
4. **Ollama** — returns an empty string (no key required)

For native Anthropic, `ANTHROPIC_AUTH_TOKEN` is a typed bearer alternative after
`--api-key` and the designated API-key environment variable, before config keys.
It sends only `Authorization: Bearer <token>` for authentication, without `x-api-key`,
following [Anthropic's API authentication contract](https://platform.claude.com/docs/en/api/overview).
An isolated custom route or an explicit `api_key_env` does not inherit this token;
`inherit_builtin_key = true` permits the native fallback for a custom route without
an alternate `api_key_env`. Credentials are never classified by their string contents.

`OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` are endpoint fallbacks when no endpoint
is configured. Configured custom endpoints always win. An OpenAI gateway fallback
selects Chat Completions; explicit `api_style` still wins. Anthropic normalizes root,
`/v1`, and `/v1/messages` URLs using the native SDK rules. All provider requests retain
same-origin redirect enforcement and configured timeouts.

See the [canonical environment reference](CONFIG.md#environment-variables) for
all public, OS/library and internal/test variables.

### Config-level API keys

```json
{
  "api_keys": {
    "openai": "sk-...",
    "anthropic": "sk-ant-..."
  }
}
```

## OpenAI API Styles

The OpenAI provider supports two API transports:

- **Responses API** (`/responses`) — the default for OpenAI's own API. Required
  for GPT-5-series models that reject `max_tokens` on Chat Completions.
- **Chat Completions API** (`/chat/completions`) — the default when a custom
  `base_url` is set, since most OpenAI-compatible gateways implement only this
  endpoint.

`api_style` is a **custom-provider field only**: it is read from a
`custom_providers` entry whose name matches the selected provider, and such an
entry always has a `base_url`. There is no top-level `api_style` key, so the
built-in `openai` provider cannot be given one — selected on its own it has no
`base_url` and therefore always uses the Responses API.

Because every custom provider carries a `base_url`, its default is
`completions`. A gateway that implements only `/responses` (Azure's `v1`
surface, a LiteLLM `/responses` route) must set `api_style` explicitly:

```json
{
  "custom_providers": {
    "azure-responses": {
      "provider_type": "openai",
      "base_url": "https://example.openai.azure.com/openai/v1",
      "api_key_env": "AZURE_OPENAI_API_KEY",
      "api_style": "responses"
    }
  }
}
```

A custom provider may reuse the name `openai`, which shadows the built-in
entirely; that is the way to attach a `base_url` and an `api_style` to that
provider name.

Reasoning effort, summary, encrypted reasoning content and Responses `store` are
configured separately, in the `[reasoning]` config section — see
[CONFIG.md](CONFIG.md#reasoning-controls-reasoning).

## Prompt caching

The owner global setting `cache_ttl = "1h"` selects native Anthropic automatic
one-hour caching, with generated tools/system breakpoints at the same TTL.
Omission or `"5m"` preserves the existing manual five-minute policy. The native
model factory applies this to ordinary, child and auxiliary requests; other
provider protocols keep their existing cache controls. Explicit incompatible
markers fail SDK validation before HTTP. These controls do not relocate mutable
memory or attest that a changed prompt/tool prefix can replay signed thinking.


mini-agent enables prompt caching automatically where the underlying rig provider supports it. The behavior depends on which provider backs the model you choose.

Anthropic-native cost accounting uses the model's cache-read and five-minute
cache-write prices from the embedded catalog. For example, Fable 5.1 cache
reads cost $0.25/M tokens, Opus 5.5 $0.20/M, and Sonnet 5.5 $0.10/M, as listed
in [Anthropic's pricing reference](https://platform.claude.com/docs/en/about-claude/pricing).
Quick models can set `cache_read_token_cost`, `cache_write_token_cost` (5m), and
`cache_write_1h_token_cost` in
USD per million tokens. A selected alias's explicit cache prices win per tier;
an absent tier inherits the catalog's cache-to-input ratio at the alias's
configured input price. Raw model selections use catalog ratios before matching
quick-model ratios. Missing or invalid prices fall back to 0.10x reads and
1.25x writes; zero cache prices are valid. Saved sessions retain their resolved
cache prices, and older sessions without these fields inherit catalog ratios.
For a zero base input price, explicitly configured native cache tiers still
charge their absolute USD/M rates; missing or free tiers contribute zero.
Reported native five-minute and one-hour writes are priced separately. The
one-hour fallback is 2x selected input, independently of a five-minute override.
Positive base prices round once over the combined equivalent-input pool.
See [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).
`/cost` shows the latest recorded request's uncached/cached input and known TTL
write breakdown; an unreported breakdown stays labeled as such.
Subagent token-budget normalization remains separate: explicit free tiers have
zero weight, while positive or unknown tiers at a zero base use fallback ratios.
Headless JSON, session totals, goal judges, and side questions use the same
model-aware billing calculation. A side question retains the prices captured
when it started, even if the main model changes before it completes. Production
prompt caching uses the default five-minute TTL; these rates do not describe
the separately priced one-hour TTL.

### Automatic — no mini-agent action

These providers cache server-side without any markers in the request:

- **OpenAI** — automatic above ~1024 tokens, 50% discount on cached input.
- **Google Gemini 2.5+** — implicit caching, 75% discount.
- **DeepSeek** — automatic, persistent across days, ~90% discount.
- **xAI / Grok** — automatic, 75% discount; benefits from setting an `x-grok-conv-id` header which mini-agent does not currently send.
- **Moonshot, Groq (Kimi K2)** — automatic.

For these providers, mini-agent passes through to rig without additional configuration.

### Explicitly enabled by mini-agent

These providers require `cache_control` markers; mini-agent adds them via rig's `.with_prompt_caching()`:

- **Anthropic (direct API)** — marks system prompt, the final tool definition, and the last message. All three breakpoints contribute to cumulative savings as the conversation grows.
- **Claude via OpenRouter** — marks both the system prompt and the last user/tool message. The latter advances the cache boundary as the conversation grows, so prior turns and tool results are reused instead of re-billed. For `anthropic/*` model IDs, mini-agent also pins `provider.order = ["Anthropic"]` with `allow_fallbacks: true`, because Bedrock and Vertex AI silently drop `cache_control` markers.

### Empirical impact

Measured on Sonnet 4.6, second turn of a tool-heavy session (grep + read across the mini-agent repo, ~6k-token system prompt including AGENTS.md and ARCHITECTURE.md):

| Configuration               | turn 2 cost | reduction |
| --------------------------- | ----------: | --------: |
| Baseline (no caching)       |      $0.186 |         — |
| Anthropic + caching         |      $0.024 |      -87% |
| OpenRouter Claude + caching |      $0.026 |      -86% |

Projected monthly cost at 50 such turns per working day: $204 (baseline) → $26 (Anthropic direct) or $29 (OpenRouter Claude). The two cached paths are within ~$3/month of each other.

Rig 0.40 marks the OpenRouter system block but not the conversation tail. For
`anthropic/*` models, mini-agent also sends OpenRouter's top-level
`cache_control = { type = "ephemeral" }`; OpenRouter then advances the boundary
to the last cacheable block on both streaming and non-streaming requests. Other
OpenRouter models keep their provider-native caching behavior.

## CLI Flags

| Flag                | Env var       | Description                         |
| ------------------- | ------------- | ----------------------------------- |
| `--provider`        | `ZS_PROVIDER` | Provider name                       |
| `--model`           | `ZS_MODEL`    | Model name                          |
| `--quick-model`     | —             | Use a named quick model from config |
| `--api-key`         | —             | API key (visible in `ps`)           |
| `--max-tokens`      | —             | Maximum response tokens             |
| `--temperature`     | —             | Model temperature (0.0–2.0)         |
| `--max-agent-turns` | —             | Maximum agent turns per response    |

## Vertex AI Claude

Select `provider = "vertex"` (alias `vertex-ai`) and configure the exact Claude
Vertex model ID, including its version suffix when required. Set
`GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`; `global` uses
`aiplatform.googleapis.com`, and regional locations use
`LOCATION-aiplatform.googleapis.com`. There is no assumed deployment/model default.

```toml
provider = "vertex"
model = "claude-sonnet-4-5@20250929"
```

Without an explicit token, the official Google Cloud Auth SDK uses Application
Default Credentials: `GOOGLE_APPLICATION_CREDENTIALS`, the platform's standard
well-known gcloud ADC file, then metadata credentials. Standard user refresh,
service-account, impersonation and workload identity credential sources remain
SDK-owned. Configure identities with your normal Google tooling; mini-agent does
not log in or modify an account. Alternatively supply an existing access token
through `VERTEX_ACCESS_TOKEN`, `--api-key`, or `api_keys.vertex`. Those declared
sources always mean a bearer token; no secret-prefix inference is used. CLI,
environment and config precedence follows the other providers. Anthropic and
Gemini keys are never borrowed.

The native SDK sends Claude Messages to
`/v1/projects/PROJECT/locations/LOCATION/publishers/anthropic/models/MODEL:streamRawPredict`
(or `:rawPredict` for blocking calls). Its typed composer sets
`anthropic_version = "vertex-2023-10-16"` and omits the body `model` field. The
native capture observes that exact composed body and URL, preserving thinking,
tool identity/order, system/tools, cache accounting and canonical replay. A
per-request override cannot change the fixed route model or Vertex version.
This kind supports Claude, rather than translating other Vertex model families.

Custom Vertex routes accept an origin-only `base_url`, use the same project and
location environment variables, and require their own token unless
`inherit_builtin_key = true` explicitly grants ADC/built-in token access. Model
auth is bound to the selected origin and exact publisher/project/location path;
redirects are disabled, and competing custom authorization and quota-project headers are removed. SDK quota
project headers are preserved for ADC credentials. Auth HTTP is independent of
model endpoint headers and TLS exceptions.

Refresh belongs to the credential client's lifetime. Cancelling a model turn
cancels its credential waiter; cached refresh work remains available to other
turns using that client. The pinned SDK amendment cancels a held refresh/sleep
when the last client drops and caps every token-provider fetch at 30 seconds.
The owning request also bounds its waiter to 30 seconds. Credential response
bytes use the upstream SDK behavior; no byte-cap guarantee is claimed.
Executable ADC credential sources, including nested impersonation sources, are
explicitly unsupported and rejected before refresh construction. Use a standard
non-executable ADC source or supply an existing access token.

`src/provider_vertex.rs` owns route/auth selection; the typed rawPredict composer
is a bounded amendment in the maintained Rig SDK. Official Google Cloud Auth
1.17.0 is pinned in `patched-deps/google-cloud-auth` with source/license attribution
and the lifetime amendment described in its `UPSTREAM.md`. Both SDK trees must be
included in Corresponding Source and third-party license inventory.

Google's [Claude request contract](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/partner-models/claude/use-claude)
documents the publisher route and Vertex body version. The official
[ADC builder](https://docs.rs/google-cloud-auth/1.17.0/google_cloud_auth/credentials/struct.Builder.html)
documents source selection and scopes.

## GitHub Copilot credential ownership

`src/provider_copilot.rs` uses the maintained SDK's Copilot chat and Responses
converters. The actual model selects its converter; Codex model IDs use
Responses. `src/auth/copilot.rs` supplies the SDK's external `AuthContext` callback,
so the application owns device authorization, refresh and private storage. The
SDK's editor registration, ambient token files and interactive fallback are
bypassed. Requests identify mini-agent and retain the SDK's tool/initiator rules.

Declared direct API tokens use `GITHUB_COPILOT_API_TOKEN`; an explicit GitHub
bootstrap token uses `COPILOT_GITHUB_TOKEN`. Generic `GH_TOKEN`, `GITHUB_TOKEN` and
other providers' credentials are not borrowed. Device authorization uses your
configured `COPILOT_CLIENT_ID`, with device flow enabled in that app. A captured
registration binds one account generation and GitHub host; later login/logout
cannot silently change an existing client's account.

Credential files use the existing owner-only, no-follow atomic storage helpers.
A rotated GitHub token is saved before fallible identity/model requests, marked
unverified until its account matches the captured registration. It cannot supply
a model credential before verification. Polling obeys the server interval and
slow-down response, expires after at most 15 minutes and is cancellable. Auth
requests have 30-second and 256 KiB bounds; a model credential acquisition has a
30-second total deadline. Refresh work stays inside its caller's future.

GitHub.com and a declared `COPILOT_GITHUB_HOST = SUBDOMAIN.ghe.com` keep separate
private registrations. Token-derived API endpoints must remain on the approved
Copilot service or that enterprise's exact Copilot endpoints. Explicit custom
routes retain the existing credential-isolation/inheritance policy and disable
redirects. GitHub Enterprise Server itself does not host Copilot subscriptions.

GitHub documents [independent OAuth apps](https://docs.github.com/en/copilot/how-tos/copilot-sdk/setup/github-oauth),
[device authorization](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow),
and [GHE.com service routing](https://docs.github.com/en/copilot/reference/copilot-allowlist-reference).


Configure your own GitHub OAuth app with device flow enabled, then authorize it:

```sh
export COPILOT_CLIENT_ID=YOUR_APP_CLIENT_ID
mini-agent --login copilot
mini-agent --provider copilot --model MODEL_ID
mini-agent --logout copilot
```

The TUI equivalents are `/login copilot`, `/provider copilot`, `/model MODEL_ID`
and `/logout copilot`; `/provider` shows the selected host's saved account and
whether the current client uses it. Copilot stores one account per GitHub host;
`--login-account` and the third `/login` argument apply only to OpenAI. A running
turn must finish or stop before TUI authentication changes. The app displays the
GitHub device verification URL and user code, waits the server's polling interval,
honors `authorization_pending` and `slow_down`, and cancels on Ctrl-C. Device
sessions stop after the advertised expiry, capped at fifteen minutes. Every auth
HTTP operation has a thirty-second total deadline and a 256 KiB response cap;
model/listing operations retain their existing owners and limits. No background
refresh task is started. Local logout invalidates captured registration generations
and pending sign-ins; revoke the OAuth app in GitHub settings to revoke the grant.

For GitHub Enterprise Cloud data residency, set `COPILOT_GITHUB_HOST=SUBDOMAIN.ghe.com`
before login and use that same host for requests. Only `github.com` and a single
enterprise subdomain of `ghe.com` are accepted authorization hosts. Arbitrary
GitHub Enterprise Server OAuth hosts are unsupported. A supplied direct model
token can use an explicitly configured gateway origin; custom aliases isolate
credentials unless `inherit_builtin_key = true` is set. `COPILOT_API_URL` is the
built-in endpoint fallback and must be an origin without a path/query. Redirects
are disabled for Copilot routes. Model HTTP errors preserve status and retry
metadata while dropping untrusted response bodies. The API token returned by GitHub is accepted only
for approved Copilot service origins for the captured GitHub host.

An API-token alternative needs no OAuth app registration:

```sh
GITHUB_COPILOT_API_TOKEN=YOUR_MODEL_TOKEN mini-agent --provider copilot --model MODEL_ID
# Or explicitly exchange a GitHub bootstrap token using the owned auth client:
COPILOT_GITHUB_TOKEN=YOUR_GITHUB_TOKEN mini-agent --provider copilot --model MODEL_ID
```

`--api-key` wins, then `GITHUB_COPILOT_API_TOKEN`, then `COPILOT_GITHUB_TOKEN`, then
`api_keys.copilot`, then the saved login. Token contents never select the credential
type. `GH_TOKEN`, `GITHUB_TOKEN`, editor registrations and editor token caches are
not Copilot sources. The integration sends its own mini-agent identity headers.
A model is required explicitly; `/models refresh` lists models available to the
current credential. Copilot uses the existing SDK's model classifier: IDs containing
`codex` use Responses, other IDs use Chat Completions. `api_style` does not override
that classifier. Both converters preserve SDK tool and usage behavior; malformed
chat usage is rejected before arithmetic or tool execution. Copilot sessions use
ordinary message history; this backend does not claim Anthropic native replay or
OpenAI encrypted-item publication authority. No live Copilot entitlement was used
for the local device-flow, refresh, tool/wire and installed terminal controls.

Copilot uses the same existing completion helpers for optional advisor requests
and model-based JavaScript skill distillation as the other typed backends. Both
chat and Responses paths preserve caller prompts and response handling; skill
parsing, capability scopes, proposal verification and admission remain owned by
the existing distillation pipeline.
