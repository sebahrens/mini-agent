//! Native Bedrock Messages keeps Anthropic bodies and SSE unchanged. Only the
//! selected credential source changes request authentication at the HTTP owner.
use crate::auth::ResolvedCredential;
use crate::provider::AnyClient;
use crate::provider_http::ProviderHttpClient;
use aws_credential_types::provider::ProvideCredentials;
use aws_smithy_runtime_api::client::{
    http::{HttpConnector, HttpConnectorFuture, SharedHttpConnector, http_client_fn},
    orchestrator::HttpRequest,
    result::ConnectorError,
};
use aws_smithy_types::body::SdkBody;
use bytes::Bytes;
use rig::client::{ApiKey, ClientBuilder, ProviderBuilder};
use rig::http_client;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const AUTH_BODY_LIMIT: usize = 1024 * 1024;

pub(crate) fn region<F>(env: &F) -> anyhow::Result<String>
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    let region = env("AWS_REGION")
        .or_else(|_| env("AWS_DEFAULT_REGION"))
        .map_err(|_| anyhow::anyhow!("Bedrock requires AWS_REGION or AWS_DEFAULT_REGION"))?;
    anyhow::ensure!(
        !region.is_empty()
            && region.len() <= 64
            && region
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
        "invalid Bedrock region"
    );
    Ok(region)
}

pub(crate) fn endpoint(endpoint: Option<&str>, region: &str) -> anyhow::Result<String> {
    let default = format!("https://bedrock-runtime.{region}.amazonaws.com/anthropic");
    let mut url = reqwest::Url::parse(endpoint.unwrap_or(&default).trim())
        .map_err(|_| anyhow::anyhow!("invalid Bedrock endpoint URL"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Bedrock endpoint must be HTTP(S) without credentials, query, or fragment"
    );
    let path = url.path().trim_end_matches('/').to_owned();
    if path.is_empty() {
        url.set_path("/anthropic");
    } else {
        anyhow::ensure!(
            path.ends_with("/anthropic"),
            "Bedrock native Messages endpoint must end in /anthropic"
        );
        url.set_path(&path);
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn auth_error(message: &'static str) -> http_client::Error {
    http_client::Error::Instance(Box::new(std::io::Error::other(message)))
}

/// Request-scoped auth never mutates model input, tools, system, or stream data.
/// An isolated route cannot construct the default-chain variant without the
/// existing explicit `inherit_builtin_key` grant in the authoritative resolver.
pub(crate) struct BedrockAuth {
    endpoint: reqwest::Url,
    region: String,
    key: Option<http::HeaderValue>,
    sdk: tokio::sync::OnceCell<aws_config::SdkConfig>,
    credentials_http: reqwest::Client,
}
impl std::fmt::Debug for BedrockAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BedrockAuth([REDACTED])")
    }
}
impl BedrockAuth {
    fn new(credential: ResolvedCredential, endpoint: &str, region: &str) -> anyhow::Result<Self> {
        let key = match credential {
            ResolvedCredential::ApiKey(key) => {
                let mut key = http::HeaderValue::from_str(&key)?;
                key.set_sensitive(true);
                Some(key)
            }
            ResolvedCredential::AwsDefaultChain => None,
            ResolvedCredential::Bearer(_)
            | ResolvedCredential::GoogleAdc
            | ResolvedCredential::CopilotLogin => {
                anyhow::bail!("Bedrock native Messages uses an API key or AWS SDK credentials")
            }
        };
        Ok(Self {
            endpoint: reqwest::Url::parse(endpoint)?,
            region: region.into(),
            key,
            sdk: tokio::sync::OnceCell::new(),
            credentials_http: crate::provider::build_http_client(
                "bedrock credentials",
                false,
                None,
                None,
            )?,
        })
    }

    pub(crate) async fn prepare(
        &self,
        request: &mut http::Request<Bytes>,
    ) -> http_client::Result<()> {
        let url = reqwest::Url::parse(&request.uri().to_string())
            .map_err(|_| auth_error("invalid Bedrock request URL"))?;
        if url.origin() != self.endpoint.origin()
            || !url
                .path()
                .starts_with(&format!("{}/", self.endpoint.path().trim_end_matches('/')))
        {
            return Err(auth_error("Bedrock request changed credential destination"));
        }
        for name in [
            "authorization",
            "api-key",
            "x-api-key",
            "x-amz-date",
            "x-amz-security-token",
            "x-amz-content-sha256",
            "host",
        ] {
            request.headers_mut().remove(name);
        }
        if let Some(key) = &self.key {
            request.headers_mut().insert("x-api-key", key.clone());
            return Ok(());
        }
        let credentials = tokio::time::timeout(AUTH_TIMEOUT, async {
            reject_process_profiles().await?;
            let sdk = self.sdk.get_or_init(|| async {
                let connector = CredentialsHttp(self.credentials_http.clone(), AUTH_TIMEOUT);
                aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .region(aws_config::Region::new(self.region.clone()))
                    .http_client(http_client_fn(move |settings, _| {
                        let bound = settings.connect_timeout().unwrap_or(AUTH_TIMEOUT).min(settings.read_timeout().unwrap_or(AUTH_TIMEOUT)).min(AUTH_TIMEOUT);
                        SharedHttpConnector::new(CredentialsHttp(connector.0.clone(), bound))
                    }))
                    .load().await
            }).await;
            let provider = sdk.credentials_provider().ok_or_else(|| auth_error("AWS credential provider unavailable"))?;
            provider.provide_credentials().await.map_err(|_| auth_error("AWS credential chain failed; configure AWS credentials or supply a Bedrock API key"))
        }).await.map_err(|_| auth_error("AWS credential acquisition timed out"))??;
        if credentials
            .expiry()
            .is_some_and(|expiry| expiry <= SystemTime::now())
        {
            return Err(auth_error(
                "AWS credential provider returned expired credentials",
            ));
        }
        sign_request(request, credentials, &self.region, SystemTime::now())
    }
}

async fn reject_process_profiles() -> http_client::Result<()> {
    let profiles = aws_config::profile::load(
        &aws_types::os_shim_internal::Fs::real(),
        &aws_types::os_shim_internal::Env::real(),
        &aws_runtime::env_config::file::EnvConfigFiles::default(),
        None,
    )
    .await
    .map_err(|_| auth_error("AWS profile configuration is invalid"))?;
    let mut name = profiles.selected_profile();
    let mut seen = std::collections::HashSet::new();
    while let Some(profile) = profiles.get_profile(name) {
        if !seen.insert(name) || seen.len() > 64 {
            return Err(auth_error("AWS source_profile chain is cyclic or too deep"));
        }
        if profile.get("credential_process").is_some() {
            return Err(auth_error(
                "AWS credential_process profiles are unsupported; use AWS environment/shared-key/SSO/role/web-identity/container/instance credentials or a Bedrock API key",
            ));
        }
        let Some(source) = profile.get("source_profile") else {
            break;
        };
        name = source;
    }
    Ok(())
}

fn sign_request(
    request: &mut http::Request<Bytes>,
    credentials: aws_credential_types::Credentials,
    region: &str,
    now: SystemTime,
) -> http_client::Result<()> {
    use aws_sigv4::http_request::{
        PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign,
    };
    let identity = credentials.into();
    let mut settings = SigningSettings::default();
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let params = aws_sigv4::sign::v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("bedrock")
        .time(now)
        .settings(settings)
        .build()
        .map_err(|_| auth_error("AWS signing parameters invalid"))?
        .into();
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| value.to_str().map(|value| (name.as_str(), value)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| auth_error("Bedrock signing requires text headers"))?;
    let uri = request.uri().to_string();
    let signable = SignableRequest::new(
        request.method().as_str(),
        &uri,
        headers.into_iter(),
        SignableBody::Bytes(request.body()),
    )
    .map_err(|_| auth_error("Bedrock request cannot be signed"))?;
    let (instructions, _) = sign(signable, &params)
        .map_err(|_| auth_error("Bedrock request signing failed"))?
        .into_parts();
    instructions.apply_to_request_http1x(request);
    for name in ["authorization", "x-amz-security-token"] {
        if let Some(value) = request.headers_mut().get_mut(name) {
            value.set_sensitive(true);
        }
    }
    Ok(())
}

// Credential requests use the same configured reqwest machinery with strict
// TLS, no model/gateway default headers, bounded bodies, and SDK deadlines.
#[derive(Clone)]
struct CredentialsHttp(reqwest::Client, Duration);
impl std::fmt::Debug for CredentialsHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialsHttp([REDACTED])")
    }
}
impl HttpConnector for CredentialsHttp {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let client = self.0.clone();
        let bound = self.1;
        HttpConnectorFuture::new(async move {
            let request = request.try_into_http1x().map_err(|_| {
                ConnectorError::user(Box::new(std::io::Error::other(
                    "invalid AWS credential request",
                )))
            })?;
            let (parts, body) = request.into_parts();
            let body = body
                .bytes()
                .ok_or_else(|| {
                    ConnectorError::user(Box::new(std::io::Error::other(
                        "streaming credential request unsupported",
                    )))
                })?
                .to_vec();
            let result = tokio::time::timeout(bound, async {
                let mut response = client
                    .request(parts.method, parts.uri.to_string())
                    .headers(parts.headers)
                    .body(body)
                    .send()
                    .await
                    .map_err(|_| ())?;
                if response
                    .content_length()
                    .is_some_and(|n| n > AUTH_BODY_LIMIT as u64)
                {
                    return Err(());
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
                    if chunk.len() > AUTH_BODY_LIMIT.saturating_sub(bytes.len()) {
                        return Err(());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let mut result = http::Response::builder()
                    .status(response.status())
                    .version(response.version());
                *result.headers_mut().ok_or(())? = response.headers().clone();
                result
                    .body(SdkBody::from(bytes))
                    .map_err(|_| ())?
                    .try_into()
                    .map_err(|_| ())
            })
            .await;
            match result {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(())) => Err(ConnectorError::other(
                    Box::new(std::io::Error::other(
                        "AWS credential HTTP failed or exceeded its byte limit",
                    )),
                    None,
                )),
                Err(_) => Err(ConnectorError::timeout(Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "AWS credential HTTP timed out",
                )))),
            }
        })
    }
}

#[derive(Clone, Default)]
struct BedrockBuilder;
struct NoAuth;
impl ApiKey for NoAuth {
    fn into_header(self) -> Option<http_client::Result<(http::HeaderName, http::HeaderValue)>> {
        None
    }
}
impl ProviderBuilder for BedrockBuilder {
    type Extension<H>
        = rig::providers::anthropic::client::AnthropicExt
    where
        H: http_client::HttpClientExt;
    type ApiKey = NoAuth;
    const BASE_URL: &'static str = "";
    fn build<H>(_: &ClientBuilder<Self, Self::ApiKey, H>) -> http_client::Result<Self::Extension<H>>
    where
        H: http_client::HttpClientExt,
    {
        Ok(rig::providers::anthropic::client::AnthropicExt::default())
    }
}

pub(crate) fn build_client(
    credential: ResolvedCredential,
    endpoint: &str,
    region: &str,
    http: reqwest::Client,
) -> anyhow::Result<AnyClient> {
    let auth = Arc::new(BedrockAuth::new(credential, endpoint, region)?);
    let builder = ClientBuilder::<BedrockBuilder>::default()
        .api_key(NoAuth)
        .base_url(endpoint)
        .http_client(ProviderHttpClient::with_bedrock(http, auth));
    let headers = http::HeaderMap::from_iter([(
        http::HeaderName::from_static("anthropic-version"),
        http::HeaderValue::from_static(
            rig::providers::anthropic::completion::ANTHROPIC_VERSION_LATEST,
        ),
    )]);
    Ok(AnyClient::Anthropic(builder.http_headers(headers).build()?))
}

#[cfg(test)]
#[path = "provider_bedrock_tests.rs"]
mod tests;
