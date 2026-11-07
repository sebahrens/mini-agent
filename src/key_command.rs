//! Command-backed, refreshable API credentials (`api_key_commands`).
//!
//! Provider keys normally come from `--api-key`, an environment variable or
//! the `api_keys` map and are baked into the HTTP client once. Short-lived
//! gateway tokens (one-hour JWTs, SSO-issued bearer tokens) outlive that
//! snapshot, so a provider can instead name a trusted command that prints the
//! key or token:
//!
//! ```toml
//! [api_key_commands.my-gateway]
//! command = ["vault-token", "--audience", "gateway"]
//! ttl_secs = 300      # re-run after this long; 0 = only after a 401
//! timeout_secs = 10   # the command is killed after this long
//! ```
//!
//! The command is an argv vector run directly (no shell; write
//! `["sh", "-c", "..."]` to opt in to one) from the user's own configuration,
//! with the user's environment, outside the model sandbox and never from an
//! untrusted project config. Its stdout (at most 16 KiB) must be one line, the
//! credential; stderr is discarded. The result is cached for the TTL, re-run
//! once when a request using it gets a `401` (the request is then resent
//! once), and kept out of logs and sessions: failures name the provider and
//! the command's program, never its output.
//!
//! The swap happens in the provider HTTP layer (`ProviderHttpClient`): a
//! request that carries a secret this module issued gets the current secret in
//! its place (in a header value, or in the URL's `key=` parameter for Gemini), so clients built earlier with an older token keep working without
//! a restart. Bedrock, Vertex and Copilot use their own credential
//! flows and ignore this table.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderValue};

/// Default seconds a fetched credential is reused before the command re-runs.
pub(crate) const DEFAULT_TTL_SECS: u64 = 300;
/// Default and maximum seconds a command may run.
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const MAX_TIMEOUT_SECS: u64 = 120;
/// Longest credential accepted from a command.
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_ARGS: usize = 32;
const MAX_ARG_BYTES: usize = 4096;
/// Earlier secrets kept so clients built with an older token still match.
const MAX_KNOWN_SECRETS: usize = 8;

/// One provider's credential command, from the `api_key_commands` table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyCommandSpec {
    /// Program and arguments, run without a shell.
    pub command: Vec<String>,
    /// Seconds a result is reused; `0` re-runs only after a `401`. Default 300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    /// Seconds before the command is killed (1-120). Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl ApiKeyCommandSpec {
    /// Structural validation that never echoes argument values.
    pub(crate) fn validate(&self, provider: &str) -> Result<(), String> {
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err(format!(
                "api_key_commands.{provider}.command must name a program"
            ));
        }
        if self.command.len() > MAX_ARGS
            || self
                .command
                .iter()
                .any(|argument| argument.len() > MAX_ARG_BYTES || argument.contains('\0'))
        {
            return Err(format!(
                "api_key_commands.{provider}.command allows at most {MAX_ARGS} arguments of {MAX_ARG_BYTES} bytes without NUL"
            ));
        }
        Ok(())
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_secs.unwrap_or(DEFAULT_TTL_SECS))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_secs
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        )
    }
}

#[derive(Default)]
struct State {
    current: Option<String>,
    /// Every secret issued recently, current included; a request carrying any
    /// of them is ours.
    known: Vec<String>,
    generation: u64,
    fetched_at: Option<Instant>,
}

/// One provider's cached, refreshable credential.
pub(crate) struct CommandCredential {
    provider: String,
    spec: ApiKeyCommandSpec,
    state: Mutex<State>,
    /// Serializes command runs (blocking and async alike).
    gate: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for CommandCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandCredential")
            .field("provider", &self.provider)
            .field("program", &self.program())
            .finish_non_exhaustive()
    }
}

impl CommandCredential {
    fn new(provider: &str, spec: ApiKeyCommandSpec) -> Arc<Self> {
        Arc::new(Self {
            provider: provider.to_owned(),
            spec,
            state: Mutex::new(State::default()),
            gate: tokio::sync::Mutex::new(()),
        })
    }

    /// The program name, for diagnostics (never arguments or output).
    pub(crate) fn program(&self) -> String {
        let program = &self.spec.command[0];
        let name = std::path::Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        name.chars().take(64).collect()
    }

    fn failure(&self, reason: &str) -> String {
        format!(
            "api_key_commands.{}: command `{}` {reason}",
            self.provider,
            self.program()
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn is_fresh(&self, state: &State) -> bool {
        match (&state.current, state.fetched_at) {
            (Some(_), Some(at)) => self.spec.ttl().is_zero() || at.elapsed() < self.spec.ttl(),
            _ => false,
        }
    }

    /// Records a command result. Returns whether it differs from the
    /// credential in use.
    fn store(&self, secret: String) -> bool {
        let mut state = self.lock();
        state.fetched_at = Some(Instant::now());
        if state.current.as_deref() == Some(secret.as_str()) {
            return false;
        }
        state.generation += 1;
        state.known.retain(|known| known != &secret);
        state.known.push(secret.clone());
        if state.known.len() > MAX_KNOWN_SECRETS {
            state.known.remove(0);
        }
        state.current = Some(secret);
        true
    }

    /// The credential for building a client: the cached one while fresh,
    /// otherwise a fresh run of the command. Blocks for at most the command's
    /// timeout; called while a provider client is constructed.
    pub(crate) fn resolve_blocking(self: &Arc<Self>) -> Result<String, String> {
        if let Some(secret) = {
            let state = self.lock();
            self.is_fresh(&state)
                .then(|| state.current.clone())
                .flatten()
        } {
            return Ok(secret);
        }
        let secret = run_blocking(self)?;
        self.store(secret.clone());
        Ok(secret)
    }

    async fn refresh(&self, force: bool) -> Result<bool, String> {
        let _gate = self.gate.lock().await;
        if !force && self.is_fresh(&self.lock()) {
            return Ok(false);
        }
        let secret = run_async(self).await?;
        Ok(self.store(secret))
    }
}

impl CommandCredential {
    /// Rewrites a secret this credential issued in `headers` to the current
    /// one, renewing it first when its TTL ran out. `Some(generation)` when
    /// the request carries one of this credential's secrets.
    async fn apply(&self, headers: &mut HeaderMap, uri: &mut http::Uri) -> Option<u64> {
        {
            let owns = |headers: &HeaderMap, state: &State| {
                headers.values().any(|value| {
                    value.to_str().is_ok_and(|text| {
                        let secret = text.strip_prefix("Bearer ").unwrap_or(text);
                        state.known.iter().any(|known| known == secret)
                    })
                })
            };
            let owns_query = |uri: &http::Uri, state: &State| {
                query_key_candidates(uri)
                    .iter()
                    .any(|value| state.known.iter().any(|known| known == value))
            };
            if !owns(headers, &self.lock()) && !owns_query(uri, &self.lock()) {
                return None;
            }
            let expired = !self.is_fresh(&self.lock());
            if expired && let Err(error) = self.refresh(false).await {
                tracing::warn!("{error}");
            }
            let (current, generation) = {
                let state = self.lock();
                (state.current.clone()?, state.generation)
            };
            let stale_query = {
                let state = self.lock();
                let candidates = query_key_candidates(uri);
                !candidates.is_empty()
                    && !candidates.contains(&current)
                    && candidates
                        .iter()
                        .any(|value| state.known.iter().any(|known| known == value))
            };
            if stale_query && let Some(rewritten) = with_query_key(uri, &current) {
                *uri = rewritten;
            }
            for value in headers.values_mut() {
                let Ok(text) = value.to_str() else { continue };
                let (prefix, secret) = match text.strip_prefix("Bearer ") {
                    Some(secret) => ("Bearer ", secret),
                    None => ("", text),
                };
                if secret == current || !self.lock().known.iter().any(|known| known == secret) {
                    continue;
                }
                if let Ok(mut replacement) = HeaderValue::from_str(&format!("{prefix}{current}")) {
                    replacement.set_sensitive(true);
                    *value = replacement;
                }
            }
            Some(generation)
        }
    }

    /// A request sent with `generation` was rejected with `401`: renew once.
    /// Returns whether a different secret is now available, so resending is
    /// worthwhile.
    async fn unauthorized(&self, generation: u64) -> bool {
        {
            // Another request already refreshed past the rejected secret.
            if self.lock().generation > generation {
                return true;
            }
            match self.refresh(true).await {
                Ok(changed) => changed,
                Err(error) => {
                    tracing::warn!("{error}");
                    false
                }
            }
        }
    }
}

/// The value of the `key=` query parameter (Gemini passes its API key in the
/// URL, not a header), if the URI has one.
fn query_key(uri: &http::Uri) -> Option<&str> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("key="))
}

/// The `key=` value as written and, when it differs, percent-decoded. A key
/// the provider SDK inserted raw matches a known secret as written; one this
/// module inserted percent-encoded matches after decoding.
fn query_key_candidates(uri: &http::Uri) -> Vec<String> {
    let Some(raw) = query_key(uri) else {
        return Vec::new();
    };
    let mut out = vec![raw.to_owned()];
    if let Some(decoded) = percent_decode(raw)
        && decoded != raw
    {
        out.push(decoded);
    }
    out
}

/// Percent-decodes `text` (`+` is left alone). `None` for malformed escapes or
/// a non-UTF-8 result.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encodes everything but RFC 3986 unreserved characters, so a secret
/// containing `+`, `&`, `#`, `%`, `=` or a space stays one query value.
fn percent_encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `uri` with its `key=` query parameter replaced by `secret`, percent-encoded
/// so reserved characters cannot split or truncate the query. `None` when the
/// result is not a valid URI.
fn with_query_key(uri: &http::Uri, secret: &str) -> Option<http::Uri> {
    let query = uri.query()?;
    let rebuilt: Vec<String> = query
        .split('&')
        .map(|pair| match pair.strip_prefix("key=") {
            Some(_) => format!("key={}", percent_encode_component(secret)),
            None => pair.to_owned(),
        })
        .collect();
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(
        format!("{}?{}", uri.path(), rebuilt.join("&"))
            .parse()
            .ok()?,
    );
    http::Uri::from_parts(parts).ok()
}

/// Whether `output` is acceptable as a credential: one nonempty line without
/// control characters. The text is returned trimmed.
fn credential_from_output(output: &[u8]) -> Result<String, &'static str> {
    if output.len() > MAX_OUTPUT_BYTES {
        return Err("produced more than 16 KiB of output");
    }
    let text = std::str::from_utf8(output).map_err(|_| "produced output that is not UTF-8")?;
    let text = text.trim();
    if text.is_empty() {
        return Err("produced no output");
    }
    if text.chars().any(char::is_control) {
        return Err("produced output that is not a single-line credential");
    }
    Ok(text.to_owned())
}

fn exit_reason(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with status {code}"),
        None => "was terminated by a signal".to_owned(),
    }
}

/// Kills the credential command's whole process group when dropped (or on
/// demand), so `sh -c` wrappers cannot leave a grandchild running or holding
/// the stdout pipe open past the deadline. The command is spawned as the
/// leader of its own group; on non-Unix targets only the direct child is
/// killed by the callers.
struct GroupKill {
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl GroupKill {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self {
                pgid: pid
                    .and_then(|pid| i32::try_from(pid).ok())
                    .filter(|pid| *pid > 1),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            Self {}
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}

fn run_blocking(credential: &CommandCredential) -> Result<String, String> {
    use crate::process_creation::StdCommandCreationExt as _;
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let spec = &credential.spec;
    let mut command = Command::new(&spec.command[0]);
    command
        .args(&spec.command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn_guarded()
        .map_err(|error| credential.failure(&format!("could not start ({:?})", error.kind())))?;
    let group = GroupKill::new(Some(child.id()));
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    // Never joined: a descendant that escaped the group and still holds the
    // pipe only keeps this detached thread alive, not the caller.
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = (&mut stdout)
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut output);
        let _ = sender.send(output);
    });
    let deadline = Instant::now() + spec.timeout();
    let timed_out =
        || credential.failure(&format!("timed out after {} s", spec.timeout().as_secs()));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                group.kill();
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out());
            }
            Err(_) => {
                group.kill();
                return Err(credential.failure("could not be waited on"));
            }
        }
    };
    // The helper exited: end anything it left behind so the pipe closes.
    group.kill();
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Ok(output) = receiver.recv_timeout(remaining.max(Duration::from_millis(200))) else {
        return Err(timed_out());
    };
    if !status.success() {
        return Err(credential.failure(&exit_reason(status)));
    }
    credential_from_output(&output).map_err(|reason| credential.failure(reason))
}

async fn run_async(credential: &CommandCredential) -> Result<String, String> {
    use crate::process_creation::TokioCommandCreationExt as _;
    use tokio::io::AsyncReadExt as _;
    let spec = &credential.spec;
    let mut command = tokio::process::Command::new(&spec.command[0]);
    command
        .args(&spec.command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn_guarded()
        .map_err(|error| credential.failure(&format!("could not start ({:?})", error.kind())))?;
    // Dropping this (timeout, cancellation, or normal return) kills the group.
    let group = GroupKill::new(child.id());
    let mut stdout = child.stdout.take().expect("piped stdout");
    let run = async {
        let mut output = Vec::new();
        let status = {
            let mut limited = (&mut stdout).take(MAX_OUTPUT_BYTES as u64 + 1);
            let mut read = Box::pin(limited.read_to_end(&mut output));
            let status = tokio::select! {
                read_result = &mut read => {
                    read_result.map_err(|_| credential.failure("output could not be read"))?;
                    child.wait().await
                }
                status = child.wait() => {
                    // The helper exited: end what it left behind so the pipe
                    // closes instead of stalling on a backgrounded daemon.
                    group.kill();
                    read.await
                        .map_err(|_| credential.failure("output could not be read"))?;
                    status
                }
            };
            status.map_err(|_| credential.failure("could not be waited on"))?
        };
        Ok::<_, String>((output, status))
    };
    let result = tokio::time::timeout(spec.timeout(), run).await;
    group.kill();
    let (output, status) = result.map_err(|_| {
        credential.failure(&format!("timed out after {} s", spec.timeout().as_secs()))
    })??;
    if !status.success() {
        return Err(credential.failure(&exit_reason(status)));
    }
    credential_from_output(&output).map_err(|reason| credential.failure(reason))
}

static REGISTRY: RwLock<Option<HashMap<String, Arc<CommandCredential>>>> = RwLock::new(None);

/// Installs the configured `api_key_commands`. An unchanged entry keeps its
/// cached credential. An invalid entry is an error naming it (never its
/// arguments).
pub(crate) fn configure(specs: Option<&HashMap<String, ApiKeyCommandSpec>>) -> Result<(), String> {
    let mut next = HashMap::new();
    let mut registry = REGISTRY.write().unwrap_or_else(|error| error.into_inner());
    for (provider, spec) in specs.into_iter().flatten() {
        spec.validate(provider)?;
        let key = provider.to_ascii_lowercase();
        let reused = registry
            .as_ref()
            .and_then(|current| current.get(&key))
            .filter(|existing| existing.spec == *spec)
            .cloned();
        next.insert(
            key,
            reused.unwrap_or_else(|| CommandCredential::new(provider, spec.clone())),
        );
    }
    *registry = Some(next);
    Ok(())
}

/// The credential command configured for `provider`, if any.
pub(crate) fn for_provider(provider: &str) -> Option<Arc<CommandCredential>> {
    REGISTRY
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()?
        .get(&provider.to_ascii_lowercase())
        .cloned()
}

/// A credential layer that rewrote a request, kept so a `401` can renew it.
pub(crate) struct Applied {
    credential: Arc<CommandCredential>,
    generation: u64,
}

/// Swaps every configured command credential's issued secrets in `headers`
/// (and the `key=` query parameter, which Gemini uses) for the current ones. Empty (and free) when no command is configured.
pub(crate) async fn apply_all(headers: &mut HeaderMap, uri: &mut http::Uri) -> Vec<Applied> {
    let credentials: Vec<Arc<CommandCredential>> = REGISTRY
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|registry| registry.values().cloned().collect())
        .unwrap_or_default();
    let mut applied = Vec::new();
    for credential in credentials {
        if let Some(generation) = credential.apply(headers, uri).await {
            applied.push(Applied {
                credential,
                generation,
            });
        }
    }
    applied
}

/// A request that carried `applied` secrets got `401`: renew each, once.
/// Returns whether any now holds a different secret.
pub(crate) async fn renew_after_unauthorized(applied: &[Applied]) -> bool {
    let mut renewed = false;
    for entry in applied {
        renewed |= entry.credential.unauthorized(entry.generation).await;
    }
    renewed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn spec(script: &str) -> ApiKeyCommandSpec {
        ApiKeyCommandSpec {
            command: vec!["sh".into(), "-c".into(), script.into()],
            ttl_secs: Some(0),
            timeout_secs: Some(5),
        }
    }

    #[test]
    fn spec_validation_is_structural_and_never_echoes_values() {
        assert!(spec("true").validate("p").is_ok());
        let empty = ApiKeyCommandSpec {
            command: vec![],
            ttl_secs: None,
            timeout_secs: None,
        };
        assert!(
            empty
                .validate("gw")
                .unwrap_err()
                .contains("api_key_commands.gw")
        );
        let blank = ApiKeyCommandSpec {
            command: vec!["  ".into()],
            ..empty.clone()
        };
        assert!(blank.validate("gw").is_err());
        let many = ApiKeyCommandSpec {
            command: vec!["x".into(); MAX_ARGS + 1],
            ..empty.clone()
        };
        assert!(many.validate("gw").is_err());
        let secretive = ApiKeyCommandSpec {
            command: vec!["x".into(), "SECRET_ARG\0".into()],
            ..empty
        };
        assert!(!secretive.validate("gw").unwrap_err().contains("SECRET_ARG"));
    }

    #[test]
    fn output_must_be_one_clean_line_within_the_bound() {
        assert_eq!(credential_from_output(b"  tok-1\n").unwrap(), "tok-1");
        assert!(credential_from_output(b"").is_err());
        assert!(credential_from_output(b" \n").is_err());
        assert!(credential_from_output(b"a\nb").is_err());
        assert!(credential_from_output(b"a\x1b[31m").is_err());
        assert!(credential_from_output(&[0xff, 0xfe]).is_err());
        assert!(credential_from_output(&vec![b'a'; MAX_OUTPUT_BYTES + 1]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_failures_name_the_command_and_never_echo_its_output() {
        let credential = CommandCredential::new("gw", spec("echo LEAKED_SECRET; exit 3"));
        let error = credential.resolve_blocking().unwrap_err();
        assert!(
            error.contains("api_key_commands.gw") && error.contains("`sh`"),
            "{error}"
        );
        assert!(error.contains("status 3"), "{error}");
        assert!(!error.contains("LEAKED_SECRET"), "{error}");
        let missing = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec![
                    "/nonexistent/mini-agent-key-helper".into(),
                    "SECRET_ARG".into(),
                ],
                ttl_secs: None,
                timeout_secs: Some(2),
            },
        );
        let error = missing.resolve_blocking().unwrap_err();
        assert!(
            error.contains("could not start") && !error.contains("SECRET_ARG"),
            "{error}"
        );
        let noisy = CommandCredential::new("gw", spec("printf 'two\\nlines'"));
        assert!(
            noisy
                .resolve_blocking()
                .unwrap_err()
                .contains("single-line")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_command_is_killed_at_the_timeout() {
        let credential = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
                ttl_secs: None,
                timeout_secs: Some(1),
            },
        );
        let started = Instant::now();
        let error = credential.resolve_blocking().unwrap_err();
        assert!(error.contains("timed out after 1 s"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn results_are_cached_for_the_ttl() {
        let dir = crate::tests::OwnedTestDirectory::new("key-command-ttl");
        let counter = dir.path().join("runs");
        let script = format!(
            "echo x >> '{0}'; echo tok-$(wc -l < '{0}' | tr -d ' ')",
            counter.display()
        );
        let credential = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec!["sh".into(), "-c".into(), script],
                ttl_secs: Some(3600),
                timeout_secs: Some(5),
            },
        );
        assert_eq!(credential.resolve_blocking().unwrap(), "tok-1");
        assert_eq!(credential.resolve_blocking().unwrap(), "tok-1", "cached");
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1
        );
    }

    /// The registry is process-global: tests that install entries take turns.
    static REGISTRY_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn configure_rejects_invalid_entries_and_looks_providers_up_case_insensitively() {
        let _turn = REGISTRY_TESTS.blocking_lock();
        let mut specs = HashMap::new();
        specs.insert("Gateway-X".to_owned(), spec("echo t"));
        configure(Some(&specs)).unwrap();
        assert!(for_provider("gateway-x").is_some());
        assert!(for_provider("other").is_none());
        specs.insert(
            "bad".to_owned(),
            ApiKeyCommandSpec {
                command: vec![],
                ttl_secs: None,
                timeout_secs: None,
            },
        );
        assert!(configure(Some(&specs)).is_err());
        configure(None).unwrap();
        assert!(for_provider("gateway-x").is_none());
    }

    // Default closed: without an `api_key_commands` entry nothing changes and
    // nothing runs. With one, the command outranks an ambient environment
    // variable but never an explicit `--api-key`.
    #[cfg(unix)]
    #[test]
    fn credential_commands_are_closed_by_default_and_rank_below_the_cli_key() {
        use crate::auth::{AuthResolver, CredentialSource, ProviderKind};
        let _turn = REGISTRY_TESTS.blocking_lock();
        let resolver = || {
            AuthResolver::new(ProviderKind::OpenAI)
                .with_custom_provider_name(Some("prio-gw"))
                .with_env_override(Some("PRIO_GW_KEY"))
        };
        let env = |_: &str| Ok::<_, std::env::VarError>("ENV_KEY".to_owned());
        configure(None).unwrap();
        let credential = resolver().resolve_credential_with_env(env).unwrap();
        assert_eq!(credential.secret(), "ENV_KEY");
        assert_eq!(
            resolver().credential_source_with_env(env).unwrap(),
            CredentialSource::Environment("PRIO_GW_KEY".into())
        );

        let mut specs = HashMap::new();
        specs.insert("prio-gw".to_owned(), spec("echo COMMAND_KEY"));
        configure(Some(&specs)).unwrap();
        let credential = resolver().resolve_credential_with_env(env).unwrap();
        assert_eq!(credential.secret(), "COMMAND_KEY");
        assert_eq!(
            resolver().credential_source_with_env(env).unwrap(),
            CredentialSource::Command("prio-gw".into())
        );
        // Read-only reporting names the source and does not run the command.
        let failing = {
            let mut specs = HashMap::new();
            specs.insert("prio-gw".to_owned(), spec("exit 9"));
            specs
        };
        configure(Some(&failing)).unwrap();
        assert_eq!(
            resolver().credential_source_with_env(env).unwrap(),
            CredentialSource::Command("prio-gw".into())
        );
        let error = resolver().resolve_credential_with_env(env).unwrap_err();
        assert!(
            error.to_string().contains("api_key_commands.prio-gw"),
            "{error}"
        );
        // An explicit --api-key still wins.
        let cli = resolver().with_cli_key(Some("CLI_KEY"));
        assert_eq!(
            cli.credential_source_with_env(env).unwrap(),
            CredentialSource::Cli
        );
        assert_eq!(
            cli.resolve_credential_with_env(env).unwrap().secret(),
            "CLI_KEY"
        );
        configure(None).unwrap();
    }

    fn sse_event(kind: &str, body: serde_json::Value) -> String {
        format!("event: {kind}\ndata: {body}\n\n")
    }

    /// An Anthropic-shaped loopback server that accepts one token only. Every
    /// request's `x-api-key` is recorded.
    async fn token_server(
        valid: &'static str,
        requests: usize,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        use crate::tests::support::loopback_http::asynchronous as http;
        let (listener, base) = http::bind().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            for _ in 0..requests {
                let (mut socket, request) = http::accept_request(&listener).await;
                let key = request
                    .headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("x-api-key")
                            .then(|| value.trim().to_owned())
                    })
                    .unwrap_or_default();
                recorded.lock().unwrap().push(key.clone());
                if key != valid {
                    http::respond(
                        &mut socket,
                        "401 Unauthorized",
                        "application/json",
                        r#"{"type":"error","error":{"type":"authentication_error","message":"token expired"}}"#,
                    )
                    .await;
                } else if request.json()["stream"] == true {
                    let body = sse_event(
                        "message_start",
                        serde_json::json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-opus-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
                    ) + &sse_event(
                        "content_block_start",
                        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    ) + &sse_event(
                        "content_block_delta",
                        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"streamed"}}),
                    ) + &sse_event(
                        "content_block_stop",
                        serde_json::json!({"type":"content_block_stop","index":0}),
                    ) + &sse_event(
                        "message_delta",
                        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
                    ) + &sse_event(
                        "message_stop",
                        serde_json::json!({"type":"message_stop"}),
                    );
                    http::respond_sse(&mut socket, &body).await;
                } else {
                    http::respond_json(
                        &mut socket,
                        r#"{"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-opus-5-5","stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":2}}"#,
                    )
                    .await;
                }
            }
        });
        (base, seen)
    }

    fn gateway_client(provider: &str, base: &str) -> crate::provider::AnyClient {
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "custom_providers":{provider:{"provider_type":"anthropic","base_url":base}}
        }))
        .unwrap();
        crate::provider::create_client(provider, None, config.custom_providers_map(), None).unwrap()
    }

    #[cfg(unix)]
    fn counting_command(dir: &Path, prefix: &str, distinct: bool) -> ApiKeyCommandSpec {
        let counter = dir.join(format!("{prefix}-runs"));
        let value = if distinct {
            format!("{prefix}-$(wc -l < '{}' | tr -d ' ')", counter.display())
        } else {
            format!("{prefix}-1")
        };
        ApiKeyCommandSpec {
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("echo x >> '{}'; echo {value}", counter.display()),
            ],
            ttl_secs: Some(0),
            timeout_secs: Some(5),
        }
    }

    // mini-agent-toywy: a token that expired mid-session is refreshed after
    // the first 401 and the request succeeds without restarting the session;
    // the next request, from the same already-built client, already carries
    // the new token. Streaming and blocking requests both go through the layer.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_expired_token_is_refreshed_once_on_401_and_the_request_succeeds() {
        use futures::StreamExt as _;
        use rig::completion::CompletionModel as _;
        let _turn = REGISTRY_TESTS.lock().await;
        let dir = crate::tests::OwnedTestDirectory::new("key-command-refresh");
        let (base, seen) = token_server("rot-2", 3).await;
        let mut specs = HashMap::new();
        specs.insert(
            "refresh-gw".to_owned(),
            counting_command(dir.path(), "rot", true),
        );
        configure(Some(&specs)).unwrap();
        let client = gateway_client("refresh-gw", &base);
        let crate::provider::AnyClient::Anthropic(client) = client else {
            panic!("anthropic route")
        };
        let model =
            rig::providers::anthropic::completion::CompletionModel::new(client, "claude-opus-5-5");
        let request = || model.completion_request("hello").max_tokens(100).build();
        // First request: the initial token (rot-1) is rejected, the command
        // runs again (rot-2) and the same request is resent once.
        model
            .completion(request())
            .await
            .expect("refreshed request succeeds");
        assert_eq!(*seen.lock().unwrap(), ["rot-1", "rot-2"]);
        // Same client, streaming: the swap happens before sending.
        let mut stream = model.stream(request()).await.expect("stream starts");
        while let Some(part) = stream.next().await {
            part.unwrap();
        }
        assert_eq!(*seen.lock().unwrap(), ["rot-1", "rot-2", "rot-2"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("rot-runs"))
                .unwrap()
                .lines()
                .count(),
            2,
            "one initial run and exactly one refresh"
        );
        configure(None).unwrap();
    }

    // A refresh that yields the same rejected token is not retried forever:
    // the 401 surfaces after the single refresh.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unchanged_token_after_refresh_surfaces_the_401_without_a_resend() {
        use rig::completion::CompletionModel as _;
        let _turn = REGISTRY_TESTS.lock().await;
        let dir = crate::tests::OwnedTestDirectory::new("key-command-same");
        let (base, seen) = token_server("never", 3).await;
        let mut specs = HashMap::new();
        specs.insert(
            "same-gw".to_owned(),
            counting_command(dir.path(), "same", false),
        );
        configure(Some(&specs)).unwrap();
        let crate::provider::AnyClient::Anthropic(client) = gateway_client("same-gw", &base) else {
            panic!("anthropic route")
        };
        let model =
            rig::providers::anthropic::completion::CompletionModel::new(client, "claude-opus-5-5");
        let error = model
            .completion(model.completion_request("hello").max_tokens(100).build())
            .await
            .expect_err("still unauthorized");
        assert!(format!("{error:?}").contains("401"), "{error:?}");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no resend of an unchanged token"
        );
        assert!(
            !format!("{error:?}").contains("same-1"),
            "token never in errors"
        );
        configure(None).unwrap();
    }

    // The TTL re-runs the command before the next request, with no 401.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_expired_ttl_refreshes_before_the_request() {
        use rig::completion::CompletionModel as _;
        let _turn = REGISTRY_TESTS.lock().await;
        let dir = crate::tests::OwnedTestDirectory::new("key-command-ttl-live");
        let (base, seen) = token_server("ttl-2", 1).await;
        let mut spec = counting_command(dir.path(), "ttl", true);
        spec.ttl_secs = Some(1);
        let mut specs = HashMap::new();
        specs.insert("ttl-gw".to_owned(), spec);
        configure(Some(&specs)).unwrap();
        let crate::provider::AnyClient::Anthropic(client) = gateway_client("ttl-gw", &base) else {
            panic!("anthropic route")
        };
        let model =
            rig::providers::anthropic::completion::CompletionModel::new(client, "claude-opus-5-5");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        model
            .completion(model.completion_request("hello").max_tokens(100).build())
            .await
            .expect("expired token renewed before sending");
        assert_eq!(*seen.lock().unwrap(), ["ttl-2"]);
        configure(None).unwrap();
    }

    #[cfg(unix)]
    fn process_alive(marker: &str) -> bool {
        std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn blocking_timeout_returns_promptly_and_kills_grandchildren() {
        let marker = "sleep 30.4817";
        let credential = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec!["sh".into(), "-c".into(), format!("{marker}; echo tok")],
                ttl_secs: Some(0),
                timeout_secs: Some(1),
            },
        );
        let started = Instant::now();
        let error = run_blocking(&credential).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(!process_alive(marker), "grandchild survived the timeout");
    }

    #[cfg(unix)]
    #[test]
    fn blocking_success_does_not_wait_for_a_backgrounded_daemon() {
        let marker = "sleep 31.5291";
        let credential = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec!["sh".into(), "-c".into(), format!("{marker} & echo tok")],
                ttl_secs: Some(0),
                timeout_secs: Some(5),
            },
        );
        let started = Instant::now();
        assert_eq!(run_blocking(&credential).unwrap(), "tok");
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!process_alive(marker), "daemon survived the helper");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn async_timeout_kills_grandchildren_and_success_ignores_daemons() {
        let marker = "sleep 32.6113";
        let hung = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec!["sh".into(), "-c".into(), format!("{marker}; echo tok")],
                ttl_secs: Some(0),
                timeout_secs: Some(1),
            },
        );
        let started = Instant::now();
        assert!(run_async(&hung).await.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(4));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!process_alive(marker));
        let marker = "sleep 33.7127";
        let daemon = CommandCredential::new(
            "gw",
            ApiKeyCommandSpec {
                command: vec!["sh".into(), "-c".into(), format!("{marker} & echo tok")],
                ttl_secs: Some(0),
                timeout_secs: Some(5),
            },
        );
        assert_eq!(run_async(&daemon).await.unwrap(), "tok");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!process_alive(marker));
    }

    #[test]
    fn query_key_is_found_and_rewritten_without_touching_other_parameters() {
        let uri: http::Uri = "http://h/v1beta/models/m:streamGenerateContent?alt=sse&key=old-1&x=y"
            .parse()
            .unwrap();
        assert_eq!(query_key(&uri), Some("old-1"));
        let rewritten = with_query_key(&uri, "new-2").unwrap();
        assert_eq!(
            rewritten.to_string(),
            "http://h/v1beta/models/m:streamGenerateContent?alt=sse&key=new-2&x=y"
        );
        let plain: http::Uri = "http://h/p?monkey=1".parse().unwrap();
        assert_eq!(query_key(&plain), None);
        // Control characters are encoded, never smuggled into the request line.
        let hostile = with_query_key(&uri, "a b\r\nHost: evil").unwrap();
        assert!(!hostile.to_string().contains(['\r', '\n', ' ']));
    }

    // mini-agent-07f21: reserved characters in a command-issued key are
    // percent-encoded and cannot inject parameters or truncate the query.
    #[test]
    fn query_key_rewrite_percent_encodes_reserved_characters() {
        let uri: http::Uri = "http://h/p?alt=sse&key=old&x=y".parse().unwrap();
        for (secret, encoded) in [
            ("a+b", "a%2Bb"),
            ("a&b=c", "a%26b%3Dc"),
            ("a#b", "a%23b"),
            ("a%b", "a%25b"),
            ("a b", "a%20b"),
        ] {
            let rewritten = with_query_key(&uri, secret).unwrap();
            assert_eq!(
                rewritten.to_string(),
                format!("http://h/p?alt=sse&key={encoded}&x=y"),
            );
            // Exactly the original parameters survive; nothing was injected.
            assert_eq!(rewritten.query().unwrap().split('&').count(), 3);
            assert_eq!(
                percent_decode(query_key(&rewritten).unwrap()).unwrap(),
                secret
            );
            assert!(query_key_candidates(&rewritten).iter().any(|c| c == secret));
        }
    }

    // mini-agent-dxbuy: Gemini carries its key in the URL; a stale key there is
    // swapped for the current one and a 401 renews it and resends once.
    #[cfg(unix)]
    #[tokio::test]
    async fn gemini_query_string_key_is_refreshed_and_renewed_on_401() {
        use crate::tests::support::loopback_http::asynchronous as http;
        use rig::completion::CompletionModel as _;
        let _turn = REGISTRY_TESTS.lock().await;
        let dir = crate::tests::OwnedTestDirectory::new("key-command-gemini");
        let (listener, base) = http::bind().await;
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = seen.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, request) = http::accept_request(&listener).await;
                let line = request.head.lines().next().unwrap_or_default().to_owned();
                let key = line
                    .split(['?', '&', ' '])
                    .find_map(|part| part.strip_prefix("key=").map(str::to_owned))
                    .unwrap_or_default();
                recorded.lock().unwrap().push(key.clone());
                if key != "gem-2" {
                    http::respond(
                        &mut socket,
                        "401 Unauthorized",
                        "application/json",
                        r#"{"error":{"code":401,"message":"expired","status":"UNAUTHENTICATED"}}"#,
                    )
                    .await;
                } else {
                    http::respond_json(&mut socket,
                        r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#).await;
                }
            }
        });
        let mut specs = HashMap::new();
        specs.insert(
            "gem-gw".to_owned(),
            counting_command(dir.path(), "gem", true),
        );
        configure(Some(&specs)).unwrap();
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "custom_providers":{"gem-gw":{"provider_type":"gemini","base_url":base}}
        }))
        .unwrap();
        let crate::provider::AnyClient::Gemini(client) =
            crate::provider::create_client("gem-gw", None, config.custom_providers_map(), None)
                .unwrap()
        else {
            panic!("gemini route")
        };
        let model = rig::providers::gemini::completion::CompletionModel::new(client, "gemini-test");
        let request = || model.completion_request("hello").build();
        model
            .completion(request())
            .await
            .expect("renewed request succeeds");
        assert_eq!(*seen.lock().unwrap(), ["gem-1", "gem-2"]);
        model
            .completion(request())
            .await
            .expect("next request carries the new key");
        assert_eq!(*seen.lock().unwrap(), ["gem-1", "gem-2", "gem-2"]);
        configure(None).unwrap();
    }
}
