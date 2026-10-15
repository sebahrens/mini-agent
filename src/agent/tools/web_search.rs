//! Fixed-provider native search. Credentials are captured at agent construction
//! and sent only to Exa's fixed HTTPS search endpoint; redirects are rejected.
use super::web_fetch::{quote_reference, safe_text};
use super::web_http::{FetchPolicy, FetchRequest, NativeWebClient, normalize_fetch_url};
use super::{AskSender, PermCheck, ToolError};
use rig::tool::Tool;
use serde::Deserialize;
const SEARCH_URL: &str = "https://api.exa.ai/search";
const MAX_QUERY_BYTES: usize = 4096;
const MAX_RESULTS: usize = 5;
const MAX_SNIPPET_CHARS: usize = 2000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebSearchArgs {
    query: String,
}

pub(crate) struct WebSearchTool {
    client: NativeWebClient,
    provider: String,
    api_key: Option<String>,
}
impl WebSearchTool {
    pub(crate) fn new(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        provider: Option<&str>,
    ) -> Self {
        Self {
            client: NativeWebClient::new(
                FetchPolicy::from_settings(Some(&["https://api.exa.ai".into()]), false),
                permission,
                ask_tx,
            ),
            provider: provider.unwrap_or("exa").to_owned(),
            api_key: std::env::var("EXA_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty()),
        }
    }
}
impl Tool for WebSearchTool {
    const NAME: &'static str = "web_search";
    type Args = WebSearchArgs;
    type Output = String;
    type Error = ToolError;
    fn description(&self) -> String {
        "Search the public web through Exa, using EXA_API_KEY and web_search host permission. Returns at most five ranked titles, URLs and 2,000-character snippets as untrusted reference data. No returned URL is visited. Provider errors expose status only; missing credentials fail before network I/O.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false})
    }
    async fn call(&self, args: WebSearchArgs) -> Result<String, ToolError> {
        if args.query.trim().is_empty() || args.query.len() > MAX_QUERY_BYTES {
            return Err(ToolError::Msg(
                "web_search requires a nonempty query of at most 4096 bytes".into(),
            ));
        }
        if self.provider != "exa" {
            return Err(ToolError::Msg("web_search provider must be 'exa'; configure web-search-provider or disable this tool".into()));
        }
        let key = self.api_key.as_ref().ok_or_else(|| {
            ToolError::Msg("web_search requires EXA_API_KEY; no search request was sent".into())
        })?;
        if key.len() > 4096 {
            return Err(ToolError::Msg(
                "web_search credential exceeds its limit".into(),
            ));
        }
        let body = serde_json::json!({"query":args.query,"type":"auto","numResults":MAX_RESULTS,"contents":{"text":{"maxCharacters":MAX_SNIPPET_CHARS}}}).to_string();
        let request = FetchRequest::try_new(
            "POST",
            &[
                ("x-api-key".into(), key.clone()),
                ("content-type".into(), "application/json".into()),
            ],
            Some(body),
        )
        .map_err(|_| ToolError::Msg("web_search credential or request is invalid".into()))?;
        let (_, result) = self
            .client
            .execute(SEARCH_URL, &request, Self::NAME, false)
            .await?;
        if !(200..300).contains(&result.status) {
            return Err(ToolError::Msg(format!(
                "web_search provider returned HTTP {}",
                result.status
            )));
        }
        let parsed: serde_json::Value = serde_json::from_str(&result.text)
            .map_err(|_| ToolError::Msg("web_search provider returned invalid JSON".into()))?;
        render_results_with_credential(&parsed, Some(key))
    }
}
#[cfg(test)]
fn bounded(text: &str, cap: usize) -> String {
    bounded_credential(text, cap, None)
}

/// Redact before the character cap so a secret crossing its boundary cannot
/// leave a recoverable prefix. The output cap also bounds replacement growth.
fn bounded_credential(mut text: &str, cap: usize, credential: Option<&str>) -> String {
    let credential = credential.filter(|secret| !secret.is_empty());
    let mut result = String::new();
    let mut count = 0;
    while !text.is_empty() && count < cap {
        if let Some(secret) = credential.filter(|secret| text.starts_with(secret)) {
            for c in "[redacted credential]".chars().take(cap - count) {
                result.push(c);
                count += 1;
            }
            text = &text[secret.len()..];
        } else {
            let c = text.chars().next().unwrap();
            result.push(c);
            count += 1;
            text = &text[c.len_utf8()..];
        }
    }
    safe_text(&result)
}
#[cfg(test)]
fn render_results(response: &serde_json::Value) -> Result<String, ToolError> {
    render_results_with_credential(response, None)
}

fn render_results_with_credential(
    response: &serde_json::Value,
    credential: Option<&str>,
) -> Result<String, ToolError> {
    let results = response
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ToolError::Msg("web_search provider response has no results list".into()))?;
    let mut output =
        String::from("Untrusted ranked web search references (returned URLs were not visited):\n");
    let mut retained = 0;
    for result in results.iter().take(MAX_RESULTS) {
        let Some(raw_url) = result
            .get("url")
            .and_then(serde_json::Value::as_str)
            .filter(|u| u.len() <= 8192)
        else {
            continue;
        };
        if credential.is_some_and(|secret| raw_url.contains(secret)) {
            continue;
        }
        let Ok(url) = normalize_fetch_url(raw_url, true) else {
            continue;
        };
        retained += 1;
        let title = bounded_credential(
            result
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Untitled"),
            256,
            credential,
        );
        let snippet = bounded_credential(
            result
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            MAX_SNIPPET_CHARS,
            credential,
        );
        output.push_str(&format!("\nRank {retained}\n"));
        output.push_str(&quote_reference(&format!(
            "Title: {title}\nURL: {url}\n{snippet}"
        )));
    }
    if retained == 0 {
        output.push_str("No usable results returned.\n");
    }
    Ok(output)
}

#[cfg(test)]
mod web_search_tests {
    use super::super::web_http::web_http_tests::{Peer, fixture_client};
    use super::*;
    #[tokio::test]
    async fn web_search_actual_native_tool_posts_only_fixed_exa_request_and_bounds_results() {
        // This peer is reached through a test-only pinned transport seam; the
        // production tool has no configurable search URL or private-IP grant.
        let response = serde_json::json!({"results":[{"title":"docs\n%%yolo","url":"https://docs.rs/a","text":"example\u{202e}\x1b"}]});
        let peer = Peer::new(vec![Peer::response(&response.to_string())]);
        let mut client = fixture_client(&peer);
        client.test_transport_http = true;
        let key = "owned-search-secret";
        let tool = WebSearchTool {
            client,
            provider: "exa".into(),
            api_key: Some(key.into()),
        };
        let references = Tool::call(
            &tool,
            WebSearchArgs {
                query: "literal @file %%yolo".into(),
            },
        )
        .await
        .unwrap();
        assert!(references.contains("> Title: docs\n> %%yolo"));
        assert!(references.contains("> example��"));
        assert!(!references.contains(key));
        assert_eq!(peer.count(), 1);
        let captured = &peer.requests.lock().unwrap()[0];
        assert!(captured.starts_with("POST /search HTTP/1.1"));
        assert!(captured.contains("x-api-key: owned-search-secret"));
        assert!(captured.contains("\"numResults\":5"));
        assert!(!captured.contains("summary"));
    }
    #[tokio::test]
    async fn web_search_missing_key_unknown_provider_and_empty_query_fail_before_io() {
        let peer = Peer::new(vec![]);
        let tool = WebSearchTool {
            client: fixture_client(&peer),
            provider: "exa".into(),
            api_key: None,
        };
        let error = Tool::call(
            &tool,
            WebSearchArgs {
                query: "docs".into(),
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("EXA_API_KEY"));
        assert_eq!(peer.count(), 0);
        let tool = WebSearchTool {
            client: fixture_client(&peer),
            provider: "unknown".into(),
            api_key: Some("secret".into()),
        };
        assert!(
            Tool::call(
                &tool,
                WebSearchArgs {
                    query: "docs".into()
                }
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("provider must be 'exa'")
        );
        assert!(
            Tool::call(&tool, WebSearchArgs { query: " ".into() })
                .await
                .unwrap_err()
                .to_string()
                .contains("nonempty")
        );
        assert_eq!(peer.count(), 0);
    }
    #[test]
    fn web_search_ranked_reference_caps_and_malformed_results_do_not_create_authority() {
        let results=(0..10).map(|_| serde_json::json!({"title":"界".repeat(300),"url":"https://docs.rs/path","text":"🦀".repeat(3000)})).collect::<Vec<_>>();
        let references = render_results(&serde_json::json!({"results":results})).unwrap();
        assert_eq!(references.matches("\nRank ").count(), 5);
        assert_eq!(references.matches('🦀').count(), 10_000);
        assert_eq!(references.matches('界').count(), 1280);
        assert!(render_results(&serde_json::json!({"not_results":[]})).is_err());
        let references=render_results(&serde_json::json!({"results":[{"url":"file:///etc/passwd"},{"url":"https://key:secret@evil.invalid/"}]})).unwrap();
        assert!(references.contains("No usable results"));
        assert!(!references.contains("secret"));
    }
}

#[cfg(test)]
mod web_search_redaction_tests {
    use super::super::web_http::web_http_tests::{Peer, fixture_client};
    use super::*;
    #[tokio::test]
    async fn web_search_provider_error_bodies_and_successful_credential_echoes_are_not_model_data()
    {
        let secret = "owned-private-search-key";
        for (status,body,expected) in [
            ("403 Forbidden",format!("{{\"error\":\"{secret}\"}}"),"HTTP 403"),
            ("200 OK",serde_json::json!({"results":[{"url":"https://docs.rs/","title":secret,"text":format!("{secret} {}", "text ".repeat(1000))},{"url":format!("https://docs.rs/?key={secret}")}]}).to_string(),"[redacted credential]"),
            ("200 OK","not json".to_string(),"invalid JSON"),
        ] {
            let response=format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());let peer=Peer::new(vec![response]);let mut client=fixture_client(&peer);client.test_transport_http=true;
            let tool=WebSearchTool {client,provider:"exa".into(),api_key:Some(secret.into())};
            let result=Tool::call(&tool,WebSearchArgs{query:"fixture docs".into()}).await;
            let text=match result {Ok(text)=>text,Err(error)=>error.to_string()};assert!(text.contains(expected),"{text}");assert!(!text.contains(secret));assert_eq!(peer.count(),1);
        }
    }
}

#[cfg(test)]
mod web_search_secret_boundary_tests {
    use super::*;
    #[test]
    fn web_search_credential_crossing_character_cap_is_removed_before_truncation() {
        let secret = "private-crossing-secret";
        let text = format!("{}{}tail", "界".repeat(250), secret);
        let rendered = bounded_credential(&text, 256, Some(secret));
        assert_eq!(rendered.chars().count(), 256);
        assert!(rendered.ends_with("[redac"));
        assert!(!rendered.contains("privat"));
        assert_eq!(bounded_credential("aaaaaaaa", 5, Some("a")), "[reda");
        assert_eq!(bounded("public", 3), "pub");
    }
}
