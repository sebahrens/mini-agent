//! OAuth 2.0 (authorization code + PKCE) support for URL-based MCP servers.
//!
//! Tokens are persisted below the machine-local credential root under opaque,
//! identity-derived names via a file-backed [`CredentialStore`]. The interactive
//! login is driven from the `/mcp login <server>` slash command; afterwards the
//! stored refresh token lets startup reconnect without a browser.

use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationSession, CredentialStore,
    StoredCredentials,
};

use super::config::OAuthSettings;

const CLIENT_NAME: &str = "zerostack";
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
const MIGRATION_MARKER_VERSION: u8 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct MigrationMarker {
    version: u8,
    legacy_digest: Option<String>,
}

fn resolved_paths() -> anyhow::Result<crate::paths::AppPaths> {
    static PATHS: OnceLock<Result<crate::paths::AppPaths, String>> = OnceLock::new();
    match PATHS.get_or_init(|| {
        crate::paths::AppPaths::from_process(None).map_err(|error| error.to_string())
    }) {
        Ok(paths) => Ok(paths.clone()),
        Err(error) => Err(anyhow::anyhow!(error.clone())),
    }
}

fn oauth_dir(paths: &crate::paths::AppPaths) -> PathBuf {
    paths.credentials_dir.join("mcp-oauth")
}

/// Build the opaque credential filename for an exact MCP server identity.
///
/// The configured map key is display metadata only. Its exact bytes, normalized
/// endpoint, and explicit client id are length-prefixed into a versioned digest.
pub(crate) fn token_filename(
    server_name: &str,
    url: &str,
    client_id: Option<&str>,
) -> anyhow::Result<String> {
    let normalized_url = normalize_http_url(url)?;
    Ok(crate::paths::digest_filename(
        "mcp-oauth-server",
        &[
            server_name.as_bytes(),
            normalized_url.as_bytes(),
            client_id.unwrap_or("").as_bytes(),
        ],
        "json",
    )?)
}

pub(crate) fn token_path(
    paths: &crate::paths::AppPaths,
    server_name: &str,
    url: &str,
    settings: &OAuthSettings,
) -> anyhow::Result<PathBuf> {
    Ok(oauth_dir(paths).join(token_filename(
        server_name,
        url,
        settings.client_id.as_deref(),
    )?))
}

fn normalize_http_url(value: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid MCP OAuth endpoint: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        anyhow::bail!("MCP OAuth endpoint must be an absolute HTTP(S) URL");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("MCP OAuth endpoint must not contain user information");
    }
    url.set_fragment(None);
    let default_port = match url.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    if url.port() == default_port {
        url.set_port(None)
            .map_err(|()| anyhow::anyhow!("could not normalize MCP OAuth endpoint port"))?;
    }
    Ok(url.to_string())
}

/// File-backed credential store. One JSON file per MCP server.
#[derive(Clone)]
pub(crate) struct FileCredentialStore {
    path: PathBuf,
    migration_marker: PathBuf,
    legacy_dir: PathBuf,
    legacy_server_name: String,
    explicit_client_id: Option<String>,
}

impl FileCredentialStore {
    fn new(server_name: &str, url: &str, settings: &OAuthSettings) -> anyhow::Result<Self> {
        Self::for_paths(&resolved_paths()?, server_name, url, settings)
    }

    pub(crate) fn for_paths(
        paths: &crate::paths::AppPaths,
        server_name: &str,
        url: &str,
        settings: &OAuthSettings,
    ) -> anyhow::Result<Self> {
        let path = token_path(paths, server_name, url, settings)?;
        Ok(Self {
            migration_marker: path.with_extension("migration.json"),
            path,
            legacy_dir: paths.data_dir.join("mcp-oauth"),
            legacy_server_name: server_name.to_string(),
            explicit_client_id: settings.client_id.clone(),
        })
    }

    pub(crate) fn read_blocking(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.with_lock(|| {
            let bytes = match read_private_bounded(&self.path)? {
                Some(bytes) => bytes,
                None => match self.migrate_legacy()? {
                    Some(bytes) => bytes,
                    None => return Ok(None),
                },
            };
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| storage_error("parse", std::io::ErrorKind::InvalidData))
        })
    }

    pub(crate) fn write_blocking(&self, creds: &StoredCredentials) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(creds)
            .map_err(|_| storage_error("serialize", std::io::ErrorKind::InvalidData))?;
        if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err(storage_error("serialize", std::io::ErrorKind::FileTooLarge));
        }
        self.with_lock(|| {
            secure_atomic_write(&self.path, &bytes)
                .map_err(|error| storage_error("save", error.kind()))
        })
    }

    pub(crate) fn clear_blocking(&self) -> Result<bool, AuthError> {
        self.with_lock(|| {
            self.write_migration_marker(None)?;
            secure_remove_file(&self.path).map_err(|error| storage_error("remove", error.kind()))
        })
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, AuthError>,
    ) -> Result<T, AuthError> {
        let directory = self
            .path
            .parent()
            .ok_or_else(|| storage_error("resolve", std::io::ErrorKind::InvalidInput))?;
        let credential_root = directory
            .parent()
            .ok_or_else(|| storage_error("resolve", std::io::ErrorKind::InvalidInput))?;
        ensure_private_directory(credential_root)
            .map_err(|error| storage_error("prepare", error.kind()))?;
        ensure_private_directory(directory)
            .map_err(|error| storage_error("prepare", error.kind()))?;
        let lock_path = self.path.with_extension("json.lock");
        let _lock = CredentialLock::acquire(&lock_path)
            .map_err(|error| storage_error("lock", error.kind()))?;
        operation()
    }

    fn migrate_legacy(&self) -> Result<Option<Vec<u8>>, AuthError> {
        if path_kind(&self.path)
            .map_err(|error| storage_error("inspect", error.kind()))?
            .is_some()
        {
            return read_private_bounded(&self.path);
        }
        if self.migration_was_handled()? {
            return Ok(None);
        }
        let Some(candidate) =
            unambiguous_legacy_candidate(&self.legacy_dir, &self.legacy_server_name)
                .map_err(|error| storage_error("migrate", error.kind()))?
        else {
            return Ok(None);
        };
        let Some(bytes) = read_private_bounded(&candidate)? else {
            return Ok(None);
        };
        let credentials: StoredCredentials = serde_json::from_slice(&bytes)
            .map_err(|_| storage_error("migrate", std::io::ErrorKind::InvalidData))?;
        if self
            .explicit_client_id
            .as_ref()
            .is_some_and(|expected| credentials.client_id.as_str() != expected.as_str())
        {
            return Err(storage_error("migrate", std::io::ErrorKind::InvalidData));
        }
        let canonical = serde_json::to_vec_pretty(&credentials)
            .map_err(|_| storage_error("migrate", std::io::ErrorKind::InvalidData))?;
        if canonical.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err(storage_error("migrate", std::io::ErrorKind::FileTooLarge));
        }
        secure_atomic_write(&self.path, &canonical)
            .map_err(|error| storage_error("migrate", error.kind()))?;
        self.write_migration_marker(Some(crate::paths::opaque_name(
            "mcp-oauth-legacy-record",
            &[bytes.as_slice()],
        )))?;
        Ok(Some(canonical))
    }

    fn migration_was_handled(&self) -> Result<bool, AuthError> {
        let Some(bytes) = read_private_bounded(&self.migration_marker)? else {
            return Ok(false);
        };
        let marker: MigrationMarker = serde_json::from_slice(&bytes)
            .map_err(|_| storage_error("migrate", std::io::ErrorKind::InvalidData))?;
        if marker.version != MIGRATION_MARKER_VERSION {
            return Err(storage_error("migrate", std::io::ErrorKind::InvalidData));
        }
        if marker.legacy_digest.as_ref().is_some_and(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(storage_error("migrate", std::io::ErrorKind::InvalidData));
        }
        Ok(true)
    }

    fn write_migration_marker(&self, legacy_digest: Option<String>) -> Result<(), AuthError> {
        let marker = serde_json::to_vec(&MigrationMarker {
            version: MIGRATION_MARKER_VERSION,
            legacy_digest,
        })
        .map_err(|_| storage_error("migrate", std::io::ErrorKind::InvalidData))?;
        secure_atomic_write(&self.migration_marker, &marker)
            .map_err(|error| storage_error("migrate", error.kind()))
    }
}

fn storage_error(operation: &str, kind: std::io::ErrorKind) -> AuthError {
    AuthError::InternalError(format!(
        "MCP OAuth credential {operation} failed ({kind:?})"
    ))
}

fn path_kind(path: &Path) -> std::io::Result<Option<std::fs::FileType>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "credential path must not be a symbolic link",
                ))
            } else {
                Ok(Some(metadata.file_type()))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn legacy_sanitized_stem(server_name: &str) -> String {
    server_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn unambiguous_legacy_candidate(
    directory: &Path,
    server_name: &str,
) -> std::io::Result<Option<PathBuf>> {
    let expected_name = format!("{server_name}.json");
    let portable_expected = crate::paths::validate_portable_component(&expected_name).is_ok();
    let expected_collision = crate::paths::collision_key(&expected_name).ok();
    let sanitized = legacy_sanitized_stem(server_name);
    let directory_before = match std::fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata_is_link_or_reparse(&metadata) && metadata.is_dir() => metadata,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "legacy MCP OAuth root is a link or has the wrong type",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let entries = std::fs::read_dir(directory)?;
    let mut exact = None;
    let mut ambiguous = false;
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let portable_collision = crate::paths::collision_key(&name).ok();
        if legacy_sanitized_stem(stem) == sanitized
            || expected_collision
                .as_ref()
                .is_some_and(|expected| portable_collision.as_ref() == Some(expected))
        {
            if portable_expected && name == expected_name && exact.is_none() {
                exact = Some(entry.path());
            } else {
                ambiguous = true;
            }
        }
    }
    let directory_after = std::fs::symlink_metadata(directory)?;
    if metadata_is_link_or_reparse(&directory_after) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "legacy MCP OAuth root changed during migration",
        ));
    }
    crate::fs::ensure_same_file(directory, &directory_before, &directory_after)?;
    if ambiguous {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "legacy MCP OAuth filename is ambiguous",
        ));
    }
    Ok(exact)
}

fn read_private_bounded(path: &Path) -> Result<Option<Vec<u8>>, AuthError> {
    let Some(file_type) = path_kind(path).map_err(|error| storage_error("read", error.kind()))?
    else {
        return Ok(None);
    };
    if !file_type.is_file() {
        return Err(storage_error("read", std::io::ErrorKind::InvalidInput));
    }
    let mut file =
        open_private_existing(path).map_err(|error| storage_error("read", error.kind()))?;
    let length = file
        .metadata()
        .map_err(|error| storage_error("read", error.kind()))?
        .len();
    if length > MAX_CREDENTIAL_BYTES {
        return Err(storage_error("read", std::io::ErrorKind::FileTooLarge));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| storage_error("read", error.kind()))?;
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(storage_error("read", std::io::ErrorKind::FileTooLarge));
    }
    Ok(Some(bytes))
}

fn secure_remove_file(path: &Path) -> std::io::Result<bool> {
    let Some(file_type) = path_kind(path)? else {
        return Ok(false);
    };
    if !file_type.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential path is not a regular file",
        ));
    }
    let file = open_private_existing(path)?;
    let opened = file.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    crate::fs::ensure_same_file(path, &opened, &current)?;
    std::fs::remove_file(path)?;
    Ok(true)
}

struct CredentialLock {
    file: std::fs::File,
}

impl CredentialLock {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        let file = open_private_lock(path)?;
        lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for CredentialLock {
    fn drop(&mut self) {
        unlock_file(&self.file);
    }
}

#[cfg(unix)]
const OPEN_NOFOLLOW: std::os::raw::c_int = if cfg!(target_os = "macos") {
    0x100
} else {
    0x2_0000
};

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    if path.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential root must not be a filesystem root",
        ));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() || !before.is_dir() || before.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "credential root is not an owned regular directory",
        ));
    }
    let directory = std::fs::File::open(path)?;
    let opened = directory.metadata()?;
    let after = std::fs::symlink_metadata(path)?;
    crate::fs::ensure_same_file(path, &before, &opened)?;
    crate::fs::ensure_same_file(path, &opened, &after)?;
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
fn open_private_existing(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() || !before.is_file() || before.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "credential file is not an owned regular file",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    let after = std::fs::symlink_metadata(path)?;
    crate::fs::ensure_same_file(path, &before, &opened)?;
    crate::fs::ensure_same_file(path, &opened, &after)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(unix)]
fn open_private_lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(OPEN_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "credential lock is not an owned regular file",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(unix)]
fn secure_atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(file_type) = path_kind(path)? {
        if !file_type.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "credential path is not a regular file",
            ));
        }
        drop(open_private_existing(path)?);
    }
    crate::fs::atomic_write_sync(path, bytes)?;
    drop(open_private_existing(path)?);
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: `getuid` takes no arguments and has no failure mode.
    unsafe { getuid() }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(
            file_descriptor: std::os::raw::c_int,
            operation: std::os::raw::c_int,
        ) -> std::os::raw::c_int;
    }
    loop {
        // SAFETY: the descriptor remains owned by `file` for the duration of the call.
        if unsafe { flock(file.as_raw_fd(), 2) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn unlock_file(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(
            file_descriptor: std::os::raw::c_int,
            operation: std::os::raw::c_int,
        ) -> std::os::raw::c_int;
    }
    // SAFETY: the descriptor remains valid while the lock is released.
    let _ = unsafe { flock(file.as_raw_fd(), 8) };
}

#[cfg(windows)]
fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    windows_private::ensure_directory(path)
}

#[cfg(windows)]
fn open_private_existing(path: &Path) -> std::io::Result<std::fs::File> {
    windows_private::open_existing(path)
}

#[cfg(windows)]
fn open_private_lock(path: &Path) -> std::io::Result<std::fs::File> {
    windows_private::open_lock(path)
}

#[cfg(windows)]
fn secure_atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    windows_private::atomic_write(path, bytes)
}

#[cfg(windows)]
fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    windows_private::lock(file)
}

#[cfg(windows)]
fn unlock_file(file: &std::fs::File) {
    windows_private::unlock(file);
}

#[cfg(all(test, windows))]
pub(crate) fn windows_private_dacl_sddl(path: &Path, directory: bool) -> std::io::Result<String> {
    windows_private::dacl_sddl(path, directory)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "MCP OAuth credential storage is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_private_existing(_path: &Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "MCP OAuth credential storage is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_private_lock(_path: &Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "MCP OAuth credential storage is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn secure_atomic_write(_path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "MCP OAuth credential storage is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn lock_exclusive(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "MCP OAuth credential storage is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn unlock_file(_file: &std::fs::File) {}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_private {
    use std::ffi::{OsStr, c_void};
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr::{null, null_mut};

    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    type LocalHandle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_READ: Dword = 0x8000_0000;
    const GENERIC_WRITE: Dword = 0x4000_0000;
    const READ_CONTROL: Dword = 0x0002_0000;
    const WRITE_DAC: Dword = 0x0004_0000;
    const FILE_SHARE_READ: Dword = 0x0000_0001;
    const FILE_SHARE_WRITE: Dword = 0x0000_0002;
    const FILE_SHARE_DELETE: Dword = 0x0000_0004;
    const CREATE_NEW: Dword = 1;
    const OPEN_EXISTING: Dword = 3;
    const OPEN_ALWAYS: Dword = 4;
    const FILE_ATTRIBUTE_NORMAL: Dword = 0x0000_0080;
    const FILE_ATTRIBUTE_DIRECTORY: Dword = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: Dword = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: Dword = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: Dword = 0x0200_0000;
    const FILE_FLAG_WRITE_THROUGH: Dword = 0x8000_0000;
    const MOVEFILE_WRITE_THROUGH: Dword = 0x0000_0008;
    const REPLACEFILE_WRITE_THROUGH: Dword = 0x0000_0001;
    const SDDL_REVISION_1: Dword = 1;
    const SE_FILE_OBJECT: Dword = 1;
    const OWNER_SECURITY_INFORMATION: Dword = 0x0000_0001;
    const DACL_SECURITY_INFORMATION: Dword = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: Dword = 0x8000_0000;
    const LOCKFILE_EXCLUSIVE_LOCK: Dword = 0x0000_0002;
    const TOKEN_QUERY: Dword = 0x0000_0008;
    const TOKEN_USER_CLASS: Dword = 1;
    const ERROR_ALREADY_EXISTS: Dword = 183;

    #[repr(C)]
    struct SecurityAttributes {
        _length: Dword,
        _security_descriptor: *mut c_void,
        _inherit_handle: Bool,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Overlapped {
        _internal: usize,
        _internal_high: usize,
        _offset: Dword,
        _offset_high: Dword,
        _event: Handle,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: Dword,
        _creation_time_low: Dword,
        _creation_time_high: Dword,
        _last_access_time_low: Dword,
        _last_access_time_high: Dword,
        _last_write_time_low: Dword,
        _last_write_time_high: Dword,
        _volume_serial_number: Dword,
        _file_size_high: Dword,
        _file_size_low: Dword,
        _number_of_links: Dword,
        _file_index_high: Dword,
        _file_index_low: Dword,
    }

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        _attributes: Dword,
    }

    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> Bool;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            security_descriptor: *mut c_void,
            string_sd_revision: Dword,
            security_information: Dword,
            string_security_descriptor: *mut *mut u16,
            string_security_descriptor_length: *mut Dword,
        ) -> Bool;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: Dword,
            security_descriptor: *mut *mut c_void,
            security_descriptor_size: *mut Dword,
        ) -> Bool;
        fn EqualSid(first: *mut c_void, second: *mut c_void) -> Bool;
        fn GetSecurityInfo(
            handle: Handle,
            object_type: Dword,
            security_info: Dword,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            security_descriptor: *mut *mut c_void,
        ) -> Dword;
        fn GetSecurityDescriptorDacl(
            security_descriptor: *mut c_void,
            dacl_present: *mut Bool,
            dacl: *mut *mut c_void,
            dacl_defaulted: *mut Bool,
        ) -> Bool;
        fn SetSecurityInfo(
            handle: Handle,
            object_type: Dword,
            security_info: Dword,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> Dword;
        fn GetTokenInformation(
            token: Handle,
            information_class: Dword,
            information: *mut c_void,
            information_length: Dword,
            return_length: *mut Dword,
        ) -> Bool;
        fn OpenProcessToken(process: Handle, desired_access: Dword, token: *mut Handle) -> Bool;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CloseHandle(object: Handle) -> Bool;
        fn CreateDirectoryW(
            path_name: *const u16,
            security_attributes: *const SecurityAttributes,
        ) -> Bool;
        fn CreateFileW(
            file_name: *const u16,
            desired_access: Dword,
            share_mode: Dword,
            security_attributes: *const SecurityAttributes,
            creation_disposition: Dword,
            flags_and_attributes: Dword,
            template_file: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut ByHandleFileInformation,
        ) -> Bool;
        fn GetCurrentProcess() -> Handle;
        fn GetLastError() -> Dword;
        fn LocalFree(memory: LocalHandle) -> LocalHandle;
        fn LockFileEx(
            file: Handle,
            flags: Dword,
            reserved: Dword,
            bytes_low: Dword,
            bytes_high: Dword,
            overlapped: *mut Overlapped,
        ) -> Bool;
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: Dword) -> Bool;
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: Dword,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> Bool;
        fn UnlockFileEx(
            file: Handle,
            reserved: Dword,
            bytes_low: Dword,
            bytes_high: Dword,
            overlapped: *mut Overlapped,
        ) -> Bool;
    }

    struct SecurityDescriptor(*mut c_void);

    impl SecurityDescriptor {
        fn private() -> std::io::Result<Self> {
            let current_user = current_user_sid_string()?;
            let sddl = wide(OsStr::new(&format!(
                "D:P(A;;FA;;;SY)(A;;FA;;;{current_user})"
            )));
            let mut descriptor = null_mut();
            let result = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            };
            if result == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(Self(descriptor))
            }
        }

        fn attributes(&self) -> SecurityAttributes {
            SecurityAttributes {
                _length: std::mem::size_of::<SecurityAttributes>() as Dword,
                _security_descriptor: self.0,
                _inherit_handle: 0,
            }
        }

        fn dacl(&self) -> std::io::Result<*mut c_void> {
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl = null_mut();
            let result = unsafe {
                GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted)
            };
            if result == 0 || present == 0 || dacl.is_null() {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(dacl)
            }
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                let _ = unsafe { LocalFree(self.0) };
            }
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn with_current_user_sid<T>(
        operation: impl FnOnce(*mut c_void) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| {
            let mut required = 0;
            let _ = unsafe {
                GetTokenInformation(token, TOKEN_USER_CLASS, null_mut(), 0, &mut required)
            };
            if required == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
            let mut buffer = vec![0usize; words];
            if unsafe {
                GetTokenInformation(
                    token,
                    TOKEN_USER_CLASS,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut required,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let token_user = unsafe { &*(buffer.as_ptr().cast::<TokenUser>()) };
            operation(token_user.user.sid)
        })();
        let _ = unsafe { CloseHandle(token) };
        result
    }

    fn current_user_sid_string() -> std::io::Result<String> {
        with_current_user_sid(|sid| {
            let mut string_sid = null_mut();
            if unsafe { ConvertSidToStringSidW(sid, &mut string_sid) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut length = 0;
            while unsafe { *string_sid.add(length) } != 0 {
                length += 1;
            }
            let string =
                String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, length) })
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "current-user SID is not valid UTF-16",
                        )
                    });
            let _ = unsafe { LocalFree(string_sid.cast()) };
            string
        })
    }

    fn ensure_current_owner(handle: Handle) -> std::io::Result<()> {
        let mut owner = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }
        let owned = with_current_user_sid(|current| {
            if owner.is_null() || unsafe { EqualSid(owner, current) } == 0 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "credential path is not owned by the current user",
                ))
            } else {
                Ok(())
            }
        });
        if !descriptor.is_null() {
            let _ = unsafe { LocalFree(descriptor) };
        }
        owned
    }

    fn open_handle(
        path: &Path,
        access: Dword,
        disposition: Dword,
        flags: Dword,
        attributes: Option<&SecurityAttributes>,
    ) -> std::io::Result<Handle> {
        let path = wide(path.as_os_str());
        let attributes = attributes.map_or(null(), |value| value as *const SecurityAttributes);
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes,
                disposition,
                flags,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(handle)
        }
    }

    fn information(handle: Handle) -> std::io::Result<ByHandleFileInformation> {
        let mut information = ByHandleFileInformation {
            file_attributes: 0,
            _creation_time_low: 0,
            _creation_time_high: 0,
            _last_access_time_low: 0,
            _last_access_time_high: 0,
            _last_write_time_low: 0,
            _last_write_time_high: 0,
            _volume_serial_number: 0,
            _file_size_high: 0,
            _file_size_low: 0,
            _number_of_links: 0,
            _file_index_high: 0,
            _file_index_low: 0,
        };
        if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(information)
        }
    }

    fn apply_private_dacl(handle: Handle, expect_directory: bool) -> std::io::Result<()> {
        let information = information(handle)?;
        if information.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (information.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != expect_directory
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "credential path is a reparse point or has the wrong type",
            ));
        }
        ensure_current_owner(handle)?;
        let descriptor = SecurityDescriptor::private()?;
        let status = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                descriptor.dacl()?,
                null_mut(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    }

    fn repair_path(path: &Path, directory: bool) -> std::io::Result<()> {
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
        let handle = open_handle(path, READ_CONTROL | WRITE_DAC, OPEN_EXISTING, flags, None)?;
        let result = apply_private_dacl(handle, directory);
        let _ = unsafe { CloseHandle(handle) };
        result
    }

    pub(super) fn ensure_directory(path: &Path) -> std::io::Result<()> {
        if path.file_name().is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "credential root must not be a filesystem root",
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                    && metadata.is_dir() =>
            {
                repair_path(path, true)
            }
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "credential root is a reparse point or has the wrong type",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let descriptor = SecurityDescriptor::private()?;
                let attributes = descriptor.attributes();
                let path_wide = wide(path.as_os_str());
                if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
                    let error = unsafe { GetLastError() };
                    if error != ERROR_ALREADY_EXISTS {
                        return Err(std::io::Error::from_raw_os_error(error as i32));
                    }
                }
                repair_path(path, true)
            }
            Err(error) => Err(error),
        }
    }

    fn create_private_file(path: &Path, disposition: Dword) -> std::io::Result<std::fs::File> {
        let descriptor = SecurityDescriptor::private()?;
        let attributes = descriptor.attributes();
        let handle = open_handle(
            path,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
            disposition,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
            Some(&attributes),
        )?;
        if let Err(error) = apply_private_dacl(handle, false) {
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    pub(super) fn open_existing(path: &Path) -> std::io::Result<std::fs::File> {
        repair_path(path, false)?;
        let before = std::fs::symlink_metadata(path)?;
        if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !before.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "credential file is a reparse point or has the wrong type",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let opened = file.metadata()?;
        let after = std::fs::symlink_metadata(path)?;
        crate::fs::ensure_same_file(path, &before, &opened)?;
        crate::fs::ensure_same_file(path, &opened, &after)?;
        Ok(file)
    }

    pub(super) fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || !metadata.is_file()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "credential lock is a reparse point or has the wrong type",
                    ));
                }
                repair_path(path, false)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        create_private_file(path, OPEN_ALWAYS)
    }

    pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || !metadata.is_file()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "credential target is a reparse point or has the wrong type",
                    ));
                }
                repair_path(path, false)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "credential target has no parent",
            )
        })?;
        ensure_directory(parent)?;
        let temp = parent.join(format!(".oauth-{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut temp_identity = None;
        let write_result = (|| {
            let mut file = create_private_file(&temp, CREATE_NEW)?;
            temp_identity = Some(file.metadata()?);
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            let target_wide = wide(path.as_os_str());
            let temp_wide = wide(temp.as_os_str());
            let target_exists = std::fs::symlink_metadata(path).is_ok();
            let replaced = if target_exists {
                unsafe {
                    ReplaceFileW(
                        target_wide.as_ptr(),
                        temp_wide.as_ptr(),
                        null(),
                        REPLACEFILE_WRITE_THROUGH,
                        null_mut(),
                        null_mut(),
                    )
                }
            } else {
                unsafe {
                    MoveFileExW(
                        temp_wide.as_ptr(),
                        target_wide.as_ptr(),
                        MOVEFILE_WRITE_THROUGH,
                    )
                }
            };
            if replaced == 0 {
                return Err(std::io::Error::last_os_error());
            }
            repair_path(path, false)
        })();
        if write_result.is_err() {
            if let (Some(identity), Ok(current)) =
                (temp_identity.as_ref(), std::fs::symlink_metadata(&temp))
                && crate::fs::ensure_same_file(&temp, identity, &current).is_ok()
            {
                let _ = std::fs::remove_file(&temp);
            }
        }
        write_result
    }

    pub(super) fn lock(file: &std::fs::File) -> std::io::Result<()> {
        let mut overlapped = Overlapped::default();
        if unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        } == 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn unlock(file: &std::fs::File) {
        let mut overlapped = Overlapped::default();
        let _ =
            unsafe { UnlockFileEx(file.as_raw_handle(), 0, u32::MAX, u32::MAX, &mut overlapped) };
    }

    #[cfg(test)]
    pub(super) fn dacl_sddl(path: &Path, directory: bool) -> std::io::Result<String> {
        let flags = FILE_FLAG_OPEN_REPARSE_POINT
            | if directory {
                FILE_FLAG_BACKUP_SEMANTICS
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
        let handle = open_handle(path, READ_CONTROL, OPEN_EXISTING, flags, None)?;
        let result = (|| {
            let mut descriptor = null_mut();
            let status = unsafe {
                GetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut descriptor,
                )
            };
            if status != 0 {
                return Err(std::io::Error::from_raw_os_error(status as i32));
            }
            let mut sddl = null_mut();
            let converted = unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    DACL_SECURITY_INFORMATION,
                    &mut sddl,
                    null_mut(),
                )
            };
            let output = if converted == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                let mut length = 0;
                while unsafe { *sddl.add(length) } != 0 {
                    length += 1;
                }
                String::from_utf16(unsafe { std::slice::from_raw_parts(sddl, length) }).map_err(
                    |_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "credential DACL is not valid UTF-16",
                        )
                    },
                )
            };
            if !sddl.is_null() {
                let _ = unsafe { LocalFree(sddl.cast()) };
            }
            if !descriptor.is_null() {
                let _ = unsafe { LocalFree(descriptor) };
            }
            output
        })();
        let _ = unsafe { CloseHandle(handle) };
        result
    }
}

// The `CredentialStore` trait is declared with `#[async_trait]`, so its methods
// desugar to `-> Pin<Box<dyn Future + Send>>`. We implement that shape directly
// to avoid pulling in the `async-trait` proc-macro as a dependency.
type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AuthError>> + Send + 'a>>;

impl CredentialStore for FileCredentialStore {
    fn load<'life0, 'async_trait>(
        &'life0 self,
    ) -> StoreFuture<'async_trait, Option<StoredCredentials>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.read_blocking())
                .await
                .map_err(|_| storage_error("worker", std::io::ErrorKind::Other))?
        })
    }

    fn save<'life0, 'async_trait>(
        &'life0 self,
        credentials: StoredCredentials,
    ) -> StoreFuture<'async_trait, ()>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.write_blocking(&credentials))
                .await
                .map_err(|_| storage_error("worker", std::io::ErrorKind::Other))?
        })
    }

    fn clear<'life0, 'async_trait>(&'life0 self) -> StoreFuture<'async_trait, ()>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.clear_blocking().map(|_| ()))
                .await
                .map_err(|_| storage_error("worker", std::io::ErrorKind::Other))?
        })
    }
}

/// Delete the stored token for a server. Returns whether a file was removed.
pub fn logout(server_name: &str, url: &str, settings: &OAuthSettings) -> anyhow::Result<bool> {
    FileCredentialStore::new(server_name, url, settings)?
        .clear_blocking()
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Build an [`AuthClient`] for a server that already has stored credentials.
///
/// Returns an error (without prompting) when no usable token is stored, so the
/// caller can tell the user to run `/mcp login`.
pub async fn build_auth_client(
    server_name: &str,
    url: &str,
    settings: &OAuthSettings,
) -> anyhow::Result<AuthClient<reqwest::Client>> {
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| anyhow::anyhow!("OAuth init failed: {e}"))?;
    manager.set_credential_store(FileCredentialStore::new(server_name, url, settings)?);

    let restored = manager
        .initialize_from_store()
        .await
        .map_err(|e| anyhow::anyhow!("OAuth restore failed: {e}"))?;
    if !restored {
        anyhow::bail!("no OAuth token stored; run `/mcp login {server_name}`");
    }

    Ok(AuthClient::new(reqwest::Client::new(), manager))
}

/// Result of starting an interactive login: the URL to open and the live session.
pub struct LoginSession {
    pub auth_url: String,
    session: AuthorizationSession,
    redirect_port: u16,
}

/// Begin an interactive OAuth login: discover metadata, register/authorize, and
/// return the URL the user must open. Call [`LoginSession::wait_for_callback`]
/// to complete the flow.
pub async fn begin_login(
    server_name: &str,
    url: &str,
    settings: &OAuthSettings,
) -> anyhow::Result<LoginSession> {
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| anyhow::anyhow!("OAuth init failed: {e}"))?;
    manager.set_credential_store(FileCredentialStore::new(server_name, url, settings)?);

    let metadata = manager
        .discover_metadata()
        .await
        .map_err(|e| anyhow::anyhow!("OAuth metadata discovery failed: {e}"))?;
    manager.set_metadata(metadata);

    let redirect_uri = settings.redirect_uri();
    let scope_refs: Vec<&str> = settings.scopes.iter().map(|s| s.as_str()).collect();

    let session =
        AuthorizationSession::new(manager, &scope_refs, &redirect_uri, Some(CLIENT_NAME), None)
            .await
            .map_err(|e| anyhow::anyhow!("OAuth authorization setup failed: {e}"))?;

    Ok(LoginSession {
        auth_url: session.get_authorization_url().to_string(),
        session,
        redirect_port: settings.redirect_port(),
    })
}

impl LoginSession {
    /// Run a one-shot loopback listener to catch the redirect, then exchange the
    /// code for a token (persisted via the credential store). Times out after
    /// `timeout`.
    pub async fn wait_for_callback(self, timeout: Duration) -> anyhow::Result<()> {
        let port = self.redirect_port;
        let captured =
            tokio::task::spawn_blocking(move || listen_for_callback(port, timeout)).await??;

        self.session
            .handle_callback(&captured.code, &captured.state)
            .await
            .map_err(|e| anyhow::anyhow!("OAuth token exchange failed: {e}"))?;
        Ok(())
    }
}

struct CapturedCode {
    code: String,
    state: String,
}

/// Blocking single-request loopback HTTP listener for the OAuth redirect.
fn listen_for_callback(port: u16, timeout: Duration) -> anyhow::Result<CapturedCode> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("cannot bind 127.0.0.1:{port} for OAuth redirect: {e}"))?;
    listener.set_nonblocking(false).ok();

    let deadline = std::time::Instant::now() + timeout;
    // Poll accept with a short read timeout so the overall deadline is honored.
    listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("listener config failed: {e}"))?;

    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for OAuth redirect on port {port}");
        }
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let request_line = read_request_line(&mut stream)?;
                let (code, state) = parse_callback(&request_line)?;
                let body = "<html><body><h3>zerostack: authorization complete.</h3>You can close this tab and return to the terminal.</body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                return Ok(CapturedCode { code, state });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(anyhow::anyhow!("accept failed: {e}")),
        }
    }
}

fn read_request_line(stream: &mut std::net::TcpStream) -> anyhow::Result<String> {
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .map_err(|e| anyhow::anyhow!("read redirect request failed: {e}"))?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let first = text
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty redirect request"))?;
    Ok(first.to_string())
}

/// Parse `GET /callback?code=...&state=... HTTP/1.1` and return (code, state).
pub(crate) fn parse_callback(request_line: &str) -> anyhow::Result<(String, String)> {
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("malformed redirect request line"))?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v),
            "error" => error = Some(v),
            _ => {}
        }
    }

    if let Some(err) = error {
        anyhow::bail!("authorization server returned an error: {err}");
    }
    match (code, state) {
        (Some(code), Some(state)) => Ok((code, state)),
        _ => anyhow::bail!("redirect missing code or state"),
    }
}

/// Minimal percent-decoding for query values (handles `%XX` and `+`).
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
