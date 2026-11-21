# OpenTelemetry export (opt-in)

Build with `--features otel` (no extra dependencies). Export is active only when an OTLP endpoint is
configured in the environment; otherwise no socket is opened.

| Variable | Meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL; `/v1/metrics` and `/v1/traces` are appended |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Per-signal URLs |
| `OTEL_EXPORTER_OTLP_HEADERS` | `key=value,key=value` request headers |
| `OTEL_SERVICE_NAME` | Defaults to `mini-agent` |
| `OTEL_SDK_DISABLED=true` | Turns export off |

Metrics (cumulative sums): `mini_agent.session.count`, `mini_agent.token.usage` (attribute `type`:
input, output, cache_read, cache_creation, reasoning), `mini_agent.cost.usage` (USD), `mini_agent.tool.calls`
(`tool.name`, `outcome`). Traces: `mini_agent.turn` with child `mini_agent.tool_call` spans. Prompts,
model output and tool arguments or results are never exported. Payloads are posted at the end of each
turn; failures are logged at debug level only.

Not done: a managed-policy key to force export, lines-changed counters, and routing through the
provider proxy/CA client (the export uses the process environment's proxy settings).
