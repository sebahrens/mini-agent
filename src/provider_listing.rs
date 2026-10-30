//! Bounded metadata GETs on the actual retained provider route; never completion authority.
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Weak;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::provider::{AnyClient, ModelEntry, OpenAiClient};
use crate::provider_http::ProviderHttpClient;

pub(crate) const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_PAGES: usize = 32;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
enum Protocol {
    OpenAI,
    Anthropic,
    Gemini,
    OpenRouter,
    Ollama,
}

#[derive(Clone)]
pub(crate) struct ListingKey {
    provider: String,
    transport: Weak<()>,
    protocol: Protocol,
    request_digest: [u8; 32],
}
impl std::fmt::Debug for ListingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ListingKey([REDACTED])")
    }
}
impl PartialEq for ListingKey {
    fn eq(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.protocol == other.protocol
            && self.request_digest == other.request_digest
            && self.transport.ptr_eq(&other.transport)
    }
}
impl Eq for ListingKey {}
impl Hash for ListingKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.provider.hash(h);
        self.protocol.hash(h);
        self.request_digest.hash(h);
        self.transport.as_ptr().hash(h);
    }
}
impl ListingKey {
    pub(crate) fn is_live(&self) -> bool {
        self.transport.strong_count() != 0
    }
    pub(crate) fn matches_provider(&self, provider: &str) -> bool {
        self.provider == provider
    }
    pub(crate) fn has_live_prices(&self) -> bool {
        self.protocol == Protocol::OpenRouter
    }
}

impl AnyClient {
    fn listing_protocol(&self) -> Protocol {
        match self {
            Self::OpenAI(_) => Protocol::OpenAI,
            Self::Anthropic(_) => Protocol::Anthropic,
            Self::Gemini(_) => Protocol::Gemini,
            Self::OpenRouter(_) => Protocol::OpenRouter,
            Self::Ollama(_) => Protocol::Ollama,
            Self::Copilot(_) => Protocol::OpenAI,
        }
    }
    fn listing_request(
        &self,
        path: &str,
    ) -> anyhow::Result<(http::Request<Vec<u8>>, ProviderHttpClient)> {
        macro_rules! build {
            ($client:expr) => {{
                let c = $client;
                (c.get(path)?.body(Vec::new())?, c.http_client().clone())
            }};
        }
        let result: anyhow::Result<_> = (|| {
            Ok(match self {
                Self::OpenAI(OpenAiClient::Responses(c)) => build!(c),
                Self::OpenAI(OpenAiClient::Completions(c, _)) => build!(c),
                Self::Anthropic(c) => build!(c),
                Self::Gemini(c) => build!(c),
                Self::OpenRouter(c) => build!(c),
                Self::Ollama(c) => build!(c),
                Self::Copilot(c) => build!(c),
            })
        })();
        result.map_err(|_| anyhow::anyhow!("invalid model listing route"))
    }
    pub(crate) fn listing_key(&self, provider: &str) -> anyhow::Result<ListingKey> {
        let protocol = self.listing_protocol();
        let (request, backend) = self.listing_request(&path(protocol, None))?;
        let mut digest = Sha256::new();
        let mut part = |bytes: &[u8]| {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        };
        part(request.uri().to_string().as_bytes());
        let mut names: Vec<_> = request.headers().keys().collect();
        names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        part(&(names.len() as u64).to_le_bytes());
        for name in names {
            part(name.as_str().as_bytes());
            let values = request.headers().get_all(name);
            part(&(values.iter().count() as u64).to_le_bytes());
            for value in values.iter() {
                part(value.as_bytes());
            }
        }
        let mut request_digest = [0; 32];
        request_digest.copy_from_slice(digest.finalize().as_ref());
        Ok(ListingKey {
            provider: provider.into(),
            protocol,
            transport: backend.listing_identity(),
            request_digest,
        })
    }
    pub(crate) async fn bounded_model_listing(&self) -> anyhow::Result<Vec<ModelEntry>> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.listing_pages(tokio::time::Instant::now() + Duration::from_secs(10)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("model listing deadline exceeded"))?
    }
    async fn listing_pages(
        &self,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<Vec<ModelEntry>> {
        let protocol = self.listing_protocol();
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut ids = HashSet::new();
        let mut used = 0_usize;
        let mut entries = 0_usize;
        let mut all = Vec::new();
        for _ in 0..MAX_PAGES {
            let (request, backend) = self.listing_request(&path(protocol, cursor.as_deref()))?;
            let body = backend
                .listing_get(request, MAX_BYTES.saturating_sub(used))
                .await?;
            used += body.len();
            let value: Value = serde_json::from_slice(&body)
                .map_err(|_| anyhow::anyhow!("invalid model listing response"))?;
            let array = value
                .get(if matches!(protocol, Protocol::Gemini | Protocol::Ollama) {
                    "models"
                } else {
                    "data"
                })
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("invalid model listing response"))?;
            entries = entries.saturating_add(array.len());
            anyhow::ensure!(entries <= MAX_ENTRIES, "model listing entry limit exceeded");
            for value in array {
                if let Some(model) = decode(protocol, value)? {
                    anyhow::ensure!(ids.insert(model.id.clone()), "duplicate model listing id");
                    if crate::provider::is_agent_model(&model) {
                        all.push(model);
                    }
                }
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "model listing deadline exceeded"
            );
            cursor = next_cursor(protocol, &value)?;
            match &cursor {
                None => return Ok(all),
                Some(cursor) => anyhow::ensure!(
                    cursors.insert(cursor.clone()),
                    "model listing cursor repeated"
                ),
            }
        }
        anyhow::bail!("model listing page limit exceeded")
    }
}
fn path(protocol: Protocol, cursor: Option<&str>) -> String {
    let base = match protocol {
        Protocol::Anthropic => "/v1/models",
        Protocol::Gemini => "/v1beta/models",
        Protocol::Ollama => "/api/tags",
        _ => "/models",
    };
    let mut query =
        reqwest::Url::parse(&format!("https://listing.invalid{base}")).expect("fixed listing path");
    if protocol == Protocol::Anthropic {
        query.query_pairs_mut().append_pair("limit", "1000");
    }
    if protocol == Protocol::Gemini {
        query.query_pairs_mut().append_pair("pageSize", "1000");
    }
    if let Some(cursor) = cursor {
        query.query_pairs_mut().append_pair(
            if protocol == Protocol::Anthropic {
                "after_id"
            } else {
                "pageToken"
            },
            cursor,
        );
    }
    format!(
        "{}{}",
        query.path(),
        query.query().map(|q| format!("?{q}")).unwrap_or_default()
    )
}
fn next_cursor(protocol: Protocol, value: &Value) -> anyhow::Result<Option<String>> {
    let cursor = match protocol {
        Protocol::Anthropic => {
            let more = value
                .get("has_more")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow::anyhow!("invalid model listing pagination"))?;
            if !more {
                return Ok(None);
            }
            value.get("last_id")
        }
        Protocol::Gemini => value.get("nextPageToken"),
        _ => return Ok(None),
    };
    if protocol == Protocol::Gemini && cursor.is_none_or(Value::is_null) {
        return Ok(None);
    }
    let cursor = cursor
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control))
        .ok_or_else(|| anyhow::anyhow!("invalid model listing pagination"))?;
    Ok(Some(cursor.into()))
}
fn text<'a>(value: &'a Value, key: &str, cap: usize) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= cap)
        .ok_or_else(|| anyhow::anyhow!("invalid model listing entry"))
}
fn context(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 0)
}
fn decode(protocol: Protocol, value: &Value) -> anyhow::Result<Option<ModelEntry>> {
    if protocol == Protocol::Gemini
        && !value
            .get("supportedGenerationMethods")
            .and_then(Value::as_array)
            .is_some_and(|methods| {
                methods
                    .iter()
                    .any(|v| v.as_str() == Some("generateContent"))
            })
    {
        return Ok(None);
    }
    let raw_id = text(
        value,
        match protocol {
            Protocol::Gemini => "name",
            Protocol::Ollama => "model",
            _ => "id",
        },
        1024,
    )?;
    let id = if protocol == Protocol::Gemini {
        raw_id.strip_prefix("models/").unwrap_or(raw_id)
    } else {
        raw_id
    };
    anyhow::ensure!(
        !id.is_empty()
            && !id.chars().any(|c| c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        "invalid model listing id"
    );
    let name_key = match protocol {
        Protocol::Anthropic => "display_name",
        Protocol::Gemini => "displayName",
        _ => "name",
    };
    let name = match value.get(name_key) {
        None | Some(Value::Null) => id,
        Some(_) => text(value, name_key, 4096)?,
    };
    let display: String = name
        .chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    anyhow::ensure!(
        display.len() <= 4096,
        "model listing display limit exceeded"
    );
    let mut model = ModelEntry {
        id: id.into(),
        display,
        context_length: match protocol {
            Protocol::Anthropic => context(value, "max_input_tokens"),
            Protocol::Gemini => context(value, "inputTokenLimit"),
            Protocol::OpenRouter => context(value, "context_length"),
            Protocol::OpenAI | Protocol::Ollama => None,
        },
        kind: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
    };
    if protocol == Protocol::OpenRouter {
        let rate = |key| {
            value
                .get("pricing")
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
                .map(|v| v * 1_000_000.0)
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        model.input_price = rate("prompt");
        model.output_price = rate("completion");
    }
    Ok(Some(model))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_listing48_decoder_and_pagination_preserve_exact_native_metadata() {
        let anthropic = decode(Protocol::Anthropic,&serde_json::json!({"id":"claude-opus-5-5","display_name":"Opus 雪","max_input_tokens":200000,"max_tokens":64000})).unwrap().unwrap();
        assert_eq!(anthropic.context_length, Some(200000));
        assert_eq!(anthropic.input_price, None);
        assert_eq!(anthropic.display, "Opus 雪");
        assert!(
            decode(
                Protocol::Anthropic,
                &serde_json::json!({"id":"safe","display_name":"\u{1b}".repeat(4096)})
            )
            .err()
            .unwrap()
            .to_string()
            .contains("display limit")
        );
        let openai=decode(Protocol::OpenAI,&serde_json::json!({"id":"new-model","context_length":1000000,"max_input_tokens":1000000})).unwrap().unwrap();
        assert_eq!(
            openai.context_length, None,
            "unstandardized OpenAI fields are not native capacity authority"
        );

        let gemini = decode(Protocol::Gemini,&serde_json::json!({"name":"models/gemini-precise-001","baseModelId":"gemini-base","inputTokenLimit":1000000,"supportedGenerationMethods":["generateContent"]})).unwrap().unwrap();
        assert_eq!(gemini.id, "gemini-precise-001");
        assert!(decode(Protocol::Gemini,&serde_json::json!({"name":"models/embed","supportedGenerationMethods":["embedContent"]})).unwrap().is_none());
        for bad in [
            serde_json::json!({"has_more":true}),
            serde_json::json!({"has_more":true,"last_id":""}),
            serde_json::json!({"has_more":true,"last_id":"x".repeat(1025)}),
        ] {
            assert!(next_cursor(Protocol::Anthropic, &bad).is_err());
        }
        let safe = decode(Protocol::Anthropic,&serde_json::json!({"id":"safe","display_name":"\u{1b}[31m\n\u{202e}雪","max_input_tokens":0})).unwrap().unwrap();
        assert!(!safe.display.contains('\u{1b}'));
        assert!(!safe.display.contains('\u{202e}'));
        assert_eq!(safe.context_length, None);
        assert!(path(Protocol::Anthropic, Some("cursor&secret=x")).contains("cursor%26secret%3Dx"));
        let free = decode(
            Protocol::OpenRouter,
            &serde_json::json!({"id":"free","pricing":{"prompt":"0","completion":"0"}}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(free.input_price.zip(free.output_price), Some((0.0, 0.0)));
    }
}
