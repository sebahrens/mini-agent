//! App-owned Copilot device authorization and refresh. No editor identity,
//! ambient GitHub CLI cache, or SDK-owned token file is used.
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf, time::Duration};

const MAX_BYTES: usize = 256 * 1024;
const MAX_TOKEN: usize = 32 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    host: String,
    client_id: Option<String>,
    #[serde(skip)]
    #[cfg(test)]
    endpoints: Option<TestEndpoints>,
}
impl PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host && self.client_id == other.client_id
    }
}
impl Eq for Profile {}
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct TestEndpoints {
    origin: String,
}
impl Profile {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let host = std::env::var("COPILOT_GITHUB_HOST")
            .unwrap_or_else(|_| "github.com".into())
            .to_ascii_lowercase();
        let client_id = std::env::var("COPILOT_CLIENT_ID").context(
            "Set COPILOT_CLIENT_ID to your GitHub OAuth app's client ID with device flow enabled",
        )?;
        let profile = Self {
            host,
            client_id: Some(client_id),
            #[cfg(test)]
            endpoints: None,
        };
        profile.validate()?;
        Ok(profile)
    }
    pub(crate) fn for_host(host: &str) -> anyhow::Result<Self> {
        let profile = Self {
            host: host.to_ascii_lowercase(),
            client_id: None,
            #[cfg(test)]
            endpoints: None,
        };
        profile.validate()?;
        Ok(profile)
    }
    fn validate(&self) -> anyhow::Result<()> {
        validate_host(&self.host)?;
        ensure!(
            self.client_id.as_ref().is_none_or(|id| bounded(id, 256)
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))),
            "Invalid Copilot OAuth client ID"
        );
        Ok(())
    }
    fn url(&self, path: &str, api: bool) -> String {
        #[cfg(test)]
        if let Some(test) = &self.endpoints {
            return format!("{}{path}", test.origin);
        }
        let prefix = if api { "api." } else { "" };
        format!("https://{prefix}{}{path}", self.host)
    }
    pub(crate) fn model_origin(&self, value: Option<&str>) -> anyhow::Result<String> {
        let default = if self.host == "github.com" {
            "https://api.githubcopilot.com".into()
        } else {
            format!("https://copilot-api.{}", self.host)
        };
        let url = reqwest::Url::parse(value.unwrap_or(&default))
            .map_err(|_| anyhow::anyhow!("Invalid Copilot API endpoint"))?;
        let common = url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none();
        let host = url.host_str().unwrap_or("");
        let approved = if self.host == "github.com" {
            host == "githubcopilot.com" || host.ends_with(".githubcopilot.com")
        } else {
            host == format!("copilot-api.{}", self.host)
                || host == format!("copilot-proxy.{}", self.host)
        };
        #[cfg(test)]
        let approved = approved
            || self
                .endpoints
                .as_ref()
                .is_some_and(|e| e.origin == value.unwrap_or(""));
        #[cfg(test)]
        let common = common
            || self
                .endpoints
                .as_ref()
                .is_some_and(|e| e.origin == value.unwrap_or(""));
        ensure!(
            common && approved,
            "Copilot token response changed its approved API destination"
        );
        Ok(url.to_string().trim_end_matches('/').into())
    }
}
fn validate_host(host: &str) -> anyhow::Result<()> {
    ensure!(
        host == "github.com"
            || host
                .strip_suffix(".ghe.com")
                .is_some_and(|slug| !slug.is_empty()
                    && slug.len() <= 63
                    && slug
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')),
        "Copilot GitHub host must be github.com or your enterprise's SUBDOMAIN.ghe.com"
    );
    Ok(())
}
fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}
fn now() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}
fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::paths::process_paths()?
        .credentials_dir
        .join("copilot"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    version: u8,
    revision: u64,
    generation: String,
    profile: Profile,
    account: u64,
    login: String,
    tokens: Option<GitHubTokens>,
    model: Option<ModelToken>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitHubTokens {
    access: String,
    refresh: Option<String>,
    expires: Option<u64>,
    refresh_expires: Option<u64>,
    #[serde(default)]
    verified: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelToken {
    token: String,
    expires: u64,
    origin: String,
}
impl Store {
    fn validate(&self) -> anyhow::Result<()> {
        self.profile.validate()?;
        ensure!(
            self.profile.client_id.is_some(),
            "Copilot registration has no app client ID"
        );
        ensure!(
            self.version == 1
                && uuid::Uuid::parse_str(&self.generation).is_ok()
                && (self.tokens.is_none() || self.account > 0)
                && self.login.len() <= 256
                && !self.login.chars().any(char::is_control),
            "Invalid Copilot registration"
        );
        if let Some(t) = &self.tokens {
            ensure!(
                bounded(&t.access, MAX_TOKEN)
                    && t.refresh.as_ref().is_none_or(|r| bounded(r, MAX_TOKEN)),
                "Invalid Copilot credentials"
            );
        }
        if let Some(t) = &self.model {
            ensure!(
                self.tokens.as_ref().is_some_and(|tokens| tokens.verified),
                "Unverified Copilot credentials cannot authorize a model token"
            );
            ensure!(
                bounded(&t.token, MAX_TOKEN),
                "Invalid Copilot model credential"
            );
            self.profile.model_origin(Some(&t.origin))?;
        }
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
#[error("Copilot credentials are busy; retry after the other sign-in or refresh finishes")]
struct CredentialsBusy;
struct FileLock(std::fs::File);
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
struct LockedStore {
    directory: PathBuf,
    data: Option<Store>,
    _lock: FileLock,
}
impl LockedStore {
    fn open(directory: PathBuf, create: bool) -> anyhow::Result<Option<Self>> {
        if !create && !directory.exists() {
            return Ok(None);
        }
        crate::fs::ensure_private_directory(&directory)
            .context("Copilot credential directory unavailable")?;
        let path = directory.join("session.lock");
        match crate::fs::private_atomic_create_sync(&path, b"") {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => anyhow::bail!("Copilot credential lock unavailable"),
        };
        let file =
            crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Append)
                .context("Copilot credential lock unavailable")?;
        match file.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Err(CredentialsBusy.into()),
            Err(_) => anyhow::bail!("Copilot credential lock unavailable"),
        }
        let lock = FileLock(file);
        let data = match crate::fs::open_private_file_checked(
            &directory.join("session.json"),
            crate::fs::PrivateFileAccess::Read,
        ) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .context("Copilot credentials unavailable")?;
                ensure!(
                    bytes.len() <= MAX_BYTES,
                    "Copilot credential byte limit exceeded"
                );
                let data: Store = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("Invalid Copilot credential record"))?;
                data.validate()?;
                Some(data)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => anyhow::bail!("Copilot credentials are not a private owned regular file"),
        };
        Ok(Some(Self {
            directory,
            data,
            _lock: lock,
        }))
    }
    fn save(&mut self) -> anyhow::Result<()> {
        let data = self
            .data
            .as_mut()
            .context("Copilot registration unavailable")?;
        data.validate()?;
        data.revision = data
            .revision
            .checked_add(1)
            .context("Copilot credential revision exhausted")?;
        let bytes = serde_json::to_vec(data)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "Copilot credential byte limit exceeded"
        );
        crate::fs::private_atomic_write_sync(&self.directory.join("session.json"), &bytes)
            .context("Copilot credentials could not be saved")
    }
}

/// Captures one local account generation. A later sign-in cannot silently
/// change its account; logout makes subsequent credential acquisition fail.
#[derive(Clone)]
pub(crate) struct Registration {
    directory: PathBuf,
    generation: String,
    profile: Profile,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CopilotRegistration([REDACTED])")
    }
}
/// Read-only local presence for a captured host; no lock/cache repair or
/// credential acquisition is performed and no registration is returned.
pub(crate) fn diagnostic_state_at(
    credentials_dir: &std::path::Path,
    host: &str,
) -> super::LoginRecordState {
    use super::LoginRecordState;
    let inspect = || -> anyhow::Result<LoginRecordState> {
        let profile = Profile::for_host(host)?;
        let Some(bytes) = super::diagnostic_record_at(
            credentials_dir,
            &["copilot", &profile.host, "session.json"],
            MAX_BYTES,
        )?
        else {
            return Ok(LoginRecordState::Absent);
        };
        let store: Store = serde_json::from_slice(&bytes)?;
        store.validate()?;
        ensure!(
            store.profile.host == profile.host,
            "Copilot host registration changed"
        );
        Ok(if store.tokens.is_some() {
            LoginRecordState::Stored
        } else {
            LoginRecordState::Absent
        })
    };
    inspect().unwrap_or(LoginRecordState::Unavailable)
}

pub(crate) fn selected() -> anyhow::Result<Option<Registration>> {
    let host = std::env::var("COPILOT_GITHUB_HOST")
        .unwrap_or_else(|_| "github.com".into())
        .to_ascii_lowercase();
    selected_for_profile(&Profile::for_host(&host)?)
}
pub(crate) fn selected_for_profile(profile: &Profile) -> anyhow::Result<Option<Registration>> {
    profile.validate()?;
    let host = &profile.host;
    let directory = root()?.join(host);
    let Some(store) = LockedStore::open(directory.clone(), false)? else {
        return Ok(None);
    };
    let Some(data) = store.data.filter(|d| d.tokens.is_some()) else {
        return Ok(None);
    };
    ensure!(
        data.profile.host.as_str() == host.as_str(),
        "Copilot host registration changed"
    );
    Ok(Some(Registration {
        directory,
        generation: data.generation,
        profile: data.profile,
    }))
}

fn http() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("mini-agent/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
async fn json_response(request: reqwest::RequestBuilder) -> anyhow::Result<serde_json::Value> {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Copilot authorization request timed out")
            } else {
                anyhow::anyhow!("Copilot authorization HTTP failed")
            }
        })?;
        let response_code = response.status().as_u16();
        ensure!(
            (200..300).contains(&response_code),
            "Copilot authorization HTTP status {response_code}"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_BYTES as u64),
            "Copilot authorization response exceeds byte limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Copilot authorization response failed"))?
        {
            ensure!(
                chunk.len() <= MAX_BYTES.saturating_sub(bytes.len()),
                "Copilot authorization response exceeds byte limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid Copilot authorization response"))
    })
    .await
    .map_err(|_| anyhow::anyhow!("Copilot authorization request timed out"))?
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token_expires_in: Option<u64>,
}
fn tokens(value: serde_json::Value) -> anyhow::Result<GitHubTokens> {
    let response: TokenResponse = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("Invalid GitHub OAuth token response"))?;
    ensure!(
        response.token_type.eq_ignore_ascii_case("bearer")
            && bounded(&response.access_token, MAX_TOKEN)
            && response
                .refresh_token
                .as_ref()
                .is_none_or(|r| bounded(r, MAX_TOKEN)),
        "Invalid GitHub OAuth token response"
    );
    let at = now()?;
    let expiry = |seconds: Option<u64>| -> anyhow::Result<Option<u64>> {
        seconds
            .map(|s| {
                ensure!(s > 0, "Invalid GitHub token lifetime");
                at.checked_add(s).context("GitHub token lifetime overflow")
            })
            .transpose()
    };
    Ok(GitHubTokens {
        access: response.access_token,
        refresh: response.refresh_token,
        expires: expiry(response.expires_in)?,
        refresh_expires: expiry(response.refresh_token_expires_in)?,
        verified: false,
    })
}
async fn model_token(profile: &Profile, access: &str) -> anyhow::Result<ModelToken> {
    let value = json_response(
        http()?
            .get(profile.url("/copilot_internal/v2/token", true))
            .header("accept", "application/json")
            .header("authorization", format!("token {access}"))
            .header(
                "editor-version",
                concat!("mini-agent/", env!("CARGO_PKG_VERSION")),
            )
            .header(
                "editor-plugin-version",
                concat!("mini-agent/", env!("CARGO_PKG_VERSION")),
            ),
    )
    .await?;
    let token = value
        .get("token")
        .and_then(|v| v.as_str())
        .filter(|v| bounded(v, MAX_TOKEN))
        .context("Copilot token response did not contain a bounded token")?;
    let expires = value
        .get("expires_at")
        .and_then(|v| v.as_u64())
        .context("Copilot token response did not contain an expiry")?;
    ensure!(
        expires > now()?.saturating_add(5),
        "Copilot returned an expired token"
    );
    let origin = profile.model_origin(value.pointer("/endpoints/api").and_then(|v| v.as_str()))?;
    Ok(ModelToken {
        token: token.into(),
        expires,
        origin,
    })
}
impl Registration {
    pub(crate) async fn auth_context(
        &self,
    ) -> anyhow::Result<rig::providers::copilot::AuthContext> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.auth_context_inner())
            .await
            .map_err(|_| anyhow::anyhow!("Copilot credential acquisition timed out"))?
    }
    async fn auth_context_inner(&self) -> anyhow::Result<rig::providers::copilot::AuthContext> {
        let mut store = loop {
            match LockedStore::open(self.directory.clone(), false) {
                Ok(Some(store)) => break store,
                Ok(None) => anyhow::bail!("Copilot is signed out; run /login copilot"),
                Err(error) if error.is::<CredentialsBusy>() => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => return Err(error),
            }
        };
        let data = store
            .data
            .as_mut()
            .context("Copilot is signed out; run /login copilot")?;
        ensure!(
            data.generation == self.generation && data.profile == self.profile,
            "Copilot registration changed; rebuild the provider client"
        );
        let t = data
            .tokens
            .as_mut()
            .context("Copilot is signed out; run /login copilot")?;
        let at = now()?;
        if t.expires.is_some_and(|e| e <= at.saturating_add(60)) {
            ensure!(
                t.refresh_expires.is_none_or(|e| e > at),
                "Copilot GitHub authorization expired; run /login copilot"
            );
            let refresh = t
                .refresh
                .as_ref()
                .context("Copilot GitHub authorization expired; run /login copilot")?;
            let value = json_response(
                http()?
                    .post(self.profile.url("/login/oauth/access_token", false))
                    .header("accept", "application/json")
                    .form(&[
                        (
                            "client_id",
                            self.profile
                                .client_id
                                .as_deref()
                                .context("Copilot app identity unavailable")?,
                        ),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh.as_str()),
                    ]),
            )
            .await?;
            let mut next = tokens(value)?;

            if next.refresh.is_none() {
                next.refresh = t.refresh.clone();
                next.refresh_expires = t.refresh_expires;
            }
            *t = next;
            data.model = None;
            // Retain rotation before fallible identity/model requests. The
            // pending token cannot authorize a model until account verification.
            store.save()?;
        }
        let data = store.data.as_mut().unwrap();
        if !data.tokens.as_ref().unwrap().verified {
            ensure!(
                user(&self.profile, &data.tokens.as_ref().unwrap().access)
                    .await?
                    .0
                    == data.account,
                "GitHub account changed during Copilot refresh; run /login copilot"
            );
            data.tokens.as_mut().unwrap().verified = true;
            store.save()?;
        }
        let data = store.data.as_mut().unwrap();
        if data
            .model
            .as_ref()
            .is_none_or(|m| m.expires <= at.saturating_add(30))
        {
            data.model =
                Some(model_token(&self.profile, &data.tokens.as_ref().unwrap().access).await?);
            store.save()?;
        }
        let model = store.data.as_ref().unwrap().model.as_ref().unwrap();
        Ok(rig::providers::copilot::AuthContext {
            api_key: model.token.clone(),
            api_base: Some(model.origin.clone()),
        })
    }
}
async fn user(profile: &Profile, access: &str) -> anyhow::Result<(u64, String)> {
    let value = json_response(
        http()?
            .get(profile.url("/user", true))
            .bearer_auth(access)
            .header("accept", "application/json"),
    )
    .await?;
    let id = value
        .get("id")
        .and_then(|v| v.as_u64())
        .filter(|v| *v > 0)
        .context("GitHub did not return an account identity")?;
    let login = value
        .get("login")
        .and_then(|v| v.as_str())
        .filter(|v| bounded(v, 256) && !v.chars().any(char::is_control))
        .context("GitHub did not return an account name")?;
    Ok((id, login.into()))
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}
async fn device_tokens(profile: &Profile) -> anyhow::Result<GitHubTokens> {
    let value = json_response(
        http()?
            .post(profile.url("/login/device/code", false))
            .header("accept", "application/json")
            .form(&[
                (
                    "client_id",
                    profile
                        .client_id
                        .as_deref()
                        .context("Copilot app identity unavailable")?,
                ),
                ("scope", "read:user"),
            ]),
    )
    .await?;
    let device: DeviceResponse = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("Invalid GitHub device authorization response"))?;
    ensure!(
        bounded(&device.device_code, 1024)
            && bounded(&device.user_code, 64)
            && !device.user_code.chars().any(char::is_control),
        "Invalid GitHub device authorization codes"
    );
    let expected = profile.url("/login/device", false);
    ensure!(
        device.verification_uri == expected,
        "GitHub device response changed its verification destination"
    );
    ensure!(device.expires_in > 0, "GitHub device authorization expired");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in.min(900));
    let mut interval = device.interval.unwrap_or(5).max(1);
    ensure!(
        interval <= 900,
        "GitHub device poll interval exceeds authorization lifetime"
    );
    println!(
        "Sign in with your mini-agent GitHub OAuth app:\n1) Visit {}\n2) Enter code: {}\nPress Ctrl-C to cancel.",
        device.verification_uri, device.user_code
    );
    loop {
        tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_secs(interval)))
            .await
            .map_err(|_| anyhow::anyhow!("GitHub device authorization timed out"))?;
        ensure!(
            tokio::time::Instant::now() < deadline,
            "GitHub device authorization timed out"
        );
        let value = tokio::time::timeout_at(
            deadline,
            json_response(
                http()?
                    .post(profile.url("/login/oauth/access_token", false))
                    .header("accept", "application/json")
                    .form(&[
                        ("client_id", profile.client_id.as_deref().unwrap()),
                        ("device_code", device.device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ]),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("GitHub device authorization timed out"))??;
        if value.get("access_token").is_some() {
            return tokens(value);
        }
        match value.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => (),
            Some("slow_down") => {
                interval = interval
                    .checked_add(5)
                    .context("GitHub poll interval overflow")?;
                if let Some(server) = value.get("interval").and_then(|v| v.as_u64()) {
                    interval = interval.max(server);
                }
            }
            Some("expired_token" | "token_expired") => {
                anyhow::bail!("GitHub device authorization expired; start sign-in again")
            }
            Some("access_denied") => anyhow::bail!("GitHub device authorization was denied"),
            Some("device_flow_disabled") => {
                anyhow::bail!("Enable device flow in your GitHub OAuth app before signing in")
            }
            _ => anyhow::bail!(
                "GitHub device authorization failed; check the configured app client ID"
            ),
        }
        ensure!(
            interval <= 900,
            "GitHub device authorization cannot complete within its lifetime"
        );
    }
}
async fn sign_in_profile(profile: Profile, directory: PathBuf) -> anyhow::Result<()> {
    let expected = {
        let mut store = LockedStore::open(directory.clone(), true)?.unwrap();
        if store.data.is_none() {
            store.data = Some(Store {
                version: 1,
                revision: 0,
                generation: uuid::Uuid::new_v4().to_string(),
                profile: profile.clone(),
                account: 0,
                login: String::new(),
                tokens: None,
                model: None,
            });
            store.save()?;
        }
        store.data.map(|d| (d.generation, d.revision))
    };
    let mut tokens = device_tokens(&profile).await?;
    let (account, login) = user(&profile, &tokens.access).await?;
    tokens.verified = true;
    let model = model_token(&profile, &tokens.access).await?;
    let mut store = LockedStore::open(directory, true)?.unwrap();
    ensure!(
        store.data.as_ref().map(|d| (&d.generation, d.revision))
            == expected.as_ref().map(|(g, r)| (g, *r)),
        "Copilot registration changed during sign-in; retry sign-in"
    );
    store.data = Some(Store {
        version: 1,
        revision: expected.map_or(0, |(_, r)| r),
        generation: uuid::Uuid::new_v4().to_string(),
        profile,
        account,
        login,
        tokens: Some(tokens),
        model: Some(model),
    });
    store.save()
}
pub(crate) async fn sign_in() -> anyhow::Result<()> {
    let profile = Profile::from_env()?;
    let directory = root()?.join(&profile.host);
    tokio::select! {
        result=sign_in_profile(profile,directory)=>result,
        _=tokio::signal::ctrl_c()=>Err(anyhow::anyhow!("Copilot sign-in cancelled")),
    }
}
pub(crate) fn status() -> anyhow::Result<Vec<String>> {
    let Some(registration) = selected()? else {
        return Ok(vec!["Copilot: signed out".into()]);
    };
    let store = LockedStore::open(registration.directory, false)?
        .context("Copilot registration unavailable")?;
    let data = store.data.context("Copilot registration unavailable")?;
    Ok(vec![format!(
        "Copilot: signed in as {} on {}",
        data.login, data.profile.host
    )])
}
pub(crate) async fn logout() -> anyhow::Result<String> {
    let host = std::env::var("COPILOT_GITHUB_HOST")
        .unwrap_or_else(|_| "github.com".into())
        .to_ascii_lowercase();
    validate_host(&host)?;
    let Some(mut store) = LockedStore::open(root()?.join(&host), false)? else {
        return Ok("Copilot is already signed out".into());
    };
    if let Some(data) = store.data.as_mut() {
        data.tokens = None;
        data.model = None;
        data.generation = uuid::Uuid::new_v4().to_string();
        store.save()?;
    }
    Ok(
        "Copilot signed out locally; revoke the OAuth app in GitHub settings to revoke the grant"
            .into(),
    )
}

#[derive(Default)]
pub(crate) struct ExternalTokenCache(Option<ModelToken>);
pub(crate) async fn external_context(
    profile: &Profile,
    access: &str,
    cache: &mut ExternalTokenCache,
) -> anyhow::Result<rig::providers::copilot::AuthContext> {
    let at = now()?;
    if cache
        .0
        .as_ref()
        .is_none_or(|token| token.expires <= at.saturating_add(30))
    {
        cache.0 = Some(model_token(profile, access).await?);
    }
    let token = cache.0.as_ref().unwrap();
    Ok(rig::providers::copilot::AuthContext {
        api_key: token.token.clone(),
        api_base: Some(token.origin.clone()),
    })
}

#[cfg(test)]
#[path = "copilot_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "copilot_protocol_tests.rs"]
mod protocol_tests;
