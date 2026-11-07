use std::pin::Pin;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    pub max_attempts: usize,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    /// Ceiling for a provider-supplied wait (`Retry-After`, `retry-after-ms`,
    /// rate-limit reset headers). A server hint may extend the sleep beyond
    /// `max_backoff_ms` up to this value; `0` restores the old behaviour of
    /// capping hints at `max_backoff_ms`.
    pub max_retry_after_ms: u64,
}

/// Default attempt budget. Six attempts with 0.5 s doubling to a 30 s ceiling
/// ride out about 15 s of backoff (plus jitter and any provider hint), enough
/// for a short 529/5xx/overload burst, while staying well under the
/// `MAX_RETRY_ATTEMPTS` hard cap of 10.
pub const DEFAULT_MAX_ATTEMPTS: usize = 6;
/// Default ceiling for one ordinary (hint-free) backoff sleep.
pub const DEFAULT_MAX_BACKOFF_MS: u64 = 30_000;

/// Default ceiling for a provider-supplied retry wait: five minutes.
pub const DEFAULT_MAX_RETRY_AFTER_MS: u64 = 300_000;

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            initial_backoff_ms: 500,
            max_backoff_ms: DEFAULT_MAX_BACKOFF_MS,
            max_retry_after_ms: DEFAULT_MAX_RETRY_AFTER_MS,
        }
    }
}

/// Hard ceiling on `retry.max_attempts`. Every provider call path (agent
/// turns, compaction summaries, goal judging, session titles) reads the same
/// `RetryConfig`, so a larger value would multiply every transient failure.
pub const MAX_RETRY_ATTEMPTS: usize = 10;
/// Hard ceiling on either backoff, in milliseconds. A larger value cannot
/// make a retry more useful, only let one request stall a headless run.
pub const MAX_RETRY_BACKOFF_MS: u64 = 60_000;
/// Hard ceiling on `retry.max_retry_after_ms`: a provider hint can never make
/// one retry sleep longer than ten minutes.
pub const MAX_RETRY_AFTER_CEILING_MS: u64 = 600_000;

impl RetryConfig {
    /// Clamp to the safety envelope: `max_attempts` in
    /// `1..=MAX_RETRY_ATTEMPTS`, both backoffs at most `MAX_RETRY_BACKOFF_MS`,
    /// and `initial_backoff_ms <= max_backoff_ms`. Returns the names of the
    /// fields that had to change so the caller can say so.
    pub fn clamp_to_limits(&mut self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        let attempts = self.max_attempts.clamp(1, MAX_RETRY_ATTEMPTS);
        if attempts != self.max_attempts {
            self.max_attempts = attempts;
            changed.push("max_attempts");
        }
        if self.max_backoff_ms > MAX_RETRY_BACKOFF_MS {
            self.max_backoff_ms = MAX_RETRY_BACKOFF_MS;
            changed.push("max_backoff_ms");
        }
        if self.initial_backoff_ms > self.max_backoff_ms {
            self.initial_backoff_ms = self.max_backoff_ms;
            changed.push("initial_backoff_ms");
        }
        if self.max_retry_after_ms > MAX_RETRY_AFTER_CEILING_MS {
            self.max_retry_after_ms = MAX_RETRY_AFTER_CEILING_MS;
            changed.push("max_retry_after_ms");
        }
        changed
    }

    /// The longest sleep a provider retry hint may request: the configured
    /// hint ceiling, never below the ordinary backoff ceiling.
    pub fn hint_ceiling(&self) -> Duration {
        Duration::from_millis(self.max_retry_after_ms.max(self.max_backoff_ms))
    }

    /// Cap every field at `ceiling`, then apply [`Self::clamp_to_limits`].
    /// Used for repository-controlled project config, which may make retrying
    /// stop sooner or wait less but never retry more often or wait longer than
    /// the user's own (global or default) policy. Returns the changed fields.
    pub fn cap_at(&mut self, ceiling: &RetryConfig) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.max_attempts > ceiling.max_attempts {
            self.max_attempts = ceiling.max_attempts;
            changed.push("max_attempts");
        }
        if self.initial_backoff_ms > ceiling.initial_backoff_ms {
            self.initial_backoff_ms = ceiling.initial_backoff_ms;
            changed.push("initial_backoff_ms");
        }
        if self.max_backoff_ms > ceiling.max_backoff_ms {
            self.max_backoff_ms = ceiling.max_backoff_ms;
            changed.push("max_backoff_ms");
        }
        if self.max_retry_after_ms > ceiling.max_retry_after_ms {
            self.max_retry_after_ms = ceiling.max_retry_after_ms;
            changed.push("max_retry_after_ms");
        }
        for field in self.clamp_to_limits() {
            if !changed.contains(&field) {
                changed.push(field);
            }
        }
        changed
    }

    /// A copy clamped to the safety envelope. The retry loops use this so a
    /// `RetryConfig` built anywhere other than config load stays bounded too.
    pub fn bounded(&self) -> RetryConfig {
        let mut bounded = self.clone();
        bounded.clamp_to_limits();
        bounded
    }
}

/// The sleep before the next retry: the current backoff plus jitter, never
/// more than `max_backoff`.
pub fn retry_delay(backoff: Duration, max_backoff: Duration) -> Duration {
    let backoff = backoff.min(max_backoff);
    let jitter = simple_jitter(backoff.as_millis() as u64);
    (backoff + jitter).min(max_backoff)
}

/// Retry hints belong to the particular failed HTTP request, carried through
/// Rig's error source chain. Neither shared clients nor nested agents share a
/// mutable last-response slot. Hints are capped at `max_backoff`; production
/// loops use [`retry_delay_for_error_with_hint_ceiling`].
#[cfg(test)]
pub fn retry_delay_for_error(
    backoff: Duration,
    max_backoff: Duration,
    error: &(dyn std::error::Error + 'static),
) -> Duration {
    retry_delay_for_error_with_hint_ceiling(backoff, max_backoff, max_backoff, error)
}

/// The sleep before the next retry of `error`: backoff plus jitter (capped at
/// `max_backoff`), extended to a provider retry hint when one is present. A
/// hint may exceed `max_backoff` up to `hint_ceiling`, so a server that says
/// "come back in 45s" is honoured instead of being retried early.
pub fn retry_delay_for_error_with_hint_ceiling(
    backoff: Duration,
    max_backoff: Duration,
    hint_ceiling: Duration,
    error: &(dyn std::error::Error + 'static),
) -> Duration {
    let computed = retry_delay(backoff, max_backoff);
    let ceiling = hint_ceiling.max(max_backoff);
    provider_retry_hint(error, SystemTime::now())
        .map_or(computed, |hint| computed.max(hint).min(ceiling))
}

/// The provider's own wait hint for this failed request, if any. Precedence:
/// `retry-after-ms`, then `Retry-After` (seconds or HTTP date).
pub(crate) fn provider_retry_hint(
    error: &(dyn std::error::Error + 'static),
    now: SystemTime,
) -> Option<Duration> {
    let Some(response) = error_sources(error).find_map(|candidate| {
        candidate.downcast_ref::<crate::provider_http::ProviderRetryAfterError>()
    }) else {
        // Rig preserved only a status: still honour a wait stated in the body.
        return provider_response_status(error)
            .filter(|status| matches!(status.as_u16(), 429 | 503))
            .and_then(|_| body_retry_hint(error));
    };
    let header = |name: &str| {
        response
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    header("retry-after-ms")
        .and_then(parse_retry_after_ms)
        .or_else(|| header("retry-after").and_then(|value| parse_retry_after(value, now)))
        .or_else(|| {
            // Gemini and some OpenAI-compatible gateways carry the wait in
            // the error body instead of a header.
            matches!(response.status.as_u16(), 429 | 503)
                .then(|| body_retry_hint(error))
                .flatten()
        })
        .or_else(|| {
            // Without an explicit hint, a rate-limited response's reset
            // headers say when capacity returns. Which limit was hit is not
            // reported, so wait for the latest reset.
            (response.status == http::StatusCode::TOO_MANY_REQUESTS)
                .then(|| rate_limit_reset(&response.headers, now))
                .flatten()
        })
}

/// A wait stated in an error body: Gemini's `google.rpc.RetryInfo`
/// (`"retryDelay": "38s"`) or prose such as "Please retry in 38.2s" /
/// "Please try again in 1.5s" / "try again in 20ms". The longest hint found in
/// the source chain wins; callers clamp it to the hint ceiling.
pub(crate) fn body_retry_hint(error: &(dyn std::error::Error + 'static)) -> Option<Duration> {
    error_sources(error)
        .filter_map(|candidate| parse_body_retry_hint(&candidate.to_string()))
        .max()
}

pub(crate) fn parse_body_retry_hint(message: &str) -> Option<Duration> {
    let lower = message.to_ascii_lowercase();
    let mut best: Option<Duration> = None;
    let mut keep = |value: Option<Duration>| {
        if let Some(value) = value {
            best = Some(best.map_or(value, |current| current.max(value)));
        }
    };
    // RetryInfo: "retryDelay": "38s" (JSON, possibly escaped quotes).
    let mut from = 0;
    while let Some(found) = lower[from..].find("retrydelay") {
        let rest = &lower[from + found + "retrydelay".len()..];
        let rest = rest
            .trim_start_matches(|c: char| c == '"' || c == '\\' || c == ':' || c.is_whitespace());
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == 's' || c == 'm'))
            .unwrap_or(rest.len());
        keep(parse_go_duration(&rest[..end]));
        from += found + "retrydelay".len();
    }
    for needle in ["retry in ", "try again in ", "retry after "] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(needle) {
            let rest = &lower[from + found + needle.len()..];
            let number_end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(rest.len());
            if number_end > 0
                && let Ok(number) = rest[..number_end].parse::<f64>()
            {
                let unit = &rest[number_end..];
                let scale = if unit.starts_with("ms") || unit.starts_with("millisecond") {
                    Some(0.001)
                } else if unit.starts_with("min")
                    || unit.starts_with('m') && !unit.starts_with("ms")
                {
                    Some(60.0)
                } else if unit.starts_with('s') {
                    Some(1.0)
                } else {
                    None
                };
                if let Some(scale) = scale
                    && number.is_finite()
                {
                    keep(Some(Duration::from_secs_f64(
                        (number * scale).clamp(0.0, 1e9),
                    )));
                }
            }
            from += found + needle.len();
        }
    }
    best
}

/// The latest reset among `anthropic-ratelimit-*-reset` (RFC 3339 instants)
/// and OpenAI `x-ratelimit-reset-*` (durations such as `1s`, `6m0s`,
/// `20ms`).
fn rate_limit_reset(headers: &http::HeaderMap, now: SystemTime) -> Option<Duration> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            let value = value.to_str().ok()?.trim();
            if name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset") {
                let instant = chrono::DateTime::parse_from_rfc3339(value).ok()?;
                let instant = SystemTime::UNIX_EPOCH
                    + Duration::from_millis(u64::try_from(instant.timestamp_millis()).ok()?);
                Some(instant.duration_since(now).unwrap_or_default())
            } else if name.starts_with("x-ratelimit-reset") {
                parse_go_duration(value)
            } else {
                None
            }
        })
        .max()
}

/// A Go-style duration (`1h2m3.5s`, `250ms`, `6m0s`) or plain seconds.
fn parse_go_duration(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0)
            .then(|| Duration::from_secs_f64(seconds.min(1e9)));
    }
    let mut total = 0.0_f64;
    let mut rest = value;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|&end| end > 0)?;
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (scale, unit_len) = if rest.starts_with("ms") {
            (0.001, 2)
        } else if rest.starts_with('h') {
            (3600.0, 1)
        } else if rest.starts_with('m') {
            (60.0, 1)
        } else if rest.starts_with('s') {
            (1.0, 1)
        } else {
            return None;
        };
        total += number * scale;
        rest = &rest[unit_len..];
    }
    (total.is_finite() && total >= 0.0).then(|| Duration::from_secs_f64(total.min(1e9)))
}

/// Provider errors that no retry can fix: an exhausted quota, an unpaid
/// balance or a hard billing limit. They often arrive as HTTP 429, which
/// would otherwise be retried until the budget is spent.
pub fn is_quota_exhausted_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error_sources(error).any(|candidate| is_quota_exhausted_message(&candidate.to_string()))
}

fn is_quota_exhausted_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // Gemini opens every 429 RESOURCE_EXHAUSTED, including ordinary
    // per-minute limits, with "You exceeded your current quota". It is
    // terminal only for a daily/billing limit or when the body gives no
    // recovery hint; a RetryInfo delay or a per-minute metric is recoverable.
    if lower.contains("exceeded your current quota") || lower.contains("quota exceeded for metric")
    {
        let daily = lower.contains("perday") || lower.contains("per day");
        let per_minute = lower.contains("perminute") || lower.contains("per minute");
        let hinted = parse_body_retry_hint(message).is_some();
        if daily || !(hinted || per_minute) {
            return true;
        }
    }
    lower.contains("insufficient_quota")
        || lower.contains("billing_hard_limit")
        || lower.contains("credit balance is too low")
        || lower.contains("usage_limit_reached")
        || lower.contains("billing_not_active")
}

/// Guidance appended to a quota or billing failure.
pub const QUOTA_EXHAUSTED_HINT: &str = "The provider reports an exhausted quota, credit balance or billing limit; retrying will not help. Check the account's plan, credits and billing settings, or switch provider/model.";

/// `retry-after-ms`: a non-negative decimal number of milliseconds.
fn parse_retry_after_ms(value: &str) -> Option<Duration> {
    let millis: f64 = value.trim_matches([' ', '\t']).parse().ok()?;
    (millis.is_finite() && millis >= 0.0)
        .then(|| Duration::from_secs_f64(millis.min(1e12) / 1000.0))
}

fn error_sources<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> impl Iterator<Item = &'a (dyn std::error::Error + 'static)> {
    std::iter::successors(Some(error), |candidate| candidate.source()).take(32)
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim_matches([' ', '\t']);
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        // Oversized positive hints remain hints: the configured ceiling will
        // clamp them rather than turning a provider's long wait into no wait.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

pub fn simple_jitter(range_ms: u64) -> Duration {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = nanos % range_ms.max(1);
    Duration::from_millis(jitter)
}

pub fn is_retryable(error: &(dyn std::error::Error + 'static)) -> bool {
    // Context overflow cannot recover without changing the request. Check the
    // entire source chain before provider status or message fallbacks so token
    // counts such as 213500 never masquerade as HTTP 500.
    for candidate in error_sources(error) {
        if is_context_length_error_message(&candidate.to_string()) {
            return false;
        }
    }

    // Exhausted quota / billing errors never recover by waiting, even when
    // the provider labels them 429.
    if is_quota_exhausted_error(error) {
        return false;
    }

    // An explicit provider verdict (`x-should-retry`, sent by Anthropic) wins
    // over status heuristics, as in the official SDKs.
    if let Some(verdict) = provider_should_retry(error) {
        return verdict;
    }

    // Prefer Rig's preserved provider status over rendered-message heuristics.
    // In particular, Anthropic overloads can arrive as the non-standard 529
    // status.  A known 4xx response must not become retryable merely because
    // its body happens to contain a word such as "connection".
    if let Some(provider_status) = provider_response_status(error) {
        return is_retryable_status(provider_status.as_u16());
    }

    for e in error_sources(error) {
        let msg = e.to_string();

        if message_has_retryable_status(&msg) {
            return true;
        }

        if let Some(io_error) = e.downcast_ref::<std::io::Error>()
            && matches!(
                io_error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            )
        {
            return true;
        }

        let lower = msg.to_lowercase();
        if lower.contains("stream ended")
            || lower.contains("connection reset")
            || lower.contains("timeout")
            || lower.contains("timed out")
            || lower.contains("reset by peer")
            || lower.contains("broken pipe")
            || lower.contains("dns")
            || lower.contains("rate limit")
            || lower.contains("too many requests")
            || lower.contains("service unavailable")
            || lower.contains("temporarily unavailable")
            || lower.contains("overloaded")
            || lower.contains("internal server error")
            || lower.contains("bad gateway")
            || lower.contains("gateway timeout")
            || lower.contains("error sending request")
            || lower.contains("closed before message completed")
            || lower.contains("error reading a body from connection")
        {
            return true;
        }
    }
    false
}

fn provider_should_retry(error: &(dyn std::error::Error + 'static)) -> Option<bool> {
    let response = error_sources(error).find_map(|candidate| {
        candidate.downcast_ref::<crate::provider_http::ProviderRetryAfterError>()
    })?;
    let value = response.headers.get("x-should-retry")?.to_str().ok()?;
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn is_retryable_status(code: u16) -> bool {
    // Like the official SDKs: every 5xx except 501 Not Implemented
    // (Cloudflare 520-524/530, 507, 598/599 proxies included).
    matches!(code, 408 | 409 | 425 | 429) || (500..=599).contains(&code) && code != 501
}

fn message_has_retryable_status(message: &str) -> bool {
    let tokens = message
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let retryable = |token: &str| token.parse().is_ok_and(is_retryable_status);

    tokens
        .windows(2)
        .any(|window| matches!(window[0].as_str(), "http" | "status") && retryable(&window[1]))
        || tokens.windows(3).any(|window| {
            matches!(
                (window[0].as_str(), window[1].as_str()),
                ("status", "code") | ("http", "status") | ("http", "error")
            ) && retryable(&window[2])
        })
}

fn provider_response_status(error: &(dyn std::error::Error + 'static)) -> Option<http::StatusCode> {
    for candidate in error_sources(error) {
        if let Some(error) = candidate.downcast_ref::<rig::completion::PromptError>()
            && let Some(status) = error.provider_response_status()
        {
            return Some(status);
        }
        if let Some(error) = candidate.downcast_ref::<rig::completion::CompletionError>()
            && let Some(status) = error.provider_response_status()
        {
            return Some(status);
        }
        if let Some(response) =
            candidate.downcast_ref::<crate::provider_http::ProviderRetryAfterError>()
        {
            return Some(response.status);
        }
    }
    None
}

/// Provider-neutral recognition for errors that mean the submitted prompt did
/// not fit the model context window. These are non-retryable without changing
/// the conversation, so callers should surface compaction guidance instead of
/// suggesting an ordinary retry.
pub fn is_context_length_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("max context length")
        || lower.contains("prompt is too long")
        || lower.contains("request too large for model")
        // Amazon Bedrock ValidationException.
        || lower.contains("input is too long for requested model")
        // Google Gemini INVALID_ARGUMENT: "The input token count (N)
        // exceeds the maximum number of tokens allowed (M)".
        || (lower.contains("input token count")
            && (lower.contains("exceed") || lower.contains("too long")))
        // GitHub Copilot: "prompt token count of N exceeds the limit of M"
        // (code `model_max_prompt_tokens_exceeded`).
        || lower.contains("model_max_prompt_tokens_exceeded")
        || (lower.contains("prompt token count") && lower.contains("exceed"))
        || (lower.contains("context window")
            && (lower.contains("exceed")
                || lower.contains("too long")
                || lower.contains("too large")))
        || (lower.contains("context limit")
            && (lower.contains("exceed") || lower.contains("too long")))
}

/// The user-facing text of a terminal provider error: the message plus
/// recovery guidance for context overflow or a provider deadline.
pub fn with_provider_error_hint(error: &(dyn std::error::Error + 'static)) -> String {
    let message = with_context_length_hint(&error.to_string());
    if is_quota_exhausted_error(error) {
        return format!("{message}\n{QUOTA_EXHAUSTED_HINT}");
    }
    if is_provider_timeout(error) {
        return format!("{message}\n{PROVIDER_TIMEOUT_HINT}");
    }
    match provider_retry_hint(error, SystemTime::now()) {
        Some(wait) if !wait.is_zero() => format!(
            "{message}\nThe provider's rate limit resets in about {}s.",
            wait.as_secs().max(1)
        ),
        _ => message,
    }
}

/// Guidance appended to a provider deadline failure, naming the keys to raise.
pub const PROVIDER_TIMEOUT_HINT: &str = "The provider did not respond within the configured deadline. For slow first tokens (large prompts, local model loading, long reasoning) raise `[provider_timeouts] stream_idle_timeout_secs` (default 300) or `connect_timeout_secs` (default 30); a custom provider's own `stream_idle_timeout_secs` takes precedence.";

fn is_provider_timeout(error: &(dyn std::error::Error + 'static)) -> bool {
    error_sources(error).any(|candidate| {
        candidate
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
            || candidate
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
    })
}

pub fn with_context_length_hint(message: &str) -> String {
    if crate::agent::image_budget::is_image_limit_error_message(message) {
        format!(
            "{message}\nThe request carried too many images. Older images are replaced by a placeholder automatically (at most 20 per request); retry, or start a new turn after /compress."
        )
    } else if is_context_length_error_message(message) {
        format!(
            "{message}\nContext limit reached. Run /compress before retrying. Automatic compaction requires compact_enabled = true (the default)."
        )
    } else {
        message.to_string()
    }
}

pub async fn retry_stream_chat<T, E, Fut, S>(
    config: &RetryConfig,
    factory: impl FnMut() -> Fut,
) -> Result<Pin<Box<dyn futures::Stream<Item = Result<T, E>> + Send>>, E>
where
    E: std::error::Error + Send + 'static,
    Fut: std::future::Future<Output = S>,
    S: futures::Stream<Item = Result<T, E>> + Send + Unpin + 'static,
    T: Send + 'static,
{
    retry_stream_chat_with(config, factory, |_| async {}).await
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryNotice {
    pub attempt: usize,
    pub max_attempts: usize,
    pub delay: Duration,
    pub error: String,
}

pub async fn retry_stream_chat_with<T, E, Fut, S, C, CFut>(
    config: &RetryConfig,
    mut factory: impl FnMut() -> Fut,
    mut on_retry: C,
) -> Result<Pin<Box<dyn futures::Stream<Item = Result<T, E>> + Send>>, E>
where
    E: std::error::Error + Send + 'static,
    Fut: std::future::Future<Output = S>,
    S: futures::Stream<Item = Result<T, E>> + Send + Unpin + 'static,
    T: Send + 'static,
    C: FnMut(RetryNotice) -> CFut,
    CFut: std::future::Future<Output = ()>,
{
    let config = &config.bounded();
    let mut attempt: usize = 0;
    let mut backoff = Duration::from_millis(config.initial_backoff_ms);
    let max_backoff = Duration::from_millis(config.max_backoff_ms);
    let mut image_budget_retry_used = false;

    loop {
        attempt += 1;
        let mut stream = factory().await;
        let first = stream.next().await;
        match first {
            Some(Ok(item)) => {
                return Ok(futures::stream::once(std::future::ready(Ok(item)))
                    .chain(stream)
                    .boxed());
            }
            Some(Err(e)) => {
                // A "many-image requests" rejection: shrink the per-request
                // image budget and rebuild the request once (mini-agent-xql2b).
                // It is not an ordinary retry, so it costs no attempt or delay.
                if !image_budget_retry_used
                    && error_sources(&e).any(|c| {
                        crate::agent::image_budget::is_image_limit_error_message(&c.to_string())
                    })
                    && crate::agent::image_budget::tighten_after_provider_rejection()
                {
                    image_budget_retry_used = true;
                    attempt -= 1;
                    tracing::warn!(
                        "provider rejected the image count; retrying with fewer images: {e}"
                    );
                    continue;
                }
                if attempt >= config.max_attempts || !is_retryable(&e) {
                    return Err(e);
                }
                let delay = retry_delay_for_error_with_hint_ceiling(
                    backoff,
                    max_backoff,
                    config.hint_ceiling(),
                    &e,
                );
                tracing::warn!(
                    "retryable error on first stream item (attempt {attempt}/{}), retrying in {:.1}s: {e}",
                    config.max_attempts,
                    delay.as_secs_f64()
                );
                on_retry(RetryNotice {
                    attempt,
                    max_attempts: config.max_attempts,
                    delay,
                    error: e.to_string(),
                })
                .await;
                tokio::time::sleep(delay).await;
                backoff = (backoff * 2).min(max_backoff);
            }
            None => return Ok(stream.boxed()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn many_image_rejection_tightens_the_budget_and_retries_once_without_a_delay() {
        let _guard = crate::agent::image_budget::CAP_TEST_LOCK.lock().await;
        crate::agent::image_budget::reset_cap_for_tests();
        let calls = Arc::new(Mutex::new(0usize));
        let observed = Arc::clone(&calls);
        let config = RetryConfig {
            max_attempts: 1,
            ..RetryConfig::default()
        };
        let stream = retry_stream_chat(&config, || {
            let calls = Arc::clone(&observed);
            async move {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                let item: Result<u8, io::Error> = if *calls == 1 {
                    Err(io::Error::other(
                        "invalid_request_error: image dimensions exceed max allowed size for many-image requests: 2000 pixels",
                    ))
                } else {
                    Ok(7)
                };
                futures::stream::iter(vec![item])
            }
        })
        .await
        .expect("second attempt succeeds");
        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1);
        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(
            crate::agent::image_budget::image_cap(),
            crate::agent::image_budget::MAX_REQUEST_IMAGES / 2
        );
        crate::agent::image_budget::reset_cap_for_tests();
        // A second rejection is surfaced rather than retried again.
        let failures = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&failures);
        let error = retry_stream_chat(&config, || {
            let seen = Arc::clone(&seen);
            async move {
                *seen.lock().unwrap() += 1;
                futures::stream::iter(vec![Err::<u8, _>(io::Error::other("Too many images"))])
            }
        })
        .await
        .err()
        .expect("persistent rejection fails");
        assert!(error.to_string().contains("Too many images"));
        assert_eq!(*failures.lock().unwrap(), 2);
        crate::agent::image_budget::reset_cap_for_tests();
    }

    #[test]
    fn retry_after_seconds_http_dates_and_invalid_values_are_parsed_without_waiting() {
        let now = httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        for (value, expected) in [
            ("0", Some(0)),
            (" 120\t", Some(120)),
            ("Sun, 06 Nov 1994 08:49:39 GMT", Some(2)),
            ("Sunday, 06-Nov-94 08:49:39 GMT", Some(2)),
            ("Sun Nov  6 08:49:39 1994", Some(2)),
            ("Sun, 06 Nov 1994 08:49:36 GMT", Some(0)),
            ("", None),
            ("-1", None),
            ("+1", None),
            ("1.5", None),
            ("invalid", None),
        ] {
            assert_eq!(
                parse_retry_after(value, now),
                expected.map(Duration::from_secs),
                "{value}"
            );
        }
        assert_eq!(
            parse_retry_after("99999999999999999999999999", now),
            Some(Duration::from_secs(u64::MAX))
        );
    }

    fn hinted(
        status: u16,
        headers: &[(&str, &str)],
    ) -> crate::provider_http::ProviderRetryAfterError {
        crate::provider_http::ProviderRetryAfterError::for_test(status, headers, "provider body")
    }

    #[test]
    fn provider_retry_after_may_exceed_max_backoff_up_to_the_hint_ceiling() {
        let config = RetryConfig {
            max_backoff_ms: 10_000,
            ..RetryConfig::default()
        };
        let error = hinted(429, &[("retry-after", "45")]);
        let delay = retry_delay_for_error_with_hint_ceiling(
            Duration::from_millis(500),
            Duration::from_millis(config.max_backoff_ms),
            config.hint_ceiling(),
            &error,
        );
        assert!(delay >= Duration::from_secs(45), "{delay:?}");
        assert!(delay <= Duration::from_millis(DEFAULT_MAX_RETRY_AFTER_MS));

        // A hint beyond the ceiling is capped at it, never ignored.
        let error = hinted(429, &[("retry-after", "3600")]);
        assert_eq!(
            retry_delay_for_error_with_hint_ceiling(
                Duration::ZERO,
                Duration::from_secs(10),
                config.hint_ceiling(),
                &error,
            ),
            Duration::from_millis(DEFAULT_MAX_RETRY_AFTER_MS)
        );

        // `max_retry_after_ms = 0` restores capping at max_backoff.
        let legacy = RetryConfig {
            max_retry_after_ms: 0,
            ..config.clone()
        };
        let error = hinted(429, &[("retry-after", "45")]);
        assert_eq!(
            retry_delay_for_error_with_hint_ceiling(
                Duration::ZERO,
                Duration::from_secs(10),
                legacy.hint_ceiling(),
                &error,
            ),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn unhinted_429_keeps_ordinary_backoff() {
        let error = hinted(429, &[("x-request-id", "ignored")]);
        let delay = retry_delay_for_error_with_hint_ceiling(
            Duration::from_millis(500),
            Duration::from_secs(10),
            Duration::from_secs(300),
            &error,
        );
        assert!(delay >= Duration::from_millis(500) && delay < Duration::from_secs(1));
        assert!(is_retryable(&error));
    }

    #[test]
    fn retry_after_ms_takes_precedence_over_retry_after() {
        let error = hinted(429, &[("retry-after-ms", "2500"), ("retry-after", "60")]);
        assert_eq!(
            retry_delay_for_error_with_hint_ceiling(
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(300),
                &error,
            ),
            Duration::from_millis(2500)
        );
        for invalid in ["-5", "NaN", "inf", "soon"] {
            assert_eq!(parse_retry_after_ms(invalid), None, "{invalid}");
        }
        assert_eq!(
            parse_retry_after_ms(" 1.5 "),
            Some(Duration::from_micros(1500))
        );
    }

    #[test]
    fn x_should_retry_header_overrides_status_classification() {
        assert!(!is_retryable(&hinted(529, &[("x-should-retry", "false")])));
        assert!(is_retryable(&hinted(400, &[("x-should-retry", "true")])));
        assert!(is_retryable(&hinted(529, &[("x-should-retry", "maybe")])));
        // Context overflow is never retried, whatever the header says.
        let overflow = crate::provider_http::ProviderRetryAfterError::for_test(
            400,
            &[("x-should-retry", "true")],
            "prompt is too long",
        );
        assert!(!is_retryable(&overflow));
    }

    #[test]
    fn retry_after_ceiling_is_clamped_and_project_capped() {
        let mut cfg = RetryConfig {
            max_retry_after_ms: u64::MAX,
            ..RetryConfig::default()
        };
        assert_eq!(cfg.clamp_to_limits(), vec!["max_retry_after_ms"]);
        assert_eq!(cfg.max_retry_after_ms, MAX_RETRY_AFTER_CEILING_MS);
        let mut project = cfg.clone();
        assert_eq!(
            project.cap_at(&RetryConfig::default()),
            vec!["max_retry_after_ms"]
        );
        assert_eq!(project.max_retry_after_ms, DEFAULT_MAX_RETRY_AFTER_MS);
    }

    #[test]
    fn retry_notice_text_names_the_actual_wait() {
        assert_eq!(
            crate::event::retry_notice_text(1, 3, 45_000),
            "retrying in 45s... (1/3)"
        );
        assert_eq!(
            crate::event::retry_notice_text(2, 3, 1_250),
            "retrying in 1.2s... (2/3)"
        );
    }

    #[test]
    fn quota_exhausted_429_is_not_retried_and_says_to_check_billing() {
        let quota = crate::provider_http::ProviderRetryAfterError::for_test(
            429,
            &[("retry-after", "20")],
            r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        );
        assert!(!is_retryable(&quota));
        let message = with_provider_error_hint(&quota);
        assert!(message.contains("billing"), "{message}");
        let anthropic = io::Error::other(
            "HTTP status 400: Your credit balance is too low to access the Anthropic API",
        );
        assert!(!is_retryable(&anthropic));
        // An ordinary rate limit is still retried.
        assert!(is_retryable(&hinted(429, &[])));
    }

    #[test]
    fn rate_limit_reset_headers_are_used_when_retry_after_is_missing() {
        let now = SystemTime::now();
        let reset = chrono::DateTime::<chrono::Utc>::from(now + Duration::from_secs(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let anthropic = hinted(
            429,
            &[
                ("anthropic-ratelimit-requests-reset", reset.as_str()),
                ("anthropic-ratelimit-tokens-reset", "not-a-date"),
            ],
        );
        let wait = provider_retry_hint(&anthropic, now).unwrap();
        assert!(
            wait > Duration::from_secs(28) && wait <= Duration::from_secs(30),
            "{wait:?}"
        );
        let openai = hinted(
            429,
            &[
                ("x-ratelimit-reset-requests", "1s"),
                ("x-ratelimit-reset-tokens", "6m0s"),
            ],
        );
        assert_eq!(
            provider_retry_hint(&openai, now),
            Some(Duration::from_secs(360))
        );
        // Retry-After still wins, and non-429 responses ignore reset headers.
        let explicit = hinted(
            429,
            &[("retry-after", "2"), ("x-ratelimit-reset-tokens", "6m0s")],
        );
        assert_eq!(
            provider_retry_hint(&explicit, now),
            Some(Duration::from_secs(2))
        );
        let overloaded = hinted(529, &[("x-ratelimit-reset-tokens", "6m0s")]);
        assert_eq!(provider_retry_hint(&overloaded, now), None);
        assert!(with_provider_error_hint(&openai).contains("resets in about 360s"));
        for (value, expected) in [
            ("20ms", Some(Duration::from_millis(20))),
            ("1h2m3s", Some(Duration::from_secs(3723))),
            ("1.5s", Some(Duration::from_millis(1500))),
            ("7", Some(Duration::from_secs(7))),
            ("", None),
            ("5x", None),
            ("m", None),
        ] {
            assert_eq!(parse_go_duration(value), expected, "{value}");
        }
    }

    use crate::provider_http::ProviderRetryAfterError;

    const GEMINI_429_MINUTE: &str = r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details. * Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_requests, limit: 15\nPlease retry in 38.2s.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaMetric":"generativelanguage.googleapis.com/generate_content_free_tier_requests","quotaId":"GenerateRequestsPerMinutePerProjectPerModel-FreeTier"}]},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"38s"}]}}"#;

    #[test]
    fn gemini_per_minute_429_is_retried_with_the_body_delay() {
        let error = ProviderRetryAfterError::for_test(429, &[], GEMINI_429_MINUTE);
        assert!(!is_quota_exhausted_error(&error));
        assert!(is_retryable(&error));
        let delay = retry_delay_for_error_with_hint_ceiling(
            Duration::from_millis(500),
            Duration::from_secs(10),
            Duration::from_secs(300),
            &error,
        );
        assert_eq!(delay, Duration::from_millis(38_200));
        // The hint ceiling still clamps it.
        let clamped = retry_delay_for_error_with_hint_ceiling(
            Duration::from_millis(500),
            Duration::from_secs(10),
            Duration::from_secs(20),
            &error,
        );
        assert_eq!(clamped, Duration::from_secs(20));
    }

    #[test]
    fn gemini_daily_or_unhinted_exhaustion_stays_terminal() {
        let daily = ProviderRetryAfterError::for_test(
            429,
            &[],
            r#"You exceeded your current quota. Quota exceeded for metric: generate_content_free_tier_requests, quotaId: GenerateRequestsPerDayPerProjectPerModel-FreeTier. Please retry in 38s"#,
        );
        assert!(is_quota_exhausted_error(&daily));
        assert!(!is_retryable(&daily));
        let bare = ProviderRetryAfterError::for_test(
            429,
            &[],
            "You exceeded your current quota, please check your plan and billing details.",
        );
        assert!(!is_retryable(&bare));
    }

    #[test]
    fn body_retry_hints_parse_prose_and_retry_info() {
        for (text, expected) in [
            (r#""retryDelay": "38s""#, Some(Duration::from_secs(38))),
            (
                r#"\"retryDelay\":\"1.5s\""#,
                Some(Duration::from_millis(1500)),
            ),
            (
                "Please try again in 1.5s.",
                Some(Duration::from_millis(1500)),
            ),
            ("Please try again in 20ms.", Some(Duration::from_millis(20))),
            ("Please retry in 2m", Some(Duration::from_secs(120))),
            ("nothing here", None),
            ("retry in soon", None),
        ] {
            assert_eq!(parse_body_retry_hint(text), expected, "{text}");
        }
        // Only 429/503 bodies are consulted.
        let teapot = ProviderRetryAfterError::for_test(418, &[], "Please retry in 5s");
        assert_eq!(provider_retry_hint(&teapot, SystemTime::now()), None);
        let openai = ProviderRetryAfterError::for_test(
            429,
            &[],
            "Rate limit reached. Please try again in 1.5s.",
        );
        assert_eq!(
            provider_retry_hint(&openai, SystemTime::now()),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn broad_5xx_statuses_are_retryable_except_not_implemented() {
        for code in [500, 502, 507, 520, 521, 522, 523, 524, 530, 598, 599] {
            assert!(is_retryable(&hinted(code, &[])), "{code}");
        }
        assert!(!is_retryable(&hinted(501, &[])));
        assert!(!is_retryable(&hinted(400, &[])));
        assert!(is_retryable(&hinted(522, &[("x-should-retry", "true")])));
        assert!(!is_retryable(&hinted(522, &[("x-should-retry", "false")])));
    }

    #[test]
    fn truncated_body_transport_errors_are_retryable() {
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(is_retryable(&eof));
        let hyper = std::io::Error::other("connection closed before message completed");
        assert!(is_retryable(&hyper));
        let body = std::io::Error::other("error reading a body from connection");
        assert!(is_retryable(&body));
    }

    #[test]
    fn test_retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_attempts, 6);
        assert_eq!(cfg.initial_backoff_ms, 500);
        assert_eq!(cfg.max_backoff_ms, 30_000);
        assert!(cfg.max_attempts <= MAX_RETRY_ATTEMPTS);
        assert_eq!(cfg.max_retry_after_ms, DEFAULT_MAX_RETRY_AFTER_MS);
    }

    #[test]
    fn test_is_retryable_connection_error() {
        let err = io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused");
        assert!(is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_timeout() {
        let err = io::Error::new(io::ErrorKind::TimedOut, "operation timed out");
        assert!(is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_http_429() {
        let err = io::Error::other("HTTP 429 Too Many Requests");
        assert!(is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_http_500() {
        let err = io::Error::other("HTTP 500 Internal Server Error");
        assert!(is_retryable(&err));
    }

    #[test]
    fn cyclic_error_sources_are_bounded_for_classification_and_delay() {
        #[derive(Debug)]
        struct Cycle;
        impl std::fmt::Display for Cycle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("cycle")
            }
        }
        impl std::error::Error for Cycle {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self)
            }
        }
        assert_eq!(error_sources(&Cycle).count(), 32);
        assert!(!is_retryable(&Cycle));
        assert_eq!(
            retry_delay_for_error(Duration::ZERO, Duration::from_secs(1), &Cycle),
            Duration::ZERO
        );
    }

    #[test]
    fn test_is_retryable_http_408_409_and_425() {
        for code in [408, 409, 425] {
            let status = http::StatusCode::from_u16(code).unwrap();
            let completion =
                rig::completion::CompletionError::from_http_response(status, "retry later");
            assert!(is_retryable(&completion), "provider status {code}");
            let prompt = rig::completion::PromptError::from(completion);
            assert!(is_retryable(&prompt), "prompt status {code}");
            for message in [
                format!("HTTP {code}"),
                format!("HTTP error {code}"),
                format!("status code {code}"),
            ] {
                assert!(
                    is_retryable(&io::Error::other(message.clone())),
                    "{message}"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_request_statuses_schedule_a_bounded_stream_retry() {
        for code in [408, 409, 425] {
            let mut calls = 0;
            let mut notices = Vec::new();
            let config = RetryConfig {
                max_attempts: 2,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
                max_retry_after_ms: 0,
            };
            let mut stream = retry_stream_chat_with(
                &config,
                || {
                    calls += 1;
                    let item = if calls == 1 {
                        Err(rig::completion::CompletionError::from_http_response(
                            http::StatusCode::from_u16(code).unwrap(),
                            "retry later",
                        ))
                    } else {
                        Ok("recovered")
                    };
                    async { futures::stream::iter([item]) }
                },
                |notice| {
                    notices.push(notice);
                    std::future::ready(())
                },
            )
            .await
            .unwrap();
            assert_eq!(stream.next().await.unwrap().unwrap(), "recovered");
            assert_eq!(calls, 2);
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].attempt, 1);
            assert_eq!(notices[0].delay, Duration::from_millis(1));
        }
    }

    #[test]
    fn test_is_retryable_anthropic_529_from_provider_status() {
        let status = http::StatusCode::from_u16(529).expect("529 is a valid extension status");
        let err = rig::completion::CompletionError::from_http_response(
            status,
            r#"{"type":"overloaded_error"}"#,
        );

        assert!(is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_anthropic_overload_message_fallback() {
        let err = io::Error::other(r#"Invalid status code 529: {"type":"overloaded_error"}"#);

        assert!(is_retryable(&err));
    }

    #[test]
    fn context_length_token_counts_never_trigger_status_fallback() {
        let err =
            io::Error::other("prompt is too long: 213500 tokens > 200000 maximum context length");

        assert!(!is_retryable(&err));
    }

    #[test]
    fn unrelated_digits_and_connection_words_are_not_retryable() {
        for message in [
            "request id 500 was rejected by policy",
            "MCP connection refused: authentication required",
            "connection configuration is invalid",
            "processed 1500 input tokens",
        ] {
            let err = io::Error::other(message);
            assert!(!is_retryable(&err), "unexpected retry for: {message}");
        }
    }

    #[test]
    fn structured_status_message_fallbacks_remain_retryable() {
        for message in [
            "HTTP 503 Service Unavailable",
            "HTTP error 502 from upstream",
            "response status 504",
            "invalid status code 529",
        ] {
            let err = io::Error::other(message);
            assert!(is_retryable(&err), "missed retryable status: {message}");
        }
    }

    #[test]
    fn test_provider_status_takes_precedence_over_message_fallback() {
        let err = rig::completion::CompletionError::from_http_response(
            http::StatusCode::BAD_REQUEST,
            r#"{"error":"connection configuration is invalid"}"#,
        );

        assert!(!is_retryable(&err));
    }

    #[test]
    fn test_prompt_error_forwards_retryable_provider_status() {
        let status = http::StatusCode::from_u16(529).expect("529 is a valid extension status");
        let completion = rig::completion::CompletionError::from_http_response(
            status,
            r#"{"type":"overloaded_error"}"#,
        );
        let err = rig::completion::PromptError::from(completion);

        assert!(is_retryable(&err));
    }

    #[test]
    fn test_is_not_retryable_parse_error() {
        let err = serde_json::Error::io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected eof",
        ));
        assert!(!is_retryable(&err));
    }

    #[test]
    fn test_is_not_retryable_generic() {
        let err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        assert!(!is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_walks_source_chain() {
        let inner = io::Error::new(io::ErrorKind::TimedOut, "timed out");
        let outer = io::Error::other(inner);
        assert!(is_retryable(&outer));
    }

    #[test]
    fn context_length_classifier_covers_provider_variants() {
        for message in [
            "Anthropic API error: prompt is too long: 201000 tokens > 200000 maximum",
            "OpenAI: This model's maximum context length is 128000 tokens",
            "OpenRouter upstream error: context_length_exceeded",
            "input exceeds the context window for this model",
            "Gemini INVALID_ARGUMENT: The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).",
            "Bedrock ValidationException: Input is too long for requested model.",
            "Copilot 400 model_max_prompt_tokens_exceeded: prompt token count of 140000 exceeds the limit of 128000",
        ] {
            assert!(
                is_context_length_error_message(message),
                "missed provider variant: {message}"
            );
        }
        assert!(!is_context_length_error_message(
            "HTTP 400: invalid temperature"
        ));
    }

    #[test]
    fn context_length_hint_names_manual_and_automatic_recovery() {
        let hinted = with_context_length_hint("context_length_exceeded");
        assert!(hinted.contains("/compress"));
        assert!(hinted.contains("compact_enabled = true"));
        assert_eq!(
            with_context_length_hint("permission denied"),
            "permission denied"
        );
    }

    #[test]
    fn retry_config_clamps_extreme_values_to_the_safety_envelope() {
        let mut cfg = RetryConfig {
            max_attempts: 4_000_000_000,
            initial_backoff_ms: 9_000_000_000_000_000,
            max_backoff_ms: u64::MAX,
            max_retry_after_ms: 0,
        };
        let changed = cfg.clamp_to_limits();
        assert_eq!(cfg.max_attempts, MAX_RETRY_ATTEMPTS);
        assert_eq!(cfg.max_backoff_ms, MAX_RETRY_BACKOFF_MS);
        assert_eq!(cfg.initial_backoff_ms, MAX_RETRY_BACKOFF_MS);
        assert_eq!(
            changed,
            ["max_attempts", "max_backoff_ms", "initial_backoff_ms"]
        );

        let mut zero = RetryConfig {
            max_attempts: 0,
            initial_backoff_ms: 5_000,
            max_backoff_ms: 1_000,
            max_retry_after_ms: 0,
        };
        assert_eq!(
            zero.clamp_to_limits(),
            ["max_attempts", "initial_backoff_ms"]
        );
        assert_eq!(zero.max_attempts, 1);
        assert_eq!(zero.initial_backoff_ms, 1_000);

        let mut defaults = RetryConfig::default();
        assert!(defaults.clamp_to_limits().is_empty());
    }

    #[test]
    fn retry_config_cap_only_tightens_toward_the_ceiling() {
        let ceiling = RetryConfig::default();
        let mut looser = RetryConfig {
            max_attempts: 9,
            initial_backoff_ms: 50,
            max_backoff_ms: 45_000,
            max_retry_after_ms: 0,
        };
        assert_eq!(looser.cap_at(&ceiling), ["max_attempts", "max_backoff_ms"]);
        assert_eq!(looser.max_attempts, 6);
        assert_eq!(looser.initial_backoff_ms, 50);
        assert_eq!(looser.max_backoff_ms, 30_000);

        let mut tighter = RetryConfig {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            max_retry_after_ms: 0,
        };
        assert!(tighter.cap_at(&ceiling).is_empty());
    }

    #[test]
    fn first_retry_delay_never_exceeds_max_backoff() {
        // The initial backoff above the maximum and jitter on top of it both
        // used to leak past `max_backoff_ms`.
        for (initial, max) in [(500, 10_000), (10_000, 10_000), (9_000, 1_000), (5, 0)] {
            let max_backoff = Duration::from_millis(max);
            for _ in 0..64 {
                let delay = retry_delay(Duration::from_millis(initial), max_backoff);
                assert!(delay <= max_backoff, "{delay:?} > {max_backoff:?}");
            }
        }
    }

    #[tokio::test]
    async fn shared_retry_primitive_bounds_an_unclamped_config() {
        // Compaction summaries, goal judging and session titles hand
        // `cfg.retry` straight to this primitive; an out-of-range policy
        // built anywhere must still stop at the ceiling without a long sleep.
        let config = RetryConfig {
            max_attempts: usize::MAX,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            max_retry_after_ms: 0,
        };
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notices = Arc::new(Mutex::new(Vec::new()));
        let attempt_counter = Arc::clone(&attempts);
        let observed = Arc::clone(&notices);
        let result = retry_stream_chat_with(
            &config,
            move || {
                attempt_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    futures::stream::iter(vec![Err::<&str, _>(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out",
                    ))])
                }
            },
            move |notice| {
                let observed = Arc::clone(&observed);
                async move { observed.lock().unwrap().push(notice) }
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            MAX_RETRY_ATTEMPTS
        );
        let notices = notices.lock().unwrap();
        assert!(
            notices
                .iter()
                .all(|notice| notice.max_attempts == MAX_RETRY_ATTEMPTS
                    && notice.delay == Duration::ZERO)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_retry_primitive_caps_a_huge_initial_backoff() {
        let config = RetryConfig {
            max_attempts: 2,
            initial_backoff_ms: 9_000_000_000_000_000,
            max_backoff_ms: 9_000_000_000_000_000,
            max_retry_after_ms: 0,
        };
        let notices = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&notices);
        let result = retry_stream_chat_with(
            &config,
            || async {
                futures::stream::iter(vec![Err::<&str, _>(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out",
                ))])
            },
            move |notice| {
                let observed = Arc::clone(&observed);
                async move { observed.lock().unwrap().push(notice) }
            },
        )
        .await;

        assert!(result.is_err());
        let notices = notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].delay <= Duration::from_millis(MAX_RETRY_BACKOFF_MS));
    }

    #[tokio::test]
    async fn shared_retry_primitive_emits_one_notice_per_scheduled_retry() {
        let config = RetryConfig {
            max_attempts: 3,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            max_retry_after_ms: 0,
        };
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notices = Arc::new(Mutex::new(Vec::new()));
        let attempt_counter = Arc::clone(&attempts);
        let observed = Arc::clone(&notices);

        let mut stream = retry_stream_chat_with(
            &config,
            move || {
                let attempt = attempt_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if attempt < 2 {
                        futures::stream::iter(vec![Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("attempt {attempt}"),
                        ))])
                    } else {
                        futures::stream::iter(vec![Ok::<_, io::Error>("done")])
                    }
                }
            },
            move |notice| {
                let observed = Arc::clone(&observed);
                async move { observed.lock().unwrap().push(notice) }
            },
        )
        .await
        .unwrap();

        assert_eq!(stream.next().await.unwrap().unwrap(), "done");
        let notices = notices.lock().unwrap();
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].attempt, 1);
        assert_eq!(notices[1].attempt, 2);
        assert!(notices.iter().all(|notice| notice.max_attempts == 3));
    }
}
