//! Vertex Claude rawPredict uses the typed Anthropic composer and official ADC.
//! Token refresh belongs to this client; turn cancellation cancels only its
//! waiter. The pinned SDK bounds refresh work and cancels it on last-client drop.
use crate::auth::ResolvedCredential;
use crate::provider::AnyClient;
use crate::provider_http::ProviderHttpClient;
use bytes::Bytes;
use google_cloud_auth::credentials::{Builder, CacheableResource, Credentials};
use rig::client::{ApiKey, ClientBuilder, ProviderBuilder};
use rig::http_client;
use rig::providers::anthropic::client::{AnthropicExt, VertexRoute};
use std::sync::Arc;
use std::time::Duration;

const _: () = assert!(google_cloud_auth::MINI_AGENT_CLIENT_OWNED_ADC);
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn route<F>(env: &F) -> anyhow::Result<VertexRoute>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    let project = env("GOOGLE_CLOUD_PROJECT")
        .map_err(|_| anyhow::anyhow!("Vertex requires GOOGLE_CLOUD_PROJECT"))?;
    let location = env("GOOGLE_CLOUD_LOCATION")
        .map_err(|_| anyhow::anyhow!("Vertex requires GOOGLE_CLOUD_LOCATION"))?;
    Ok(VertexRoute::new(&project, &location)?)
}

pub(crate) fn endpoint(value: Option<&str>, route: &VertexRoute) -> anyhow::Result<String> {
    let default = if route.location() == "global" {
        "https://aiplatform.googleapis.com".into()
    } else {
        format!("https://{}-aiplatform.googleapis.com", route.location())
    };
    let url = reqwest::Url::parse(value.unwrap_or(&default).trim())
        .map_err(|_| anyhow::anyhow!("invalid Vertex endpoint"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "Vertex endpoint must be an HTTP(S) origin without path, credentials, query, or fragment"
    );
    Ok(url.to_string().trim_end_matches('/').into())
}
fn auth_error(message: &'static str) -> http_client::Error {
    http_client::Error::Instance(Box::new(std::io::Error::other(message)))
}

/// Auth is bound to one origin and exact publisher/project/location path. No
/// token is acquired before that destination has passed validation.
pub(crate) struct VertexAuth {
    endpoint: reqwest::Url,
    route: VertexRoute,
    bearer: Option<http::HeaderValue>,
    adc: tokio::sync::OnceCell<Credentials>,
}
impl std::fmt::Debug for VertexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VertexAuth([REDACTED])")
    }
}
impl VertexAuth {
    fn new(
        credential: ResolvedCredential,
        endpoint: &str,
        route: VertexRoute,
    ) -> anyhow::Result<Self> {
        let bearer = match credential {
            ResolvedCredential::Bearer(value) => {
                let mut value = http::HeaderValue::from_str(&format!("Bearer {value}"))?;
                value.set_sensitive(true);
                Some(value)
            }
            ResolvedCredential::GoogleAdc => None,
            _ => anyhow::bail!("Vertex requires a declared access token or Google ADC credentials"),
        };
        Ok(Self {
            endpoint: reqwest::Url::parse(endpoint)?,
            route,
            bearer,
            adc: tokio::sync::OnceCell::new(),
        })
    }
    pub(crate) async fn prepare(
        &self,
        request: &mut http::Request<Bytes>,
    ) -> http_client::Result<()> {
        let url = reqwest::Url::parse(&request.uri().to_string())
            .map_err(|_| auth_error("invalid Vertex request URL"))?;
        let path = url.path();
        let (model, streaming) = if let Some(model) = VertexRoute::streaming_model(path) {
            (model, true)
        } else {
            (
                path.rsplit('/')
                    .next()
                    .and_then(|p| p.strip_suffix(":rawPredict"))
                    .ok_or_else(|| auth_error("unsupported Vertex request path"))?,
                false,
            )
        };
        if request.method() != http::Method::POST
            || url.origin() != self.endpoint.origin()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || self.route.path(model, streaming)? != path
        {
            return Err(auth_error("Vertex request changed credential destination"));
        }
        for name in [
            "authorization",
            "api-key",
            "x-api-key",
            "x-goog-user-project",
            "anthropic-version",
            "anthropic-beta",
        ] {
            request.headers_mut().remove(name);
        }
        if let Some(bearer) = &self.bearer {
            request
                .headers_mut()
                .insert(http::header::AUTHORIZATION, bearer.clone());
            return Ok(());
        }
        let credentials = self.adc.get_or_try_init(|| async {
            // Install only if no application provider exists. ADC service-account
            // JWT signing needs a provider even though model HTTP uses native TLS.
            let _ = rustls::crypto::ring::default_provider().install_default();
            Builder::default().with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
                .build().map_err(|_|auth_error("Google ADC configuration failed; configure Application Default Credentials or VERTEX_ACCESS_TOKEN"))
        }).await?;
        let headers =
            tokio::time::timeout(AUTH_TIMEOUT, credentials.headers(http::Extensions::new()))
                .await
                .map_err(|_| auth_error("Google ADC credential wait timed out"))?
                .map_err(|_| auth_error("Google ADC credential acquisition failed"))?;
        let CacheableResource::New { data, .. } = headers else {
            return Err(auth_error("Google ADC returned no credential headers"));
        };
        // Only documented ADC output headers may reach the selected model route.
        for name in ["authorization", "x-goog-user-project"] {
            if let Some(value) = data.get(name) {
                let mut value = value.clone();
                value.set_sensitive(name == "authorization");
                request
                    .headers_mut()
                    .insert(http::HeaderName::from_static(name), value);
            }
        }
        if !request.headers().contains_key(http::header::AUTHORIZATION) {
            return Err(auth_error("Google ADC returned no bearer credential"));
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
struct VertexBuilder;
struct NoAuth;
impl ApiKey for NoAuth {
    fn into_header(self) -> Option<http_client::Result<(http::HeaderName, http::HeaderValue)>> {
        None
    }
}
impl ProviderBuilder for VertexBuilder {
    type Extension<H>
        = AnthropicExt
    where
        H: http_client::HttpClientExt;
    type ApiKey = NoAuth;
    const BASE_URL: &'static str = "";
    fn build<H>(_: &ClientBuilder<Self, Self::ApiKey, H>) -> http_client::Result<Self::Extension<H>>
    where
        H: http_client::HttpClientExt,
    {
        Ok(AnthropicExt::default())
    }
}
pub(crate) fn build_client(
    credential: ResolvedCredential,
    endpoint: &str,
    route: VertexRoute,
    http: reqwest::Client,
) -> anyhow::Result<AnyClient> {
    let auth = Arc::new(VertexAuth::new(credential, endpoint, route.clone())?);
    let client = ClientBuilder::<VertexBuilder>::default()
        .api_key(NoAuth)
        .base_url(endpoint)
        .http_client(ProviderHttpClient::with_vertex(http, auth))
        .build()?
        .with_ext(AnthropicExt::vertex(route));
    Ok(AnyClient::Anthropic(client))
}

#[cfg(test)]
#[path = "provider_vertex_adc_tests.rs"]
mod adc_tests;
#[cfg(test)]
#[path = "provider_vertex_tests.rs"]
mod tests;
