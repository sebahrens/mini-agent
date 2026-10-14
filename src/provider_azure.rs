//! Azure OpenAI v1 reuses the SDK's Responses/chat converters and bounded HTTP.
//! Tenant endpoints and typed credentials are resolved before a client exists.
use super::{AnyClient, OpenAiClient};
use crate::auth::ResolvedCredential;
use crate::config::{ApiStyle, CustomProviderConfig};
use crate::provider_http::ProviderHttpClient;
use reqwest::header::{HeaderName, HeaderValue};
use rig::client::{ApiKey, ClientBuilder, ProviderBuilder};
use rig::providers::openai;

pub(crate) fn normalize_endpoint(endpoint: Option<&str>) -> anyhow::Result<String> {
    let endpoint = endpoint.ok_or_else(|| {
        anyhow::anyhow!(
            "Azure OpenAI requires AZURE_OPENAI_ENDPOINT or a custom_providers base_url"
        )
    })?;
    let mut url = reqwest::Url::parse(endpoint.trim())
        .map_err(|_| anyhow::anyhow!("invalid Azure OpenAI endpoint URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Azure endpoint must be an HTTP(S) resource URL without credentials, query, or fragment"
    );
    let path = url.path().trim_end_matches('/');
    if path.is_empty() {
        url.set_path("/openai/v1");
    } else {
        anyhow::ensure!(
            path.ends_with("/openai/v1"),
            "Azure endpoint must be a resource root or end in /openai/v1; legacy deployment paths are not the v1 API"
        );
        let path = path.to_owned();
        url.set_path(&path);
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

pub(crate) fn api_style(custom: Option<&CustomProviderConfig>) -> ApiStyle {
    custom
        .and_then(|custom| custom.api_style)
        .unwrap_or(ApiStyle::Responses)
}

struct AzureAuth(ResolvedCredential);
impl ApiKey for AzureAuth {
    fn into_header(self) -> Option<rig::http_client::Result<(HeaderName, HeaderValue)>> {
        Some(match self.0 {
            ResolvedCredential::ApiKey(key) => HeaderValue::from_str(&key)
                .map(|mut value| {
                    value.set_sensitive(true);
                    (HeaderName::from_static("api-key"), value)
                })
                .map_err(Into::into),
            ResolvedCredential::Bearer(token) => {
                rig::http_client::make_auth_header(token).map(|(name, mut value)| {
                    value.set_sensitive(true);
                    (name, value)
                })
            }
        })
    }
}

// The stock Azure SDK adapter targets the legacy deployment/chat surface.
// Azure's v1 surface uses these exact existing OpenAI extensions instead.
macro_rules! azure_builder {
    ($builder:ident, $extension:ty) => {
        #[derive(Default, Clone)]
        struct $builder;
        impl ProviderBuilder for $builder {
            type Extension<H>
                = $extension
            where
                H: rig::http_client::HttpClientExt;
            type ApiKey = AzureAuth;
            const BASE_URL: &'static str = "";
            fn build<H>(
                _: &ClientBuilder<Self, Self::ApiKey, H>,
            ) -> rig::http_client::Result<Self::Extension<H>>
            where
                H: rig::http_client::HttpClientExt,
            {
                Ok(<$extension>::default())
            }
        }
    };
}
azure_builder!(AzureResponsesBuilder, openai::client::OpenAIResponsesExt);
azure_builder!(
    AzureCompletionsBuilder,
    openai::client::OpenAICompletionsExt
);

pub(crate) fn build_client(
    credential: ResolvedCredential,
    endpoint: &str,
    custom: Option<&CustomProviderConfig>,
    http: reqwest::Client,
) -> anyhow::Result<AnyClient> {
    let client = match api_style(custom) {
        ApiStyle::Responses => OpenAiClient::Responses(
            ClientBuilder::<AzureResponsesBuilder>::default()
                .api_key(AzureAuth(credential))
                .base_url(endpoint)
                .http_client(ProviderHttpClient::new(http))
                .build()?,
        ),
        ApiStyle::Completions => OpenAiClient::Completions(
            ClientBuilder::<AzureCompletionsBuilder>::default()
                .api_key(AzureAuth(credential))
                .base_url(endpoint)
                .http_client(ProviderHttpClient::new(http))
                .build()?,
        ),
    };
    Ok(AnyClient::OpenAI(client))
}

#[cfg(test)]
#[path = "provider_azure_tests.rs"]
mod tests;
