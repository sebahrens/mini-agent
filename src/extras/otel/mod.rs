//! Opt-in OpenTelemetry export (OTLP/HTTP with JSON bodies).
//!
//! Compiled only with the `otel` feature and active only when an endpoint is
//! configured through the standard `OTEL_EXPORTER_OTLP_*` variables; without
//! one no socket is ever opened. Metrics are counters (sessions, tokens by
//! type, cost, tool calls by name and outcome); traces are one span per turn
//! with a child span per tool call. Prompts, model output, tool arguments and
//! tool results are never exported: attributes carry only tool names, outcomes
//! and token counts. The payloads are built by hand and posted with the
//! `reqwest` client the crate already depends on, so the feature adds no
//! dependency.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::event::{AgentEvent, UsageDelta};

const SCOPE: &str = "mini-agent";
/// Bound on buffered finished spans; the oldest are dropped past it.
const MAX_BUFFERED_SPANS: usize = 2048;
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where and how to export. Built only from an explicit endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtelConfig {
    pub metrics_url: String,
    pub traces_url: String,
    pub headers: Vec<(String, String)>,
    pub service_name: String,
}

impl OtelConfig {
    /// Reads the standard variables through `get`. `None` unless an endpoint
    /// is set and `OTEL_SDK_DISABLED` is not `true`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |name: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        if set("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            return None;
        }
        let base = set("OTEL_EXPORTER_OTLP_ENDPOINT");
        let join = |base: &str, path: &str| format!("{}/{path}", base.trim_end_matches('/'));
        let metrics_url = set("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
            .or_else(|| base.as_deref().map(|b| join(b, "v1/metrics")));
        let traces_url = set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .or_else(|| base.as_deref().map(|b| join(b, "v1/traces")));
        let (metrics_url, traces_url) = match (metrics_url, traces_url) {
            (None, None) => return None,
            (m, t) => (m.unwrap_or_default(), t.unwrap_or_default()),
        };
        let headers = set("OTEL_EXPORTER_OTLP_HEADERS")
            .map(|raw| {
                raw.split(',')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .filter(|(k, _)| !k.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            metrics_url,
            traces_url,
            headers,
            service_name: set("OTEL_SERVICE_NAME").unwrap_or_else(|| SCOPE.to_string()),
        })
    }

    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64)
}

fn hex_id(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}

struct Span {
    trace_id: String,
    span_id: String,
    parent: Option<String>,
    name: &'static str,
    start: u64,
    end: u64,
    attributes: Vec<(&'static str, String)>,
    error: bool,
}

struct OpenTurn {
    trace_id: String,
    span_id: String,
    start: u64,
    tool_calls: u64,
}

struct OpenTool {
    name: String,
    span_id: String,
    start: u64,
}

#[derive(Default)]
struct State {
    started: u64,
    sessions: u64,
    tokens: BTreeMap<&'static str, u64>,
    cost_nano_usd: u64,
    tool_calls: BTreeMap<(String, &'static str), u64>,
    turn: Option<OpenTurn>,
    tools: HashMap<String, OpenTool>,
    spans: Vec<Span>,
}

/// In-process collector. Cheap to call; never blocks on the network.
pub struct Collector {
    config: OtelConfig,
    state: Mutex<State>,
}

impl Collector {
    pub fn new(config: OtelConfig) -> Self {
        let state = State {
            started: now_nanos(),
            sessions: 1,
            ..State::default()
        };
        Self {
            config,
            state: Mutex::new(state),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn ensure_turn(state: &mut State) {
        if state.turn.is_none() {
            state.turn = Some(OpenTurn {
                trace_id: hex_id(16),
                span_id: hex_id(8),
                start: now_nanos(),
                tool_calls: 0,
            });
        }
    }

    pub fn tool_call(&self, id: &str, name: &str) {
        let mut state = self.state();
        Self::ensure_turn(&mut state);
        if let Some(turn) = state.turn.as_mut() {
            turn.tool_calls += 1;
        }
        state.tools.insert(
            id.to_string(),
            OpenTool {
                name: name.to_string(),
                span_id: hex_id(8),
                start: now_nanos(),
            },
        );
    }

    pub fn tool_result(&self, id: &str, is_error: bool) {
        let mut state = self.state();
        let Some(tool) = state.tools.remove(id) else {
            return;
        };
        let outcome = if is_error { "error" } else { "success" };
        *state
            .tool_calls
            .entry((tool.name.clone(), outcome))
            .or_default() += 1;
        Self::ensure_turn(&mut state);
        let (trace_id, parent) = state
            .turn
            .as_ref()
            .map(|t| (t.trace_id.clone(), t.span_id.clone()))
            .unwrap_or_default();
        Self::push_span(
            &mut state,
            Span {
                trace_id,
                span_id: tool.span_id,
                parent: Some(parent),
                name: "mini_agent.tool_call",
                start: tool.start,
                end: now_nanos(),
                attributes: vec![
                    ("tool.name", tool.name),
                    ("tool.outcome", outcome.to_string()),
                ],
                error: is_error,
            },
        );
    }

    pub fn usage(&self, delta: &UsageDelta) {
        let mut state = self.state();
        Self::ensure_turn(&mut state);
        for (kind, count) in [
            ("input", delta.input_tokens),
            ("output", delta.output_tokens),
            ("cache_read", delta.cached_input_tokens),
            ("cache_creation", delta.cache_creation_input_tokens),
            ("reasoning", delta.reasoning_tokens),
        ] {
            if count > 0 {
                *state.tokens.entry(kind).or_default() += count;
            }
        }
        state.cost_nano_usd = state
            .cost_nano_usd
            .saturating_add(delta.reported_cost_nano_usd);
    }

    /// Closes the turn span; `failed` marks it as an error.
    pub fn end_turn(&self, failed: bool) {
        let mut state = self.state();
        let Some(turn) = state.turn.take() else {
            return;
        };
        // A tool left open by an interrupted turn ends as an error span.
        let open: Vec<(String, OpenTool)> = state.tools.drain().collect();
        for (_, tool) in open {
            *state
                .tool_calls
                .entry((tool.name.clone(), "error"))
                .or_default() += 1;
            Self::push_span(
                &mut state,
                Span {
                    trace_id: turn.trace_id.clone(),
                    span_id: tool.span_id,
                    parent: Some(turn.span_id.clone()),
                    name: "mini_agent.tool_call",
                    start: tool.start,
                    end: now_nanos(),
                    attributes: vec![
                        ("tool.name", tool.name),
                        ("tool.outcome", "error".to_string()),
                    ],
                    error: true,
                },
            );
        }
        Self::push_span(
            &mut state,
            Span {
                trace_id: turn.trace_id,
                span_id: turn.span_id,
                parent: None,
                name: "mini_agent.turn",
                start: turn.start,
                end: now_nanos(),
                attributes: vec![("tool.calls", turn.tool_calls.to_string())],
                error: failed,
            },
        );
    }

    fn push_span(state: &mut State, span: Span) {
        if state.spans.len() >= MAX_BUFFERED_SPANS {
            state.spans.remove(0);
        }
        state.spans.push(span);
    }

    /// Maps the agent's event stream onto the collector. Display-only and
    /// content-bearing variants are ignored.
    pub fn observe(&self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolCall { id, name, .. } => self.tool_call(id, name),
            AgentEvent::ToolResult { id, is_error, .. } => self.tool_result(id, *is_error),
            AgentEvent::UsageDelta { usage, .. } => self.usage(usage),
            AgentEvent::Done { .. } => self.end_turn(false),
            AgentEvent::Error { .. } | AgentEvent::Interrupted { .. } => self.end_turn(true),
            _ => {}
        }
    }

    fn resource(&self) -> Value {
        json!({"attributes": [attr("service.name", &self.config.service_name),
            attr("service.version", env!("CARGO_PKG_VERSION"))]})
    }

    /// Cumulative counters as an OTLP/JSON `ExportMetricsServiceRequest`.
    pub fn metrics_payload(&self) -> Value {
        let state = self.state();
        let now = now_nanos().to_string();
        let start = state.started.to_string();
        let point = |attrs: Vec<Value>, value: Value| {
            let mut p =
                json!({"attributes": attrs, "startTimeUnixNano": start, "timeUnixNano": now});
            let (key, val) = match value {
                Value::Number(n) if n.is_f64() => ("asDouble", Value::Number(n)),
                other => ("asInt", other),
            };
            p[key] = val;
            p
        };
        let sum = |name: &str, unit: &str, points: Vec<Value>| {
            json!({"name": name, "unit": unit,
                "sum": {"aggregationTemporality": 2, "isMonotonic": true, "dataPoints": points}})
        };
        let mut metrics = vec![sum(
            "mini_agent.session.count",
            "{session}",
            vec![point(vec![], json!(state.sessions.to_string()))],
        )];
        if !state.tokens.is_empty() {
            metrics.push(sum(
                "mini_agent.token.usage",
                "{token}",
                state
                    .tokens
                    .iter()
                    .map(|(kind, n)| point(vec![attr("type", kind)], json!(n.to_string())))
                    .collect(),
            ));
        }
        if state.cost_nano_usd > 0 {
            metrics.push(sum(
                "mini_agent.cost.usage",
                "USD",
                vec![point(
                    vec![],
                    json!(state.cost_nano_usd as f64 / 1_000_000_000.0),
                )],
            ));
        }
        if !state.tool_calls.is_empty() {
            metrics.push(sum(
                "mini_agent.tool.calls",
                "{call}",
                state
                    .tool_calls
                    .iter()
                    .map(|((name, outcome), n)| {
                        point(
                            vec![attr("tool.name", name), attr("outcome", outcome)],
                            json!(n.to_string()),
                        )
                    })
                    .collect(),
            ));
        }
        json!({"resourceMetrics": [{"resource": self.resource(),
            "scopeMetrics": [{"scope": {"name": SCOPE}, "metrics": metrics}]}]})
    }

    /// Takes the finished spans as an OTLP/JSON `ExportTraceServiceRequest`;
    /// `None` when there are none.
    pub fn take_traces_payload(&self) -> Option<Value> {
        let spans = std::mem::take(&mut self.state().spans);
        if spans.is_empty() {
            return None;
        }
        let spans: Vec<Value> = spans
            .into_iter()
            .map(|s| {
                let mut v = json!({
                    "traceId": s.trace_id, "spanId": s.span_id, "name": s.name, "kind": 1,
                    "startTimeUnixNano": s.start.to_string(), "endTimeUnixNano": s.end.to_string(),
                    "attributes": s.attributes.iter().map(|(k, v)| attr(k, v)).collect::<Vec<_>>(),
                    "status": {"code": if s.error { 2 } else { 1 }},
                });
                if let Some(parent) = s.parent {
                    v["parentSpanId"] = json!(parent);
                }
                v
            })
            .collect();
        Some(json!({"resourceSpans": [{"resource": self.resource(),
            "scopeSpans": [{"scope": {"name": SCOPE}, "spans": spans}]}]}))
    }

    /// Posts pending metrics and spans. Failures are logged at debug level and
    /// never reach the session.
    pub async fn flush(&self) {
        let client = match reqwest::Client::builder().timeout(EXPORT_TIMEOUT).build() {
            Ok(client) => client,
            Err(_) => return,
        };
        if !self.config.metrics_url.is_empty() {
            self.post(&client, &self.config.metrics_url, self.metrics_payload())
                .await;
        }
        if !self.config.traces_url.is_empty()
            && let Some(payload) = self.take_traces_payload()
        {
            self.post(&client, &self.config.traces_url, payload).await;
        }
    }

    async fn post(&self, client: &reqwest::Client, url: &str, body: Value) {
        let mut request = client.post(url).json(&body);
        for (key, value) in &self.config.headers {
            request = request.header(key, value);
        }
        if let Err(error) = request.send().await {
            tracing::debug!("otel: export to {url} failed: {error}");
        }
    }
}

fn attr(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

static GLOBAL: OnceLock<Option<Collector>> = OnceLock::new();

/// The process-wide collector, built from the environment on first use.
/// `None` (and no side effect) when no endpoint is configured.
pub fn global() -> Option<&'static Collector> {
    GLOBAL
        .get_or_init(|| OtelConfig::from_env().map(Collector::new))
        .as_ref()
}

/// Feeds one event to the global collector, if exporting.
pub fn observe(event: &AgentEvent) {
    if let Some(collector) = global() {
        collector.observe(event);
    }
}

/// Flushes the global collector, if exporting.
pub async fn flush() {
    if let Some(collector) = global() {
        collector.flush().await;
    }
}

#[cfg(test)]
mod tests;
