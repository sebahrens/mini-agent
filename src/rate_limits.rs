//! Provider rate-limit and plan-window status read from the headers of
//! successful responses (mini-agent-nvnkx.45).
//!
//! Only non-secret numeric/time headers are kept: `anthropic-ratelimit-*`,
//! `x-ratelimit-*`, `ratelimit-*` and `x-codex-*` plan windows. The latest
//! snapshot per provider host lives in a small process-wide store that
//! `/cost` and the headless result read.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Fraction of a bucket below which the status line warns.
pub(crate) const WARN_BELOW_FRACTION: f64 = 0.10;
/// Header values longer than this are not numeric/time data and are ignored.
const MAX_VALUE_LEN: usize = 64;
/// Cap on distinct provider hosts (subagents on other gateways) kept.
const MAX_HOSTS: usize = 8;

/// Whether a response header belongs in the rate-limit snapshot.
pub(crate) fn is_rate_limit_header(name: &str) -> bool {
    name.starts_with("anthropic-ratelimit-")
        || name.starts_with("x-ratelimit-")
        || name.starts_with("ratelimit-")
        || name.starts_with("x-codex-")
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub(crate) struct Bucket {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remaining: Option<f64>,
    /// Reset as the provider wrote it (RFC 3339 instant or a duration).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reset: Option<String>,
    /// Plan-window usage in percent (`x-codex-*-used-percent`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) used_percent: Option<f64>,
}

impl Bucket {
    /// Remaining share of the bucket in 0..=1 when it can be computed.
    pub(crate) fn remaining_fraction(&self) -> Option<f64> {
        if let (Some(limit), Some(remaining)) = (self.limit, self.remaining)
            && limit > 0.0
        {
            return Some((remaining / limit).clamp(0.0, 1.0));
        }
        self.used_percent
            .map(|used| (1.0 - used / 100.0).clamp(0.0, 1.0))
    }

    pub(crate) fn describe(&self) -> String {
        let mut text = format!("{}:", self.name);
        match (self.remaining, self.limit) {
            (Some(remaining), Some(limit)) => {
                text.push_str(&format!(" {} of {} left", trim(remaining), trim(limit)));
            }
            (Some(remaining), None) => text.push_str(&format!(" {} left", trim(remaining))),
            _ => {}
        }
        if let Some(used) = self.used_percent {
            text.push_str(&format!(" {}% used", trim(used)));
        }
        if let Some(reset) = &self.reset {
            text.push_str(&format!(", resets {reset}"));
        }
        text
    }
}

fn trim(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct Snapshot {
    pub(crate) host: String,
    pub(crate) buckets: Vec<Bucket>,
    #[serde(skip)]
    pub(crate) captured: Option<SystemTime>,
}

impl Snapshot {
    /// Buckets that dropped below `fraction` of their capacity.
    pub(crate) fn low_buckets(&self, fraction: f64) -> Vec<&Bucket> {
        self.buckets
            .iter()
            .filter(|bucket| bucket.remaining_fraction().is_some_and(|f| f < fraction))
            .collect()
    }
}

/// Builds a snapshot from (lowercase name, value) header pairs; `None` when
/// the response carried no recognised rate-limit header.
pub(crate) fn parse<'a>(
    host: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<Snapshot> {
    let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
    for (name, value) in headers {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_VALUE_LEN || !is_rate_limit_header(name) {
            continue;
        }
        let number = value.parse::<f64>().ok().filter(|n| n.is_finite());
        let (bucket, field) = if let Some(rest) = name.strip_prefix("anthropic-ratelimit-") {
            let Some((bucket, field)) = rest.rsplit_once('-') else {
                continue;
            };
            (bucket.to_string(), field.to_string())
        } else if let Some(rest) = name
            .strip_prefix("x-ratelimit-")
            .or_else(|| name.strip_prefix("ratelimit-"))
        {
            // OpenAI style: `<field>-<bucket>`; bare `limit`/`remaining`/`reset`
            // describe the single default bucket.
            match rest.split_once('-') {
                Some((field, bucket)) => (bucket.to_string(), field.to_string()),
                None => ("requests".to_string(), rest.to_string()),
            }
        } else if let Some(rest) = name.strip_prefix("x-codex-") {
            // `primary-used-percent`, `secondary-reset-after-seconds`, ...
            let Some((window, field)) = rest.split_once('-') else {
                continue;
            };
            let field = match field {
                "used-percent" => "used_percent",
                "reset-after-seconds" => "reset_after_seconds",
                _ => continue,
            };
            (format!("plan {window}"), field.to_string())
        } else {
            continue;
        };
        let slot = buckets.entry(bucket.clone()).or_insert_with(|| Bucket {
            name: bucket,
            ..Bucket::default()
        });
        match field.as_str() {
            "limit" => slot.limit = number,
            "remaining" => slot.remaining = number,
            "used_percent" => slot.used_percent = number,
            "reset" => slot.reset = Some(value.to_string()),
            "reset_after_seconds" => slot.reset = number.map(|s| format!("in {}s", trim(s))),
            _ => {}
        }
    }
    let buckets: Vec<Bucket> = buckets
        .into_values()
        .filter(|b| b.remaining.is_some() || b.used_percent.is_some() || b.limit.is_some())
        .collect();
    (!buckets.is_empty()).then(|| Snapshot {
        host: host.to_string(),
        buckets,
        captured: Some(SystemTime::now()),
    })
}

static STORE: Mutex<Vec<Snapshot>> = Mutex::new(Vec::new());

/// Records the headers of a successful response for `host`.
pub(crate) fn record(host: &str, headers: &http::HeaderMap) {
    let pairs = headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
    let Some(snapshot) = parse(host, pairs) else {
        return;
    };
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = store.iter_mut().find(|s| s.host == snapshot.host) {
        *slot = snapshot;
    } else if store.len() < MAX_HOSTS {
        store.push(snapshot);
    }
}

/// The latest snapshot of every host seen in this process.
pub(crate) fn snapshots() -> Vec<Snapshot> {
    STORE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// One status-line warning when any bucket is nearly exhausted.
pub(crate) fn warning(snapshots: &[Snapshot]) -> Option<String> {
    snapshots.iter().find_map(|snapshot| {
        let low = snapshot.low_buckets(WARN_BELOW_FRACTION);
        let first = low.first()?;
        Some(format!(
            "rate limit low on {}: {}",
            snapshot.host,
            first.describe()
        ))
    })
}

/// Lines for `/cost`.
pub(crate) fn report_lines(snapshots: &[Snapshot]) -> Vec<String> {
    let mut lines = Vec::new();
    for snapshot in snapshots {
        lines.push(format!("Rate limits ({}):", snapshot.host));
        for bucket in &snapshot.buckets {
            lines.push(format!("  {}", bucket.describe()));
        }
    }
    if let Some(warning) = warning(snapshots) {
        lines.push(format!("Warning: {warning}"));
    }
    lines
}

#[cfg(test)]
pub(crate) fn clear_for_test() {
    STORE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_and_anthropic_buckets() {
        let snapshot = parse(
            "api.openai.com",
            [
                ("x-ratelimit-limit-tokens", "100000"),
                ("x-ratelimit-remaining-tokens", "5000"),
                ("x-ratelimit-reset-tokens", "2s"),
                ("x-ratelimit-remaining-requests", "400"),
                ("x-ratelimit-limit-requests", "500"),
                ("content-type", "text/event-stream"),
            ],
        )
        .unwrap();
        let tokens = snapshot
            .buckets
            .iter()
            .find(|b| b.name == "tokens")
            .unwrap();
        assert_eq!(tokens.remaining, Some(5000.0));
        assert_eq!(tokens.reset.as_deref(), Some("2s"));
        // Only the nearly empty bucket trips the 10% warning.
        let low = snapshot.low_buckets(WARN_BELOW_FRACTION);
        assert_eq!(low.len(), 1);
        assert_eq!(low[0].name, "tokens");
        assert!(warning(&[snapshot]).unwrap().contains("tokens"));

        let anthropic = parse(
            "api.anthropic.com",
            [
                ("anthropic-ratelimit-input-tokens-limit", "1000"),
                ("anthropic-ratelimit-input-tokens-remaining", "900"),
                (
                    "anthropic-ratelimit-input-tokens-reset",
                    "2026-11-21T10:00:00Z",
                ),
            ],
        )
        .unwrap();
        assert_eq!(anthropic.buckets[0].name, "input-tokens");
        assert!(anthropic.low_buckets(WARN_BELOW_FRACTION).is_empty());
    }

    #[test]
    fn codex_plan_window_percent_counts_as_usage() {
        let snapshot = parse(
            "chatgpt.com",
            [
                ("x-codex-primary-used-percent", "95"),
                ("x-codex-primary-reset-after-seconds", "120"),
            ],
        )
        .unwrap();
        assert_eq!(snapshot.buckets[0].name, "plan primary");
        assert!(snapshot.buckets[0].remaining_fraction().unwrap() < 0.10);
        assert!(report_lines(&[snapshot])[1].contains("95% used"));
    }

    #[test]
    fn unrelated_headers_yield_no_snapshot() {
        assert!(parse("h", [("content-type", "x"), ("set-cookie", "a=b")]).is_none());
    }
}
