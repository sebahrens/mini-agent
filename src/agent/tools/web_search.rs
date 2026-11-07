//! Fixed-provider native search. Credentials are captured at agent construction
//! and sent only to Exa's fixed HTTPS search endpoint; redirects are rejected.
use super::web_fetch::{quote_reference, safe_text};
use super::web_http::{FetchPolicy, FetchRequest, NativeWebClient, normalize_fetch_url_with};
use super::{AskSender, PermCheck, ToolError};
use rig::tool::Tool;
use serde::Deserialize;
const SEARCH_URL: &str = "https://api.exa.ai/search";
const MAX_QUERY_BYTES: usize = 4096;
const DEFAULT_RESULTS: usize = 5;
const MAX_RESULTS: usize = 10;
const MAX_SNIPPET_CHARS: usize = 3000;
const MAX_DOMAIN_FILTERS: usize = 20;
const MAX_DOMAIN_BYTES: usize = 253;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebSearchArgs {
    query: String,
    /// Number of ranked results, 1..=10 (default 5).
    #[serde(default)]
    num_results: Option<usize>,
    /// Only return results from these domains (and their subdomains).
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    /// Never return results from these domains (and their subdomains).
    #[serde(default)]
    blocked_domains: Option<Vec<String>>,
    /// Only results published within this window: day, week, month or year.
    #[serde(default)]
    recency: Option<String>,
}

/// Whether the native Exa search tool can run: it needs `EXA_API_KEY`. When
/// absent the tool is not registered, so the model never sees a tool that can
/// only fail; the keyless built-in Exa MCP server remains the fallback.
pub(crate) fn exa_api_key_present() -> bool {
    std::env::var("EXA_API_KEY").is_ok_and(|key| !key.trim().is_empty())
}

pub(crate) fn normalized_domains(
    field: &str,
    domains: Option<Vec<String>>,
) -> Result<Vec<String>, ToolError> {
    let domains = domains.unwrap_or_default();
    if domains.len() > MAX_DOMAIN_FILTERS {
        return Err(ToolError::Msg(format!(
            "web_search {field} accepts at most {MAX_DOMAIN_FILTERS} domains"
        )));
    }
    domains
        .into_iter()
        .map(|domain| {
            let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
            let domain = domain
                .strip_prefix("https://")
                .or_else(|| domain.strip_prefix("http://"))
                .unwrap_or(&domain)
                .trim_start_matches("*.")
                .trim_end_matches('/')
                .to_owned();
            let valid = !domain.is_empty()
                && domain.len() <= MAX_DOMAIN_BYTES
                && domain.contains('.')
                && domain
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                && !domain.starts_with(['.', '-'])
                && !domain.contains("..");
            if valid {
                Ok(domain)
            } else {
                Err(ToolError::Msg(format!(
                    "web_search {field} entries must be bare domain names such as docs.rs"
                )))
            }
        })
        .collect()
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Validated provider-independent search options.
#[derive(Debug, Default, PartialEq, Eq)]
struct SearchOptions {
    num_results: usize,
    allowed: Vec<String>,
    blocked: Vec<String>,
    /// Earliest publication date accepted, as an RFC 3339 timestamp.
    published_after: Option<String>,
}

impl SearchOptions {
    fn from_args(args: &mut WebSearchArgs) -> Result<Self, ToolError> {
        let num_results = args.num_results.unwrap_or(DEFAULT_RESULTS);
        if !(1..=MAX_RESULTS).contains(&num_results) {
            return Err(ToolError::Msg(format!(
                "web_search num_results must be between 1 and {MAX_RESULTS}"
            )));
        }
        let allowed = normalized_domains("allowed_domains", args.allowed_domains.take())?;
        let blocked = normalized_domains("blocked_domains", args.blocked_domains.take())?;
        if !allowed.is_empty() && !blocked.is_empty() {
            return Err(ToolError::Msg(
                "web_search accepts allowed_domains or blocked_domains, not both".into(),
            ));
        }
        let days = match args.recency.as_deref() {
            None => None,
            Some("day") => Some(1),
            Some("week") => Some(7),
            Some("month") => Some(31),
            Some("year") => Some(366),
            Some(_) => {
                return Err(ToolError::Msg(
                    "web_search recency must be day, week, month or year".into(),
                ));
            }
        };
        let published_after = days.map(|days| {
            (chrono::Utc::now() - chrono::Duration::days(days))
                .format("%Y-%m-%dT%H:%M:%S.000Z")
                .to_string()
        });
        Ok(Self {
            num_results,
            allowed,
            blocked,
            published_after,
        })
    }

    /// Client-side enforcement of the domain filters, independent of whether
    /// the provider honoured them.
    fn admits(&self, url: &reqwest::Url) -> bool {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        (self.allowed.is_empty() || self.allowed.iter().any(|d| host_matches(&host, d)))
            && !self.blocked.iter().any(|d| host_matches(&host, d))
    }

    fn exa_body(&self, query: &str) -> serde_json::Value {
        let mut body = serde_json::json!({"query":query,"type":"auto","numResults":self.num_results,"contents":{"text":{"maxCharacters":MAX_SNIPPET_CHARS}}});
        if !self.allowed.is_empty() {
            body["includeDomains"] = serde_json::json!(self.allowed);
        }
        if !self.blocked.is_empty() {
            body["excludeDomains"] = serde_json::json!(self.blocked);
        }
        if let Some(after) = &self.published_after {
            body["startPublishedDate"] = serde_json::json!(after);
        }
        body
    }
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
        "Search the public web through Exa, using EXA_API_KEY and web_search host permission. Returns num_results (default 5, at most 10) ranked titles, URLs and snippets of up to 3,000 characters as untrusted reference data. Optional allowed_domains or blocked_domains (bare domains, subdomains included, at most 20) and recency (day, week, month, year) narrow the results. No returned URL is visited. Provider errors expose status only.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "query":{"type":"string"},
            "num_results":{"type":"integer","minimum":1,"maximum":MAX_RESULTS,"description":"Ranked results to return (default 5)"},
            "allowed_domains":{"type":"array","items":{"type":"string"},"maxItems":MAX_DOMAIN_FILTERS,"description":"Only results from these domains and their subdomains"},
            "blocked_domains":{"type":"array","items":{"type":"string"},"maxItems":MAX_DOMAIN_FILTERS,"description":"Exclude results from these domains and their subdomains"},
            "recency":{"type":"string","enum":["day","week","month","year"],"description":"Only results published within this window"}},
            "required":["query"],"additionalProperties":false})
    }
    async fn call(&self, mut args: WebSearchArgs) -> Result<String, ToolError> {
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
        let options = SearchOptions::from_args(&mut args)?;
        let body = options.exa_body(&args.query).to_string();
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
        render_results_with_credential(&parsed, Some(key), &options)
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
    render_results_with_credential(
        response,
        None,
        &SearchOptions {
            num_results: DEFAULT_RESULTS,
            ..SearchOptions::default()
        },
    )
}

fn render_results_with_credential(
    response: &serde_json::Value,
    credential: Option<&str>,
    options: &SearchOptions,
) -> Result<String, ToolError> {
    let results = response
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ToolError::Msg("web_search provider response has no results list".into()))?;
    let mut output =
        String::from("Untrusted ranked web search references (returned URLs were not visited):\n");
    let mut retained = 0;
    let mut filtered = 0;
    let mut unusable = 0;
    for result in results.iter().take(MAX_RESULTS) {
        if retained == options.num_results {
            break;
        }
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
        // Fragments are never sent to servers; strip them as web_fetch does
        // rather than dropping an otherwise valid page.
        let Ok(url) = normalize_fetch_url_with(raw_url, true, true) else {
            unusable += 1;
            continue;
        };
        if !options.admits(&url) {
            filtered += 1;
            continue;
        }
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
    if filtered > 0 {
        output.push_str(&format!(
            "{filtered} result(s) outside the requested domain filter were dropped.\n"
        ));
    }
    if unusable > 0 {
        output.push_str(&format!(
            "{unusable} result(s) with an unusable URL (not a plain public http(s) URL) were dropped.\n"
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod web_search_tests {
    use super::super::web_http::web_http_tests::{Peer, fixture_client};
    use super::*;
    #[test]
    fn web_search_lists_fragment_urls_without_the_fragment_and_counts_unusable() {
        let out = render_results(&serde_json::json!({"results":[
            {"title":"t","url":"https://docs.rs/x#section","text":"s"},
            {"title":"u","url":"ftp://example.com/a","text":"s"}]}))
        .unwrap();
        assert!(out.contains("URL: https://docs.rs/x\n"), "{out}");
        assert!(!out.contains("#section"), "{out}");
        assert!(out.contains("1 result(s) with an unusable URL"), "{out}");
    }
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
                ..Default::default()
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
                ..Default::default()
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
                    query: "docs".into(),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("provider must be 'exa'")
        );
        assert!(
            Tool::call(
                &tool,
                WebSearchArgs {
                    query: " ".into(),
                    ..Default::default()
                }
            )
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
        assert_eq!(references.matches('🦀').count(), 15_000);
        assert_eq!(references.matches('界').count(), 1280);
        assert!(render_results(&serde_json::json!({"not_results":[]})).is_err());
        let references=render_results(&serde_json::json!({"results":[{"url":"file:///etc/passwd"},{"url":"https://key:secret@evil.invalid/"}]})).unwrap();
        assert!(references.contains("No usable results"));
        assert!(!references.contains("secret"));
    }
}

#[cfg(test)]
mod web_search_filter_tests {
    use super::super::web_http::web_http_tests::{Peer, fixture_client};
    use super::*;
    #[tokio::test]
    async fn web_search_domain_count_and_recency_options_reach_exa_and_are_enforced_locally() {
        let response = serde_json::json!({"results":[
            {"title":"a","url":"https://api.docs.rs/a","text":"kept subdomain"},
            {"title":"b","url":"https://evil.example/b","text":"provider ignored filter"},
            {"title":"c","url":"https://docs.rs/c","text":"kept exact"},
            {"title":"d","url":"https://docs.rs/d","text":"over count"}]});
        let peer = Peer::new(vec![Peer::response(&response.to_string())]);
        let mut client = fixture_client(&peer);
        client.test_transport_http = true;
        let tool = WebSearchTool {
            client,
            provider: "exa".into(),
            api_key: Some("filter-key".into()),
        };
        let parameters = tool.parameters().to_string();
        for name in [
            "num_results",
            "allowed_domains",
            "blocked_domains",
            "recency",
        ] {
            assert!(parameters.contains(name), "{name}");
        }
        let references = Tool::call(
            &tool,
            WebSearchArgs {
                query: "rust docs".into(),
                num_results: Some(2),
                allowed_domains: Some(vec!["https://Docs.RS/".into()]),
                recency: Some("week".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(references.contains("kept subdomain"));
        assert!(references.contains("kept exact"));
        assert!(!references.contains("provider ignored filter"));
        assert!(!references.contains("over count"));
        assert!(references.contains("1 result(s) outside the requested domain filter"));
        let captured = peer.requests.lock().unwrap()[0].clone();
        assert!(captured.contains("\"numResults\":2"), "{captured}");
        assert!(
            captured.contains("\"includeDomains\":[\"docs.rs\"]"),
            "{captured}"
        );
        assert!(captured.contains("\"startPublishedDate\":\""), "{captured}");

        for args in [
            WebSearchArgs {
                query: "q".into(),
                num_results: Some(11),
                ..Default::default()
            },
            WebSearchArgs {
                query: "q".into(),
                allowed_domains: Some(vec!["a.example".into()]),
                blocked_domains: Some(vec!["b.example".into()]),
                ..Default::default()
            },
            WebSearchArgs {
                query: "q".into(),
                blocked_domains: Some(vec!["not a domain/../x".into()]),
                ..Default::default()
            },
            WebSearchArgs {
                query: "q".into(),
                recency: Some("decade".into()),
                ..Default::default()
            },
        ] {
            assert!(Tool::call(&tool, args).await.is_err());
        }
        assert_eq!(peer.count(), 1);
    }

    #[test]
    fn web_search_blocked_domains_drop_matching_hosts_only() {
        let options = SearchOptions {
            num_results: 5,
            blocked: vec!["pinterest.com".into()],
            ..SearchOptions::default()
        };
        let url = |u: &str| reqwest::Url::parse(u).unwrap();
        assert!(!options.admits(&url("https://www.pinterest.com/x")));
        assert!(options.admits(&url("https://notpinterest.com/x")));
        assert!(options.exa_body("q")["excludeDomains"][0] == "pinterest.com");
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
            let result=Tool::call(&tool,WebSearchArgs{query:"fixture docs".into(),..Default::default()}).await;
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
