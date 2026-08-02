#[cfg(test)]
use std::future::Future;
#[cfg(target_os = "linux")]
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(all(feature = "sandbox", test))]
use rquickjs::prelude::Opt;
#[cfg(test)]
use rquickjs::{Context, Ctx, IntoJs, Object, Value, prelude::Func};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use unicode_normalization::UnicodeNormalization;

#[cfg(feature = "sandbox")]
use reqwest::Url;
#[cfg(feature = "sandbox")]
use std::io::Read;
#[cfg(feature = "sandbox")]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(feature = "sandbox")]
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
#[cfg(feature = "sandbox")]
use std::time::Instant;

use crate::extras::js::broker::{
    AuthorizedEffect, AuthorizedTarget, GrantPrincipal, HostEffectError, NormalizedTarget,
    ParentEffectFuture, ParentEffectService, SpawnExecutableIdentity, resolve_program_identity,
};
#[cfg(target_os = "linux")]
use crate::extras::js::broker::{ExecutableCopyError, copy_and_hash_executable};
use crate::extras::js::protocol::{EffectOperation, EffectResult};
#[cfg(all(feature = "skills", test))]
use crate::extras::js::skills::proposal::{
    JsProposal, ProposalEffectService, ProposalError, ProposalHost,
};
use crate::extras::js::tool::{PermissionBridge, PermissionBridgeError};
use crate::extras::js::types::PermCancellation;
#[cfg(test)]
use crate::extras::js::types::STEP_TIMEOUT;
use crate::extras::js::types::{
    EffectServiceError, READ_FILE_MAX_BYTES, SpawnResult, WRITE_FILE_MAX_BYTES,
    canonical_spawn_permission_subject, spawn_policy_input,
};
#[cfg(any(target_os = "linux", not(unix)))]
use crate::sandbox::SandboxCommand;
use crate::sandbox::{CommandLimits, CommandOutputLimit, CommandStatus, Sandbox, SandboxPolicy};

#[cfg(all(feature = "skills", test))]
#[derive(Clone, Default)]
pub(crate) struct SkillCapabilityGate {
    stack: std::sync::Arc<std::sync::Mutex<Vec<crate::extras::js::skills::CapabilityManifest>>>,
    registered: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, crate::extras::js::skills::CapabilityManifest>,
        >,
    >,
    context: crate::extras::js::skills::capability::CapabilityContext,
}

#[cfg(all(feature = "skills", test))]
impl SkillCapabilityGate {
    pub(crate) fn register(
        &self,
        id: String,
        manifest: crate::extras::js::skills::CapabilityManifest,
    ) {
        self.registered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, manifest);
    }

    pub(crate) fn push_registered(&self, id: &str) -> rquickjs::Result<()> {
        let manifest = self
            .registered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                rquickjs::Error::new_from_js_message(
                    "skill capability",
                    "selected skill",
                    "unknown selected skill identity",
                )
            })?;
        self.stack
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(manifest);
        Ok(())
    }

    pub(crate) fn pop_registered(&self) {
        let _ = self
            .stack
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop();
    }

    pub(crate) fn enter(
        &self,
        manifest: crate::extras::js::skills::CapabilityManifest,
    ) -> SkillCapabilityGuard {
        self.stack
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(manifest);
        SkillCapabilityGuard { gate: self.clone() }
    }

    fn authorize(
        &self,
        capability: crate::extras::js::skills::HostCapability,
    ) -> rquickjs::Result<()> {
        let stack = self.stack.lock().unwrap_or_else(|error| error.into_inner());
        if stack.iter().any(|manifest| !manifest.allows(capability)) {
            return Err(rquickjs::Error::new_from_js_message(
                "skill capability",
                capability.as_token(),
                "selected skill did not declare this host capability",
            ));
        }
        self.context.authorize(capability, true).map_err(|error| {
            rquickjs::Error::new_from_js_message(
                "skill capability policy",
                capability.as_token(),
                error.to_string(),
            )
        })
    }
}

#[cfg(all(feature = "skills", test))]
pub(crate) struct SkillCapabilityGuard {
    gate: SkillCapabilityGate,
}

#[cfg(all(feature = "skills", test))]
impl Drop for SkillCapabilityGuard {
    fn drop(&mut self) {
        let _ = self
            .gate
            .stack
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileAccess {
    Read,
    Write,
}

impl FileAccess {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllowPolicyReason {
    NoConfiguredRoots(FileAccess),
    InvalidConfiguration(FileAccess),
    InvalidTarget(FileAccess),
    AmbiguousSymlink(FileAccess),
    OutsideConfiguredRoots(FileAccess),
}

impl std::fmt::Display for AllowPolicyReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (access, reason) = match self {
            Self::NoConfiguredRoots(access) => (
                access,
                "no roots are configured; unrestricted access requires an explicit opt-in",
            ),
            Self::InvalidConfiguration(access) => {
                (access, "the configured roots are invalid or ambiguous")
            }
            Self::InvalidTarget(access) => {
                (access, "the target path is invalid or cannot be resolved")
            }
            Self::AmbiguousSymlink(access) => (access, "the target is a final or dangling symlink"),
            Self::OutsideConfiguredRoots(access) => (
                access,
                "the resolved target is outside the configured roots",
            ),
        };
        write!(formatter, "JS file {} denied: {reason}", access.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthorizationDecision {
    Allowed(PathBuf),
    Denied(AllowPolicyReason),
}

#[derive(Debug, Clone)]
enum PathPolicy {
    Deny(AllowPolicyReason),
    Roots(Vec<PathBuf>),
    Unrestricted,
}

/// Canonical, component-aware narrowing policy for the JS file host globals.
///
/// Relative configured roots are resolved against `base`, which is captured
/// once at startup. The resulting policy never consults the process CWD.
#[derive(Debug, Clone)]
pub(crate) struct AllowConfig {
    base: PathBuf,
    read: PathPolicy,
    write: PathPolicy,
    #[cfg(feature = "sandbox")]
    fetch: FetchPolicy,
}

impl AllowConfig {
    #[cfg(feature = "sandbox")]
    pub(crate) fn fetch_policy(&self) -> FetchPolicy {
        self.fetch.clone()
    }

    pub(crate) fn from_settings(
        startup_base: &Path,
        configured_base: Option<&str>,
        read_roots: Option<&[String]>,
        write_roots: Option<&[String]>,
        read_unrestricted: bool,
        write_unrestricted: bool,
    ) -> Self {
        let base = resolve_policy_base(startup_base, configured_base);
        let Ok(base) = base else {
            return Self {
                base: startup_base.to_path_buf(),
                read: PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(FileAccess::Read)),
                write: PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(FileAccess::Write)),
                #[cfg(feature = "sandbox")]
                fetch: FetchPolicy::default(),
            };
        };

        Self {
            read: build_path_policy(&base, read_roots, read_unrestricted, FileAccess::Read),
            write: build_path_policy(&base, write_roots, write_unrestricted, FileAccess::Write),
            base,
            #[cfg(feature = "sandbox")]
            fetch: FetchPolicy::default(),
        }
    }

    pub(crate) fn with_fetch_settings(self, origins: Option<&[String]>, allow_http: bool) -> Self {
        #[cfg(feature = "sandbox")]
        {
            let mut configured = self;
            configured.fetch = FetchPolicy::from_settings(origins, allow_http);
            configured
        }
        #[cfg(not(feature = "sandbox"))]
        {
            let _ = (origins, allow_http);
            self
        }
    }

    #[cfg(test)]
    pub(crate) fn unrestricted(base: &Path) -> Self {
        Self::from_settings(base, None, None, None, true, true)
    }

    pub(crate) fn authorize_read(&self, target: &Path) -> AuthorizationDecision {
        let resolved = match resolve_policy_read_target(&self.base, target) {
            Ok(path) => path,
            Err(reason) => return AuthorizationDecision::Denied(reason),
        };
        authorize_resolved(&self.read, resolved, FileAccess::Read)
    }

    pub(crate) fn authorize_write(&self, target: &Path) -> AuthorizationDecision {
        let resolved = match resolve_policy_write_target(&self.base, target) {
            Ok(path) => path,
            Err(reason) => return AuthorizationDecision::Denied(reason),
        };
        authorize_resolved(&self.write, resolved, FileAccess::Write)
    }
}

fn path_has_ambiguous_spelling(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return true;
    };
    if path.is_empty() || path.contains('\0') {
        return true;
    }
    #[cfg(unix)]
    if path.contains('\\') {
        return true;
    }
    false
}

fn absolute_lexical_from(base: &Path, path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in source.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if normalized.is_absolute() {
        Ok(normalized)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path did not resolve to an absolute path",
        ))
    }
}

fn resolve_policy_base(
    startup_base: &Path,
    configured_base: Option<&str>,
) -> std::io::Result<PathBuf> {
    if path_has_ambiguous_spelling(startup_base) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid startup base",
        ));
    }
    let startup_base = std::fs::canonicalize(startup_base)?;
    if !startup_base.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "startup base is not a directory",
        ));
    }
    let configured = configured_base.map_or_else(|| Path::new("."), Path::new);
    if path_has_ambiguous_spelling(configured) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid configured base",
        ));
    }
    let base = absolute_lexical_from(&startup_base, configured)?;
    let base = std::fs::canonicalize(base)?;
    if !base.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "configured base is not a directory",
        ));
    }
    Ok(base)
}

fn build_path_policy(
    base: &Path,
    roots: Option<&[String]>,
    unrestricted: bool,
    access: FileAccess,
) -> PathPolicy {
    if unrestricted {
        return if roots.is_some() {
            PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(access))
        } else {
            PathPolicy::Unrestricted
        };
    }
    let Some(roots) = roots else {
        return PathPolicy::Deny(AllowPolicyReason::NoConfiguredRoots(access));
    };
    if roots.is_empty() {
        return PathPolicy::Deny(AllowPolicyReason::NoConfiguredRoots(access));
    }

    let mut canonical_roots = Vec::with_capacity(roots.len());
    for root in roots {
        let root = Path::new(root);
        if path_has_ambiguous_spelling(root) {
            return PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(access));
        }
        let Ok(root) = absolute_lexical_from(base, root)
            .and_then(std::fs::canonicalize)
            .and_then(|root| {
                if root.is_dir() {
                    Ok(root)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "configured root is not a directory",
                    ))
                }
            })
        else {
            return PathPolicy::Deny(AllowPolicyReason::InvalidConfiguration(access));
        };
        if !canonical_roots.contains(&root) {
            canonical_roots.push(root);
        }
    }
    PathPolicy::Roots(canonical_roots)
}

fn resolve_policy_read_target(base: &Path, target: &Path) -> Result<PathBuf, AllowPolicyReason> {
    if path_has_ambiguous_spelling(target) {
        return Err(AllowPolicyReason::InvalidTarget(FileAccess::Read));
    }
    let absolute = absolute_lexical_from(base, target)
        .map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Read))?;
    std::fs::canonicalize(absolute).map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Read))
}

fn resolve_policy_write_target(base: &Path, target: &Path) -> Result<PathBuf, AllowPolicyReason> {
    use std::path::Component;

    if path_has_ambiguous_spelling(target) {
        return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
    }
    let absolute = absolute_lexical_from(base, target)
        .map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Write))?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AllowPolicyReason::AmbiguousSymlink(FileAccess::Write));
            }
            std::fs::canonicalize(absolute)
                .map_err(|_| AllowPolicyReason::InvalidTarget(FileAccess::Write))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = absolute.as_path();
            let mut missing = Vec::new();
            let canonical_parent = loop {
                match std::fs::canonicalize(ancestor) {
                    Ok(canonical) => break canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let Some(name) = ancestor.file_name() else {
                            return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
                        };
                        missing.push(name.to_os_string());
                        let Some(parent) = ancestor.parent() else {
                            return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
                        };
                        ancestor = parent;
                    }
                    Err(_) => return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write)),
                }
            };
            if !canonical_parent.is_dir() {
                return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
            }
            let mut resolved = canonical_parent;
            for part in missing.into_iter().rev() {
                let part = Path::new(&part);
                if !part
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                {
                    return Err(AllowPolicyReason::InvalidTarget(FileAccess::Write));
                }
                resolved.push(part);
            }
            Ok(resolved)
        }
        Err(_) => Err(AllowPolicyReason::InvalidTarget(FileAccess::Write)),
    }
}

fn authorize_resolved(
    policy: &PathPolicy,
    target: PathBuf,
    access: FileAccess,
) -> AuthorizationDecision {
    if !target.is_absolute() || path_has_ambiguous_spelling(&target) {
        return AuthorizationDecision::Denied(AllowPolicyReason::InvalidTarget(access));
    }
    match policy {
        PathPolicy::Deny(reason) => AuthorizationDecision::Denied(*reason),
        PathPolicy::Unrestricted => AuthorizationDecision::Allowed(target),
        PathPolicy::Roots(roots) => {
            if roots.iter().any(|root| target.starts_with(root)) {
                AuthorizationDecision::Allowed(target)
            } else {
                AuthorizationDecision::Denied(AllowPolicyReason::OutsideConfiguredRoots(access))
            }
        }
    }
}

#[cfg(test)]
impl<'js> IntoJs<'js> for SpawnResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("stdout", self.stdout)?;
        obj.set("stderr", self.stderr)?;
        obj.set("code", self.code)?;
        obj.set("timed_out", self.timed_out)?;
        obj.set("stdout_truncated", self.stdout_truncated)?;
        obj.set("stderr_truncated", self.stderr_truncated)?;
        Ok(obj.into())
    }
}

#[cfg(feature = "sandbox")]
const FETCH_DNS_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "sandbox")]
const FETCH_MAX_REDIRECTS: usize = 5;
#[cfg(feature = "sandbox")]
const FETCH_MAX_DESTINATION_ADDRESSES: usize = 32;

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FetchOrigin {
    scheme: String,
    host: String,
    port: u16,
}

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone)]
enum FetchOriginPolicy {
    Unrestricted,
    Exact(Vec<FetchOrigin>),
    Deny,
}

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone)]
pub(crate) struct FetchPolicy {
    origins: FetchOriginPolicy,
    allow_http: bool,
}

#[cfg(feature = "sandbox")]
impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            origins: FetchOriginPolicy::Unrestricted,
            allow_http: false,
        }
    }
}

#[cfg(feature = "sandbox")]
impl FetchPolicy {
    pub(crate) fn from_settings(origins: Option<&[String]>, allow_http: bool) -> Self {
        let Some(origins) = origins else {
            return Self {
                allow_http,
                ..Self::default()
            };
        };
        if origins.is_empty() {
            return Self {
                origins: FetchOriginPolicy::Deny,
                allow_http,
            };
        }
        let parsed = origins
            .iter()
            .map(|origin| parse_configured_origin(origin, allow_http))
            .collect::<Result<Vec<_>, _>>();
        Self {
            origins: parsed
                .map(FetchOriginPolicy::Exact)
                .unwrap_or(FetchOriginPolicy::Deny),
            allow_http,
        }
    }

    fn authorize(&self, raw_url: &str) -> Result<Url, FetchError> {
        let url = normalize_fetch_url(raw_url, true)?;
        self.authorize_normalized(url)
    }

    fn authorize_normalized(&self, url: Url) -> Result<Url, FetchError> {
        if url.scheme() == "http" && !self.allow_http {
            return Err(FetchError::SchemeDenied);
        }
        let origin = fetch_origin(&url)?;
        match &self.origins {
            FetchOriginPolicy::Unrestricted => Ok(url),
            FetchOriginPolicy::Exact(origins) if origins.contains(&origin) => Ok(url),
            FetchOriginPolicy::Exact(_) | FetchOriginPolicy::Deny => Err(FetchError::OriginDenied),
        }
    }
}

#[cfg(feature = "sandbox")]
fn parse_configured_origin(raw: &str, allow_http: bool) -> Result<FetchOrigin, FetchError> {
    let url = normalize_fetch_url(raw, allow_http)?;
    if url.path() != "/" || url.query().is_some() {
        return Err(FetchError::InvalidOrigin);
    }
    fetch_origin(&url)
}

#[cfg(feature = "sandbox")]
fn normalize_fetch_url(raw: &str, allow_http: bool) -> Result<Url, FetchError> {
    let mut url = Url::parse(raw).map_err(|_| FetchError::InvalidUrl)?;
    match url.scheme() {
        "https" => {}
        "http" if allow_http => {}
        _ => return Err(FetchError::SchemeDenied),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::EmbeddedCredentials);
    }
    if url.fragment().is_some() {
        return Err(FetchError::FragmentDenied);
    }
    let host = url.host_str().ok_or(FetchError::MissingHost)?;
    if host.is_empty() || host.ends_with('.') {
        return Err(FetchError::InvalidHost);
    }
    let port = url.port_or_known_default().ok_or(FetchError::MissingPort)?;
    if url.port() == Some(port) && matches!((url.scheme(), port), ("https", 443) | ("http", 80)) {
        url.set_port(None).map_err(|_| FetchError::InvalidUrl)?;
    }
    Ok(url)
}

#[cfg(feature = "sandbox")]
fn fetch_origin(url: &Url) -> Result<FetchOrigin, FetchError> {
    Ok(FetchOrigin {
        scheme: url.scheme().to_string(),
        host: url
            .host_str()
            .ok_or(FetchError::MissingHost)?
            .to_ascii_lowercase(),
        port: url.port_or_known_default().ok_or(FetchError::MissingPort)?,
    })
}

#[cfg(feature = "sandbox")]
trait FetchResolver: Send + Sync {
    fn resolve(
        &self,
        host: &str,
        port: u16,
        permission_bridge: &PermissionBridge,
    ) -> Result<Vec<SocketAddr>, FetchError>;
}

#[cfg(feature = "sandbox")]
struct RuntimeFetchResolver {
    runtime: tokio::runtime::Handle,
}

#[cfg(feature = "sandbox")]
impl FetchResolver for RuntimeFetchResolver {
    fn resolve(
        &self,
        host: &str,
        port: u16,
        permission_bridge: &PermissionBridge,
    ) -> Result<Vec<SocketAddr>, FetchError> {
        if permission_bridge.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        if let Ok(address) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(address, port)]);
        }
        let bridge = permission_bridge.clone();
        self.runtime.block_on(async move {
            tokio::select! {
                result = tokio::time::timeout(
                    FETCH_DNS_TIMEOUT,
                    tokio::net::lookup_host((host, port)),
                ) => {
                    match result {
                        Ok(Ok(addresses)) => {
                            let addresses = addresses
                                .take(FETCH_MAX_DESTINATION_ADDRESSES + 1)
                                .collect::<Vec<_>>();
                            if addresses.is_empty() {
                                Err(FetchError::DnsResolutionFailed)
                            } else {
                                Ok(addresses)
                            }
                        }
                        Ok(Err(_)) => Err(FetchError::DnsResolutionFailed),
                        Err(_) => Err(FetchError::TimedOut),
                    }
                }
                _ = bridge.cancelled() => Err(FetchError::Cancelled),
            }
        })
    }
}

#[cfg(feature = "sandbox")]
trait FetchSender: Send + Sync {
    fn send(
        &self,
        url: Url,
        request: &FetchRequest,
        addresses: &[SocketAddr],
        permission_bridge: &PermissionBridge,
    ) -> Result<FetchTransportOutcome, FetchError>;
}

#[cfg(feature = "sandbox")]
struct BoundFetchSender;

#[cfg(feature = "sandbox")]
impl FetchSender for BoundFetchSender {
    fn send(
        &self,
        url: Url,
        request: &FetchRequest,
        addresses: &[SocketAddr],
        permission_bridge: &PermissionBridge,
    ) -> Result<FetchTransportOutcome, FetchError> {
        let host = url.host_str().ok_or(FetchError::MissingHost)?;
        let transport = FetchTransport::new(Some((host, addresses)))?;
        transport.execute(url, request, || permission_bridge.is_cancelled())
    }
}

#[cfg(feature = "sandbox")]
struct FetchExecutor {
    policy: FetchPolicy,
    resolver: Arc<dyn FetchResolver>,
    sender: Arc<dyn FetchSender>,
    permission_bridge: PermissionBridge,
}

#[cfg(feature = "sandbox")]
struct PreparedFetchTarget {
    url: Url,
    addresses: Vec<SocketAddr>,
}

#[cfg(feature = "sandbox")]
struct PreparedFetchEffect {
    target: PreparedFetchTarget,
    request: FetchRequest,
    control: Arc<FetchCallControl>,
    deadline: Instant,
    redirect_mode: FetchRedirectMode,
}

#[cfg(feature = "sandbox")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FetchRedirectMode {
    Follow,
    DenyUnaudited,
}

#[cfg(feature = "sandbox")]
const FETCH_CALL_ACTIVE: u8 = 0;
#[cfg(feature = "sandbox")]
const FETCH_CALL_CANCELLED: u8 = 1;
#[cfg(feature = "sandbox")]
const FETCH_CALL_DISPATCHED: u8 = 2;
#[cfg(feature = "sandbox")]
const FETCH_CALL_FINISHED_BEFORE_DISPATCH: u8 = 3;
#[cfg(feature = "sandbox")]
const FETCH_CALL_FINISHED_AFTER_DISPATCH: u8 = 4;

#[cfg(feature = "sandbox")]
struct FetchCallControl {
    cancellation: PermCancellation,
    phase: AtomicU8,
}

#[cfg(feature = "sandbox")]
impl FetchCallControl {
    fn new() -> Self {
        Self {
            cancellation: PermCancellation::new(),
            phase: AtomicU8::new(FETCH_CALL_ACTIVE),
        }
    }

    fn begin_dispatch(&self) -> bool {
        loop {
            match self.phase.load(Ordering::Acquire) {
                FETCH_CALL_ACTIVE => {
                    if self
                        .phase
                        .compare_exchange(
                            FETCH_CALL_ACTIVE,
                            FETCH_CALL_DISPATCHED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                FETCH_CALL_DISPATCHED => return !self.cancellation.is_cancelled(),
                FETCH_CALL_CANCELLED
                | FETCH_CALL_FINISHED_BEFORE_DISPATCH
                | FETCH_CALL_FINISHED_AFTER_DISPATCH => return false,
                _ => unreachable!("fetch call entered an unknown phase"),
            }
        }
    }

    fn cancel(&self, before_dispatch: FetchError) -> FetchError {
        self.cancellation.cancel();
        match self.phase.compare_exchange(
            FETCH_CALL_ACTIVE,
            FETCH_CALL_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(FETCH_CALL_CANCELLED | FETCH_CALL_FINISHED_BEFORE_DISPATCH) => {
                before_dispatch
            }
            Err(FETCH_CALL_DISPATCHED | FETCH_CALL_FINISHED_AFTER_DISPATCH) => {
                FetchError::OutcomeUnknown
            }
            Err(_) => unreachable!("fetch call entered an unknown phase"),
        }
    }

    fn finish(&self) {
        loop {
            let current = self.phase.load(Ordering::Acquire);
            let finished = match current {
                FETCH_CALL_ACTIVE => FETCH_CALL_FINISHED_BEFORE_DISPATCH,
                FETCH_CALL_DISPATCHED => FETCH_CALL_FINISHED_AFTER_DISPATCH,
                FETCH_CALL_CANCELLED
                | FETCH_CALL_FINISHED_BEFORE_DISPATCH
                | FETCH_CALL_FINISHED_AFTER_DISPATCH => return,
                _ => unreachable!("fetch call entered an unknown phase"),
            };
            if self
                .phase
                .compare_exchange(current, finished, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }
}

#[cfg(feature = "sandbox")]
struct FetchCallCompletion(Arc<FetchCallControl>);

#[cfg(feature = "sandbox")]
impl Drop for FetchCallCompletion {
    fn drop(&mut self) {
        self.0.finish();
    }
}

#[cfg(feature = "sandbox")]
struct CancelFetchPrepareOnDrop(Option<Arc<FetchCallControl>>);

#[cfg(feature = "sandbox")]
impl CancelFetchPrepareOnDrop {
    fn new(control: Arc<FetchCallControl>) -> Self {
        Self(Some(control))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(feature = "sandbox")]
impl Drop for CancelFetchPrepareOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            let _ = control.cancel(FetchError::Cancelled);
        }
    }
}

#[cfg(feature = "sandbox")]
impl FetchExecutor {
    #[cfg(test)]
    fn execute(&self, raw_url: &str, request: &FetchRequest) -> Result<FetchResult, FetchError> {
        self.execute_controlled(raw_url, request, Arc::new(FetchCallControl::new()))
    }

    fn execute_controlled(
        &self,
        raw_url: &str,
        request: &FetchRequest,
        control: Arc<FetchCallControl>,
    ) -> Result<FetchResult, FetchError> {
        let prepared = self.prepare(
            raw_url,
            request.clone(),
            control.clone(),
            Instant::now() + FETCH_TOTAL_TIMEOUT,
        )?;
        self.execute_prepared(prepared)
    }

    fn prepare(
        &self,
        raw_url: &str,
        request: FetchRequest,
        control: Arc<FetchCallControl>,
        deadline: Instant,
    ) -> Result<PreparedFetchEffect, FetchError> {
        let permission_bridge = self
            .permission_bridge
            .for_host_call(control.cancellation.clone());
        let target = self.prepare_target(raw_url, &permission_bridge)?;
        Ok(PreparedFetchEffect {
            target,
            request,
            control,
            deadline,
            redirect_mode: FetchRedirectMode::Follow,
        })
    }

    fn prepare_normalized(
        &self,
        url: Url,
        request: FetchRequest,
        control: Arc<FetchCallControl>,
        deadline: Instant,
    ) -> Result<PreparedFetchEffect, FetchError> {
        let permission_bridge = self
            .permission_bridge
            .for_host_call(control.cancellation.clone());
        let target = self.prepare_normalized_target(url, &permission_bridge)?;
        Ok(PreparedFetchEffect {
            target,
            request,
            control,
            deadline,
            redirect_mode: FetchRedirectMode::Follow,
        })
    }

    fn prepare_target(
        &self,
        raw_url: &str,
        permission_bridge: &PermissionBridge,
    ) -> Result<PreparedFetchTarget, FetchError> {
        let url = self.policy.authorize(raw_url)?;
        self.prepare_authorized_target(url, permission_bridge)
    }

    fn prepare_normalized_target(
        &self,
        url: Url,
        permission_bridge: &PermissionBridge,
    ) -> Result<PreparedFetchTarget, FetchError> {
        let url = self.policy.authorize_normalized(url)?;
        self.prepare_authorized_target(url, permission_bridge)
    }

    fn prepare_authorized_target(
        &self,
        url: Url,
        permission_bridge: &PermissionBridge,
    ) -> Result<PreparedFetchTarget, FetchError> {
        let host = url.host_str().ok_or(FetchError::MissingHost)?;
        let port = url.port_or_known_default().ok_or(FetchError::MissingPort)?;
        let mut addresses = self.resolver.resolve(host, port, permission_bridge)?;
        addresses.sort_unstable();
        addresses.dedup();
        validate_public_destinations(&addresses)?;
        let permission_key = fetch_permission_key(&url, &addresses);
        permission_bridge
            .check("js/fetch", &permission_key)
            .map_err(|error| FetchError::Permission(permission_service_error(error)))?;
        Ok(PreparedFetchTarget { url, addresses })
    }

    fn execute_prepared(
        &self,
        mut prepared: PreparedFetchEffect,
    ) -> Result<FetchResult, FetchError> {
        let control = prepared.control.clone();
        let _completion = FetchCallCompletion(control.clone());
        let permission_bridge = self
            .permission_bridge
            .for_host_call(control.cancellation.clone());
        for redirect_count in 0..=FETCH_MAX_REDIRECTS {
            let origin = fetch_origin(&prepared.target.url)?;
            if permission_bridge.is_cancelled() || !control.begin_dispatch() {
                return Err(FetchError::Cancelled);
            }
            match self.sender.send(
                prepared.target.url.clone(),
                &prepared.request,
                &prepared.target.addresses,
                &permission_bridge,
            )? {
                FetchTransportOutcome::Complete(result) => return Ok(result),
                FetchTransportOutcome::Redirect(redirect) => {
                    if prepared.redirect_mode == FetchRedirectMode::DenyUnaudited {
                        return Err(FetchError::UnauditedRedirectDenied);
                    }
                    if redirect_count == FETCH_MAX_REDIRECTS {
                        return Err(FetchError::TooManyRedirects);
                    }
                    let redirect = self.policy.authorize(redirect.as_str())?;
                    if prepared.request.method != reqwest::Method::GET {
                        return Err(FetchError::RedirectReplayDenied);
                    }
                    if !prepared.request.headers.is_empty() && origin != fetch_origin(&redirect)? {
                        return Err(FetchError::CrossOriginRedirectDenied);
                    }
                    prepared.target = self.prepare_target(redirect.as_str(), &permission_bridge)?;
                }
            }
        }
        Err(FetchError::TooManyRedirects)
    }
}

#[cfg(feature = "sandbox")]
fn fetch_permission_key(url: &Url, addresses: &[SocketAddr]) -> String {
    let destinations = addresses
        .iter()
        .map(SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("{} destinations=[{destinations}]", url.as_str())
}

#[cfg(feature = "sandbox")]
fn validate_public_destinations(addresses: &[SocketAddr]) -> Result<(), FetchError> {
    if addresses.len() > FETCH_MAX_DESTINATION_ADDRESSES {
        Err(FetchError::TooManyDestinations)
    } else if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        Err(FetchError::DestinationDenied)
    } else {
        Ok(())
    }
}

#[cfg(feature = "sandbox")]
fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

#[cfg(feature = "sandbox")]
fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    ![
        (0x0000_0000, 8),  // current network / unspecified
        (0x0a00_0000, 8),  // RFC1918
        (0x6440_0000, 10), // shared address space
        (0x7f00_0000, 8),  // loopback
        (0xa9fe_0000, 16), // link-local and metadata
        (0xac10_0000, 12), // RFC1918
        (0xc000_0000, 24), // IETF protocol assignments
        (0xc000_0200, 24), // TEST-NET-1
        (0xc058_6300, 24), // deprecated 6to4 relay anycast
        (0xc0a8_0000, 16), // RFC1918
        (0xc612_0000, 15), // benchmarking
        (0xc633_6400, 24), // TEST-NET-2
        (0xcb00_7100, 24), // TEST-NET-3
        (0xe000_0000, 4),  // multicast
        (0xf000_0000, 4),  // reserved and limited broadcast
    ]
    .into_iter()
    .any(|(network, prefix)| ipv4_in_prefix(value, network, prefix))
}

#[cfg(feature = "sandbox")]
fn ipv4_in_prefix(address: u32, network: u32, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
    address & mask == network & mask
}

#[cfg(feature = "sandbox")]
fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let value = u128::from(address);
    if !ipv6_in_prefix(value, 0x2000_u128 << 112, 3) {
        return false;
    }
    ![
        (0x2001_u128 << 112, 23),     // IETF special-use
        (0x2001_0db8_u128 << 96, 32), // documentation
        (0x2002_u128 << 112, 16),     // 6to4 transition addresses
        (0x3fff_u128 << 112, 20),     // documentation
    ]
    .into_iter()
    .any(|(network, prefix)| ipv6_in_prefix(value, network, prefix))
}

#[cfg(feature = "sandbox")]
fn ipv6_in_prefix(address: u128, network: u128, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0);
    address & mask == network & mask
}

#[cfg(feature = "sandbox")]
const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "sandbox")]
const FETCH_READ_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "sandbox")]
const FETCH_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(feature = "sandbox")]
const FETCH_CANCELLATION_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_HEADER_MAX_BYTES: usize = 16 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_HEADER_MAX_COUNT: usize = 64;
#[cfg(feature = "sandbox")]
const FETCH_REQUEST_BODY_MAX_BYTES: usize = 256 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_RESPONSE_HEADER_MAX_BYTES: usize = 64 * 1024;
#[cfg(feature = "sandbox")]
const FETCH_RESPONSE_HEADER_MAX_COUNT: usize = 128;
#[cfg(feature = "sandbox")]
const FETCH_RESPONSE_BODY_MAX_BYTES: usize = 1024 * 1024;

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchResult {
    pub status: u16,
    pub text: String,
}

#[cfg(feature = "sandbox")]
#[cfg(test)]
impl<'js> IntoJs<'js> for FetchResult {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("status", self.status)?;
        object.set("text", self.text)?;
        Ok(object.into())
    }
}

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone)]
pub(crate) struct FetchRequest {
    method: reqwest::Method,
    headers: reqwest::header::HeaderMap,
    body: Option<Vec<u8>>,
}

#[cfg(feature = "sandbox")]
impl FetchRequest {
    pub(crate) fn get() -> Self {
        Self {
            method: reqwest::Method::GET,
            headers: reqwest::header::HeaderMap::new(),
            body: None,
        }
    }

    #[allow(dead_code)] // Protocol integration starts using this in A15.
    pub(crate) fn try_new(
        method: &str,
        headers: &[(String, String)],
        body: Option<String>,
    ) -> Result<Self, FetchError> {
        let method = match method {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => {
                return Err(FetchError::InvalidOptions(
                    "method must be GET or POST".to_string(),
                ));
            }
        };
        if method == reqwest::Method::GET && body.is_some() {
            return Err(FetchError::InvalidOptions(
                "GET requests cannot have a body".to_string(),
            ));
        }
        let mut parsed_headers = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| FetchError::InvalidOptions("invalid header name".to_string()))?;
            if is_forbidden_fetch_header(&name) {
                return Err(FetchError::InvalidOptions(
                    "header is controlled by the host".to_string(),
                ));
            }
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| FetchError::InvalidOptions("invalid header value".to_string()))?;
            parsed_headers.append(name, value);
        }
        validate_header_limits(
            &parsed_headers,
            FETCH_REQUEST_HEADER_MAX_COUNT,
            FETCH_REQUEST_HEADER_MAX_BYTES,
            FetchError::RequestHeadersTooLarge,
        )?;
        let body = body.map(String::into_bytes);
        if body
            .as_ref()
            .is_some_and(|body| body.len() > FETCH_REQUEST_BODY_MAX_BYTES)
        {
            return Err(FetchError::RequestBodyTooLarge);
        }
        Ok(Self {
            method,
            headers: parsed_headers,
            body,
        })
    }

    #[cfg(test)]
    fn from_options(options: Option<&Object<'_>>) -> Result<Self, FetchError> {
        let Some(options) = options else {
            return Ok(Self::get());
        };
        for key in options.keys::<String>() {
            let key = key.map_err(|error| FetchError::InvalidOptions(error.to_string()))?;
            if !matches!(key.as_str(), "method" | "headers" | "body") {
                return Err(FetchError::InvalidOptions(format!(
                    "unsupported field '{key}'"
                )));
            }
        }

        let method = options
            .get::<_, Option<String>>("method")
            .map_err(|error| FetchError::InvalidOptions(error.to_string()))?
            .unwrap_or_else(|| "GET".to_string())
            .to_ascii_uppercase();
        let method = match method.as_str() {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => {
                return Err(FetchError::InvalidOptions(
                    "method must be GET or POST".to_string(),
                ));
            }
        };

        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(object) = options
            .get::<_, Option<Object<'_>>>("headers")
            .map_err(|error| FetchError::InvalidOptions(error.to_string()))?
        {
            for property in object.props::<String, String>() {
                let (name, value) =
                    property.map_err(|error| FetchError::InvalidOptions(error.to_string()))?;
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| FetchError::InvalidOptions("invalid header name".to_string()))?;
                if is_forbidden_fetch_header(&name) {
                    return Err(FetchError::InvalidOptions(format!(
                        "header '{}' is controlled by the host",
                        name.as_str()
                    )));
                }
                let value = reqwest::header::HeaderValue::from_str(&value)
                    .map_err(|_| FetchError::InvalidOptions("invalid header value".to_string()))?;
                headers.append(name, value);
            }
        }

        let body = options
            .get::<_, Option<String>>("body")
            .map_err(|error| FetchError::InvalidOptions(error.to_string()))?
            .map(String::into_bytes);
        if method == reqwest::Method::GET && body.is_some() {
            return Err(FetchError::InvalidOptions(
                "GET requests cannot have a body".to_string(),
            ));
        }
        validate_header_limits(
            &headers,
            FETCH_REQUEST_HEADER_MAX_COUNT,
            FETCH_REQUEST_HEADER_MAX_BYTES,
            FetchError::RequestHeadersTooLarge,
        )?;
        if body
            .as_ref()
            .is_some_and(|body| body.len() > FETCH_REQUEST_BODY_MAX_BYTES)
        {
            return Err(FetchError::RequestBodyTooLarge);
        }

        Ok(Self {
            method,
            headers,
            body,
        })
    }
}

#[cfg(feature = "sandbox")]
fn is_forbidden_fetch_header(name: &reqwest::header::HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-authorization"
            | "proxy-connection"
            | "authorization"
            | "cookie"
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-real-ip"
            | "via"
            | "upgrade"
            | "te"
            | "trailer"
            | "accept-encoding"
    ) || name.starts_with("proxy-")
        || name.starts_with("sec-")
}

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    InvalidUrl,
    SchemeDenied,
    EmbeddedCredentials,
    FragmentDenied,
    MissingHost,
    InvalidHost,
    MissingPort,
    InvalidOrigin,
    OriginDenied,
    DnsResolutionFailed,
    DestinationDenied,
    TooManyDestinations,
    Permission(EffectServiceError),
    TooManyRedirects,
    RedirectReplayDenied,
    CrossOriginRedirectDenied,
    UnauditedRedirectDenied,
    InvalidOptions(String),
    ClientBuild(String),
    Cancelled,
    TimedOut,
    OutcomeUnknown,
    RequestHeadersTooLarge,
    RequestBodyTooLarge,
    RequestFailed(String),
    ResponseHeadersTooLarge,
    ResponseBodyTooLarge,
    UnsupportedContentEncoding,
    InvalidRedirect,
    InvalidUtf8,
}

#[cfg(feature = "sandbox")]
impl std::fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl => formatter.write_str("fetch URL is invalid or ambiguous"),
            Self::SchemeDenied => formatter.write_str("fetch URL scheme is not allowed"),
            Self::EmbeddedCredentials => {
                formatter.write_str("fetch URL must not contain embedded credentials")
            }
            Self::FragmentDenied => formatter.write_str("fetch URL fragments are not supported"),
            Self::MissingHost => formatter.write_str("fetch URL has no host"),
            Self::InvalidHost => formatter.write_str("fetch URL host is invalid or ambiguous"),
            Self::MissingPort => formatter.write_str("fetch URL has no effective port"),
            Self::InvalidOrigin => {
                formatter.write_str("fetch origin configuration must contain only an origin")
            }
            Self::OriginDenied => {
                formatter.write_str("fetch URL origin is not allowed by configuration")
            }
            Self::DnsResolutionFailed => formatter.write_str("fetch DNS resolution failed"),
            Self::DestinationDenied => {
                formatter.write_str("fetch destination is not a public network address")
            }
            Self::TooManyDestinations => {
                formatter.write_str("fetch DNS answer exceeds the destination address limit")
            }
            Self::Permission(error) => write!(formatter, "fetch permission denied: {error}"),
            Self::TooManyRedirects => formatter.write_str("fetch redirect limit exceeded"),
            Self::RedirectReplayDenied => {
                formatter.write_str("fetch refuses to replay a non-GET request after a redirect")
            }
            Self::CrossOriginRedirectDenied => formatter.write_str(
                "fetch refuses to forward caller headers across an origin-changing redirect",
            ),
            Self::UnauditedRedirectDenied => {
                formatter.write_str("fetch redirect requires an independent durable audit record")
            }
            Self::InvalidOptions(message) => write!(formatter, "invalid fetch options: {message}"),
            Self::ClientBuild(message) => write!(formatter, "fetch client setup failed: {message}"),
            Self::Cancelled => formatter.write_str("fetch cancelled"),
            Self::TimedOut => formatter.write_str("fetch timed out"),
            Self::OutcomeUnknown => {
                formatter.write_str("fetch outcome is unknown after request dispatch")
            }
            Self::RequestHeadersTooLarge => {
                formatter.write_str("fetch request headers exceed the configured limit")
            }
            Self::RequestBodyTooLarge => {
                formatter.write_str("fetch request body exceeds the configured limit")
            }
            Self::RequestFailed(message) => write!(formatter, "fetch request failed: {message}"),
            Self::ResponseHeadersTooLarge => {
                formatter.write_str("fetch response headers exceed the configured limit")
            }
            Self::ResponseBodyTooLarge => {
                formatter.write_str("fetch response body exceeds the configured limit")
            }
            Self::UnsupportedContentEncoding => {
                formatter.write_str("fetch response content encoding is not supported")
            }
            Self::InvalidRedirect => formatter.write_str("fetch response has an invalid redirect"),
            Self::InvalidUtf8 => formatter.write_str("fetch response body is not valid UTF-8"),
        }
    }
}

#[cfg(feature = "sandbox")]
impl std::error::Error for FetchError {}

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchTransportOutcome {
    Complete(FetchResult),
    Redirect(Url),
}

#[cfg(feature = "sandbox")]
#[derive(Debug, Clone, Copy)]
struct FetchLimits {
    connect_timeout: Duration,
    read_timeout: Duration,
    total_timeout: Duration,
    request_header_max_bytes: usize,
    request_header_max_count: usize,
    request_body_max_bytes: usize,
    response_header_max_bytes: usize,
    response_header_max_count: usize,
    response_body_max_bytes: usize,
}

#[cfg(feature = "sandbox")]
impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            connect_timeout: FETCH_CONNECT_TIMEOUT,
            read_timeout: FETCH_READ_TIMEOUT,
            total_timeout: FETCH_TOTAL_TIMEOUT,
            request_header_max_bytes: FETCH_REQUEST_HEADER_MAX_BYTES,
            request_header_max_count: FETCH_REQUEST_HEADER_MAX_COUNT,
            request_body_max_bytes: FETCH_REQUEST_BODY_MAX_BYTES,
            response_header_max_bytes: FETCH_RESPONSE_HEADER_MAX_BYTES,
            response_header_max_count: FETCH_RESPONSE_HEADER_MAX_COUNT,
            response_body_max_bytes: FETCH_RESPONSE_BODY_MAX_BYTES,
        }
    }
}

#[cfg(feature = "sandbox")]
#[derive(Clone)]
pub(crate) struct FetchTransport {
    client: reqwest::blocking::Client,
    limits: FetchLimits,
}

#[cfg(feature = "sandbox")]
impl FetchTransport {
    pub(crate) fn new(resolved_host: Option<(&str, &[SocketAddr])>) -> Result<Self, FetchError> {
        Self::with_limits(FetchLimits::default(), resolved_host)
    }

    fn with_limits(
        limits: FetchLimits,
        resolved_host: Option<(&str, &[SocketAddr])>,
    ) -> Result<Self, FetchError> {
        let mut builder = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .pool_max_idle_per_host(0)
            .connect_timeout(limits.connect_timeout)
            // The blocking client applies this bound to the initial response
            // wait and every subsequent body read. `execute` separately owns
            // the total wall-clock deadline across all successful reads.
            .timeout(limits.read_timeout)
            .user_agent(concat!("mini-agent-js-fetch/", env!("CARGO_PKG_VERSION")));
        if let Some((host, addresses)) = resolved_host {
            builder = builder.resolve_to_addrs(host, addresses);
        }
        let client = builder
            .build()
            .map_err(|error| FetchError::ClientBuild(error.to_string()))?;
        Ok(Self { client, limits })
    }

    pub(crate) fn execute(
        &self,
        url: Url,
        request: &FetchRequest,
        is_cancelled: impl Fn() -> bool,
    ) -> Result<FetchTransportOutcome, FetchError> {
        if is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        validate_header_limits(
            &request.headers,
            self.limits.request_header_max_count,
            self.limits.request_header_max_bytes,
            FetchError::RequestHeadersTooLarge,
        )?;
        if request
            .body
            .as_ref()
            .is_some_and(|body| body.len() > self.limits.request_body_max_bytes)
        {
            return Err(FetchError::RequestBodyTooLarge);
        }

        let started = Instant::now();
        let mut builder = self
            .client
            .request(request.method.clone(), url.clone())
            .headers(request.headers.clone());
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let mut response = builder.send().map_err(map_reqwest_error)?;
        if is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        if started.elapsed() >= self.limits.total_timeout {
            return Err(FetchError::TimedOut);
        }

        validate_header_limits(
            response.headers(),
            self.limits.response_header_max_count,
            self.limits.response_header_max_bytes,
            FetchError::ResponseHeadersTooLarge,
        )?;
        reject_encoded_response(response.headers())?;

        if is_followable_redirect(response.status()) {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or(FetchError::InvalidRedirect)?
                .to_str()
                .map_err(|_| FetchError::InvalidRedirect)?;
            let redirect = url
                .join(location)
                .map_err(|_| FetchError::InvalidRedirect)?;
            return Ok(FetchTransportOutcome::Redirect(redirect));
        }

        if response
            .content_length()
            .is_some_and(|length| length > self.limits.response_body_max_bytes as u64)
        {
            return Err(FetchError::ResponseBodyTooLarge);
        }

        let mut body = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            if is_cancelled() {
                return Err(FetchError::Cancelled);
            }
            if started.elapsed() >= self.limits.total_timeout {
                return Err(FetchError::TimedOut);
            }
            let read = response.read(&mut buffer).map_err(map_io_error)?;
            if read == 0 {
                break;
            }
            if body.len().saturating_add(read) > self.limits.response_body_max_bytes {
                return Err(FetchError::ResponseBodyTooLarge);
            }
            body.extend_from_slice(&buffer[..read]);
        }
        if started.elapsed() >= self.limits.total_timeout {
            return Err(FetchError::TimedOut);
        }
        let text = String::from_utf8(body).map_err(|_| FetchError::InvalidUtf8)?;
        Ok(FetchTransportOutcome::Complete(FetchResult {
            status: response.status().as_u16(),
            text,
        }))
    }
}

#[cfg(feature = "sandbox")]
fn is_followable_redirect(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT
    )
}

#[cfg(feature = "sandbox")]
fn validate_header_limits(
    headers: &reqwest::header::HeaderMap,
    max_count: usize,
    max_bytes: usize,
    error: FetchError,
) -> Result<(), FetchError> {
    if headers.len() > max_count {
        return Err(error);
    }
    let mut total = 0_usize;
    for (name, value) in headers {
        total = total
            .checked_add(name.as_str().len())
            .and_then(|total| total.checked_add(value.as_bytes().len()))
            .and_then(|total| total.checked_add(4))
            .ok_or_else(|| error.clone())?;
        if total > max_bytes {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(feature = "sandbox")]
fn reject_encoded_response(headers: &reqwest::header::HeaderMap) -> Result<(), FetchError> {
    for encoding in headers.get_all(reqwest::header::CONTENT_ENCODING) {
        if !encoding
            .to_str()
            .is_ok_and(|encoding| encoding.eq_ignore_ascii_case("identity"))
        {
            return Err(FetchError::UnsupportedContentEncoding);
        }
    }
    Ok(())
}

#[cfg(feature = "sandbox")]
fn map_reqwest_error(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::TimedOut
    } else {
        FetchError::RequestFailed(error.to_string())
    }
}

#[cfg(feature = "sandbox")]
fn map_io_error(error: std::io::Error) -> FetchError {
    let mut source = std::error::Error::source(&error);
    let mut timed_out = error.kind() == std::io::ErrorKind::TimedOut
        || error.to_string().to_ascii_lowercase().contains("timed out");
    while let Some(current) = source {
        timed_out |= current
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
            || current
                .to_string()
                .to_ascii_lowercase()
                .contains("timed out");
        source = current.source();
    }
    if timed_out {
        FetchError::TimedOut
    } else {
        FetchError::RequestFailed(error.to_string())
    }
}

#[cfg(test)]
fn service_host_error(tool: &'static str, error: EffectServiceError) -> rquickjs::Error {
    let access = match tool {
        "js/read_file" => Some("read"),
        "js/write_file" => Some("write"),
        _ => None,
    };
    if let Some(access) = access {
        let reason = match error {
            EffectServiceError::FileNoConfiguredRoots => {
                Some("no roots are configured; unrestricted access requires an explicit opt-in")
            }
            EffectServiceError::FileInvalidConfiguration => {
                Some("the configured roots are invalid or ambiguous")
            }
            EffectServiceError::FileOutsideConfiguredRoots => {
                Some("the resolved target is outside the configured roots")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            return rquickjs::Error::new_from_js_message(
                "file policy",
                tool,
                format!("JS file {access} denied: {reason}"),
            );
        }
    }
    rquickjs::Error::new_from_js_message("parent effect service", tool, error.to_string())
}

fn file_policy_service_error(reason: AllowPolicyReason) -> EffectServiceError {
    match reason {
        AllowPolicyReason::NoConfiguredRoots(_) => EffectServiceError::FileNoConfiguredRoots,
        AllowPolicyReason::InvalidConfiguration(_) => EffectServiceError::FileInvalidConfiguration,
        AllowPolicyReason::OutsideConfiguredRoots(_) => {
            EffectServiceError::FileOutsideConfiguredRoots
        }
        AllowPolicyReason::InvalidTarget(_) => EffectServiceError::InvalidTarget,
        AllowPolicyReason::AmbiguousSymlink(_) => EffectServiceError::FinalSymlink,
    }
}

fn permission_service_error(error: PermissionBridgeError) -> EffectServiceError {
    match error {
        PermissionBridgeError::Denied(crate::extras::js::types::PermissionDenial::Policy(
            reason,
        )) if reason.contains("Doom loop: repeated identical tool call") => {
            EffectServiceError::DoomLoopDenied
        }
        PermissionBridgeError::Denied(_) => EffectServiceError::PermissionDenied,
        PermissionBridgeError::TimedOut => EffectServiceError::PermissionTimedOut,
        PermissionBridgeError::Cancelled => EffectServiceError::Cancelled,
        PermissionBridgeError::InvalidRequest(_)
        | PermissionBridgeError::RequestChannelClosed
        | PermissionBridgeError::ResponseChannelClosed
        | PermissionBridgeError::BackendFailure(_)
        | PermissionBridgeError::RejectedResponse(_) => EffectServiceError::BackendFailure,
    }
}

#[cfg(test)]
fn timeout_error(tool: &'static str) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host call", tool, "execution timed out")
}

#[cfg(test)]
fn file_error(
    tool: &'static str,
    kind: &'static str,
    message: impl Into<String>,
) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message(kind, tool, message.into())
}

#[cfg(test)]
async fn timeout_host_call<T>(
    tool: &'static str,
    duration: Duration,
    call: impl Future<Output = rquickjs::Result<T>>,
) -> rquickjs::Result<T> {
    timeout(duration, call)
        .await
        .map_err(|_| timeout_error(tool))?
}

struct ResolvedReadTarget {
    path: PathBuf,
    identity: std::fs::Metadata,
}

#[derive(Clone, Copy)]
enum WriteMode {
    Create,
    Replace,
}

struct ResolvedWriteTarget {
    path: PathBuf,
    parent_identity: std::fs::Metadata,
    mode: WriteMode,
}

fn absolute_lexical(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in source.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn file_path_error(error: std::io::Error) -> EffectServiceError {
    if crate::fs::is_path_changed_error(&error) {
        return EffectServiceError::TargetChanged;
    }
    match error.kind() {
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::NotADirectory
        | std::io::ErrorKind::IsADirectory => EffectServiceError::InvalidTarget,
        _ => EffectServiceError::BackendFailure,
    }
}

fn permission_path(path: &Path) -> Result<String, EffectServiceError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(EffectServiceError::InvalidTarget)
}

async fn resolve_read_target(path: &str) -> Result<ResolvedReadTarget, EffectServiceError> {
    let expanded = crate::fs::expand_tilde(path);
    let absolute = absolute_lexical(Path::new(&expanded)).map_err(file_path_error)?;
    let canonical = tokio::fs::canonicalize(absolute)
        .await
        .map_err(file_path_error)?;
    permission_path(&canonical)?;
    let identity = crate::fs::stable_path_metadata(&canonical)
        .await
        .map_err(file_path_error)?;
    Ok(ResolvedReadTarget {
        path: canonical,
        identity,
    })
}

async fn read_approved_file(target: ResolvedReadTarget) -> Result<String, EffectServiceError> {
    if !target.identity.is_file() {
        return Err(EffectServiceError::InvalidTarget);
    }
    if target.identity.len() > READ_FILE_MAX_BYTES as u64 {
        return Err(EffectServiceError::OutputLimit);
    }
    let file = crate::fs::open_stable_file(&target.path)
        .await
        .map_err(file_path_error)?;
    let opened = file.metadata().await.map_err(file_path_error)?;
    crate::fs::ensure_same_file(&target.path, &target.identity, &opened)
        .map_err(file_path_error)?;
    if !opened.is_file() {
        return Err(EffectServiceError::InvalidTarget);
    }
    if opened.len() > READ_FILE_MAX_BYTES as u64 {
        return Err(EffectServiceError::OutputLimit);
    }

    let mut bytes = Vec::new();
    file.take((READ_FILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(file_path_error)?;
    if bytes.len() > READ_FILE_MAX_BYTES {
        return Err(EffectServiceError::OutputLimit);
    }
    String::from_utf8(bytes).map_err(|_| EffectServiceError::InvalidBody)
}

async fn resolve_write_target(path: &str) -> Result<ResolvedWriteTarget, EffectServiceError> {
    use std::path::Component;

    let expanded = crate::fs::expand_tilde(path);
    let absolute = absolute_lexical(Path::new(&expanded)).map_err(file_path_error)?;
    let (path, mode) = match tokio::fs::symlink_metadata(&absolute).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(EffectServiceError::FinalSymlink);
            }
            if !metadata.is_file() {
                return Err(EffectServiceError::InvalidTarget);
            }
            (
                tokio::fs::canonicalize(&absolute)
                    .await
                    .map_err(file_path_error)?,
                WriteMode::Replace,
            )
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = absolute.as_path();
            let mut missing = Vec::new();
            let canonical_parent = loop {
                match tokio::fs::canonicalize(ancestor).await {
                    Ok(canonical) => break canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let name = ancestor
                            .file_name()
                            .ok_or(EffectServiceError::InvalidTarget)?;
                        missing.push(name.to_os_string());
                        ancestor = ancestor.parent().ok_or(EffectServiceError::InvalidTarget)?;
                    }
                    Err(error) => return Err(file_path_error(error)),
                }
            };
            if missing.len() != 1 {
                return Err(EffectServiceError::InvalidTarget);
            }
            let relative = Path::new(&missing[0]);
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(EffectServiceError::InvalidTarget);
            }
            (canonical_parent.join(relative), WriteMode::Create)
        }
        Err(error) => return Err(file_path_error(error)),
    };
    permission_path(&path)?;
    let parent = path.parent().ok_or(EffectServiceError::InvalidTarget)?;
    let parent_identity = crate::fs::stable_path_metadata(parent)
        .await
        .map_err(file_path_error)?;
    if !parent_identity.is_dir() {
        return Err(EffectServiceError::InvalidTarget);
    }
    Ok(ResolvedWriteTarget {
        path,
        parent_identity,
        mode,
    })
}

async fn write_approved_file(
    target: ResolvedWriteTarget,
    content: String,
) -> Result<(), EffectServiceError> {
    match target.mode {
        WriteMode::Create => {
            crate::fs::atomic_create_resolved_checked(
                target.path,
                content.as_bytes(),
                target.parent_identity,
            )
            .await
        }
        WriteMode::Replace => {
            crate::fs::atomic_write_resolved_checked(
                target.path,
                content.as_bytes(),
                target.parent_identity,
            )
            .await
        }
    }
    .map_err(file_path_error)
}

/// Parent-side file service. Its public contract is independent of QuickJS;
/// the legacy worker closure below is only a value/error adapter.
#[derive(Clone)]
pub(crate) struct FileEffectService {
    permission_bridge: PermissionBridge,
    allow_config: AllowConfig,
    timeout: Duration,
}

struct PreparedReadEffect(ResolvedReadTarget);

struct PreparedWriteEffect(ResolvedWriteTarget);

impl FileEffectService {
    pub(crate) fn new(
        permission_bridge: PermissionBridge,
        allow_config: AllowConfig,
        timeout: Duration,
    ) -> Self {
        Self {
            permission_bridge,
            allow_config,
            timeout,
        }
    }

    pub(crate) async fn read(
        &self,
        path: &str,
        cancellation: PermCancellation,
    ) -> Result<String, EffectServiceError> {
        let bridge = self.permission_bridge.for_host_call(cancellation);
        let prepared = self.authorize_read(path, bridge.clone()).await?;
        self.execute_read(prepared, bridge).await
    }

    async fn authorize_read(
        &self,
        path: &str,
        bridge: PermissionBridge,
    ) -> Result<PreparedReadEffect, EffectServiceError> {
        let call = async {
            let target = resolve_read_target(path).await?;
            if let AuthorizationDecision::Denied(reason) =
                self.allow_config.authorize_read(&target.path)
            {
                return Err(file_policy_service_error(reason));
            }
            let permission_path = permission_path(&target.path)?;
            bridge
                .check_path_async("js/read_file", &permission_path)
                .await
                .map_err(permission_service_error)?;
            Ok(PreparedReadEffect(target))
        };
        tokio::select! {
            result = timeout(self.timeout, call) => result.map_err(|_| EffectServiceError::TimedOut)?,
            _ = bridge.cancelled() => Err(EffectServiceError::Cancelled),
        }
    }

    async fn execute_read(
        &self,
        prepared: PreparedReadEffect,
        bridge: PermissionBridge,
    ) -> Result<String, EffectServiceError> {
        let call = async { read_approved_file(prepared.0).await };
        tokio::select! {
            result = timeout(self.timeout, call) => result.map_err(|_| EffectServiceError::TimedOut)?,
            _ = bridge.cancelled() => Err(EffectServiceError::Cancelled),
        }
    }

    pub(crate) async fn write(
        &self,
        path: &str,
        content: String,
        cancellation: PermCancellation,
    ) -> Result<(), EffectServiceError> {
        if content.len() > WRITE_FILE_MAX_BYTES {
            return Err(EffectServiceError::BodyLimit);
        }
        let bridge = self.permission_bridge.for_host_call(cancellation);
        let prepared = self.authorize_write(path, bridge.clone()).await?;
        self.execute_write(prepared, content, bridge).await
    }

    async fn authorize_write(
        &self,
        path: &str,
        bridge: PermissionBridge,
    ) -> Result<PreparedWriteEffect, EffectServiceError> {
        let call = async {
            let target = resolve_write_target(path).await?;
            if let AuthorizationDecision::Denied(reason) =
                self.allow_config.authorize_write(&target.path)
            {
                return Err(file_policy_service_error(reason));
            }
            let permission_path = permission_path(&target.path)?;
            bridge
                .check_path_async("js/write_file", &permission_path)
                .await
                .map_err(permission_service_error)?;
            Ok(PreparedWriteEffect(target))
        };
        tokio::select! {
            result = timeout(self.timeout, call) => result.map_err(|_| EffectServiceError::TimedOut)?,
            _ = bridge.cancelled() => Err(EffectServiceError::Cancelled),
        }
    }

    async fn execute_write(
        &self,
        prepared: PreparedWriteEffect,
        content: String,
        bridge: PermissionBridge,
    ) -> Result<(), EffectServiceError> {
        let call = async { write_approved_file(prepared.0, content).await };
        tokio::select! {
            result = timeout(self.timeout, call) => result.map_err(|_| EffectServiceError::TimedOut)?,
            _ = bridge.cancelled() => Err(EffectServiceError::Cancelled),
        }
    }
}

#[cfg(test)]
pub(crate) fn make_read_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    allow_config: AllowConfig,
) -> impl Fn(String) -> rquickjs::Result<String> {
    let service = FileEffectService::new(permission_bridge, allow_config, STEP_TIMEOUT);
    move |path: String| {
        runtime
            .block_on(service.read(&path, PermCancellation::new()))
            .map_err(|error| service_host_error("js/read_file", error))
    }
}

#[cfg(test)]
pub(crate) fn make_write_file(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    allow_config: AllowConfig,
) -> impl Fn(String, String) -> rquickjs::Result<()> {
    let service = FileEffectService::new(permission_bridge, allow_config, STEP_TIMEOUT);
    move |path: String, content: String| {
        runtime
            .block_on(service.write(&path, content, PermCancellation::new()))
            .map_err(|error| service_host_error("js/write_file", error))
    }
}

#[cfg(all(feature = "sandbox", test))]
fn make_fetch(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    policy: FetchPolicy,
) -> impl for<'js> Fn(String, Opt<Object<'js>>) -> rquickjs::Result<FetchResult> {
    let service = Arc::new(FetchEffectService::new(
        permission_bridge.clone(),
        runtime.clone(),
        policy,
        FETCH_TOTAL_TIMEOUT,
    ));
    move |url: String, options: Opt<Object<'_>>| {
        let request = FetchRequest::from_options(options.0.as_ref()).map_err(fetch_host_error)?;
        runtime
            .block_on(service.execute(url, request, PermCancellation::new()))
            .map_err(fetch_host_error)
    }
}

/// Parent-side network service with an outer wall-clock deadline in addition
/// to DNS, connect, read, redirect, header, and body bounds.
#[cfg(feature = "sandbox")]
pub(crate) struct FetchEffectService {
    executor: Arc<FetchExecutor>,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    timeout: Duration,
}

#[cfg(feature = "sandbox")]
impl FetchEffectService {
    pub(crate) fn new(
        permission_bridge: PermissionBridge,
        runtime: tokio::runtime::Handle,
        policy: FetchPolicy,
        timeout: Duration,
    ) -> Self {
        let resolver = Arc::new(RuntimeFetchResolver {
            runtime: runtime.clone(),
        });
        let executor = Arc::new(FetchExecutor {
            policy,
            resolver,
            sender: Arc::new(BoundFetchSender),
            permission_bridge: permission_bridge.clone(),
        });
        Self {
            executor,
            permission_bridge,
            runtime,
            timeout,
        }
    }

    pub(crate) async fn execute(
        &self,
        url: String,
        request: FetchRequest,
        cancellation: PermCancellation,
    ) -> Result<FetchResult, FetchError> {
        let control = Arc::new(FetchCallControl::new());
        let prepared = self
            .authorize(url, request, cancellation.clone(), control.clone())
            .await?;
        self.execute_prepared(prepared, cancellation).await
    }

    async fn authorize(
        &self,
        url: String,
        request: FetchRequest,
        cancellation: PermCancellation,
        control: Arc<FetchCallControl>,
    ) -> Result<PreparedFetchEffect, FetchError> {
        if cancellation.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        let deadline = Instant::now() + self.timeout;
        let mut cancel_on_drop = CancelFetchPrepareOnDrop::new(control.clone());
        let task_control = control.clone();
        let executor = self.executor.clone();
        let call = self
            .runtime
            .spawn_blocking(move || executor.prepare(&url, request, task_control, deadline));
        let bridge = self.permission_bridge.for_host_call(cancellation);
        let prepared = await_fetch_prepare(&bridge, self.timeout, call, control).await?;
        cancel_on_drop.disarm();
        Ok(prepared)
    }

    async fn authorize_normalized(
        &self,
        url: Url,
        request: FetchRequest,
        cancellation: PermCancellation,
        control: Arc<FetchCallControl>,
        deadline: Instant,
    ) -> Result<PreparedFetchEffect, FetchError> {
        if cancellation.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        let mut cancel_on_drop = CancelFetchPrepareOnDrop::new(control.clone());
        let task_control = control.clone();
        let executor = self.executor.clone();
        let call = self.runtime.spawn_blocking(move || {
            executor.prepare_normalized(url, request, task_control, deadline)
        });
        let bridge = self.permission_bridge.for_host_call(cancellation);
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(FetchError::TimedOut)?
            .min(self.timeout);
        let prepared = await_fetch_prepare(&bridge, remaining, call, control).await?;
        cancel_on_drop.disarm();
        Ok(prepared)
    }

    async fn execute_prepared(
        &self,
        prepared: PreparedFetchEffect,
        cancellation: PermCancellation,
    ) -> Result<FetchResult, FetchError> {
        let control = prepared.control.clone();
        let remaining = prepared
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(FetchError::TimedOut)?
            .min(self.timeout);
        let executor = self.executor.clone();
        let call = self
            .runtime
            .spawn_blocking(move || executor.execute_prepared(prepared));
        let bridge = self.permission_bridge.for_host_call(cancellation);
        await_fetch_host_call(&bridge, remaining, call, control).await
    }
}

#[cfg(feature = "sandbox")]
async fn await_fetch_prepare(
    permission_bridge: &PermissionBridge,
    duration: Duration,
    mut call: tokio::task::JoinHandle<Result<PreparedFetchEffect, FetchError>>,
    control: Arc<FetchCallControl>,
) -> Result<PreparedFetchEffect, FetchError> {
    tokio::select! {
        result = timeout(duration, &mut call) => match result {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(FetchError::RequestFailed("fetch executor task failed".to_string())),
            Err(_) => Err(control.cancel(FetchError::TimedOut)),
        },
        _ = permission_bridge.cancelled() => Err(control.cancel(FetchError::Cancelled)),
    }
}

#[cfg(feature = "sandbox")]
fn fetch_service_error(error: &FetchError) -> EffectServiceError {
    match error {
        FetchError::InvalidUrl
        | FetchError::SchemeDenied
        | FetchError::EmbeddedCredentials
        | FetchError::FragmentDenied
        | FetchError::MissingHost
        | FetchError::InvalidHost
        | FetchError::MissingPort
        | FetchError::InvalidOptions(_)
        | FetchError::InvalidRedirect => EffectServiceError::InvalidTarget,
        FetchError::InvalidOrigin
        | FetchError::OriginDenied
        | FetchError::DestinationDenied
        | FetchError::TooManyDestinations
        | FetchError::CrossOriginRedirectDenied
        | FetchError::RedirectReplayDenied
        | FetchError::UnauditedRedirectDenied => EffectServiceError::TargetDenied,
        FetchError::Permission(error) => *error,
        FetchError::Cancelled => EffectServiceError::Cancelled,
        FetchError::TimedOut => EffectServiceError::TimedOut,
        FetchError::RequestHeadersTooLarge | FetchError::RequestBodyTooLarge => {
            EffectServiceError::BodyLimit
        }
        FetchError::ResponseHeadersTooLarge | FetchError::ResponseBodyTooLarge => {
            EffectServiceError::OutputLimit
        }
        FetchError::InvalidUtf8 => EffectServiceError::InvalidBody,
        FetchError::OutcomeUnknown => EffectServiceError::OutcomeUnknown,
        FetchError::DnsResolutionFailed
        | FetchError::TooManyRedirects
        | FetchError::ClientBuild(_)
        | FetchError::RequestFailed(_)
        | FetchError::UnsupportedContentEncoding => EffectServiceError::BackendFailure,
    }
}

#[cfg(all(feature = "sandbox", test))]
fn make_fetch_with_timeout(
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    executor: Arc<FetchExecutor>,
    duration: Duration,
) -> impl for<'js> Fn(String, Opt<Object<'js>>) -> rquickjs::Result<FetchResult> {
    move |url: String, options: Opt<Object<'_>>| {
        let request = FetchRequest::from_options(options.0.as_ref()).map_err(fetch_host_error)?;
        let executor = executor.clone();
        let control = Arc::new(FetchCallControl::new());
        let task_control = control.clone();
        let fetch = runtime
            .spawn_blocking(move || executor.execute_controlled(&url, &request, task_control));
        block_on_fetch_host_call(&runtime, &permission_bridge, duration, fetch, control)
            .map_err(fetch_host_error)
    }
}

#[cfg(all(feature = "sandbox", test))]
fn fetch_host_error(error: FetchError) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("network policy", "js/fetch", error.to_string())
}

#[cfg(all(feature = "sandbox", test))]
fn block_on_fetch_host_call(
    runtime: &tokio::runtime::Handle,
    permission_bridge: &PermissionBridge,
    duration: Duration,
    call: tokio::task::JoinHandle<Result<FetchResult, FetchError>>,
    control: Arc<FetchCallControl>,
) -> Result<FetchResult, FetchError> {
    runtime.block_on(await_fetch_host_call(
        permission_bridge,
        duration,
        call,
        control,
    ))
}

#[cfg(feature = "sandbox")]
async fn await_fetch_host_call(
    permission_bridge: &PermissionBridge,
    duration: Duration,
    mut call: tokio::task::JoinHandle<Result<FetchResult, FetchError>>,
    control: Arc<FetchCallControl>,
) -> Result<FetchResult, FetchError> {
    enum WaitOutcome {
        Completed(Result<Result<FetchResult, FetchError>, tokio::task::JoinError>),
        TimedOut,
        Cancelled,
    }

    let outcome = tokio::select! {
        result = timeout(duration, &mut call) => match result {
            Ok(result) => WaitOutcome::Completed(result),
            Err(_) => WaitOutcome::TimedOut,
        },
        _ = permission_bridge.cancelled() => WaitOutcome::Cancelled,
    };
    match outcome {
        WaitOutcome::Completed(Ok(result)) => result,
        WaitOutcome::Completed(Err(_)) => Err(FetchError::RequestFailed(
            "fetch executor task failed".to_string(),
        )),
        WaitOutcome::TimedOut | WaitOutcome::Cancelled => {
            let before_dispatch = if matches!(outcome, WaitOutcome::TimedOut) {
                FetchError::TimedOut
            } else {
                FetchError::Cancelled
            };
            let error = control.cancel(before_dispatch);
            if timeout(FETCH_CANCELLATION_DRAIN_TIMEOUT, &mut call)
                .await
                .is_err()
            {
                tokio::spawn(async move {
                    let _ = call.await;
                });
            }
            Err(error)
        }
    }
}

#[cfg(test)]
pub(crate) fn make_spawn(
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    make_spawn_with_timeout(sandbox, permission_bridge, runtime, STEP_TIMEOUT)
}

#[cfg(all(feature = "skills", test))]
pub(crate) fn make_propose_skill(
    proposal_host: ProposalHost,
) -> impl for<'js> Fn(Object<'js>) -> rquickjs::Result<String> {
    let service = ProposalEffectService::new(proposal_host);
    move |object: Object<'_>| {
        service.reserve_attempt().map_err(proposal_host_error)?;
        let proposal = JsProposal::from_object(&object).map_err(proposal_host_error)?;
        let result = service
            .execute_reserved(proposal)
            .map_err(proposal_host_error)?;
        serde_json::to_string(&serde_json::json!({
            "id": result.skill_id,
            "proposal_id": result.proposal_id,
            "status": result.status,
            "report_id": result.report_id,
        }))
        .map_err(|_| proposal_host_error(ProposalError::StoreUnavailable))
    }
}

#[cfg(all(feature = "skills", test))]
fn proposal_host_error(error: ProposalError) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("proposal", "js/propose_skill", error.to_string())
}

#[cfg(all(feature = "skills", test))]
pub(crate) fn register_proposal_global(
    ctx: &Context,
    proposal_host: Option<ProposalHost>,
) -> rquickjs::Result<()> {
    if let Some(proposal_host) = proposal_host {
        ctx.with(|ctx| {
            ctx.globals().set(
                "propose_skill",
                Func::from(make_propose_skill(proposal_host)),
            )
        })?;
    }
    Ok(())
}

const SPAWN_STDOUT_MAX_BYTES: usize = 1024 * 1024;
const SPAWN_STDERR_MAX_BYTES: usize = 1024 * 1024;
const SPAWN_COMBINED_MAX_BYTES: usize = 1536 * 1024;
const CONSOLE_MAX_BYTES_PER_STEP: usize = 256 * 1024;

/// Parent-side structured process service. Permission identity and execution
/// consume the same program/argument vector; no shell-like joining occurs.
#[derive(Clone)]
pub(crate) struct SpawnEffectService {
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    timeout: Duration,
}

struct PreparedSpawnEffect {
    executable: SpawnExecutableIdentity,
    arguments: Vec<String>,
    target: PreparedSpawnTarget,
}

enum PreparedSpawnTarget {
    #[cfg(unix)]
    OpenedPath(std::fs::File),
    #[cfg(target_os = "linux")]
    SealedSnapshot(std::fs::File),
    #[cfg(not(unix))]
    Path,
}

impl PreparedSpawnEffect {
    fn capture(
        program: &str,
        arguments: Vec<String>,
        immutable_snapshot: bool,
    ) -> Result<Self, EffectServiceError> {
        let executable = resolve_program_identity(program)?;
        if immutable_snapshot {
            #[cfg(target_os = "linux")]
            {
                let mut source = std::fs::File::open(executable.canonical_path())
                    .map_err(|_| EffectServiceError::InvalidTarget)?;
                let opened = source
                    .metadata()
                    .map_err(|_| EffectServiceError::InvalidTarget)?;
                let current = std::fs::symlink_metadata(executable.canonical_path())
                    .map_err(|_| EffectServiceError::InvalidTarget)?;
                if !opened.is_file()
                    || !executable.matches_metadata(&opened)
                    || !executable.matches_metadata(&current)
                {
                    return Err(EffectServiceError::TargetChanged);
                }
                let (snapshot, content) = create_sealed_executable_snapshot(&mut source)?;
                let executable = executable.with_content(content);
                return Ok(Self {
                    executable,
                    arguments,
                    target: PreparedSpawnTarget::SealedSnapshot(snapshot),
                });
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(EffectServiceError::BackendFailure);
            }
        }
        #[cfg(unix)]
        let target = {
            let file = std::fs::File::open(executable.canonical_path())
                .map_err(|_| EffectServiceError::InvalidTarget)?;
            let opened = file
                .metadata()
                .map_err(|_| EffectServiceError::InvalidTarget)?;
            let current = std::fs::symlink_metadata(executable.canonical_path())
                .map_err(|_| EffectServiceError::InvalidTarget)?;
            if !executable.matches_metadata(&opened)
                || !executable.matches_metadata(&current)
                || !opened.is_file()
            {
                return Err(EffectServiceError::TargetChanged);
            }
            PreparedSpawnTarget::OpenedPath(file)
        };
        #[cfg(not(unix))]
        let target = PreparedSpawnTarget::Path;
        Ok(Self {
            executable,
            arguments,
            target,
        })
    }

    fn revalidate(&self) -> Result<(), EffectServiceError> {
        #[cfg(target_os = "linux")]
        if let PreparedSpawnTarget::SealedSnapshot(snapshot) = &self.target {
            return verify_executable_snapshot_seals(snapshot);
        }
        let current = std::fs::symlink_metadata(self.executable.canonical_path())
            .map_err(|_| EffectServiceError::TargetChanged)?;
        if !self.executable.matches_metadata(&current) {
            return Err(EffectServiceError::TargetChanged);
        }
        #[cfg(unix)]
        {
            let file = match &self.target {
                PreparedSpawnTarget::OpenedPath(file) => file,
                #[cfg(target_os = "linux")]
                PreparedSpawnTarget::SealedSnapshot(_) => {
                    return Err(EffectServiceError::TargetChanged);
                }
            };
            let opened = file
                .metadata()
                .map_err(|_| EffectServiceError::TargetChanged)?;
            if !self.executable.matches_metadata(&opened) {
                return Err(EffectServiceError::TargetChanged);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn create_sealed_executable_snapshot(
    source: &mut std::fs::File,
) -> Result<(std::fs::File, crate::extras::js::broker::ExecutableContent), EffectServiceError> {
    use std::os::fd::FromRawFd;

    // SAFETY: `memfd_create` receives a static NUL-terminated name. On success this function
    // immediately assumes ownership of the returned descriptor through `File`.
    let raw_fd = unsafe {
        libc::memfd_create(
            c"mini-agent-spawn".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw_fd < 0 {
        return Err(EffectServiceError::BackendFailure);
    }
    // SAFETY: `raw_fd` is a newly-created owned descriptor and is transferred exactly once.
    let mut snapshot = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    let content = copy_and_hash_executable(source, &mut snapshot).map_err(|error| match error {
        ExecutableCopyError::Read => EffectServiceError::TargetChanged,
        ExecutableCopyError::Write => EffectServiceError::BackendFailure,
        ExecutableCopyError::TooLarge => EffectServiceError::InvalidTarget,
    })?;
    snapshot
        .seek(SeekFrom::Start(0))
        .map_err(|_| EffectServiceError::BackendFailure)?;
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: `fcntl` operates on the owned memfd and uses the documented sealing commands.
    if unsafe { libc::fcntl(raw_fd, libc::F_ADD_SEALS, required) } < 0 {
        return Err(EffectServiceError::BackendFailure);
    }
    verify_executable_snapshot_seals(&snapshot)?;
    Ok((snapshot, content))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn verify_executable_snapshot_seals(snapshot: &std::fs::File) -> Result<(), EffectServiceError> {
    use std::os::fd::AsRawFd;

    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: `fcntl(F_GET_SEALS)` only queries the owned descriptor.
    let seals = unsafe { libc::fcntl(snapshot.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & required != required {
        return Err(EffectServiceError::TargetChanged);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn provide_snapshot_fd(command: &mut SandboxCommand, snapshot: &std::fs::File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    const BWRAP_SNAPSHOT_FD: libc::c_int = 3;
    let source_fd = snapshot.as_raw_fd();
    // SAFETY: this callback uses only async-signal-safe fcntl/dup2 operations between fork and
    // exec. `PreparedSpawnEffect` owns the sealed memfd until bwrap has been spawned. fd 3 is an
    // input to bwrap's `--ro-bind-data`, not an inherited descriptor for the final process.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            if source_fd == BWRAP_SNAPSHOT_FD {
                let flags = libc::fcntl(source_fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(source_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(source_fd, BWRAP_SNAPSHOT_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

impl SpawnEffectService {
    pub(crate) fn new(
        sandbox: Sandbox,
        permission_bridge: PermissionBridge,
        timeout: Duration,
    ) -> Self {
        Self {
            sandbox,
            permission_bridge,
            timeout,
        }
    }

    pub(crate) async fn execute(
        &self,
        program: &str,
        arguments: &[String],
        cancellation: PermCancellation,
    ) -> Result<SpawnResult, EffectServiceError> {
        let bridge = self.permission_bridge.for_host_call(cancellation);
        let prepared = self.authorize(program, arguments, bridge.clone()).await?;
        self.execute_prepared(prepared, bridge).await
    }

    async fn authorize(
        &self,
        program: &str,
        arguments: &[String],
        bridge: PermissionBridge,
    ) -> Result<PreparedSpawnEffect, EffectServiceError> {
        if self.sandbox.policy() == SandboxPolicy::RequiredButUnavailable {
            return Err(EffectServiceError::BackendFailure);
        }
        if program.is_empty()
            || program.contains('\0')
            || arguments.iter().any(|arg| arg.contains('\0'))
            || (Path::new(program).is_absolute() && !Path::new(program).is_file())
        {
            return Err(EffectServiceError::InvalidTarget);
        }
        let subject = canonical_spawn_permission_subject(program, arguments)?;
        let policy_input = spawn_policy_input(program, arguments);
        bridge
            .check_structured_async("bash", &subject, policy_input)
            .await
            .map_err(permission_service_error)?;
        PreparedSpawnEffect::capture(
            program,
            arguments.to_vec(),
            self.sandbox.supports_immutable_executable_snapshot(),
        )
    }

    async fn execute_prepared(
        &self,
        prepared: PreparedSpawnEffect,
        bridge: PermissionBridge,
    ) -> Result<SpawnResult, EffectServiceError> {
        prepared.revalidate()?;
        #[cfg(not(unix))]
        let command: SandboxCommand = return Err(EffectServiceError::BackendFailure);

        #[cfg(target_os = "linux")]
        let command = match &prepared.target {
            PreparedSpawnTarget::SealedSnapshot(snapshot) => {
                let mut command = self
                    .sandbox
                    .wrap_command_with_executable_snapshot(&prepared.arguments)
                    .map_err(|_| EffectServiceError::BackendFailure)?;
                provide_snapshot_fd(&mut command, snapshot);
                command
            }
            PreparedSpawnTarget::OpenedPath(_) => {
                // This compatibility path is model-authored/direct only. Learned skills require
                // the sealed-snapshot branch and fail before permission/audit without it.
                let mut command = self
                    .sandbox
                    .wrap_command(r#"exec "$0" "$@""#)
                    .map_err(|_| EffectServiceError::BackendFailure)?;
                command
                    .arg(prepared.executable.canonical_path())
                    .args(&prepared.arguments);
                command
            }
        };
        #[cfg(all(unix, not(target_os = "linux")))]
        let command = {
            // macOS has no fexecve/execveat equivalent and rejects exec through /dev/fd.
            // Learned-skill callers fail in `ensure_backend`; model-authored/direct callers keep
            // legacy availability with this immediately-preceding identity check and explicit
            // residual rename window between this check and exec.
            prepared.revalidate()?;
            let mut command = self
                .sandbox
                .wrap_command(r#"exec "$0" "$@""#)
                .map_err(|_| EffectServiceError::BackendFailure)?;
            command
                .arg(prepared.executable.canonical_path())
                .args(&prepared.arguments);
            command
        };
        let limits = CommandLimits {
            timeout: self.timeout,
            stdout_bytes: SPAWN_STDOUT_MAX_BYTES,
            stderr_bytes: SPAWN_STDERR_MAX_BYTES,
            combined_bytes: SPAWN_COMBINED_MAX_BYTES,
        };
        let output = tokio::select! {
            result = self.sandbox.output_built_command_with_limits(command, limits) => {
                result.map_err(|_| EffectServiceError::BackendFailure)?
            }
            _ = bridge.cancelled() => return Err(EffectServiceError::Cancelled),
        };
        match output.status {
            CommandStatus::Cancelled => return Err(EffectServiceError::Cancelled),
            CommandStatus::Failed => return Err(EffectServiceError::BackendFailure),
            CommandStatus::Completed
            | CommandStatus::TimedOut
            | CommandStatus::OutputLimitExceeded(_) => {}
        }
        let timed_out = output.status == CommandStatus::TimedOut;
        let stdout_truncated = matches!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stdout)
                | CommandStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
        );
        let stderr_truncated = matches!(
            output.status,
            CommandStatus::OutputLimitExceeded(CommandOutputLimit::Stderr)
                | CommandStatus::OutputLimitExceeded(CommandOutputLimit::Combined)
        );
        Ok(SpawnResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output
                .exit_status
                .and_then(|status| status.code())
                .unwrap_or(-1),
            timed_out,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

enum PreparedParentEffect {
    Read(PreparedReadEffect),
    Write {
        target: PreparedWriteEffect,
        content: String,
    },
    Spawn(PreparedSpawnEffect),
    #[cfg(feature = "sandbox")]
    Fetch(PreparedFetchEffect),
    #[cfg(feature = "sandbox")]
    FetchPending {
        url: Url,
        request: FetchRequest,
        control: Arc<FetchCallControl>,
        deadline: Instant,
    },
}

fn workspace_relative_path(base: &Path, target: &Path) -> Result<Option<String>, HostEffectError> {
    use std::path::Component;

    let Ok(relative) = target.strip_prefix(base) else {
        return Ok(None);
    };
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(HostEffectError::InvalidTarget);
        };
        let component = component
            .to_str()
            .ok_or(HostEffectError::InvalidTarget)?
            .nfc()
            .collect::<String>();
        if component.is_empty() {
            return Err(HostEffectError::InvalidTarget);
        }
        components.push(component);
    }
    if components.is_empty() {
        return Ok(None);
    }
    Ok(Some(components.join("/")))
}

/// Concrete implementation of the A11 parent effect seam. Authorization
/// stores a prepared, exact target; execution can only consume that target.
pub(crate) struct ParentHostEffectService {
    file: FileEffectService,
    spawn: SpawnEffectService,
    #[cfg(feature = "sandbox")]
    fetch: Option<FetchEffectService>,
    prepared: Option<PreparedParentEffect>,
}

impl ParentHostEffectService {
    pub(crate) fn new(file: FileEffectService, spawn: SpawnEffectService) -> Self {
        Self {
            file,
            spawn,
            #[cfg(feature = "sandbox")]
            fetch: None,
            prepared: None,
        }
    }

    #[cfg(feature = "sandbox")]
    pub(crate) fn with_fetch(mut self, fetch: FetchEffectService) -> Self {
        self.fetch = Some(fetch);
        self
    }
}

impl ParentEffectService for ParentHostEffectService {
    fn discard_prepared(&mut self) {
        self.prepared = None;
    }

    fn validate_target(
        &mut self,
        _authorized: &AuthorizedEffect,
        operation: &EffectOperation,
    ) -> Result<(), HostEffectError> {
        self.prepared = None;
        match operation {
            EffectOperation::ReadFile { path } if path.is_empty() || path.contains('\0') => {
                Err(HostEffectError::InvalidTarget)
            }
            EffectOperation::WriteFile { path, .. } if path.is_empty() || path.contains('\0') => {
                Err(HostEffectError::InvalidTarget)
            }
            EffectOperation::WriteFile { content, .. } if content.len() > WRITE_FILE_MAX_BYTES => {
                Err(HostEffectError::OutputLimit)
            }
            EffectOperation::Spawn { program, arguments }
                if program.is_empty()
                    || program.contains('\0')
                    || arguments.iter().any(|argument| argument.contains('\0')) =>
            {
                Err(HostEffectError::InvalidTarget)
            }
            EffectOperation::Fetch { url, .. } if url.is_empty() || url.contains('\0') => {
                Err(HostEffectError::InvalidTarget)
            }
            EffectOperation::ProposeSkill { draft }
                if draft.source.is_empty()
                    || draft.description.is_empty()
                    || draft.exports.is_empty()
                    || draft.tests.is_empty() =>
            {
                Err(HostEffectError::InvalidTarget)
            }
            _ => Ok(()),
        }
    }

    fn ensure_backend(
        &mut self,
        authorized: &AuthorizedEffect,
        operation: &EffectOperation,
    ) -> Result<(), HostEffectError> {
        match operation {
            #[cfg(feature = "sandbox")]
            EffectOperation::Fetch { .. } if self.fetch.is_none() => {
                Err(HostEffectError::BackendFailure)
            }
            #[cfg(not(feature = "sandbox"))]
            EffectOperation::Fetch { .. } => Err(HostEffectError::BackendFailure),
            EffectOperation::Spawn { .. }
                if self.spawn.sandbox.policy() == SandboxPolicy::RequiredButUnavailable
                    || (matches!(authorized.principal(), GrantPrincipal::Skill { .. })
                        && !self.spawn.sandbox.supports_immutable_executable_snapshot()) =>
            {
                Err(HostEffectError::BackendFailure)
            }
            // A12's direct proposal service accepts the complete v2 proposal.
            // The provisional worker draft omits identity-bearing fields, so
            // the broker must fail closed instead of inventing them.
            EffectOperation::ProposeSkill { .. } => Err(HostEffectError::BackendFailure),
            _ => Ok(()),
        }
    }

    fn normalize_target<'a>(
        &'a mut self,
        authorized: &'a AuthorizedEffect,
        operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<NormalizedTarget, HostEffectError>> {
        Box::pin(async move {
            self.prepared = None;
            match operation {
                EffectOperation::ReadFile { path } => {
                    let target = tokio::select! {
                        result = timeout(self.file.timeout, resolve_read_target(path)) => {
                            result.map_err(|_| HostEffectError::EffectTimedOut)?
                                .map_err(HostEffectError::from)?
                        }
                        _ = cancellation.cancelled() => {
                            return Err(HostEffectError::InvocationCancelled);
                        }
                    };
                    let workspace_relative =
                        workspace_relative_path(&self.file.allow_config.base, &target.path)?;
                    self.prepared = Some(PreparedParentEffect::Read(PreparedReadEffect(target)));
                    Ok(NormalizedTarget::ReadFile { workspace_relative })
                }
                EffectOperation::WriteFile { path, content } => {
                    let target = tokio::select! {
                        result = timeout(self.file.timeout, resolve_write_target(path)) => {
                            result.map_err(|_| HostEffectError::EffectTimedOut)?
                                .map_err(HostEffectError::from)?
                        }
                        _ = cancellation.cancelled() => {
                            return Err(HostEffectError::InvocationCancelled);
                        }
                    };
                    let workspace_relative =
                        workspace_relative_path(&self.file.allow_config.base, &target.path)?;
                    self.prepared = Some(PreparedParentEffect::Write {
                        target: PreparedWriteEffect(target),
                        content: content.clone(),
                    });
                    Ok(NormalizedTarget::WriteFile { workspace_relative })
                }
                EffectOperation::Spawn { program, arguments } => {
                    if program.is_empty()
                        || program.contains('\0')
                        || arguments.iter().any(|argument| argument.contains('\0'))
                    {
                        return Err(HostEffectError::InvalidTarget);
                    }
                    let learned_skill =
                        matches!(authorized.principal(), GrantPrincipal::Skill { .. });
                    let snapshot_available =
                        self.spawn.sandbox.supports_immutable_executable_snapshot();
                    if learned_skill && !snapshot_available {
                        return Err(HostEffectError::BackendFailure);
                    }
                    let prepared = PreparedSpawnEffect::capture(
                        program,
                        arguments.clone(),
                        snapshot_available,
                    )
                    .map_err(HostEffectError::from)?;
                    let resolved = prepared.executable.clone();
                    self.prepared = Some(PreparedParentEffect::Spawn(prepared));
                    Ok(NormalizedTarget::Spawn {
                        program: program.nfc().collect(),
                        resolved_executable: resolved,
                    })
                }
                #[cfg(feature = "sandbox")]
                EffectOperation::Fetch {
                    url,
                    method,
                    headers,
                    body,
                } => {
                    let url = normalize_fetch_url(url, true)
                        .map_err(|error| HostEffectError::from(fetch_service_error(&error)))?;
                    let method_name = match method {
                        crate::extras::js::protocol::HttpMethod::Get => "GET",
                        crate::extras::js::protocol::HttpMethod::Post => "POST",
                    };
                    let headers = headers
                        .iter()
                        .map(|header| (header.name.clone(), header.value.clone()))
                        .collect::<Vec<_>>();
                    let request = FetchRequest::try_new(method_name, &headers, body.clone())
                        .map_err(|error| HostEffectError::from(fetch_service_error(&error)))?;
                    let control = Arc::new(FetchCallControl::new());
                    self.prepared = Some(PreparedParentEffect::FetchPending {
                        url: url.clone(),
                        request,
                        control,
                        deadline: Instant::now()
                            + self
                                .fetch
                                .as_ref()
                                .map_or(STEP_TIMEOUT, |fetch| fetch.timeout),
                    });
                    Ok(NormalizedTarget::Fetch {
                        origin: url.origin().ascii_serialization(),
                        method: method_name.to_string(),
                    })
                }
                #[cfg(not(feature = "sandbox"))]
                EffectOperation::Fetch { .. } => Err(HostEffectError::BackendFailure),
                EffectOperation::ProposeSkill { .. } => Ok(NormalizedTarget::ProposeSkill),
            }
        })
    }

    fn authorize<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        _operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<AuthorizedTarget, HostEffectError>> {
        Box::pin(async move {
            let prepared = self
                .prepared
                .take()
                .ok_or(HostEffectError::BackendFailure)?;
            let (prepared, audit_target) = match prepared {
                PreparedParentEffect::Read(target) => {
                    let bridge = self.file.permission_bridge.for_host_call(cancellation);
                    let call = async {
                        if let AuthorizationDecision::Denied(reason) =
                            self.file.allow_config.authorize_read(&target.0.path)
                        {
                            return Err(file_policy_service_error(reason));
                        }
                        let permission_path = permission_path(&target.0.path)?;
                        bridge
                            .check_path_async("js/read_file", &permission_path)
                            .await
                            .map_err(permission_service_error)?;
                        Ok::<_, EffectServiceError>(())
                    };
                    tokio::select! {
                        result = timeout(self.file.timeout, call) => {
                            result.map_err(|_| HostEffectError::EffectTimedOut)?
                                .map_err(HostEffectError::from)?;
                        }
                        _ = bridge.cancelled() => return Err(HostEffectError::InvocationCancelled),
                    }
                    let canonical_path =
                        permission_path(&target.0.path).map_err(HostEffectError::from)?;
                    (
                        PreparedParentEffect::Read(target),
                        AuthorizedTarget::ReadFile { canonical_path },
                    )
                }
                PreparedParentEffect::Write { target, content } => {
                    let bridge = self.file.permission_bridge.for_host_call(cancellation);
                    let call = async {
                        if let AuthorizationDecision::Denied(reason) =
                            self.file.allow_config.authorize_write(&target.0.path)
                        {
                            return Err(file_policy_service_error(reason));
                        }
                        let permission_path = permission_path(&target.0.path)?;
                        bridge
                            .check_path_async("js/write_file", &permission_path)
                            .await
                            .map_err(permission_service_error)?;
                        Ok::<_, EffectServiceError>(())
                    };
                    tokio::select! {
                        result = timeout(self.file.timeout, call) => {
                            result.map_err(|_| HostEffectError::EffectTimedOut)?
                                .map_err(HostEffectError::from)?;
                        }
                        _ = bridge.cancelled() => return Err(HostEffectError::InvocationCancelled),
                    }
                    let canonical_path =
                        permission_path(&target.0.path).map_err(HostEffectError::from)?;
                    (
                        PreparedParentEffect::Write { target, content },
                        AuthorizedTarget::WriteFile { canonical_path },
                    )
                }
                PreparedParentEffect::Spawn(target) => {
                    if self.spawn.sandbox.policy() == SandboxPolicy::RequiredButUnavailable {
                        return Err(HostEffectError::BackendFailure);
                    }
                    let bridge = self.spawn.permission_bridge.for_host_call(cancellation);
                    let subject = canonical_spawn_permission_subject(
                        target.executable.canonical_path(),
                        &target.arguments,
                    )
                    .map_err(HostEffectError::from)?;
                    let policy_input =
                        spawn_policy_input(target.executable.canonical_path(), &target.arguments);
                    bridge
                        .check_structured_async("bash", &subject, policy_input)
                        .await
                        .map_err(permission_service_error)
                        .map_err(HostEffectError::from)?;
                    // The permission decision may have been blocked on Ask. Revalidate the
                    // sealed immutable version (or model-only compatibility identity) captured
                    // during normalization before durable audit.
                    target.revalidate().map_err(HostEffectError::from)?;
                    let resolved_executable = target.executable.canonical_path().to_string();
                    (
                        PreparedParentEffect::Spawn(target),
                        AuthorizedTarget::Spawn {
                            resolved_executable,
                        },
                    )
                }
                #[cfg(feature = "sandbox")]
                PreparedParentEffect::FetchPending {
                    url,
                    request,
                    control,
                    deadline,
                } => {
                    let fetch = self.fetch.as_ref().ok_or(HostEffectError::BackendFailure)?;
                    let method = request.method.as_str().to_string();
                    let mut prepared = fetch
                        .authorize_normalized(url, request, cancellation, control, deadline)
                        .await
                        .map_err(|error| HostEffectError::from(fetch_service_error(&error)))?;
                    // Preserve A14: redirects remain denied until each hop has its own intent.
                    prepared.redirect_mode = FetchRedirectMode::DenyUnaudited;
                    let normalized_url = prepared.target.url.as_str().to_string();
                    (
                        PreparedParentEffect::Fetch(prepared),
                        AuthorizedTarget::Fetch {
                            normalized_url,
                            method,
                        },
                    )
                }
                #[cfg(feature = "sandbox")]
                PreparedParentEffect::Fetch(_) => return Err(HostEffectError::BackendFailure),
            };
            self.prepared = Some(prepared);
            Ok(audit_target)
        })
    }

    fn execute<'a>(
        &'a mut self,
        _authorized: &'a AuthorizedEffect,
        _operation: &'a EffectOperation,
        cancellation: PermCancellation,
    ) -> ParentEffectFuture<'a, Result<EffectResult, HostEffectError>> {
        Box::pin(async move {
            let prepared = self
                .prepared
                .take()
                .ok_or(HostEffectError::BackendFailure)?;
            match prepared {
                PreparedParentEffect::Read(target) => {
                    let bridge = self.file.permission_bridge.for_host_call(cancellation);
                    let content = self
                        .file
                        .execute_read(target, bridge)
                        .await
                        .map_err(HostEffectError::from)?;
                    Ok(EffectResult::ReadFile { content })
                }
                PreparedParentEffect::Write { target, content } => {
                    let bridge = self.file.permission_bridge.for_host_call(cancellation);
                    self.file
                        .execute_write(target, content, bridge)
                        .await
                        .map_err(HostEffectError::from)?;
                    Ok(EffectResult::WriteFile)
                }
                PreparedParentEffect::Spawn(target) => {
                    let bridge = self.spawn.permission_bridge.for_host_call(cancellation);
                    let result = self
                        .spawn
                        .execute_prepared(target, bridge)
                        .await
                        .map_err(HostEffectError::from)?;
                    Ok(EffectResult::Spawn {
                        stdout: result.stdout,
                        stderr: result.stderr,
                        exit_code: result.code,
                        timed_out: result.timed_out,
                        stdout_truncated: result.stdout_truncated,
                        stderr_truncated: result.stderr_truncated,
                    })
                }
                #[cfg(feature = "sandbox")]
                PreparedParentEffect::Fetch(target) => {
                    let fetch = self.fetch.as_ref().ok_or(HostEffectError::BackendFailure)?;
                    let result = fetch
                        .execute_prepared(target, cancellation)
                        .await
                        .map_err(|error| HostEffectError::from(fetch_service_error(&error)))?;
                    Ok(EffectResult::Fetch {
                        status: result.status,
                        headers: Vec::new(),
                        body: result.text,
                        truncated: false,
                    })
                }
                #[cfg(feature = "sandbox")]
                PreparedParentEffect::FetchPending { .. } => Err(HostEffectError::BackendFailure),
            }
        })
    }
}

#[cfg(test)]
fn make_spawn_with_timeout(
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    duration: Duration,
) -> impl Fn(String, Vec<String>) -> rquickjs::Result<SpawnResult> {
    let service = SpawnEffectService::new(sandbox, permission_bridge, duration);
    move |cmd: String, args: Vec<String>| {
        runtime
            .block_on(service.execute(&cmd, &args, PermCancellation::new()))
            .map_err(|error| service_host_error("js/spawn", error))
    }
}

#[cfg(test)]
pub(crate) fn register_host_globals(
    ctx: &Context,
    sandbox: Sandbox,
    permission_bridge: PermissionBridge,
    runtime: tokio::runtime::Handle,
    allow_config: AllowConfig,
    #[cfg(feature = "skills")] skill_gate: SkillCapabilityGate,
) -> rquickjs::Result<()> {
    ctx.with(|ctx| {
        let globals = ctx.globals();

        let read_file = make_read_file(
            permission_bridge.clone(),
            runtime.clone(),
            allow_config.clone(),
        );
        #[cfg(feature = "skills")]
        let read_file = {
            let gate = skill_gate.clone();
            move |path: String| {
                gate.authorize(crate::extras::js::skills::HostCapability::ReadFile)?;
                read_file(path)
            }
        };
        globals.set("read_file", Func::from(read_file))?;
        let write_file = make_write_file(
            permission_bridge.clone(),
            runtime.clone(),
            allow_config.clone(),
        );
        #[cfg(feature = "skills")]
        let write_file = {
            let gate = skill_gate.clone();
            move |path: String, content: String| {
                gate.authorize(crate::extras::js::skills::HostCapability::WriteFile)?;
                write_file(path, content)
            }
        };
        globals.set("write_file", Func::from(write_file))?;
        #[cfg(feature = "sandbox")]
        {
            let fetch = make_fetch(
                permission_bridge.clone(),
                runtime.clone(),
                allow_config.fetch,
            );
            #[cfg(feature = "skills")]
            let fetch = {
                let gate = skill_gate.clone();
                move |url: String, options: Opt<Object<'_>>| {
                    gate.authorize(crate::extras::js::skills::HostCapability::Fetch)?;
                    fetch(url, options)
                }
            };
            globals.set("fetch", Func::from(fetch))?;
        }
        let spawn = make_spawn(sandbox, permission_bridge, runtime);
        #[cfg(feature = "skills")]
        let spawn = {
            let gate = skill_gate.clone();
            move |command: String, arguments: Vec<String>| {
                gate.authorize(crate::extras::js::skills::HostCapability::Spawn)?;
                spawn(command, arguments)
            }
        };
        globals.set("spawn", Func::from(spawn))?;

        let console = Object::new(ctx.clone())?;
        let console_bytes_remaining = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
            CONSOLE_MAX_BYTES_PER_STEP,
        ));
        console.set(
            "log",
            Func::from(move |msg: Value| {
                let text = format!("{msg:?}");
                let len = text.len();
                let remaining = console_bytes_remaining
                    .fetch_update(
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                        |r| r.checked_sub(len),
                    )
                    .unwrap_or(0);
                if remaining > 0 {
                    eprintln!("[js] {text}");
                }
            }),
        )?;
        globals.set("console", console)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    #[cfg(any(feature = "sandbox", feature = "skills"))]
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    #[cfg(feature = "sandbox")]
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    #[cfg(feature = "skills")]
    use std::time::Instant;

    use super::*;
    #[cfg(feature = "sandbox")]
    use crate::extras::js::audit::AuditFailurePoint;
    #[cfg(any(feature = "sandbox", feature = "skills"))]
    use crate::extras::js::audit::{AuditState, EffectAudit};
    #[cfg(any(feature = "sandbox", feature = "skills"))]
    use crate::extras::js::broker::{
        GrantPrincipal, HostCapability, InvocationBroker, InvocationGrant,
    };
    #[cfg(any(feature = "sandbox", feature = "skills"))]
    use crate::extras::js::protocol::{AdvisoryAttribution, EffectRequest, InvocationId};
    #[cfg(feature = "skills")]
    use crate::extras::js::skills::{CapabilityManifest, CapabilityScope, CapabilityTier};
    use crate::extras::js::tool::PermissionBridgeOwner;
    #[cfg(any(feature = "sandbox", feature = "skills"))]
    use crate::paths::AppPaths;
    use crate::permission::ask::{AskSender, UserDecision};
    use crate::permission::checker::{PermCheck, PermissionChecker};
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "zerostack_js_write_permission_test_{}_{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "sandbox")]
    fn test_fetch_limits() -> FetchLimits {
        FetchLimits {
            connect_timeout: Duration::from_millis(100),
            read_timeout: Duration::from_millis(50),
            total_timeout: Duration::from_millis(150),
            request_header_max_bytes: 128,
            request_header_max_count: 8,
            request_body_max_bytes: 1024,
            response_header_max_bytes: 128,
            response_header_max_count: 8,
            response_body_max_bytes: 1024,
        }
    }

    #[cfg(feature = "sandbox")]
    fn serve_fetch_once(
        handler: impl FnOnce(std::net::TcpStream) + Send + 'static,
    ) -> (Url, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handler(stream);
        });
        (
            Url::parse(&format!("http://{address}/start")).unwrap(),
            thread,
        )
    }

    #[cfg(feature = "sandbox")]
    fn read_fetch_request(stream: &mut std::net::TcpStream) {
        use std::io::Read as _;

        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            assert!(request.len() <= 16 * 1024, "test request headers grew");
        }
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_returns_bounded_utf8_response() {
        use std::io::Write as _;

        let (url, server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .unwrap();
        });
        let transport = FetchTransport::new(None).unwrap();

        let result = transport
            .execute(url, &FetchRequest::get(), || false)
            .expect("bounded response should succeed");

        assert_eq!(
            result,
            FetchTransportOutcome::Complete(FetchResult {
                status: 200,
                text: "hello".to_string(),
            })
        );
        server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_binds_the_authorized_resolution_without_dns() {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nbound",
                )
                .unwrap();
        });
        let url = Url::parse(&format!("http://public.invalid:{}/", address.port())).unwrap();
        let transport = FetchTransport::new(Some(("public.invalid", &[address]))).unwrap();

        assert_eq!(
            transport.execute(url, &FetchRequest::get(), || false),
            Ok(FetchTransportOutcome::Complete(FetchResult {
                status: 200,
                text: "bound".to_string(),
            }))
        );
        server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_hands_redirect_to_authorization_layer() {
        use std::io::Write as _;

        let (url, server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let expected = url.join("/next").unwrap();
        let transport = FetchTransport::with_limits(test_fetch_limits(), None).unwrap();

        let result = transport
            .execute(url, &FetchRequest::get(), || false)
            .expect("redirect response should be handed off");

        assert_eq!(result, FetchTransportOutcome::Redirect(expected));
        server.join().unwrap();

        let (not_modified_url, not_modified_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        assert_eq!(
            transport
                .execute(not_modified_url, &FetchRequest::get(), || false)
                .unwrap(),
            FetchTransportOutcome::Complete(FetchResult {
                status: 304,
                text: String::new(),
            })
        );
        not_modified_server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_enforces_header_and_streaming_body_limits() {
        use std::io::Write as _;

        let (header_url, header_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nX-Large: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "x".repeat(256)
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let transport = FetchTransport::with_limits(test_fetch_limits(), None).unwrap();
        assert_eq!(
            transport.execute(header_url, &FetchRequest::get(), || false),
            Err(FetchError::ResponseHeadersTooLarge)
        );
        header_server.join().unwrap();

        let (length_url, length_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        assert_eq!(
            transport.execute(length_url, &FetchRequest::get(), || false),
            Err(FetchError::ResponseBodyTooLarge)
        );
        length_server.join().unwrap();

        let (stream_url, stream_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            for _ in 0..32 {
                if stream
                    .write_all(b"80\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n")
                    .is_err()
                {
                    break;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        });
        assert_eq!(
            transport.execute(stream_url, &FetchRequest::get(), || false),
            Err(FetchError::ResponseBodyTooLarge)
        );
        stream_server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_bounds_slow_headers_and_body_then_recovers() {
        use std::io::Write as _;

        let transport = FetchTransport::with_limits(test_fetch_limits(), None).unwrap();
        let (slow_headers_url, slow_headers_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            std::thread::sleep(Duration::from_millis(100));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        assert_eq!(
            transport.execute(slow_headers_url, &FetchRequest::get(), || false),
            Err(FetchError::TimedOut)
        );
        slow_headers_server.join().unwrap();

        let (slow_body_url, slow_body_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(b"hello");
        });
        assert_eq!(
            transport.execute(slow_body_url, &FetchRequest::get(), || false),
            Err(FetchError::TimedOut)
        );
        slow_body_server.join().unwrap();

        let (endless_url, endless_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            for _ in 0..100 {
                if stream.write_all(b"1\r\nx\r\n").is_err() {
                    break;
                }
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        assert_eq!(
            transport.execute(endless_url, &FetchRequest::get(), || false),
            Err(FetchError::TimedOut)
        );
        endless_server.join().unwrap();

        let (recovery_url, recovery_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        assert_eq!(
            transport.execute(recovery_url, &FetchRequest::get(), || false),
            Ok(FetchTransportOutcome::Complete(FetchResult {
                status: 204,
                text: String::new(),
            }))
        );
        recovery_server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_rejects_encoding_malformed_utf8_and_response() {
        use std::io::Write as _;

        let transport = FetchTransport::with_limits(test_fetch_limits(), None).unwrap();
        let (encoded_url, encoded_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
                )
                .unwrap();
        });
        assert_eq!(
            transport.execute(encoded_url, &FetchRequest::get(), || false),
            Err(FetchError::UnsupportedContentEncoding)
        );
        encoded_server.join().unwrap();

        let (utf8_url, utf8_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n\xff\xfe",
                )
                .unwrap();
        });
        assert_eq!(
            transport.execute(utf8_url, &FetchRequest::get(), || false),
            Err(FetchError::InvalidUtf8)
        );
        utf8_server.join().unwrap();

        let (malformed_url, malformed_server) = serve_fetch_once(|mut stream| {
            read_fetch_request(&mut stream);
            stream.write_all(b"not-http\r\n\r\n").unwrap();
        });
        assert!(matches!(
            transport.execute(malformed_url, &FetchRequest::get(), || false),
            Err(FetchError::RequestFailed(_))
        ));
        malformed_server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_transport_rejects_request_limits_and_cancellation_before_io() {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};

        let transport = FetchTransport::with_limits(test_fetch_limits(), None).unwrap();
        let url = Url::parse("https://example.com/").unwrap();
        let mut request = FetchRequest::get();
        request.body = Some(vec![0; 1025]);
        assert_eq!(
            transport.execute(url.clone(), &request, || false),
            Err(FetchError::RequestBodyTooLarge)
        );

        let mut request = FetchRequest::get();
        request.headers.insert(
            reqwest::header::HeaderName::from_static("x-large"),
            reqwest::header::HeaderValue::from_str(&"x".repeat(256)).unwrap(),
        );
        assert_eq!(
            transport.execute(url.clone(), &request, || false),
            Err(FetchError::RequestHeadersTooLarge)
        );
        assert_eq!(
            transport.execute(url, &FetchRequest::get(), || true),
            Err(FetchError::Cancelled)
        );

        let cancellation = Arc::new(AtomicBool::new(false));
        let server_cancellation = cancellation.clone();
        let (cancel_url, cancel_server) = serve_fetch_once(move |mut stream| {
            read_fetch_request(&mut stream);
            server_cancellation.store(true, Ordering::Release);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .unwrap();
        });
        assert_eq!(
            transport.execute(cancel_url, &FetchRequest::get(), || {
                cancellation.load(Ordering::Acquire)
            }),
            Err(FetchError::Cancelled)
        );
        cancel_server.join().unwrap();
    }

    #[cfg(feature = "sandbox")]
    #[derive(Default)]
    struct FakeFetchResolver {
        responses: Mutex<std::collections::VecDeque<Result<Vec<SocketAddr>, FetchError>>>,
    }

    #[cfg(feature = "sandbox")]
    impl FakeFetchResolver {
        fn new(responses: Vec<Result<Vec<SocketAddr>, FetchError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[cfg(feature = "sandbox")]
    impl FetchResolver for FakeFetchResolver {
        fn resolve(
            &self,
            _host: &str,
            _port: u16,
            _permission_bridge: &PermissionBridge,
        ) -> Result<Vec<SocketAddr>, FetchError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("fake resolver response exhausted")
        }
    }

    #[cfg(feature = "sandbox")]
    #[derive(Default)]
    struct FakeFetchSender {
        responses: Mutex<std::collections::VecDeque<Result<FetchTransportOutcome, FetchError>>>,
        calls: Mutex<Vec<(Url, Vec<SocketAddr>)>>,
    }

    #[cfg(feature = "sandbox")]
    impl FakeFetchSender {
        fn new(responses: Vec<Result<FetchTransportOutcome, FetchError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[cfg(feature = "sandbox")]
    impl FetchSender for FakeFetchSender {
        fn send(
            &self,
            url: Url,
            _request: &FetchRequest,
            addresses: &[SocketAddr],
            _permission_bridge: &PermissionBridge,
        ) -> Result<FetchTransportOutcome, FetchError> {
            self.calls.lock().unwrap().push((url, addresses.to_vec()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("fake sender response exhausted")
        }
    }

    #[cfg(feature = "sandbox")]
    struct BlockingFetchSender {
        started: Arc<tokio::sync::Notify>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    #[cfg(feature = "sandbox")]
    impl FetchSender for BlockingFetchSender {
        fn send(
            &self,
            _url: Url,
            _request: &FetchRequest,
            _addresses: &[SocketAddr],
            _permission_bridge: &PermissionBridge,
        ) -> Result<FetchTransportOutcome, FetchError> {
            self.started.notify_one();
            let (released, wake) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Ok(completed_fetch())
        }
    }

    #[cfg(feature = "sandbox")]
    fn fetch_permission(action: Action) -> PermCheck {
        let config = PermissionConfig {
            js_fetch: Some(ToolPerm::Simple(action)),
            doom_loop: Some(Action::Allow),
            ..PermissionConfig::default()
        };
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(std::env::current_dir().unwrap()),
            Some(vec!["standard".to_string()]),
        )))
    }

    #[cfg(feature = "sandbox")]
    fn public_address() -> SocketAddr {
        "93.184.216.34:443".parse().unwrap()
    }

    #[cfg(feature = "sandbox")]
    fn completed_fetch() -> FetchTransportOutcome {
        FetchTransportOutcome::Complete(FetchResult {
            status: 200,
            text: "ok".to_string(),
        })
    }

    #[cfg(feature = "sandbox")]
    fn brokered_fetch(
        directory: &TempDir,
        tag: &str,
        responses: Vec<Result<FetchTransportOutcome, FetchError>>,
    ) -> (
        InvocationBroker<ParentHostEffectService>,
        EffectRequest,
        Arc<FakeFetchSender>,
        PermissionBridgeOwner,
    ) {
        let owner =
            PermissionBridgeOwner::new(Some(fetch_permission(Action::Allow)), None, STEP_TIMEOUT);
        let sender = Arc::new(FakeFetchSender::new(responses));
        let fetch = FetchEffectService {
            executor: Arc::new(FetchExecutor {
                policy: FetchPolicy::from_settings(None, false),
                resolver: Arc::new(FakeFetchResolver::new(vec![
                    Ok(vec![public_address()]),
                    Ok(vec![public_address()]),
                ])),
                sender: sender.clone(),
                permission_bridge: owner.bridge(),
            }),
            permission_bridge: owner.bridge(),
            runtime: tokio::runtime::Handle::current(),
            timeout: Duration::from_secs(1),
        };
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        )
        .with_fetch(fetch);
        let invocation = InvocationId::new(format!("fetch-{tag}")).unwrap();
        let grant = InvocationGrant::issue(
            invocation.clone(),
            GrantPrincipal::ModelAuthored {
                tool_call_id: format!("call-{tag}"),
            },
            BTreeSet::from([HostCapability::Fetch]),
            Instant::now() + Duration::from_secs(10),
        );
        let request = EffectRequest {
            effect_ordinal: 0,
            grant_id: grant.grant_id().clone(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::Fetch {
                url: "https://example.com/start".to_string(),
                method: crate::extras::js::protocol::HttpMethod::Get,
                headers: Vec::new(),
                body: None,
            },
        };
        let audit_root = directory.path().join(format!("audit-{tag}"));
        let audit = EffectAudit::open(
            AppPaths {
                config_dir: audit_root.join("config"),
                data_dir: audit_root.join("data"),
                local_data_dir: audit_root.join("local"),
                state_dir: audit_root.join("state"),
                cache_dir: audit_root.join("cache"),
                credentials_dir: audit_root.join("credentials"),
                project_dir: None,
            }
            .effect_audit(),
        )
        .unwrap();
        let broker = InvocationBroker::new(
            invocation,
            vec![grant],
            BTreeSet::from([HostCapability::Fetch]),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        (broker, request, sender, owner)
    }

    #[cfg(feature = "skills")]
    fn scoped_host_broker(
        directory: &TempDir,
        tag: &str,
        service: ParentHostEffectService,
        scope: CapabilityScope,
        operation: EffectOperation,
    ) -> (InvocationBroker<ParentHostEffectService>, EffectRequest) {
        let invocation = InvocationId::new(format!("scoped-{tag}")).unwrap();
        let principal = GrantPrincipal::Skill {
            artifact_id: format!("artifact-{tag}"),
            export: "run".to_string(),
            invocation_id: invocation.to_string(),
        };
        let grant = InvocationGrant::issue_scoped_skill(
            invocation.clone(),
            principal,
            CapabilityManifest::new(CapabilityTier::SideEffecting, vec![scope]).unwrap(),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        let request = EffectRequest {
            effect_ordinal: 0,
            grant_id: grant.grant_id().clone(),
            advisory: AdvisoryAttribution {
                artifact_id: Some(format!("artifact-{tag}")),
                export: Some("run".to_string()),
            },
            operation,
        };
        let capability = match &request.operation {
            EffectOperation::ReadFile { .. } => HostCapability::ReadFile,
            EffectOperation::WriteFile { .. } => HostCapability::WriteFile,
            EffectOperation::Fetch { .. } => HostCapability::Fetch,
            EffectOperation::Spawn { .. } => HostCapability::Spawn,
            EffectOperation::ProposeSkill { .. } => HostCapability::ProposeSkill,
        };
        let audit_root = directory.path().join(format!("audit-{tag}"));
        let audit = EffectAudit::open(
            AppPaths {
                config_dir: audit_root.join("config"),
                data_dir: audit_root.join("data"),
                local_data_dir: audit_root.join("local"),
                state_dir: audit_root.join("state"),
                cache_dir: audit_root.join("cache"),
                credentials_dir: audit_root.join("credentials"),
                project_dir: None,
            }
            .effect_audit(),
        )
        .unwrap();
        let broker = InvocationBroker::new(
            invocation,
            vec![grant],
            BTreeSet::from([capability]),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        (broker, request)
    }

    #[cfg(all(feature = "skills", unix))]
    #[tokio::test]
    async fn scoped_capability_intersection_real_host_file_scope_is_symlink_and_race_safe() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new();
        let workspace = directory.path().join("workspace");
        let allowed = workspace.join("allowed");
        let denied = workspace.join("denied");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        let denied_target = denied.join("secret.txt");
        std::fs::write(&denied_target, "secret").unwrap();
        let escape_link = workspace.join("looks-allowed.txt");
        symlink(&denied_target, &escape_link).unwrap();

        let permission = host_permission(workspace.clone(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(2);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(&workspace),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let (mut broker, request) = scoped_host_broker(
            &directory,
            "symlink-denied",
            service,
            CapabilityScope::ReadFile {
                workspace_prefixes: vec!["allowed".to_string()],
            },
            EffectOperation::ReadFile {
                path: escape_link.to_string_lossy().into_owned(),
            },
        );
        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::ManifestDenied)
        );
        assert!(
            ask_rx.try_recv().is_err(),
            "manifest denial reached permission"
        );
        assert!(broker.audit_records_for_test().is_empty());

        let source = allowed.join("source.txt");
        let original = allowed.join("original.txt");
        std::fs::write(&source, "approved identity").unwrap();
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(&workspace),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let (mut broker, request) = scoped_host_broker(
            &directory,
            "race-denied",
            service,
            CapabilityScope::ReadFile {
                workspace_prefixes: vec!["allowed".to_string()],
            },
            EffectOperation::ReadFile {
                path: source.to_string_lossy().into_owned(),
            },
        );
        let dispatch = tokio::spawn(async move {
            let result = broker.dispatch(request, PermCancellation::new()).await;
            (result, broker)
        });
        let prompt = ask_rx.recv().await.expect("read should request permission");
        std::fs::rename(&source, &original).unwrap();
        symlink(&denied_target, &source).unwrap();
        prompt.reply.send(UserDecision::AllowOnce).unwrap();
        let (result, broker) = dispatch.await.unwrap();
        assert_eq!(result, Err(HostEffectError::InvalidTarget));
        let records = broker.audit_records_for_test();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].state, AuditState::Intent);
        assert_eq!(records[1].state, AuditState::Completed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn brokered_spawn_replacement_during_ask_executes_nothing_and_records_no_intent() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let executable = directory.path().join("approved-command");
        let replacement = directory.path().join("replacement-command");
        let marker = directory.path().join("must-not-exist");
        let script = |label: &str| {
            format!(
                "#!/bin/sh\nprintf '%s' '{label}' > '{}'\n",
                marker.to_string_lossy()
            )
        };
        std::fs::write(&executable, script("approved")).unwrap();
        std::fs::write(&replacement, script("replacement")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();

        let permission =
            host_permission(directory.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let invocation = InvocationId::new("spawn-ask-replacement").unwrap();
        let grant = InvocationGrant::issue(
            invocation.clone(),
            GrantPrincipal::ModelAuthored {
                tool_call_id: "spawn-replacement".into(),
            },
            BTreeSet::from([HostCapability::Spawn]),
            Instant::now() + Duration::from_secs(10),
        );
        let request = EffectRequest {
            effect_ordinal: 0,
            grant_id: grant.grant_id().clone(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::Spawn {
                program: executable.to_string_lossy().into_owned(),
                arguments: vec![],
            },
        };
        let audit_root = directory.path().join("audit-spawn-replacement");
        let audit = EffectAudit::open(
            AppPaths {
                config_dir: audit_root.join("config"),
                data_dir: audit_root.join("data"),
                local_data_dir: audit_root.join("local"),
                state_dir: audit_root.join("state"),
                cache_dir: audit_root.join("cache"),
                credentials_dir: audit_root.join("credentials"),
                project_dir: None,
            }
            .effect_audit(),
        )
        .unwrap();
        let mut broker = InvocationBroker::new(
            invocation,
            vec![grant],
            BTreeSet::from([HostCapability::Spawn]),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        let dispatch = tokio::spawn(async move {
            let result = broker.dispatch(request, PermCancellation::new()).await;
            (result, broker)
        });
        let prompt = ask_rx
            .recv()
            .await
            .expect("spawn should request permission");
        std::fs::rename(&replacement, &executable).unwrap();
        prompt.reply.send(UserDecision::AllowOnce).unwrap();

        let (result, broker) = dispatch.await.unwrap();
        assert_eq!(result, Err(HostEffectError::InvalidTarget));
        assert!(
            !marker.exists(),
            "an executable ran after its identity changed"
        );
        assert!(
            broker.audit_records_for_test().is_empty(),
            "a denied replacement must not acquire a durable authorized intent"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a real Linux bubblewrap backend"]
    async fn linux_spawn_executes_sealed_script_snapshot_after_in_place_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let executable = directory.path().join("approved-command");
        std::fs::write(&executable, "#!/bin/sh\nprintf approved").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let prepared =
            PreparedSpawnEffect::capture(executable.to_string_lossy().as_ref(), Vec::new(), true)
                .unwrap();
        let PreparedSpawnTarget::SealedSnapshot(snapshot) = &prepared.target else {
            panic!("Linux immutable capture must produce a sealed snapshot");
        };
        verify_executable_snapshot_seals(snapshot).unwrap();
        let sandbox = Sandbox::new(true, "bwrap").with_shell("/definitely/not-a-shell");
        let mut command = sandbox
            .wrap_command_with_executable_snapshot(&prepared.arguments)
            .unwrap();
        provide_snapshot_fd(&mut command, snapshot);

        std::fs::write(&executable, "#!/bin/sh\nprintf replacement").unwrap();
        let output = sandbox
            .output_built_command_with_limits(
                command,
                CommandLimits {
                    timeout: Duration::from_secs(1),
                    stdout_bytes: 1024,
                    stderr_bytes: 1024,
                    combined_bytes: 2048,
                },
            )
            .await
            .unwrap();
        assert_eq!(output.status, CommandStatus::Completed);
        assert_eq!(output.stdout, b"approved");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a real Linux bubblewrap backend"]
    async fn linux_spawn_executes_sealed_elf_snapshot_without_a_shell() {
        let prepared = PreparedSpawnEffect::capture(
            "sh",
            vec![
                "-c".to_string(),
                "test ! -e /proc/self/fd/3 || exit 70; /bin/sh -c 'test ! -e /proc/self/fd/3' || exit 71; printf elf-snapshot".to_string(),
            ],
            true,
        )
        .unwrap();
        let PreparedSpawnTarget::SealedSnapshot(snapshot) = &prepared.target else {
            panic!("Linux immutable capture must produce a sealed snapshot");
        };
        let sandbox = Sandbox::new(true, "bwrap").with_shell("/definitely/not-a-shell");
        let mut command = sandbox
            .wrap_command_with_executable_snapshot(&prepared.arguments)
            .unwrap();
        provide_snapshot_fd(&mut command, snapshot);
        let output = sandbox
            .output_built_command_with_limits(
                command,
                CommandLimits {
                    timeout: Duration::from_secs(1),
                    stdout_bytes: 1024,
                    stderr_bytes: 1024,
                    combined_bytes: 2048,
                },
            )
            .await
            .unwrap();
        assert_eq!(output.status, CommandStatus::Completed);
        assert_eq!(output.stdout, b"elf-snapshot");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a real Linux bubblewrap backend"]
    async fn linux_spawn_in_place_overwrite_during_ask_executes_original_snapshot() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new();
        let executable = directory.path().join("approved-command");
        std::fs::write(&executable, "#!/bin/sh\nprintf original-snapshot").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let permission =
            host_permission(directory.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(true, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let invocation = InvocationId::new("spawn-ask-in-place-overwrite").unwrap();
        let grant = InvocationGrant::issue(
            invocation.clone(),
            GrantPrincipal::ModelAuthored {
                tool_call_id: "spawn-snapshot-overwrite".into(),
            },
            BTreeSet::from([HostCapability::Spawn]),
            Instant::now() + Duration::from_secs(10),
        );
        let request = EffectRequest {
            effect_ordinal: 0,
            grant_id: grant.grant_id().clone(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::Spawn {
                program: executable.to_string_lossy().into_owned(),
                arguments: vec![],
            },
        };
        let audit_root = directory.path().join("audit-spawn-snapshot-overwrite");
        let audit = EffectAudit::open(
            AppPaths {
                config_dir: audit_root.join("config"),
                data_dir: audit_root.join("data"),
                local_data_dir: audit_root.join("local"),
                state_dir: audit_root.join("state"),
                cache_dir: audit_root.join("cache"),
                credentials_dir: audit_root.join("credentials"),
                project_dir: None,
            }
            .effect_audit(),
        )
        .unwrap();
        let mut broker = InvocationBroker::new(
            invocation,
            vec![grant],
            BTreeSet::from([HostCapability::Spawn]),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();
        let dispatch = tokio::spawn(async move {
            let result = broker.dispatch(request, PermCancellation::new()).await;
            (result, broker)
        });
        let prompt = ask_rx.recv().await.expect("spawn should block at Ask");
        std::fs::write(&executable, "#!/bin/sh\nprintf replacement-bytes").unwrap();
        prompt.reply.send(UserDecision::AllowOnce).unwrap();
        let (result, broker) = dispatch.await.unwrap();
        assert!(matches!(
            result,
            Ok(EffectResult::Spawn { stdout, exit_code: 0, .. })
                if stdout == "original-snapshot"
        ));
        assert_eq!(broker.audit_records_for_test().len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_oversized_snapshot_failure_closes_the_memfd() {
        let directory = TempDir::new();
        let executable = directory.path().join("oversized-command");
        let file = std::fs::File::create(&executable).unwrap();
        file.set_len(crate::extras::js::broker::MAX_SPAWN_EXECUTABLE_BYTES + 1)
            .unwrap();
        drop(file);
        let mut source = std::fs::File::open(&executable).unwrap();
        let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        assert!(matches!(
            create_sealed_executable_snapshot(&mut source),
            Err(EffectServiceError::InvalidTarget)
        ));
        let after = std::fs::read_dir("/proc/self/fd").unwrap().count();
        assert_eq!(after, before, "failed snapshot capture leaked a descriptor");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_bwrap_construction_failure_closes_snapshot_without_spawning() {
        let before = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let prepared = PreparedSpawnEffect::capture("printf", Vec::new(), true).unwrap();
        let captured = std::fs::read_dir("/proc/self/fd").unwrap().count();
        assert_eq!(captured, before + 1, "capture should own exactly one memfd");
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let service = SpawnEffectService::new(
            Sandbox::new(true, "__unavailable_snapshot_backend__"),
            owner.bridge(),
            Duration::from_secs(1),
        );
        assert!(matches!(
            service.execute_prepared(prepared, owner.bridge()).await,
            Err(EffectServiceError::BackendFailure)
        ));
        let after = std::fs::read_dir("/proc/self/fd").unwrap().count();
        assert_eq!(after, before, "failed bwrap construction leaked a memfd");
    }

    #[cfg(all(feature = "skills", target_os = "linux"))]
    #[tokio::test]
    async fn learned_skill_spawn_fails_closed_on_linux_without_snapshot_backend() {
        let directory = TempDir::new();
        let permission =
            host_permission(directory.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let (mut broker, request) = scoped_host_broker(
            &directory,
            "linux-disabled-spawn",
            service,
            CapabilityScope::Spawn {
                programs: vec!["printf".to_string()],
            },
            EffectOperation::Spawn {
                program: "printf".to_string(),
                arguments: vec!["denied".to_string()],
            },
        );
        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::BackendFailure)
        );
        assert!(ask_rx.try_recv().is_err());
        assert!(broker.audit_records_for_test().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn model_authored_spawn_remains_available_without_identity_preserving_dispatch() {
        let directory = TempDir::new();
        let permission =
            host_permission(directory.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let invocation = InvocationId::new("model-spawn-non-preserving").unwrap();
        let grant = InvocationGrant::issue(
            invocation.clone(),
            GrantPrincipal::ModelAuthored {
                tool_call_id: "model-spawn".into(),
            },
            BTreeSet::from([HostCapability::Spawn]),
            Instant::now() + Duration::from_secs(10),
        );
        let request = EffectRequest {
            effect_ordinal: 0,
            grant_id: grant.grant_id().clone(),
            advisory: AdvisoryAttribution::default(),
            operation: EffectOperation::Spawn {
                program: "printf".into(),
                arguments: vec!["available".into()],
            },
        };
        let audit_root = directory.path().join("audit-model-spawn");
        let audit = EffectAudit::open(
            AppPaths {
                config_dir: audit_root.join("config"),
                data_dir: audit_root.join("data"),
                local_data_dir: audit_root.join("local"),
                state_dir: audit_root.join("state"),
                cache_dir: audit_root.join("cache"),
                credentials_dir: audit_root.join("credentials"),
                project_dir: None,
            }
            .effect_audit(),
        )
        .unwrap();
        let mut broker = InvocationBroker::new(
            invocation,
            vec![grant],
            BTreeSet::from([HostCapability::Spawn]),
            service,
            Arc::new(Mutex::new(audit)),
        )
        .unwrap();

        let dispatch = tokio::spawn(async move {
            let result = broker.dispatch(request, PermCancellation::new()).await;
            (result, broker)
        });
        let prompt = ask_rx.recv().await.expect("model spawn should reach Ask");
        prompt.reply.send(UserDecision::AllowOnce).unwrap();
        let (result, broker) = dispatch.await.unwrap();
        let result = result.unwrap();
        assert!(matches!(
            result,
            EffectResult::Spawn {
                stdout,
                exit_code: 0,
                ..
            } if stdout == "available"
        ));
        assert_eq!(broker.audit_records_for_test().len(), 2);
    }

    #[cfg(all(feature = "skills", target_os = "macos"))]
    #[tokio::test]
    async fn learned_skill_spawn_fails_closed_on_macos_before_permission_or_audit() {
        let directory = TempDir::new();
        let permission =
            host_permission(directory.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let (mut broker, request) = scoped_host_broker(
            &directory,
            "macos-spawn-denied",
            service,
            CapabilityScope::Spawn {
                programs: vec!["printf".into()],
            },
            EffectOperation::Spawn {
                program: "printf".into(),
                arguments: vec!["must-not-run".into()],
            },
        );

        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::BackendFailure)
        );
        assert!(ask_rx.try_recv().is_err());
        assert!(broker.audit_records_for_test().is_empty());
    }

    #[cfg(all(feature = "skills", windows))]
    #[tokio::test]
    async fn learned_skill_spawn_fails_closed_on_windows_before_permission_or_audit() {
        let directory = TempDir::new();
        let permission =
            host_permission(directory.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(Some(permission), Some(ask_tx), STEP_TIMEOUT);
        let service = ParentHostEffectService::new(
            FileEffectService::new(
                owner.bridge(),
                AllowConfig::unrestricted(directory.path()),
                Duration::from_secs(1),
            ),
            SpawnEffectService::new(
                Sandbox::new(false, "bwrap"),
                owner.bridge(),
                Duration::from_secs(1),
            ),
        );
        let (mut broker, request) = scoped_host_broker(
            &directory,
            "windows-spawn-denied",
            service,
            CapabilityScope::Spawn {
                programs: vec!["cmd".into()],
            },
            EffectOperation::Spawn {
                program: "cmd".into(),
                arguments: vec!["/C".into(), "exit".into(), "0".into()],
            },
        );

        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::BackendFailure)
        );
        assert!(ask_rx.try_recv().is_err());
        assert!(broker.audit_records_for_test().is_empty());
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_brokered_fetch_redirect_has_no_unaudited_second_send() {
        let directory = TempDir::new();
        let redirect = Url::parse("https://example.com/redirected").unwrap();
        let (mut broker, request, sender, _owner) = brokered_fetch(
            &directory,
            "redirect",
            vec![
                Ok(FetchTransportOutcome::Redirect(redirect)),
                Ok(completed_fetch()),
            ],
        );

        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::TargetDenied)
        );
        assert_eq!(
            sender.call_count(),
            1,
            "redirect send was not independently audited"
        );
        let records = broker.audit_records_for_test();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].state, AuditState::Intent);
        assert_eq!(records[1].state, AuditState::Completed);

        let (mut broker, request, sender, _owner) = brokered_fetch(
            &directory,
            "redirect-audit-failure",
            vec![Ok(FetchTransportOutcome::Redirect(
                Url::parse("https://example.com/redirected").unwrap(),
            ))],
        );
        broker.fail_next_audit_durability_for_test(AuditFailurePoint::Append);
        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::AuditFailure)
        );
        assert_eq!(
            sender.call_count(),
            0,
            "audit failure reached the redirect chain"
        );
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_brokered_fetch_persists_real_outcome_unknown() {
        let directory = TempDir::new();
        let (mut broker, request, sender, _owner) = brokered_fetch(
            &directory,
            "outcome-unknown",
            vec![Err(FetchError::OutcomeUnknown)],
        );

        assert_eq!(
            broker.dispatch(request, PermCancellation::new()).await,
            Err(HostEffectError::OutcomeUnknown)
        );
        assert_eq!(sender.call_count(), 1);
        let records = broker.audit_records_for_test();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].state, AuditState::Intent);
        assert_eq!(records[1].state, AuditState::OutcomeUnknown);
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_effect_service_is_cancellable_and_recovers_after_target_error() {
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let sender = Arc::new(FakeFetchSender::new(vec![Ok(completed_fetch())]));
        let service = FetchEffectService {
            executor: Arc::new(FetchExecutor {
                policy: FetchPolicy::from_settings(None, false),
                resolver: Arc::new(FakeFetchResolver::new(vec![Ok(vec![public_address()])])),
                sender: sender.clone(),
                permission_bridge: owner.bridge(),
            }),
            permission_bridge: owner.bridge(),
            runtime: tokio::runtime::Handle::current(),
            timeout: Duration::from_secs(1),
        };

        assert_eq!(
            service
                .execute(
                    "not a url".to_string(),
                    FetchRequest::get(),
                    PermCancellation::new(),
                )
                .await,
            Err(FetchError::InvalidUrl)
        );
        let cancellation = PermCancellation::new();
        cancellation.cancel();
        assert_eq!(
            service
                .execute(
                    "https://example.com/".to_string(),
                    FetchRequest::get(),
                    cancellation,
                )
                .await,
            Err(FetchError::Cancelled)
        );
        assert_eq!(
            service
                .execute(
                    "https://example.com/".to_string(),
                    FetchRequest::get(),
                    PermCancellation::new(),
                )
                .await,
            Ok(FetchResult {
                status: 200,
                text: "ok".to_string(),
            })
        );
        assert_eq!(sender.call_count(), 1);
    }

    #[cfg(feature = "sandbox")]
    async fn run_fake_fetch(
        policy: FetchPolicy,
        resolution: Vec<Result<Vec<SocketAddr>, FetchError>>,
        responses: Vec<Result<FetchTransportOutcome, FetchError>>,
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        permission_timeout: Duration,
        raw_url: &str,
    ) -> (Result<FetchResult, FetchError>, Arc<FakeFetchSender>) {
        run_fake_fetch_request(
            policy,
            resolution,
            responses,
            permission,
            ask_tx,
            permission_timeout,
            raw_url,
            FetchRequest::get(),
        )
        .await
    }

    #[cfg(feature = "sandbox")]
    #[allow(clippy::too_many_arguments)]
    async fn run_fake_fetch_request(
        policy: FetchPolicy,
        resolution: Vec<Result<Vec<SocketAddr>, FetchError>>,
        responses: Vec<Result<FetchTransportOutcome, FetchError>>,
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        permission_timeout: Duration,
        raw_url: &str,
        request: FetchRequest,
    ) -> (Result<FetchResult, FetchError>, Arc<FakeFetchSender>) {
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, permission_timeout);
        let sender = Arc::new(FakeFetchSender::new(responses));
        let executor = FetchExecutor {
            policy,
            resolver: Arc::new(FakeFetchResolver::new(resolution)),
            sender: sender.clone(),
            permission_bridge: owner.bridge(),
        };
        let raw_url = raw_url.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            executor.execute(&raw_url, &request)
        })
        .await
        .expect("fake fetch task panicked");
        (result, sender)
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_host_call_bounds_dispatched_request_and_leaves_js_context_usable() {
        let outer_timeout = Duration::from_millis(25);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let owner =
            PermissionBridgeOwner::new(Some(fetch_permission(Action::Allow)), None, STEP_TIMEOUT);
        let executor = Arc::new(FetchExecutor {
            policy: FetchPolicy::from_settings(None, false),
            resolver: Arc::new(FakeFetchResolver::new(vec![Ok(vec![public_address()])])),
            sender: Arc::new(BlockingFetchSender {
                started: started.clone(),
                release: release.clone(),
            }),
            permission_bridge: owner.bridge(),
        });
        let fetch = make_fetch_with_timeout(
            owner.bridge(),
            tokio::runtime::Handle::current(),
            executor,
            outer_timeout,
        );

        let host_call = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            let runtime = rquickjs::Runtime::new().expect("create QuickJS runtime");
            let context = Context::full(&runtime).expect("create QuickJS context");
            context.with(|ctx| {
                ctx.globals()
                    .set("fetch", Func::from(fetch))
                    .expect("install fetch host global");
                assert!(matches!(
                    ctx.eval::<(), _>("fetch('https://example.com/')"),
                    Err(rquickjs::Error::Exception)
                ));
                let error = ctx
                    .catch()
                    .as_exception()
                    .expect("fetch host error must be an exception")
                    .message()
                    .expect("fetch host error must have a message");
                let recovery = ctx
                    .eval::<i32, _>("6 * 7")
                    .expect("subsequent JS evaluation must remain usable");
                (error, recovery)
            })
        });

        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("blocked sender must start");
        let started_at = Instant::now();
        let completed = tokio::time::timeout(Duration::from_secs(1), host_call).await;

        let (released, wake) = &*release;
        *released.lock().unwrap() = true;
        wake.notify_one();

        let (error, recovery) = completed
            .expect("outer fetch timeout must bound the host call")
            .expect("fetch host-call task panicked");
        assert!(
            started_at.elapsed() < Duration::from_millis(500),
            "outer fetch timeout exceeded the host-call bound"
        );

        assert!(
            error.contains(&FetchError::OutcomeUnknown.to_string()),
            "unexpected dispatched-fetch timeout error: {error}"
        );
        assert_eq!(recovery, 42);
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_timeout_cancels_delayed_permission_before_dispatch_and_reaps_executor() {
        let outer_timeout = Duration::from_millis(25);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let owner = PermissionBridgeOwner::new(
            Some(fetch_permission(Action::Ask)),
            Some(ask_tx),
            STEP_TIMEOUT,
        );
        let sender = Arc::new(FakeFetchSender::new(vec![Ok(completed_fetch())]));
        let executor = Arc::new(FetchExecutor {
            policy: FetchPolicy::from_settings(None, false),
            resolver: Arc::new(FakeFetchResolver::new(vec![Ok(vec![public_address()])])),
            sender: sender.clone(),
            permission_bridge: owner.bridge(),
        });
        let fetch = make_fetch_with_timeout(
            owner.bridge(),
            tokio::runtime::Handle::current(),
            executor.clone(),
            outer_timeout,
        );

        let host_call = tokio::task::spawn_blocking(move || {
            fetch("https://example.com/".to_string(), Opt(None))
                .expect_err("permission wait must exceed the outer timeout")
                .to_string()
        });
        let permission = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
            .await
            .expect("fetch must reach permission before its outer timeout")
            .expect("permission channel closed");
        let error = tokio::time::timeout(Duration::from_secs(1), host_call)
            .await
            .expect("outer timeout must return")
            .expect("fetch host-call task panicked");
        let retained_executor_after_timeout = Arc::strong_count(&executor);

        let _ = permission.reply.send(UserDecision::AllowOnce);
        tokio::time::timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&executor) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fetch executor did not finish after the permission resolved");

        assert!(error.contains(&FetchError::TimedOut.to_string()));
        assert_eq!(
            retained_executor_after_timeout, 1,
            "timed-out host call left a detached executor"
        );
        assert_eq!(
            sender.call_count(),
            0,
            "permission approved after timeout reached the network sender"
        );
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_host_call_cancellation_marks_dispatched_request_outcome_unknown() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let owner =
            PermissionBridgeOwner::new(Some(fetch_permission(Action::Allow)), None, STEP_TIMEOUT);
        let executor = Arc::new(FetchExecutor {
            policy: FetchPolicy::from_settings(None, false),
            resolver: Arc::new(FakeFetchResolver::new(vec![Ok(vec![public_address()])])),
            sender: Arc::new(BlockingFetchSender {
                started: started.clone(),
                release: release.clone(),
            }),
            permission_bridge: owner.bridge(),
        });
        let fetch = make_fetch_with_timeout(
            owner.bridge(),
            tokio::runtime::Handle::current(),
            executor,
            FETCH_TOTAL_TIMEOUT,
        );
        let host_call = tokio::task::spawn_blocking(move || {
            fetch("https://example.com/".to_string(), Opt(None))
                .expect_err("cancelled fetch executor must not complete")
                .to_string()
        });

        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("blocked sender must start");
        owner.shutdown();
        let completed = tokio::time::timeout(Duration::from_secs(1), host_call).await;

        let (released, wake) = &*release;
        *released.lock().unwrap() = true;
        wake.notify_one();

        let error = completed
            .expect("fetch cancellation must bound the host call")
            .expect("fetch host-call task panicked");
        assert!(
            error.contains(&FetchError::OutcomeUnknown.to_string()),
            "unexpected dispatched-fetch cancellation error: {error}"
        );
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_host_call_preserves_completed_fetch() {
        let owner =
            PermissionBridgeOwner::new(Some(fetch_permission(Action::Allow)), None, STEP_TIMEOUT);
        let executor = Arc::new(FetchExecutor {
            policy: FetchPolicy::from_settings(None, false),
            resolver: Arc::new(FakeFetchResolver::new(vec![Ok(vec![public_address()])])),
            sender: Arc::new(FakeFetchSender::new(vec![Ok(completed_fetch())])),
            permission_bridge: owner.bridge(),
        });
        let fetch = make_fetch_with_timeout(
            owner.bridge(),
            tokio::runtime::Handle::current(),
            executor,
            FETCH_TOTAL_TIMEOUT,
        );

        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            fetch("https://example.com/".to_string(), Opt(None))
        })
        .await
        .expect("fetch host-call task panicked")
        .expect("completed fetch must survive the outer host-call wrapper");

        assert_eq!(
            result,
            FetchResult {
                status: 200,
                text: "ok".to_string(),
            }
        );
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_ssrf_policy_rejects_special_ip_classes_and_mapped_forms() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.100.100.200",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "3fff::1",
            "5f00::1",
        ] {
            assert!(
                !is_public_ip(address.parse().unwrap()),
                "{address} must be denied"
            );
        }
        for address in ["8.8.8.8", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(
                is_public_ip(address.parse().unwrap()),
                "{address} should be public"
            );
        }
    }

    #[cfg(feature = "sandbox")]
    #[test]
    fn js_fetch_ssrf_policy_normalizes_origins_and_rejects_ambiguous_urls() {
        let origins = vec!["https://example.com".to_string()];
        let policy = FetchPolicy::from_settings(Some(&origins), false);
        assert_eq!(
            policy.authorize("https://EXAMPLE.com:443/a?b=1").unwrap(),
            Url::parse("https://example.com/a?b=1").unwrap()
        );
        assert_eq!(
            policy.authorize("https://example.com.evil/a"),
            Err(FetchError::OriginDenied)
        );
        assert_eq!(
            policy.authorize("http://example.com/a"),
            Err(FetchError::SchemeDenied)
        );
        assert_eq!(
            policy.authorize("ftp://example.com/a"),
            Err(FetchError::SchemeDenied)
        );
        assert_eq!(
            policy.authorize("https://user:secret@example.com/a"),
            Err(FetchError::EmbeddedCredentials)
        );
        assert_eq!(
            policy.authorize("https://example.com/a#fragment"),
            Err(FetchError::FragmentDenied)
        );
        assert_eq!(
            policy.authorize("https://example.com./a"),
            Err(FetchError::InvalidHost)
        );

        let http_origins = vec!["http://example.com:8080".to_string()];
        let http_policy = FetchPolicy::from_settings(Some(&http_origins), true);
        assert!(http_policy.authorize("http://example.com:8080/a").is_ok());
        assert_eq!(
            http_policy.authorize("http://example.com/a"),
            Err(FetchError::OriginDenied)
        );
        assert_eq!(
            normalize_fetch_url("http://2130706433/", true)
                .unwrap()
                .host_str(),
            Some("127.0.0.1")
        );
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_ssrf_policy_denies_private_mixed_permission_and_rebinding_before_io() {
        let policy = FetchPolicy::from_settings(None, false);
        let private = "127.0.0.1:443".parse().unwrap();
        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(vec![private])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert_eq!(result, Err(FetchError::DestinationDenied));
        assert_eq!(sender.call_count(), 0);

        let excessive_addresses = (1..=FETCH_MAX_DESTINATION_ADDRESSES + 1)
            .map(|last| {
                SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(8, 8, 8, u8::try_from(last).unwrap())),
                    443,
                )
            })
            .collect();
        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(excessive_addresses)],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert_eq!(result, Err(FetchError::TooManyDestinations));
        assert_eq!(sender.call_count(), 0);

        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(vec![public_address(), private])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert_eq!(result, Err(FetchError::DestinationDenied));
        assert_eq!(sender.call_count(), 0);

        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(vec![public_address()])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Deny),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert!(matches!(result, Err(FetchError::Permission(_))));
        assert_eq!(sender.call_count(), 0);

        let redirect = Url::parse("https://example.com/again").unwrap();
        let (result, sender) = run_fake_fetch(
            policy,
            vec![Ok(vec![public_address()]), Ok(vec![private])],
            vec![Ok(FetchTransportOutcome::Redirect(redirect))],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert_eq!(result, Err(FetchError::DestinationDenied));
        assert_eq!(sender.call_count(), 1);
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_ssrf_policy_reauthorizes_redirects_and_caps_loops() {
        let redirect = Url::parse("https://example.com/next").unwrap();
        let (result, sender) = run_fake_fetch(
            FetchPolicy::from_settings(None, false),
            vec![Ok(vec![public_address()]), Ok(vec![public_address()])],
            vec![
                Ok(FetchTransportOutcome::Redirect(redirect)),
                Ok(completed_fetch()),
            ],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/start",
        )
        .await;
        assert_eq!(
            result,
            Ok(FetchResult {
                status: 200,
                text: "ok".to_string()
            })
        );
        assert_eq!(sender.call_count(), 2);

        let resolutions = (0..=FETCH_MAX_REDIRECTS)
            .map(|_| Ok(vec![public_address()]))
            .collect();
        let responses = (0..=FETCH_MAX_REDIRECTS)
            .map(|_| {
                Ok(FetchTransportOutcome::Redirect(
                    Url::parse("https://example.com/loop").unwrap(),
                ))
            })
            .collect();
        let (result, sender) = run_fake_fetch(
            FetchPolicy::from_settings(None, false),
            resolutions,
            responses,
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/loop",
        )
        .await;
        assert_eq!(result, Err(FetchError::TooManyRedirects));
        assert_eq!(sender.call_count(), FETCH_MAX_REDIRECTS + 1);
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_ssrf_policy_does_not_replay_posts_or_leak_headers_across_origins() {
        let mut post = FetchRequest::get();
        post.method = reqwest::Method::POST;
        post.body = Some(b"mutation".to_vec());
        let (result, sender) = run_fake_fetch_request(
            FetchPolicy::from_settings(None, false),
            vec![Ok(vec![public_address()])],
            vec![Ok(FetchTransportOutcome::Redirect(
                Url::parse("https://example.com/after-post").unwrap(),
            ))],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/start",
            post,
        )
        .await;
        assert_eq!(result, Err(FetchError::RedirectReplayDenied));
        assert_eq!(sender.call_count(), 1);

        let mut request = FetchRequest::get();
        request.headers.insert(
            reqwest::header::HeaderName::from_static("x-api-key"),
            reqwest::header::HeaderValue::from_static("secret"),
        );
        let (result, sender) = run_fake_fetch_request(
            FetchPolicy::from_settings(None, false),
            vec![Ok(vec![public_address()])],
            vec![Ok(FetchTransportOutcome::Redirect(
                Url::parse("https://other.example/steal").unwrap(),
            ))],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/start",
            request,
        )
        .await;
        assert_eq!(result, Err(FetchError::CrossOriginRedirectDenied));
        assert_eq!(
            sender.call_count(),
            1,
            "redirect target must not reach DNS, permission, or transport"
        );
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_ssrf_policy_handles_allow_ask_timeout_and_channel_closure() {
        let policy = FetchPolicy::from_settings(None, false);
        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(vec![public_address()])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Allow),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(sender.call_count(), 1);

        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(vec![public_address()])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Ask),
            None,
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert!(matches!(result, Err(FetchError::Permission(_))));
        assert_eq!(sender.call_count(), 0);

        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        let (result, sender) = run_fake_fetch(
            policy.clone(),
            vec![Ok(vec![public_address()])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Ask),
            Some(ask_tx),
            Duration::from_millis(30),
            "https://example.com/",
        )
        .await;
        drop(ask_rx);
        assert!(matches!(
            result,
            Err(FetchError::Permission(
                EffectServiceError::PermissionTimedOut
            ))
        ));
        assert_eq!(sender.call_count(), 0);

        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        drop(ask_rx);
        let (result, sender) = run_fake_fetch(
            policy,
            vec![Ok(vec![public_address()])],
            vec![Ok(completed_fetch())],
            fetch_permission(Action::Ask),
            Some(ask_tx),
            STEP_TIMEOUT,
            "https://example.com/",
        )
        .await;
        assert!(matches!(
            result,
            Err(FetchError::Permission(EffectServiceError::BackendFailure))
        ));
        assert_eq!(sender.call_count(), 0);
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn js_fetch_ssrf_policy_permission_key_contains_normalized_url_and_destinations() {
        let owner = PermissionBridgeOwner::new(
            Some(fetch_permission(Action::Ask)),
            {
                let (ask_tx, mut ask_rx) =
                    tokio::sync::mpsc::channel::<crate::permission::ask::AskRequest>(1);
                let approval = tokio::spawn(async move {
                    let request = ask_rx.recv().await.expect("fetch should ask permission");
                    assert_eq!(request.tool.as_str(), "js/fetch");
                    assert_eq!(
                        request.input.as_str(),
                        "https://example.com/path destinations=[93.184.216.34:443]"
                    );
                    request
                        .reply
                        .send(UserDecision::AllowOnce)
                        .expect("fetch approval receiver dropped");
                });
                std::mem::drop(approval);
                Some(ask_tx)
            },
            STEP_TIMEOUT,
        );
        let sender = Arc::new(FakeFetchSender::new(vec![Ok(completed_fetch())]));
        let executor = FetchExecutor {
            policy: FetchPolicy::from_settings(None, false),
            resolver: Arc::new(FakeFetchResolver::new(vec![Ok(vec![public_address()])])),
            sender: sender.clone(),
            permission_bridge: owner.bridge(),
        };
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            executor.execute("https://EXAMPLE.com:443/path", &FetchRequest::get())
        })
        .await
        .expect("fake fetch task panicked");

        assert_eq!(
            result,
            Ok(FetchResult {
                status: 200,
                text: "ok".to_string()
            })
        );
        assert_eq!(sender.call_count(), 1);
    }

    fn expect_allowed(decision: AuthorizationDecision) -> PathBuf {
        match decision {
            AuthorizationDecision::Allowed(path) => path,
            AuthorizationDecision::Denied(reason) => panic!("expected allow, got {reason}"),
        }
    }

    fn expect_denied(
        decision: AuthorizationDecision,
        expected: AllowPolicyReason,
    ) -> AllowPolicyReason {
        match decision {
            AuthorizationDecision::Allowed(path) => {
                panic!("expected denial, allowed {}", path.display())
            }
            AuthorizationDecision::Denied(reason) => {
                assert_eq!(reason, expected);
                reason
            }
        }
    }

    #[test]
    fn js_file_allow_policy_uses_canonical_component_containment() {
        let temp = TempDir::new();
        let safe = temp.path().join("safe");
        let sibling = temp.path().join("safe-evil");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let descendant = safe.join("child.txt");
        let sibling_file = sibling.join("secret.txt");
        std::fs::write(&descendant, "allowed").unwrap();
        std::fs::write(&sibling_file, "denied").unwrap();
        let roots = vec![safe.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);

        assert_eq!(
            expect_allowed(policy.authorize_read(&safe)),
            safe.canonicalize().unwrap()
        );
        assert_eq!(
            expect_allowed(policy.authorize_read(&descendant)),
            descendant.canonicalize().unwrap()
        );
        expect_denied(
            policy.authorize_read(&sibling_file),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
        expect_denied(
            policy.authorize_read(&safe.join("..").join("safe-evil").join("secret.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
    }

    #[test]
    fn js_file_allow_policy_resolves_relative_roots_against_explicit_base() {
        let temp = TempDir::new();
        let configured_base = temp.path().join("policy-base");
        let safe = configured_base.join("safe");
        std::fs::create_dir_all(&safe).unwrap();
        let source = safe.join("source.txt");
        std::fs::write(&source, "allowed").unwrap();
        let roots = vec!["safe".to_string()];
        let policy = AllowConfig::from_settings(
            temp.path(),
            Some("policy-base"),
            Some(&roots),
            Some(&roots),
            false,
            false,
        );

        assert_ne!(std::env::current_dir().unwrap(), configured_base);
        assert_eq!(
            expect_allowed(policy.authorize_read(&source)),
            source.canonicalize().unwrap()
        );
    }

    #[test]
    fn js_file_allow_policy_keeps_read_and_write_roots_separate() {
        let temp = TempDir::new();
        let read_root = temp.path().join("read");
        let write_root = temp.path().join("write");
        std::fs::create_dir_all(&read_root).unwrap();
        std::fs::create_dir_all(&write_root).unwrap();
        let read_roots = vec![read_root.to_string_lossy().into_owned()];
        let write_roots = vec![write_root.to_string_lossy().into_owned()];
        let policy = AllowConfig::from_settings(
            temp.path(),
            None,
            Some(&read_roots),
            Some(&write_roots),
            false,
            false,
        );

        expect_allowed(policy.authorize_read(&read_root));
        expect_denied(
            policy.authorize_write(&read_root.join("new.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Write),
        );
        expect_allowed(policy.authorize_write(&write_root.join("new.txt")));
        expect_denied(
            policy.authorize_read(&write_root),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
    }

    #[test]
    fn js_file_allow_policy_nonexistent_write_uses_nearest_canonical_parent() {
        let temp = TempDir::new();
        let safe = temp.path().join("safe");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let roots = vec![safe.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, None, Some(&roots), false, false);

        assert_eq!(
            expect_allowed(policy.authorize_write(&safe.join("missing/child/file.txt"))),
            safe.canonicalize().unwrap().join("missing/child/file.txt")
        );
        expect_denied(
            policy.authorize_write(&safe.join("../outside/escaped.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Write),
        );
    }

    #[test]
    fn js_file_allow_policy_empty_malformed_and_ambiguous_settings_deny() {
        let temp = TempDir::new();
        let empty = Vec::new();
        let empty_policy =
            AllowConfig::from_settings(temp.path(), None, Some(&empty), Some(&empty), false, false);
        expect_denied(
            empty_policy.authorize_read(temp.path()),
            AllowPolicyReason::NoConfiguredRoots(FileAccess::Read),
        );

        let malformed = vec!["missing-root".to_string()];
        let malformed_policy = AllowConfig::from_settings(
            temp.path(),
            None,
            Some(&malformed),
            Some(&malformed),
            false,
            false,
        );
        expect_denied(
            malformed_policy.authorize_read(temp.path()),
            AllowPolicyReason::InvalidConfiguration(FileAccess::Read),
        );

        let valid = vec![temp.path().to_string_lossy().into_owned()];
        let ambiguous =
            AllowConfig::from_settings(temp.path(), None, Some(&valid), Some(&valid), true, true);
        expect_denied(
            ambiguous.authorize_read(temp.path()),
            AllowPolicyReason::InvalidConfiguration(FileAccess::Read),
        );
    }

    #[test]
    fn js_file_allow_policy_requires_explicit_unrestricted_opt_in() {
        let temp = TempDir::new();
        let denied = AllowConfig::from_settings(temp.path(), None, None, None, false, false);
        expect_denied(
            denied.authorize_read(temp.path()),
            AllowPolicyReason::NoConfiguredRoots(FileAccess::Read),
        );

        let unrestricted = AllowConfig::unrestricted(temp.path());
        expect_allowed(unrestricted.authorize_read(temp.path()));
        expect_allowed(unrestricted.authorize_write(&temp.path().join("missing/file.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn js_file_allow_policy_defines_symlink_behavior() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let safe = temp.path().join("safe");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let inside_file = safe.join("inside.txt");
        let outside_file = outside.join("outside.txt");
        std::fs::write(&inside_file, "inside").unwrap();
        std::fs::write(&outside_file, "outside").unwrap();
        let inside_link = safe.join("inside-link");
        let escape_link = safe.join("escape-link");
        let dangling_link = safe.join("dangling-link");
        let parent_link = safe.join("parent-link");
        symlink(&inside_file, &inside_link).unwrap();
        symlink(&outside_file, &escape_link).unwrap();
        symlink(safe.join("missing"), &dangling_link).unwrap();
        symlink(&outside, &parent_link).unwrap();
        let roots = vec![safe.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);

        assert_eq!(
            expect_allowed(policy.authorize_read(&inside_link)),
            inside_file.canonicalize().unwrap()
        );
        expect_denied(
            policy.authorize_read(&escape_link),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Read),
        );
        expect_denied(
            policy.authorize_read(&dangling_link),
            AllowPolicyReason::InvalidTarget(FileAccess::Read),
        );
        expect_denied(
            policy.authorize_write(&inside_link),
            AllowPolicyReason::AmbiguousSymlink(FileAccess::Write),
        );
        expect_denied(
            policy.authorize_write(&parent_link.join("new.txt")),
            AllowPolicyReason::OutsideConfiguredRoots(FileAccess::Write),
        );
    }

    #[cfg(unix)]
    #[test]
    fn js_file_allow_policy_rejects_alternate_separator_spelling() {
        let temp = TempDir::new();
        let roots = vec![temp.path().to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);

        expect_denied(
            policy.authorize_write(Path::new(r"safe\..\outside.txt")),
            AllowPolicyReason::InvalidTarget(FileAccess::Write),
        );
    }

    fn standard_permission(working_dir: PathBuf) -> PermCheck {
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(working_dir),
            Some(vec!["standard".to_string()]),
        )))
    }

    fn host_permission(working_dir: PathBuf, action: Action, doom_loop: Action) -> PermCheck {
        let config = PermissionConfig {
            bash: Some(ToolPerm::Simple(action)),
            read: Some(ToolPerm::Simple(action)),
            write: Some(ToolPerm::Simple(action)),
            doom_loop: Some(doom_loop),
            ..PermissionConfig::default()
        };
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(working_dir),
            Some(vec!["standard".to_string()]),
        )))
    }

    #[tokio::test]
    async fn register_host_globals_returns_error_under_memory_pressure() {
        let runtime = rquickjs::Runtime::new().expect("create QuickJS runtime");
        let ctx = Context::full(&runtime).expect("create QuickJS context");
        runtime.set_memory_limit(1);

        let permission_owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let result = register_host_globals(
            &ctx,
            Sandbox::new(false, "bwrap"),
            permission_owner.bridge(),
            tokio::runtime::Handle::current(),
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
            #[cfg(feature = "skills")]
            SkillCapabilityGate::default(),
        );

        assert!(
            result.is_err(),
            "host-global registration should report allocation failure"
        );
    }

    async fn call_read_file(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
    ) -> Result<String, String> {
        call_read_file_with_policy(
            permission,
            ask_tx,
            path,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        )
        .await
    }

    async fn call_read_file_with_policy(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        allow_config: AllowConfig,
    ) -> Result<String, String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_read_file(bridge, runtime, allow_config)(path.to_string_lossy().into_owned())
                .map_err(|error| error.to_string())
        })
        .await
        .expect("read_file test task panicked")
    }

    #[tokio::test]
    async fn js_file_allow_policy_denies_before_mandatory_permission() {
        let temp = TempDir::new();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("source.txt");
        std::fs::write(&source, "must not be read").unwrap();
        let policy = AllowConfig::from_settings(&workspace, None, None, None, false, false);
        let permission = host_permission(workspace, Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let error = call_read_file_with_policy(permission, Some(ask_tx), source, policy)
            .await
            .expect_err("empty read policy must deny");

        assert!(error.contains("JS file read denied: no roots are configured"));
        assert!(
            ask_rx.try_recv().is_err(),
            "policy denial reached the permission service"
        );
    }

    async fn call_write_file(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        content: &'static str,
    ) -> Result<(), String> {
        call_write_file_with_policy(
            permission,
            ask_tx,
            path,
            content,
            AllowConfig::unrestricted(&std::env::current_dir().unwrap()),
        )
        .await
    }

    async fn call_write_file_with_policy(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        path: PathBuf,
        content: &'static str,
        allow_config: AllowConfig,
    ) -> Result<(), String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_write_file(bridge, runtime, allow_config)(
                path.to_string_lossy().into_owned(),
                content.to_string(),
            )
            .map_err(|error| error.to_string())
        })
        .await
        .expect("write_file test task panicked")
    }

    #[tokio::test]
    async fn js_file_allow_enforcement_narrows_allowed_permissions_for_reads_and_writes() {
        let temp = TempDir::new();
        let allowed_root = temp.path().join("safe");
        let sibling_root = temp.path().join("safe-evil");
        std::fs::create_dir_all(&allowed_root).unwrap();
        std::fs::create_dir_all(&sibling_root).unwrap();
        let allowed_source = allowed_root.join("source.txt");
        let denied_source = sibling_root.join("source.txt");
        std::fs::write(&allowed_source, "allowed").unwrap();
        std::fs::write(&denied_source, "must stay private").unwrap();
        let roots = vec![allowed_root.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);
        let permission = host_permission(temp.path().to_path_buf(), Action::Allow, Action::Allow);

        assert_eq!(
            call_read_file_with_policy(permission.clone(), None, allowed_source, policy.clone(),)
                .await
                .expect("in-root read should succeed"),
            "allowed"
        );
        let read_error =
            call_read_file_with_policy(permission.clone(), None, denied_source, policy.clone())
                .await
                .expect_err("an allowed permission must not override the read roots");
        assert!(
            read_error.contains("outside the configured roots"),
            "unexpected read policy error: {read_error}"
        );

        let allowed_target = allowed_root.join("created.txt");
        call_write_file_with_policy(
            permission.clone(),
            None,
            allowed_target.clone(),
            "created safely",
            policy.clone(),
        )
        .await
        .expect("in-root write should succeed");
        assert_eq!(
            std::fs::read_to_string(allowed_target).unwrap(),
            "created safely"
        );

        let denied_target = sibling_root.join("must-not-exist.txt");
        let write_error = call_write_file_with_policy(
            permission,
            None,
            denied_target.clone(),
            "must not escape",
            policy,
        )
        .await
        .expect_err("an allowed permission must not override the write roots");
        assert!(
            write_error.contains("outside the configured roots"),
            "unexpected write policy error: {write_error}"
        );
        assert!(
            !denied_target.exists(),
            "policy-denied write created an out-of-root file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_allow_enforcement_revalidates_after_permission_wait() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let allowed_root = temp.path().join("safe");
        let write_parent = allowed_root.join("write-parent");
        let outside_root = temp.path().join("outside");
        std::fs::create_dir_all(&write_parent).unwrap();
        std::fs::create_dir_all(&outside_root).unwrap();
        let source = allowed_root.join("source.txt");
        let original_source = allowed_root.join("original-source.txt");
        let outside_source = outside_root.join("outside.txt");
        std::fs::write(&source, "approved identity").unwrap();
        std::fs::write(&outside_source, "must not be read").unwrap();
        let roots = vec![allowed_root.to_string_lossy().into_owned()];
        let policy =
            AllowConfig::from_settings(temp.path(), None, Some(&roots), Some(&roots), false, false);
        let permission = host_permission(temp.path().to_path_buf(), Action::Ask, Action::Allow);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let read = tokio::spawn(call_read_file_with_policy(
            permission.clone(),
            Some(ask_tx.clone()),
            source.clone(),
            policy.clone(),
        ));
        let request = ask_rx.recv().await.expect("read should request permission");
        std::fs::rename(&source, &original_source).unwrap();
        symlink(&outside_source, &source).unwrap();
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("read permission request receiver dropped");
        let read_error = read
            .await
            .expect("read task panicked")
            .expect_err("swapped read target must fail");
        assert!(
            read_error.contains("Path changed after permission check"),
            "unexpected read swap error: {read_error}"
        );

        let target = write_parent.join("created.txt");
        let original_parent = allowed_root.join("original-write-parent");
        let outside_target = outside_root.join("created.txt");
        let write = tokio::spawn(call_write_file_with_policy(
            permission,
            Some(ask_tx),
            target,
            "must not escape",
            policy,
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("write should request permission");
        std::fs::rename(&write_parent, &original_parent).unwrap();
        symlink(&outside_root, &write_parent).unwrap();
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("write permission request receiver dropped");
        let write_error = write
            .await
            .expect("write task panicked")
            .expect_err("swapped write parent must fail");
        assert!(
            !write_error.is_empty(),
            "swapped write parent returned an empty error"
        );
        assert!(
            !outside_target.exists(),
            "swapped parent redirected the policy-approved write"
        );
    }

    async fn call_spawn(
        permission: PermCheck,
        ask_tx: Option<AskSender>,
        cmd: &'static str,
        args: Vec<String>,
    ) -> Result<SpawnResult, String> {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(Some(permission), ask_tx, STEP_TIMEOUT);
        let bridge = owner.bridge();
        tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_spawn(Sandbox::new(false, "bwrap"), bridge, runtime)(cmd.to_string(), args)
                .map_err(|error| error.to_string())
        })
        .await
        .expect("spawn test task panicked")
    }

    #[tokio::test]
    async fn js_file_host_permissions_host_call_timeout_reports_execution_timed_out() {
        for tool in ["js/read_file", "js/write_file"] {
            let error = timeout_host_call(
                tool,
                Duration::from_millis(1),
                std::future::pending::<rquickjs::Result<()>>(),
            )
            .await
            .expect_err("pending host call should time out");

            assert!(
                error.to_string().contains("execution timed out"),
                "unexpected {tool} timeout error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn js_file_host_permissions_read_allows_paths_within_working_directory() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let source = working_dir.join("source.txt");
        std::fs::write(&source, "allowed").unwrap();

        let contents = call_read_file(standard_permission(working_dir), None, source)
            .await
            .expect("workspace read should be allowed");

        assert_eq!(contents, "allowed");
    }

    #[tokio::test]
    async fn js_file_host_permissions_read_ask_uses_exact_canonical_permission_key() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let source = external_dir.join("source.txt");
        std::fs::write(&source, "approved").unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let read = tokio::spawn(call_read_file(
            standard_permission(working_dir),
            Some(ask_tx),
            source.clone(),
        ));
        let request = ask_rx.recv().await.expect("read should request permission");
        assert_eq!(request.tool.as_str(), "js/read_file");
        assert_eq!(
            request.input.as_str(),
            source.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("permission request receiver dropped");

        assert_eq!(
            read.await
                .expect("read task panicked")
                .expect("approved read should succeed"),
            "approved"
        );
    }

    #[tokio::test]
    async fn js_file_host_permissions_read_rejects_oversized_and_non_utf8_content() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let oversized = working_dir.join("oversized.txt");
        let non_utf8 = working_dir.join("non-utf8.txt");
        std::fs::write(&oversized, vec![b'x'; READ_FILE_MAX_BYTES + 1]).unwrap();
        std::fs::write(&non_utf8, [0xff, 0xfe]).unwrap();

        let error = call_read_file(standard_permission(working_dir.clone()), None, oversized)
            .await
            .expect_err("oversized read should fail");
        assert!(
            error.contains("resource limit"),
            "unexpected error: {error}"
        );

        let error = call_read_file(standard_permission(working_dir), None, non_utf8)
            .await
            .expect_err("non-UTF-8 read should fail");
        assert!(
            error.contains("invalid encoding"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn js_file_host_permissions_read_denies_external_path_without_response() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let source = external_dir.join("secret.txt");
        std::fs::write(&source, "secret").unwrap();

        let error = call_read_file(standard_permission(working_dir), None, source)
            .await
            .expect_err("external read should require permission");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_host_permissions_read_denies_relative_parent_traversal() {
        let working_dir = std::env::current_dir().unwrap();
        let path = PathBuf::from("../".repeat(32)).join("etc/passwd");

        let error = call_read_file(standard_permission(working_dir), None, path)
            .await
            .expect_err("relative traversal should require permission");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_host_permissions_read_denies_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let external_source = external_dir.join("secret.txt");
        let allowed_link = working_dir.join("source-link.txt");
        std::fs::write(&external_source, "secret").unwrap();
        symlink(&external_source, &allowed_link).unwrap();

        let error = call_read_file(standard_permission(working_dir), None, allowed_link)
            .await
            .expect_err("symlinked external read should require permission");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
    }

    #[tokio::test]
    async fn js_file_host_permissions_write_allows_paths_within_working_directory() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let target = working_dir.join("created.txt");

        call_write_file(
            standard_permission(working_dir),
            None,
            target.clone(),
            "allowed",
        )
        .await
        .expect("workspace write should be allowed");

        assert_eq!(std::fs::read_to_string(target).unwrap(), "allowed");
    }

    #[tokio::test]
    async fn js_file_host_permissions_write_rejects_oversized_content_before_mutation() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let target = working_dir.join("oversized.txt");
        let runtime = tokio::runtime::Handle::current();
        let owner =
            PermissionBridgeOwner::new(Some(standard_permission(working_dir)), None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let target_for_call = target.clone();
        let allow_config = AllowConfig::unrestricted(&std::env::current_dir().unwrap());

        let error = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_write_file(bridge, runtime, allow_config)(
                target_for_call.to_string_lossy().into_owned(),
                "x".repeat(WRITE_FILE_MAX_BYTES + 1),
            )
            .expect_err("oversized write should fail")
            .to_string()
        })
        .await
        .expect("write task panicked");

        assert!(
            error.contains("resource limit"),
            "unexpected error: {error}"
        );
        assert!(!target.exists(), "oversized write created the target");
    }

    #[tokio::test]
    async fn js_file_host_permissions_write_denies_external_path_without_response() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let target = external_dir.join("denied.txt");

        let error = call_write_file(
            standard_permission(working_dir),
            None,
            target.clone(),
            "forbidden",
        )
        .await
        .expect_err("external write should require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(!target.exists(), "denied external target was written");
    }

    #[tokio::test]
    async fn js_file_host_permissions_write_prompts_for_external_directory() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let target = external_dir.join("approved.txt");
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let write = tokio::spawn(call_write_file(
            standard_permission(working_dir),
            Some(ask_tx),
            target.clone(),
            "approved",
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("external write should request permission");
        assert_eq!(request.tool.as_str(), "js/write_file");
        let expected_target = target
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(target.file_name().unwrap());
        assert_eq!(
            request.input.as_str(),
            expected_target.to_string_lossy().as_ref()
        );
        request
            .reply
            .send(UserDecision::AllowOnce)
            .expect("permission request receiver dropped");

        write
            .await
            .expect("write task panicked")
            .expect("approved external write should succeed");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "approved");
    }

    #[tokio::test]
    async fn js_file_host_permissions_use_js_tool_names_and_honor_denial() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let source = working_dir.join("source.txt");
        let target = working_dir.join("target.txt");
        std::fs::write(&source, "secret").unwrap();
        let permission = host_permission(working_dir, Action::Ask, Action::Deny);
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);

        let read = tokio::spawn(call_read_file(
            permission.clone(),
            Some(ask_tx.clone()),
            source.clone(),
        ));
        let request = ask_rx.recv().await.expect("read should request permission");
        assert_eq!(request.tool, "js/read_file");
        assert_eq!(
            request.input.as_str(),
            source.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        request
            .reply
            .send(UserDecision::Deny)
            .expect("read permission request receiver dropped");
        let error = read
            .await
            .expect("read task panicked")
            .expect_err("denied read must fail");
        assert!(error.contains("Permission denied by user"));

        let write = tokio::spawn(call_write_file(
            permission.clone(),
            Some(ask_tx.clone()),
            target.clone(),
            "forbidden",
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("write should request permission");
        assert_eq!(request.tool, "js/write_file");
        let expected_target = target
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(target.file_name().unwrap());
        assert_eq!(
            request.input.as_str(),
            expected_target.to_string_lossy().as_ref()
        );
        request
            .reply
            .send(UserDecision::Deny)
            .expect("write permission request receiver dropped");
        let error = write
            .await
            .expect("write task panicked")
            .expect_err("denied write must fail");
        assert!(error.contains("Permission denied by user"));
        assert!(!target.exists(), "denied write created the target");

        let spawn = tokio::spawn(call_spawn(
            permission,
            Some(ask_tx),
            "touch",
            vec![target.to_string_lossy().into_owned()],
        ));
        let request = ask_rx
            .recv()
            .await
            .expect("spawn should request permission");
        assert_eq!(request.tool, "bash");
        assert_eq!(
            request.input,
            canonical_spawn_permission_subject("touch", &[target.to_string_lossy().into_owned()])
                .unwrap()
        );
        request
            .reply
            .send(UserDecision::Deny)
            .expect("spawn permission request receiver dropped");
        let error = spawn
            .await
            .expect("spawn task panicked")
            .expect_err("denied spawn must fail");
        assert!(error.contains("Permission denied by user"));
        assert!(!target.exists(), "denied spawn created the target");
    }

    #[tokio::test]
    async fn spawn_restricted_command_is_denied_before_execution() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let target = working_dir.join("must-not-exist.txt");
        let permission = host_permission(working_dir, Action::Deny, Action::Deny);

        let error = call_spawn(
            permission,
            None,
            "touch",
            vec![target.to_string_lossy().into_owned()],
        )
        .await
        .expect_err("restricted spawn must fail");

        assert!(
            error.contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(!target.exists(), "denied spawn created the target");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_preserves_output_and_exit_code_through_sandbox_wrapper() {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_spawn(Sandbox::new(false, "bwrap"), bridge, runtime)(
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    "printf stdout; printf stderr >&2; exit 7".to_string(),
                ],
            )
        })
        .await
        .expect("spawn output test task panicked")
        .expect("wrapped spawn should succeed");

        assert_eq!(result.stdout, "stdout");
        assert_eq!(result.stderr, "stderr");
        assert_eq!(result.code, 7);
    }

    #[cfg(feature = "sandbox")]
    #[tokio::test]
    async fn spawn_requested_unavailable_sandbox_starts_no_child() {
        let marker = std::env::current_dir().unwrap().join(format!(
            ".mini-agent-js-spawn-unavailable-{}",
            uuid::Uuid::new_v4()
        ));
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let marker_arg = marker.to_string_lossy().into_owned();
        let error = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_spawn(
                Sandbox::new(true, "__mini_agent_unavailable_sandbox__"),
                bridge,
                runtime,
            )("touch".to_string(), vec![marker_arg])
            .expect_err("unavailable required sandbox must deny JS spawn")
            .to_string()
        })
        .await
        .expect("spawn sandbox integration test task panicked");

        assert!(
            error.contains("unavailable"),
            "sandbox denial should explain backend unavailability: {error}"
        );
        let started = marker.exists();
        let _ = std::fs::remove_file(&marker);
        assert!(
            !started,
            "JS spawn must not start a child when required isolation is unavailable"
        );
    }

    #[cfg(all(feature = "sandbox", target_os = "macos"))]
    #[tokio::test]
    async fn spawn_uses_real_macos_seatbelt_write_boundary() {
        let outside_marker = std::env::current_dir()
            .unwrap()
            .parent()
            .expect("test repository must not be filesystem root")
            .join(format!(
                ".mini-agent-js-spawn-seatbelt-denied-{}",
                uuid::Uuid::new_v4()
            ));
        let script = format!(
            "if touch '{}' 2>/dev/null; then exit 10; fi; printf JS_SPAWN_SEATBELT_PASS",
            outside_marker.to_string_lossy().replace('\'', "'\"'\"'")
        );
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            make_spawn(Sandbox::new(true, "seatbelt"), bridge, runtime)(
                "bash".to_string(),
                vec!["-c".to_string(), script],
            )
            .expect("Seatbelt-wrapped JS spawn should complete")
        })
        .await
        .expect("spawn Seatbelt integration test task panicked");

        let escaped = outside_marker.exists();
        let _ = std::fs::remove_file(&outside_marker);
        assert_eq!(result.code, 0, "unexpected stderr: {}", result.stderr);
        assert_eq!(result.stdout, "JS_SPAWN_SEATBELT_PASS");
        assert!(!escaped, "JS spawn escaped the Seatbelt write boundary");
    }

    #[tokio::test]
    async fn js_file_host_permissions_repeated_reads_trigger_doom_loop_detection() {
        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        let source = working_dir.join("source.txt");
        std::fs::write(&source, "allowed").unwrap();
        let permission = host_permission(working_dir, Action::Allow, Action::Deny);

        for _ in 0..2 {
            let contents = call_read_file(permission.clone(), None, source.clone())
                .await
                .expect("first two reads should be allowed");
            assert_eq!(contents, "allowed");
        }

        let error = call_read_file(permission, None, source)
            .await
            .expect_err("third identical read should trigger doom-loop denial");
        assert!(
            error.contains("Doom loop: repeated identical tool call"),
            "unexpected doom-loop error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_host_permissions_write_rejects_broken_final_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let external_target = external_dir.join("created-through-link.txt");
        let allowed_link = working_dir.join("safe-link.txt");
        symlink(&external_target, &allowed_link).unwrap();

        let error = call_write_file(
            standard_permission(working_dir),
            None,
            allowed_link,
            "forbidden",
        )
        .await
        .expect_err("final symlink should be rejected");

        assert!(
            error.to_string().contains("does not follow final symlinks"),
            "unexpected permission error: {error}"
        );
        assert!(
            !external_target.exists(),
            "permission denial must happen before the symlink target is written"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_host_permissions_write_denies_symlinked_parent_escape() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let working_dir = temp.path().join("workspace");
        let external_dir = temp.path().join("external");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        let linked_dir = working_dir.join("linked-dir");
        symlink(&external_dir, &linked_dir).unwrap();
        let allowed_path = linked_dir.join("created-through-parent-link.txt");
        let external_target = external_dir.join("created-through-parent-link.txt");

        let error = call_write_file(
            standard_permission(working_dir),
            None,
            allowed_path,
            "forbidden",
        )
        .await
        .expect_err("external target beneath symlinked parent should require permission");

        assert!(
            error.to_string().contains("Permission denied"),
            "unexpected permission error: {error}"
        );
        assert!(
            !external_target.exists(),
            "permission denial must happen before the symlinked parent target is written"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn js_file_host_permissions_reject_target_and_parent_swaps() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let workspace = temp.path().join("workspace");
        let parent = workspace.join("parent");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let source = workspace.join("source.txt");
        let original = workspace.join("original.txt");
        std::fs::write(&source, "approved identity").unwrap();
        let approved_read = resolve_read_target(source.to_str().unwrap())
            .await
            .expect("resolve read target");
        std::fs::rename(&source, &original).unwrap();
        std::fs::write(&source, "replacement").unwrap();
        let error = read_approved_file(approved_read)
            .await
            .expect_err("replaced read target must fail");
        assert!(
            error
                .to_string()
                .contains("Path changed after permission check"),
            "unexpected read swap error: {error}"
        );

        let target = parent.join("created.txt");
        let external_target = external.join("created.txt");
        let approved_write = resolve_write_target(target.to_str().unwrap())
            .await
            .expect("resolve write target");
        let original_parent = workspace.join("original-parent");
        std::fs::rename(&parent, &original_parent).unwrap();
        symlink(&external, &parent).unwrap();
        let error = write_approved_file(approved_write, "must not escape".to_string())
            .await
            .expect_err("swapped parent must fail");
        assert!(!error.to_string().is_empty(), "parent swap error was empty");
        assert!(
            !external_target.exists(),
            "parent swap redirected the approved write"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_timeout_reports_execution_timed_out() {
        let runtime = tokio::runtime::Handle::current();
        let owner = PermissionBridgeOwner::new(None, None, STEP_TIMEOUT);
        let bridge = owner.bridge();
        let result = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            let spawn = make_spawn_with_timeout(
                Sandbox::new(false, "bwrap"),
                bridge,
                runtime,
                Duration::from_millis(25),
            );
            spawn("sleep".to_string(), vec!["5".to_string()])
                .expect("spawn should return Ok with timed_out=true, not Err")
        })
        .await
        .expect("spawn timeout test task panicked");

        assert!(
            result.timed_out,
            "timed_out must be true when deadline expires"
        );
        assert_eq!(result.code, -1, "timed-out process has no exit code");
    }
}
