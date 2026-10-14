---
description: "Configure LLM providers in mini-agent: OpenRouter, OpenAI-compatible endpoints, Azure OpenAI, Anthropic, Gemini, Ollama, custom headers, and prompt caching."
---

# Providers

mini-agent supports six built-in providers and allows custom provider
definitions for OpenAI-compatible endpoints.

## Built-in Providers

| Provider   | Config name         | Default env var for API key |
| ---------- | ------------------- | --------------------------- |
| OpenRouter | `openrouter`        | `OPENROUTER_API_KEY`        |
| OpenAI     | `openai`            | `OPENAI_API_KEY`            |
| Azure OpenAI | `azure` / `azure-openai` | `AZURE_OPENAI_API_KEY` |
| Anthropic  | `anthropic`         | `ANTHROPIC_API_KEY`         |
| Gemini     | `gemini` / `google` | `GEMINI_API_KEY`            |
| Ollama     | `ollama`            | (no key required)           |

Select a provider via the config file, the `--provider` CLI flag, or the
`ZS_PROVIDER` environment variable:

```
mini-agent --provider anthropic
```

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
