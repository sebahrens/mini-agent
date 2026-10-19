//! Provider sign-in using the documented OpenAI public OSS registration flow.
//! No Claude Code/Codex client identity or third-party credential file is read.

use anyhow::{Context, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Read, path::PathBuf, time::Duration};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const REVOKE: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const MAX_BYTES: usize = 256 * 1024;
const MAX_TOKEN: usize = 32 * 1024;
const MAX_ACCOUNTS: usize = 16;

#[derive(Clone, PartialEq, Eq)]
struct AuthEndpoints {
    token: String,
    jwks: String,
}
impl Default for AuthEndpoints {
    fn default() -> Self {
        Self {
            token: TOKEN.into(),
            jwks: JWKS.into(),
        }
    }
}

pub(crate) const ANTHROPIC_UNAVAILABLE: &str = "Anthropic has not documented independent third-party Pro/Max OAuth registration for mini-agent. Use an Anthropic API key or supported cloud provider; Claude Code credentials are not imported.";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    version: u8,
    revision: u64,
    host: String,
    active: Option<String>,
    accounts: Vec<Account>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    client_id: String,
    subject: String,
    email: Option<String>,
    tokens: Option<Tokens>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tokens {
    access: String,
    refresh: String,
    id: String,
    scopes: String,
    expires: u64,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
}
#[derive(Debug, thiserror::Error)]
#[error("ChatGPT authorization expired or was revoked; run /login openai again")]
struct ReauthorizationRequired;
// A code can expire after registration issued a public client ID. Keep that
// registration only in this invocation until identity validation succeeds.
#[derive(Debug, thiserror::Error)]
#[error("Authorization code expired; repeat sign-in with its issued registration")]
struct CodeRetry {
    root: PathBuf,
    host: String,
    revision: u64,
    client: String,
    subject: Option<String>,
}
#[derive(Deserialize)]
struct IdClaims {
    iss: String,
    aud: serde_json::Value,
    sub: String,
    exp: u64,
    nonce: Option<String>,
    email: Option<String>,
}
#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}
#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    n: String,
    e: String,
}
#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    kid: String,
    crit: Option<Vec<String>>,
}

fn now() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}
fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}
fn plan_scopes(scopes: &str) -> bool {
    [
        "openid",
        "offline_access",
        "resource.invoke",
        "chatgpt.tokens.use.direct",
    ]
    .iter()
    .all(|scope| scopes.split_ascii_whitespace().any(|s| s == *scope))
}
fn random() -> String {
    B64.encode(
        [
            uuid::Uuid::new_v4().as_bytes().as_slice(),
            uuid::Uuid::new_v4().as_bytes().as_slice(),
        ]
        .concat(),
    )
}

impl Store {
    fn new() -> Self {
        Self {
            version: 1,
            revision: 0,
            host: format!("urn:uuid:{}", uuid::Uuid::new_v4()),
            active: None,
            accounts: Vec::new(),
        }
    }
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.version == 1
                && self
                    .host
                    .strip_prefix("urn:uuid:")
                    .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok()),
            "Invalid provider credential record"
        );
        ensure!(
            self.accounts.len() <= MAX_ACCOUNTS,
            "Too many provider registrations"
        );
        let mut seen = std::collections::HashSet::new();
        for account in &self.accounts {
            ensure!(
                bounded(&account.client_id, 256)
                    && account.client_id != "dynamic_agent_client"
                    && bounded(&account.subject, 512)
                    && account.email.as_ref().is_none_or(|e| e.len() <= 320)
                    && seen.insert(&account.client_id),
                "Invalid provider registration"
            );
            if let Some(t) = &account.tokens {
                ensure!(
                    [&t.access, &t.refresh, &t.id]
                        .iter()
                        .all(|v| bounded(v, MAX_TOKEN))
                        && t.scopes.len() <= 1024
                        && plan_scopes(&t.scopes),
                    "Invalid provider token record"
                );
            }
        }
        ensure!(
            self.active
                .as_ref()
                .is_none_or(|id| self.accounts.iter().any(|a| a.client_id == *id)),
            "Invalid active provider registration"
        );
        Ok(())
    }
}

/// A selected registration, not a token. Captured clients cannot switch accounts
/// implicitly. Logout fences them until this same issued registration and
/// validated subject are explicitly reauthorized.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Registration {
    root: PathBuf,
    client_id: String,
    #[cfg(test)]
    endpoints: Option<AuthEndpoints>,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPTRegistration([REDACTED])")
    }
}
struct CredentialLock(std::fs::File);
impl Drop for CredentialLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
struct LockedStore {
    root: PathBuf,
    _lock: CredentialLock,
    data: Store,
}
impl LockedStore {
    fn open(root: PathBuf, create: bool) -> anyhow::Result<Option<Self>> {
        #[cfg(test)]
        {
            let mut opens = STORE_OPENS.lock().unwrap_or_else(|e| e.into_inner());
            match opens.iter_mut().find(|(path, _)| *path == root) {
                Some((_, count)) => *count += 1,
                None => opens.push((root.clone(), 1)),
            }
        }
        if !create && !root.exists() {
            return Ok(None);
        }
        crate::fs::ensure_private_directory(&root)
            .context("Provider credential directory unavailable")?;
        let path = root.join("session.lock");
        match crate::fs::private_atomic_create_sync(&path, b"") {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => anyhow::bail!("Provider credential lock unavailable"),
        }
        let lock =
            crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Append)
                .context("Provider credential lock unavailable")?;
        lock.try_lock().map_err(|_| {
            anyhow::anyhow!(
                "Provider credentials are busy; retry after the other sign-in or refresh finishes"
            )
        })?;
        let lock = CredentialLock(lock);
        let path = root.join("openai.json");
        let data =
            match crate::fs::open_private_file_checked(&path, crate::fs::PrivateFileAccess::Read) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(MAX_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                        .context("Provider credentials could not be read")?;
                    ensure!(
                        bytes.len() <= MAX_BYTES,
                        "Provider credential byte limit exceeded"
                    );
                    serde_json::from_slice::<Store>(&bytes)
                        .map_err(|_| anyhow::anyhow!("Invalid provider credential record"))?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::new(),
                Err(_) => {
                    anyhow::bail!("Provider credentials are not a private owned regular file")
                }
            };
        data.validate()?;
        Ok(Some(Self {
            root,
            _lock: lock,
            data,
        }))
    }
    fn install_login(
        &mut self,
        expected: (&str, u64),
        client: String,
        tokens: Tokens,
        claims: IdClaims,
    ) -> anyhow::Result<()> {
        ensure!(
            self.data.host == expected.0 && self.data.revision == expected.1,
            "Provider host registration changed during sign-in"
        );
        if let Some(account) = self
            .data
            .accounts
            .iter_mut()
            .find(|a| a.client_id == client)
        {
            ensure!(
                account.subject == claims.sub,
                "Provider account registration changed"
            );
            account.tokens = Some(tokens);
            account.email = claims.email;
        } else {
            ensure!(
                self.data.accounts.len() < MAX_ACCOUNTS,
                "Too many ChatGPT registrations"
            );
            self.data.accounts.push(Account {
                client_id: client.clone(),
                subject: claims.sub,
                email: claims.email,
                tokens: Some(tokens),
            });
        }
        self.data.active = Some(client);
        self.save()
    }
    fn save(&mut self) -> anyhow::Result<()> {
        self.data.validate()?;
        self.data.revision = self
            .data
            .revision
            .checked_add(1)
            .context("Provider credential revision exhausted")?;
        let bytes = serde_json::to_vec(&self.data)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "Provider credential byte limit exceeded"
        );
        crate::fs::private_atomic_write_sync(&self.root.join("openai.json"), &bytes)
            .context("Provider credentials could not be saved")
    }
}
fn root() -> anyhow::Result<PathBuf> {
    Ok(crate::paths::process_paths()?
        .credentials_dir
        .join("provider-oauth"))
}

pub(crate) fn official_api_base(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        value.len() <= 2048
            && reqwest::Url::parse(value).is_ok_and(|url| {
                url.scheme() == "https"
                    && url.host_str() == Some("api.openai.com")
                    && url.port_or_known_default() == Some(443)
                    && matches!(url.path(), "/v1" | "/v1/")
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
    })
}

/// Presence only, under the explicitly captured credential root. In
/// particular an active revoked registration fences API-key fallback.
pub(crate) fn diagnostic_state_at(credentials_dir: &std::path::Path) -> super::LoginRecordState {
    use super::LoginRecordState;
    let inspect = || -> anyhow::Result<LoginRecordState> {
        let Some(bytes) = super::diagnostic_record_at(
            credentials_dir,
            &["provider-oauth", "openai.json"],
            MAX_BYTES,
        )?
        else {
            return Ok(LoginRecordState::Absent);
        };
        let store: Store = serde_json::from_slice(&bytes)?;
        store.validate()?;
        let Some(active) = store.active.as_ref() else {
            return Ok(LoginRecordState::Absent);
        };
        let account = store
            .accounts
            .iter()
            .find(|a| &a.client_id == active)
            .context("Active registration unavailable")?;
        Ok(if account.tokens.is_some() {
            LoginRecordState::Stored
        } else {
            LoginRecordState::SignInRequired
        })
    };
    inspect().unwrap_or(LoginRecordState::Unavailable)
}

pub(crate) fn selected() -> anyhow::Result<Option<Registration>> {
    let root = root()?;
    let Some(store) = LockedStore::open(root.clone(), false)? else {
        return Ok(None);
    };
    Ok(store.data.active.as_ref().map(|id| Registration {
        root,
        client_id: id.clone(),
        #[cfg(test)]
        endpoints: None,
    }))
}
pub(crate) fn status() -> anyhow::Result<Vec<String>> {
    let Some(store) = LockedStore::open(root()?, false)? else {
        return Ok(vec!["ChatGPT: not signed in".into()]);
    };
    let mut lines =
        vec!["ChatGPT registrations (use /login openai <number> to reauthorize):".into()];
    for (i, a) in store.data.accounts.iter().enumerate() {
        let label = a.email.as_deref().unwrap_or("account");
        lines.push(format!(
            "  {}: {} {} ({})",
            i + 1,
            crate::ui::events::sanitize_output(label),
            if store.data.active.as_deref() == Some(&a.client_id) {
                "[active]"
            } else {
                ""
            },
            if a.tokens.is_some() {
                "signed in"
            } else if store.data.active.as_deref() == Some(&a.client_id) {
                "sign-in required"
            } else {
                "signed out"
            }
        ));
    }
    if store.data.accounts.is_empty() {
        lines.push("ChatGPT: not signed in".into());
    }
    Ok(lines)
}

fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()?)
}
fn json_response<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
) -> anyhow::Result<T> {
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Provider authentication response failed"))?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "Provider authentication response limit exceeded"
    );
    if !status.is_success() {
        let error = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        if matches!(status.as_u16(), 400 | 401)
            && error
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|e| matches!(e, "invalid_grant" | "invalid_token"))
        {
            return Err(ReauthorizationRequired.into());
        }
        anyhow::bail!("Provider authentication HTTP status {}", status.as_u16());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Invalid provider authentication response"))
}
fn verify_id(
    token: &str,
    keys: &Jwks,
    client: &str,
    nonce: Option<&str>,
    at: u64,
) -> anyhow::Result<IdClaims> {
    ensure!(bounded(token, MAX_TOKEN), "Invalid provider identity token");
    let parts = token.split('.').collect::<Vec<_>>();
    ensure!(parts.len() == 3, "Invalid provider identity token");
    let header: JwtHeader = serde_json::from_slice(
        &B64.decode(parts[0])
            .map_err(|_| anyhow::anyhow!("Invalid provider identity header"))?,
    )
    .map_err(|_| anyhow::anyhow!("Invalid provider identity header"))?;
    ensure!(
        header.alg == "RS256" && header.crit.is_none() && keys.keys.len() <= 32,
        "Unsupported provider identity signature"
    );
    let matches = keys
        .keys
        .iter()
        .filter(|k| {
            k.kid == header.kid
                && k.kty == "RSA"
                && k.alg.as_deref().is_none_or(|a| a == "RS256")
                && k.usage.as_deref().is_none_or(|u| u == "sig")
        })
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "Provider signing key unavailable or ambiguous"
    );
    let key = matches[0];
    let n = B64
        .decode(&key.n)
        .map_err(|_| anyhow::anyhow!("Invalid provider signing key"))?;
    let e = B64
        .decode(&key.e)
        .map_err(|_| anyhow::anyhow!("Invalid provider signing key"))?;
    let signature = B64
        .decode(parts[2])
        .map_err(|_| anyhow::anyhow!("Invalid provider identity signature"))?;
    ring::signature::RsaPublicKeyComponents { n, e }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &signature,
        )
        .map_err(|_| anyhow::anyhow!("Invalid provider identity signature"))?;
    let claims: IdClaims = serde_json::from_slice(
        &B64.decode(parts[1])
            .map_err(|_| anyhow::anyhow!("Invalid provider identity claims"))?,
    )
    .map_err(|_| anyhow::anyhow!("Invalid provider identity claims"))?;
    let audience = match &claims.aud {
        serde_json::Value::String(v) => v == client,
        serde_json::Value::Array(v) => v.len() == 1 && v[0].as_str() == Some(client),
        _ => false,
    };
    ensure!(
        claims.iss == ISSUER
            && audience
            && claims.exp > at
            && bounded(&claims.sub, 512)
            && nonce.is_none_or(|n| claims.nonce.as_deref() == Some(n)),
        "Provider identity binding failed"
    );
    Ok(claims)
}
fn accept_tokens(
    response: TokenResponse,
    keys: &Jwks,
    client: &str,
    nonce: Option<&str>,
    subject: Option<&str>,
    at: u64,
) -> anyhow::Result<(Tokens, IdClaims)> {
    ensure!(
        response.token_type.eq_ignore_ascii_case("bearer")
            && plan_scopes(&response.scope)
            && response.expires_in > 0
            && response.expires_in <= 86400,
        "ChatGPT plan permission was not granted"
    );
    ensure!(
        [
            &response.access_token,
            &response.refresh_token,
            &response.id_token
        ]
        .iter()
        .all(|v| bounded(v, MAX_TOKEN)),
        "Invalid provider token response"
    );
    let claims = verify_id(&response.id_token, keys, client, nonce, at)?;
    ensure!(
        subject.is_none_or(|sub| sub == claims.sub),
        "ChatGPT account changed during reauthorization"
    );
    Ok((
        Tokens {
            access: response.access_token,
            refresh: response.refresh_token,
            id: response.id_token,
            scopes: response.scope,
            expires: at
                .checked_add(response.expires_in)
                .context("Invalid token expiry")?,
        },
        claims,
    ))
}

/// The access token a request used, with the file state it was read from.
struct CachedAccess {
    root: PathBuf,
    client_id: String,
    fingerprint: super::FileFingerprint,
    access: String,
    /// Unix second after which the token is due for the locked refresh path.
    valid_until: u64,
}
const CACHED_ACCESS_TOKENS: usize = 8;
static ACCESS_CACHE: std::sync::Mutex<Vec<CachedAccess>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
static STORE_OPENS: std::sync::Mutex<Vec<(PathBuf, usize)>> = std::sync::Mutex::new(Vec::new());
/// Test-only: how many times the locked store of `root` was opened.
#[cfg(test)]
pub(crate) fn store_opens(root: &std::path::Path) -> usize {
    let opens = STORE_OPENS.lock().unwrap_or_else(|e| e.into_inner());
    opens
        .iter()
        .find(|(path, _)| path == root)
        .map_or(0, |(_, count)| *count)
}
impl Registration {
    pub(crate) async fn access_token(&self) -> anyhow::Result<String> {
        let registration = self.clone();
        crate::agent::runner::spawn_blocking_scoped(move || registration.access_token_blocking())
            .await
            .context("Provider refresh worker failed")?
    }
    /// The cached token when it is still valid and the record is unchanged
    /// on disk (one private-file open and fstat, no lock, no parse). A logout,
    /// account switch or rotation rewrites the record and so misses.
    fn cached_access(&self) -> Option<String> {
        let (fingerprint, access, valid_until) = {
            let cache = ACCESS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let entry = cache
                .iter()
                .find(|entry| entry.root == self.root && entry.client_id == self.client_id)?;
            (
                entry.fingerprint.clone(),
                entry.access.clone(),
                entry.valid_until,
            )
        };
        (now().ok()? < valid_until
            && super::FileFingerprint::of_private_file(&self.root.join("openai.json"))?
                == fingerprint)
            .then_some(access)
    }
    /// Remember `access` while the credential lock is still held, so the
    /// fingerprint is that of the final record.
    fn remember_access(&self, access: &str, expires: u64) {
        let Some(fingerprint) =
            super::FileFingerprint::of_private_file(&self.root.join("openai.json"))
        else {
            return;
        };
        let mut cache = ACCESS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|entry| !(entry.root == self.root && entry.client_id == self.client_id));
        if cache.len() >= CACHED_ACCESS_TOKENS {
            cache.remove(0);
        }
        cache.push(CachedAccess {
            root: self.root.clone(),
            client_id: self.client_id.clone(),
            fingerprint,
            access: access.to_owned(),
            valid_until: expires.saturating_sub(60),
        });
    }
    fn access_token_blocking(&self) -> anyhow::Result<String> {
        if let Some(access) = self.cached_access() {
            return Ok(access);
        }
        let endpoints = self.endpoints();
        let mut store =
            LockedStore::open(self.root.clone(), false)?.context("ChatGPT is signed out")?;
        ensure!(
            store.data.active.as_deref() == Some(&self.client_id),
            "ChatGPT account changed; rebuild the provider client"
        );
        let account = store
            .data
            .accounts
            .iter_mut()
            .find(|a| a.client_id == self.client_id)
            .context("ChatGPT registration unavailable")?;
        let token = account.tokens.as_ref().ok_or(ReauthorizationRequired)?;
        let at = now()?;
        if token.expires > at.saturating_add(60) {
            let (access, expires) = (token.access.clone(), token.expires);
            self.remember_access(&access, expires);
            return Ok(access);
        }
        let http = http()?;
        let response: anyhow::Result<TokenResponse> = json_response(
            http.post(&endpoints.token)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", self.client_id.as_str()),
                    ("refresh_token", token.refresh.as_str()),
                    ("resource", RESOURCE),
                ])
                .send()
                .map_err(|_| {
                    anyhow::anyhow!("ChatGPT refresh failed; sign in again if access was revoked")
                })?,
        );
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if error.is::<ReauthorizationRequired>() {
                    account.tokens = None;
                    store.save()?;
                }
                return Err(error);
            }
        };
        let keys: Jwks = json_response(
            http.get(&endpoints.jwks)
                .send()
                .map_err(|_| anyhow::anyhow!("Provider signing keys unavailable"))?,
        )?;
        let (tokens, _) = accept_tokens(
            response,
            &keys,
            &self.client_id,
            None,
            Some(&account.subject),
            at,
        )?;
        let access = tokens.access.clone();
        let expires = tokens.expires;
        account.tokens = Some(tokens);
        store.save()?;
        self.remember_access(&access, expires);
        Ok(access)
    }
    fn endpoints(&self) -> AuthEndpoints {
        #[cfg(test)]
        if let Some(endpoints) = &self.endpoints {
            return endpoints.clone();
        }
        AuthEndpoints::default()
    }
}

pub(crate) struct Login {
    listener: tokio::net::TcpListener,
    root: PathBuf,
    host: String,
    revision: u64,
    selected: Option<(String, Option<String>)>,
    state: String,
    nonce: String,
    verifier: String,
    redirect: String,
    url: String,
    #[cfg(test)]
    endpoints: Option<AuthEndpoints>,
}
impl Login {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }
    // Returning browser navigation carries a retained ID-token hint. The
    // manually displayed fallback remains usable without that optional hint.
    fn display_url(&self) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(&self.url)?;
        let public: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| !matches!(key.as_ref(), "id_token_hint" | "login_hint"))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut().extend_pairs(public);
        Ok(url.into())
    }
    pub(crate) async fn begin(account: Option<usize>) -> anyhow::Result<Self> {
        Self::begin_with_retry(account, None).await
    }
    async fn begin_with_retry(
        account: Option<usize>,
        retry: Option<CodeRetry>,
    ) -> anyhow::Result<Self> {
        let root = root()?;
        let store = crate::agent::runner::spawn_blocking_scoped({
            let root = root.clone();
            move || -> anyhow::Result<_> {
                let mut store =
                    LockedStore::open(root, true)?.context("Provider credentials unavailable")?;
                if let Some(retry) = retry {
                    ensure!(
                        store.root == retry.root
                            && store.data.host == retry.host
                            && store.data.revision == retry.revision,
                        "Provider host registration changed during sign-in"
                    );
                    return Ok((
                        store.data.host.clone(),
                        Some((retry.client, retry.subject)),
                        store.data.revision,
                        None,
                    ));
                }
                store.save()?; // stable host identity precedes browser authorization
                let selected = if account == Some(0) {
                    None
                } else if let Some(index) = account {
                    Some(
                        store
                            .data
                            .accounts
                            .get(index.checked_sub(1).context("Account numbers start at1")?)
                            .context("Unknown ChatGPT account number")?,
                    )
                } else {
                    store
                        .data
                        .active
                        .as_ref()
                        .and_then(|id| store.data.accounts.iter().find(|a| a.client_id == *id))
                };
                Ok((
                    store.data.host.clone(),
                    selected.map(|a| (a.client_id.clone(), Some(a.subject.clone()))),
                    store.data.revision,
                    selected
                        .and_then(|a| a.tokens.as_ref())
                        .map(|t| t.id.clone()),
                ))
            }
        })
        .await
        .context("Provider credential worker failed")??;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("OAuth loopback listener unavailable")?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr()?.port()
        );
        let state = random();
        let nonce = random();
        let verifier = random();
        let mut url = reqwest::Url::parse(AUTHORIZE)?;
        url.query_pairs_mut().extend_pairs([
            (
                "client_id",
                store
                    .1
                    .as_ref()
                    .map(|a| a.0.as_str())
                    .unwrap_or("dynamic_agent_client"),
            ),
            ("ext_agent_host_id", &store.0),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &state),
            ("nonce", &nonce),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                &B64.encode(Sha256::digest(verifier.as_bytes())),
            ),
        ]);
        if store.1.is_none() {
            url.query_pairs_mut()
                .append_pair("agent_name_hint", crate::product::PUBLIC_NAME);
        }
        if let Some(hint) = &store.3 {
            url.query_pairs_mut().append_pair("id_token_hint", hint);
        }
        Ok(Self {
            listener,
            root,
            host: store.0,
            revision: store.2,
            selected: store.1,
            state,
            nonce,
            verifier,
            redirect,
            url: url.into(),
            #[cfg(test)]
            endpoints: None,
        })
    }
    pub(crate) async fn complete(self) -> anyhow::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let endpoints = self.endpoints();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        let (code, client) = loop {
            let (mut stream, peer) = tokio::select! { biased;
                _ = tokio::signal::ctrl_c() => return Err(anyhow::anyhow!("Sign-in cancelled")),
                result = tokio::time::timeout_at(deadline, self.listener.accept()) => result.context("Sign-in timed out")??,
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let mut request = Vec::new();
            let parsed = tokio::time::timeout(Duration::from_secs(3), async {
                while !request.ends_with(b"\r\n\r\n") {
                    ensure!(request.len() < 8192, "OAuth callback limit exceeded");
                    request.push(stream.read_u8().await?);
                }
                self.callback(std::str::from_utf8(&request).context("Invalid OAuth callback")?)
            })
            .await;
            let declined = matches!(parsed, Ok(Ok(None)));
            let (status, accepted) = match parsed {
                Ok(Ok(Some(value))) => ("200 OK", Some(value)),
                _ => ("400 Bad Request", None),
            };
            let body = "Return to mini-agent to finish.\n";
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())).await;
            if declined {
                anyhow::bail!("Sign-in declined; current credentials were not changed");
            }
            if let Some(value) = accepted {
                break value;
            }
        };
        crate::agent::runner::spawn_blocking_scoped(move || -> anyhow::Result<()> {
            let http = http()?;
            let response: TokenResponse = json_response(
                http.post(&endpoints.token)
                    .form(&[
                        ("grant_type", "authorization_code"),
                        ("client_id", client.as_str()),
                        ("code", code.as_str()),
                        ("code_verifier", self.verifier.as_str()),
                        ("redirect_uri", self.redirect.as_str()),
                        ("resource", RESOURCE),
                    ])
                    .send()
                    .map_err(|_| {
                        anyhow::anyhow!("Provider token exchange failed; start a fresh sign-in")
                    })?,
            )
            .map_err(|error| {
                if error.is::<ReauthorizationRequired>() {
                    CodeRetry {
                        root: self.root.clone(),
                        host: self.host.clone(),
                        revision: self.revision,
                        client: client.clone(),
                        subject: self.selected.as_ref().and_then(|a| a.1.clone()),
                    }
                    .into()
                } else {
                    error
                }
            })?;
            let keys = json_response(
                http.get(&endpoints.jwks)
                    .send()
                    .map_err(|_| anyhow::anyhow!("Provider signing keys unavailable"))?,
            )?;
            let (tokens, claims) = accept_tokens(
                response,
                &keys,
                &client,
                Some(&self.nonce),
                self.selected.as_ref().and_then(|a| a.1.as_deref()),
                now()?,
            )?;
            let mut store =
                LockedStore::open(self.root, true)?.context("Provider credentials unavailable")?;
            store.install_login((&self.host, self.revision), client, tokens, claims)
        })
        .await
        .context("Provider credential worker failed")?
    }
    fn callback(&self, request: &str) -> anyhow::Result<Option<(String, String)>> {
        let mut words = request
            .lines()
            .next()
            .context("Invalid callback")?
            .split_whitespace();
        ensure!(words.next() == Some("GET"), "Invalid callback method");
        let target = words.next().context("Missing callback target")?;
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
        ensure!(
            target.starts_with("/auth/callback?")
                && url.path() == "/auth/callback"
                && url.fragment().is_none(),
            "Invalid callback path"
        );
        let mut params = std::collections::HashMap::new();
        for (key, value) in url.query_pairs() {
            ensure!(
                params
                    .insert(key.into_owned(), value.into_owned())
                    .is_none(),
                "Duplicate OAuth callback parameter"
            );
        }
        ensure!(
            params.get("state") == Some(&self.state),
            "Invalid OAuth state"
        );
        if params.contains_key("error") {
            return Ok(None);
        }
        let client = match &self.selected {
            Some((id, _)) => {
                ensure!(
                    params.get("client_id").is_none_or(|value| value == id),
                    "OAuth client changed"
                );
                id.clone()
            }
            None => params
                .get("client_id")
                .filter(|id| bounded(id, 256) && id.as_str() != "dynamic_agent_client")
                .cloned()
                .context("Registration did not return an issued client ID")?,
        };
        let code = params
            .get("code")
            .filter(|v| bounded(v, 4096))
            .cloned()
            .context("Missing authorization code")?;
        Ok(Some((code, client)))
    }
    fn endpoints(&self) -> AuthEndpoints {
        #[cfg(test)]
        if let Some(endpoints) = &self.endpoints {
            return endpoints.clone();
        }
        AuthEndpoints::default()
    }
}

pub(crate) async fn logout() -> anyhow::Result<String> {
    crate::agent::runner::spawn_blocking_scoped(|| logout_at(root()?, REVOKE))
        .await
        .context("Provider credential worker failed")?
}
fn logout_at(root: PathBuf, revoke: &str) -> anyhow::Result<String> {
    let Some(mut store) = LockedStore::open(root, false)? else {
        return Ok("ChatGPT: not signed in".into());
    };
    let Some(id) = store.data.active.take() else {
        return Ok("ChatGPT: not signed in".into());
    };
    let account = store
        .data
        .accounts
        .iter_mut()
        .find(|a| a.client_id == id)
        .context("Provider registration unavailable")?;
    let token = account.tokens.take();
    let revoked = token.as_ref().is_some_and(|t| {
        http()
            .and_then(|http| {
                http.post(revoke)
                    .form(&[
                        ("token", t.refresh.as_str()),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", id.as_str()),
                    ])
                    .send()
                    .map_err(Into::into)
            })
            .is_ok_and(|r| r.status().as_u16() == 200)
    });
    store.save()?;
    Ok(if revoked { "Signed out of ChatGPT; renewable session revoked" } else { "Signed out locally; remote revocation was not confirmed. Disconnect mini-agent in ChatGPT Settings if needed" }.into())
}

pub(crate) async fn sign_in(provider: &str, account: Option<&str>) -> anyhow::Result<()> {
    ensure!(
        provider.eq_ignore_ascii_case("openai"),
        "{}",
        if provider.eq_ignore_ascii_case("anthropic") {
            ANTHROPIC_UNAVAILABLE
        } else {
            "usage: /login openai [new|account-number]"
        }
    );
    let account = account
        .map(|v| {
            if v == "new" {
                Ok(0)
            } else {
                v.parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .context("Use new or a positive ChatGPT account number")
            }
        })
        .transpose()?;
    let scope = crate::agent::runner::AgentWorkScope::new();
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let result = scope
        .run(async {
            let mut retry = None;
            for attempt in 0..2 {
                let login = if let Some(retry) = retry.take() {
                    Login::begin_with_retry(account, Some(retry)).await?
                } else { Login::begin(account).await? };
                println!(
                    "Continue with ChatGPT in your browser (Ctrl-C cancels while waiting).\n{}",
                    login.display_url()?
                );
                if crate::ui::renderer::open_url(login.url()).await.is_err() {
                    println!("Open the URL above in your system browser.");
                }
                match login.complete().await {
                    Err(error) if attempt == 0 && error.is::<CodeRetry>() => {
                        retry = Some(error.downcast::<CodeRetry>()?);
                        println!("Authorization code expired; reopening the same registration with a fresh code.");
                    }
                    result => return result,
                }
            }
            unreachable!("the final sign-in attempt returns")
        })
        .await;
    cleanup.settle().await;
    result
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
