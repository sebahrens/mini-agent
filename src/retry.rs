use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    pub max_attempts: usize,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 500,
            max_backoff_ms: 10_000,
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
        changed
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
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(candidate) = current {
        if is_context_length_error_message(&candidate.to_string()) {
            return false;
        }
        current = candidate.source();
    }

    // Prefer Rig's preserved provider status over rendered-message heuristics.
    // In particular, Anthropic overloads can arrive as the non-standard 529
    // status.  A known 4xx response must not become retryable merely because
    // its body happens to contain a word such as "connection".
    if let Some(provider_status) = provider_response_status(error) {
        let code = provider_status.as_u16();
        return code == 429
            || code == 500
            || code == 502
            || code == 503
            || code == 504
            || code == 529;
    }

    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
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
        {
            return true;
        }

        current = e.source();
    }
    false
}

fn message_has_retryable_status(message: &str) -> bool {
    let tokens = message
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let retryable = |token: &str| matches!(token, "429" | "500" | "502" | "503" | "504" | "529");

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
    let mut current = Some(error);
    while let Some(candidate) = current {
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
        current = candidate.source();
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
        || (lower.contains("context window")
            && (lower.contains("exceed")
                || lower.contains("too long")
                || lower.contains("too large")))
        || (lower.contains("context limit")
            && (lower.contains("exceed") || lower.contains("too long")))
}

pub fn with_context_length_hint(message: &str) -> String {
    if is_context_length_error_message(message) {
        format!(
            "{message}\nContext limit reached. Run /compress before retrying. To compact automatically before future requests, set compact_enabled = true."
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
                if attempt >= config.max_attempts || !is_retryable(&e) {
                    return Err(e);
                }
                let delay = retry_delay(backoff, max_backoff);
                tracing::warn!(
                    "retryable error on first stream item (attempt {attempt}/{}): {e}",
                    config.max_attempts
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

    #[test]
    fn test_retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.initial_backoff_ms, 500);
        assert_eq!(cfg.max_backoff_ms, 10_000);
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
            max_backoff_ms: 30_000,
        };
        assert_eq!(looser.cap_at(&ceiling), ["max_attempts", "max_backoff_ms"]);
        assert_eq!(looser.max_attempts, 3);
        assert_eq!(looser.initial_backoff_ms, 50);
        assert_eq!(looser.max_backoff_ms, 10_000);

        let mut tighter = RetryConfig {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
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
