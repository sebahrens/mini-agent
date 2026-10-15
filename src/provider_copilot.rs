//! Copilot reuses Rig's chat/Responses converters. Host-owned external auth
//! bypasses the SDK's editor registration, ambient cache and interactive login.
use crate::auth::{
    ResolvedCredential,
    copilot::{Profile, Registration},
};
use crate::provider::AnyClient;
use crate::provider_http::ProviderHttpClient;
use bytes::Bytes;
use rig::client::ClientBuilder;
use rig::providers::copilot as sdk;
use std::{sync::Arc, time::Duration};

fn error(message: &'static str) -> rig::http_client::Error {
    rig::http_client::Error::Instance(Box::new(std::io::Error::other(message)))
}
fn origin(value: &str) -> anyhow::Result<String> {
    let url =
        reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid Copilot endpoint"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "https" | "http")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "Copilot endpoint must be an HTTP(S) origin without credentials, path, query or fragment"
    );
    Ok(url.to_string().trim_end_matches('/').into())
}
enum Source {
    Direct(String),
    GitHub {
        access: String,
        profile: Profile,
        token: tokio::sync::Mutex<crate::auth::copilot::ExternalTokenCache>,
    },
    Login(Registration),
}
pub(crate) struct CopilotAuth {
    source: Source,
    fixed: Option<String>,
    profile: Profile,
    logical: String,
}
impl std::fmt::Debug for CopilotAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CopilotAuth([REDACTED])")
    }
}
impl CopilotAuth {
    fn new(
        credential: ResolvedCredential,
        fixed: Option<&str>,
        profile: Profile,
    ) -> anyhow::Result<Self> {
        let logical = fixed
            .map(origin)
            .transpose()?
            .unwrap_or_else(|| "https://api.githubcopilot.com".into());
        let fixed = fixed.map(origin).transpose()?;
        let source = match credential {
            ResolvedCredential::ApiKey(token) => Source::Direct(token),
            ResolvedCredential::Bearer(access) => Source::GitHub {
                access,
                profile: profile.clone(),
                token: tokio::sync::Mutex::new(crate::auth::copilot::ExternalTokenCache::default()),
            },
            ResolvedCredential::CopilotLogin => Source::Login(
                crate::auth::copilot::selected_for_profile(&profile)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Copilot is signed out; run /login copilot or configure a Copilot API token"
                    )
                })?,
            ),
            _ => anyhow::bail!("Copilot cannot use cloud credential chains"),
        };
        let fixed = if matches!(&source, Source::Direct(_)) {
            Some(fixed.unwrap_or(profile.model_origin(None)?))
        } else {
            fixed
        };
        Ok(Self {
            source,
            fixed,
            profile,
            logical,
        })
    }
    pub(crate) fn is_login(&self) -> bool {
        matches!(self.source, Source::Login(_))
    }
    pub(crate) async fn context(&self) -> anyhow::Result<sdk::AuthContext> {
        let mut context = match &self.source {
            Source::Direct(token) => sdk::AuthContext {
                api_key: token.clone(),
                api_base: self.fixed.clone(),
            },
            Source::Login(registration) => registration.auth_context().await?,
            Source::GitHub {
                access,
                profile,
                token,
            } => {
                // The bootstrap token is explicitly supplied, not selected from
                // GitHub CLI/Actions ambient state. Exchange runs in this call.
                let mut token = token.lock().await;
                // The opaque GitHub-token source uses the host's expiry-aware
                // exchange cache; it never invokes the SDK's cache files.
                crate::auth::copilot::external_context(profile, access, &mut token).await?
            }
        };
        if let Some(fixed) = &self.fixed {
            context.api_base = Some(fixed.clone());
        }
        Ok(context)
    }
    pub(crate) async fn prepare(
        &self,
        request: &mut http::Request<Bytes>,
    ) -> rig::http_client::Result<()> {
        let parsed = reqwest::Url::parse(&request.uri().to_string())
            .map_err(|_| error("Invalid Copilot request destination"))?;
        let listing = request.method() == http::Method::GET
            && parsed.path() == "/models"
            && parsed.query().is_none()
            && parsed.origin().ascii_serialization() == self.logical
            && parsed.username().is_empty()
            && parsed.password().is_none();
        if request.method() == http::Method::GET && !listing {
            return Err(error("Copilot listing changed its captured route"));
        }
        if listing {
            let context = self
                .context()
                .await
                .map_err(|_| error("Copilot model listing authentication failed"))?;
            let endpoint = context
                .api_base
                .as_deref()
                .ok_or_else(|| error("Copilot listing destination unavailable"))?;
            request.headers_mut().insert(
                "x-github-api-version",
                http::HeaderValue::from_static("2025-04-01"),
            );
            request.headers_mut().insert(
                "x-request-id",
                http::HeaderValue::from_str(&uuid::Uuid::new_v4().to_string())
                    .map_err(|_| error("Invalid Copilot request identity"))?,
            );
            *request.uri_mut() = format!("{endpoint}/models")
                .parse()
                .map_err(|_| error("Invalid Copilot listing destination"))?;
            let mut value = http::HeaderValue::from_str(&format!("Bearer {}", context.api_key))
                .map_err(|_| error("Invalid Copilot authorization header"))?;
            value.set_sensitive(true);
            request
                .headers_mut()
                .insert(http::header::AUTHORIZATION, value);
        }
        let url = reqwest::Url::parse(&request.uri().to_string())
            .map_err(|_| error("Invalid Copilot request destination"))?;
        let allowed = if let Some(fixed) = &self.fixed {
            origin(&url.origin().ascii_serialization()).is_ok_and(|actual| &actual == fixed)
        } else {
            self.profile
                .model_origin(Some(&url.origin().ascii_serialization()))
                .is_ok()
        };
        if !allowed
            || url.query().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || !(listing
                || (request.method() == http::Method::POST
                    && matches!(
                        url.path(),
                        "/chat/completions" | "/responses" | "/embeddings"
                    )))
        {
            return Err(error("Copilot request changed credential destination"));
        }
        let auth = request
            .headers_mut()
            .get_mut(http::header::AUTHORIZATION)
            .ok_or_else(|| error("Copilot request is missing its actual credential"))?;
        auth.set_sensitive(true);
        for name in [
            "api-key",
            "x-api-key",
            "x-goog-user-project",
            "x-vscode-user-agent-library-version",
            "copilot-integration-id",
            "editor-version",
            "editor-plugin-version",
            "user-agent",
        ] {
            request.headers_mut().remove(name);
        }
        for (name, value) in [
            ("copilot-integration-id", "mini-agent"),
            (
                "editor-version",
                concat!("mini-agent/", env!("CARGO_PKG_VERSION")),
            ),
            (
                "editor-plugin-version",
                concat!("mini-agent/", env!("CARGO_PKG_VERSION")),
            ),
            (
                "user-agent",
                concat!("mini-agent/", env!("CARGO_PKG_VERSION")),
            ),
        ] {
            request.headers_mut().insert(
                http::HeaderName::from_static(name),
                http::HeaderValue::from_static(value),
            );
        }
        Ok(())
    }
}
pub(crate) fn build_client(
    credential: ResolvedCredential,
    endpoint: Option<&str>,
    profile: Profile,
    http: reqwest::Client,
) -> anyhow::Result<AnyClient> {
    let auth = Arc::new(CopilotAuth::new(credential, endpoint, profile)?);
    let provider = auth.clone();
    let callback = sdk::ExternalAuthenticator::new(move || {
        let auth = provider.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(30),auth.context()).await
                .map_err(|_|sdk::AuthError::Message("Copilot credential acquisition timed out".into()))?
                .map_err(|_|sdk::AuthError::Message("Copilot authentication failed; check the configured token or run /login copilot".into()))
        }
    });
    let mut builder = ClientBuilder::<sdk::CopilotBuilder>::default()
        .api_key(sdk::CopilotAuth::External(callback))
        .allow_device_flow(false)
        .http_client(ProviderHttpClient::with_copilot(http, auth));
    if let Some(endpoint) = endpoint {
        builder = builder.base_url(origin(endpoint)?);
    }
    Ok(AnyClient::Copilot(builder.build()?))
}

pub(crate) fn instruction_protocol(
    model: &sdk::CompletionModel<ProviderHttpClient>,
) -> crate::context::instructions::InstructionProtocol {
    if model.uses_responses() {
        crate::context::instructions::InstructionProtocol::Responses
    } else {
        crate::context::instructions::InstructionProtocol::Chat
    }
}
pub(crate) fn reasoning_protocol(
    model: &sdk::CompletionModel<ProviderHttpClient>,
) -> crate::provider::effort::Protocol {
    use crate::provider::effort::Protocol;
    if model.uses_responses() {
        Protocol::OpenAiResponses
    } else if model.model.to_ascii_lowercase().starts_with("gpt-")
        || model.model.starts_with("o1")
        || model.model.starts_with("o3")
        || model.model.starts_with("o4")
    {
        Protocol::OpenAiCompletions
    } else {
        Protocol::Other
    }
}

#[cfg(test)]
#[path = "provider_copilot_tests.rs"]
mod tests;
