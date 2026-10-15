//! Windows-only, exact-snapshot MCP configuration publication.
//!
//! This does not implement or relax the general workspace exchange API. The
//! KTM transaction owns directory/target write exclusion through replacement;
//! every transacted file handle is closed before MoveFileTransacted/commit.

#![allow(unsafe_code)]

use super::WorkspaceBinding;
use crate::fs::{CheckedMetadata, ContentDigest};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorLength,
    OWNER_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CommitTransaction, CreateFileTransactedW, CreateTransaction, DELETE,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFinalPathNameByHandleW, MOVEFILE_REPLACE_EXISTING, MoveFileTransactedW, OPEN_EXISTING,
    READ_CONTROL, RollbackTransaction, VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
};

const MAX_WINDOWS_PATH: usize = 32_767;
const MAX_SECURITY_DESCRIPTOR: usize = 128 * 1024;

/// Compatibility data only. This cannot open a file or authorize publication.
pub(crate) fn security_fingerprint(file: &File) -> io::Result<[u8; 32]> {
    let mut descriptor = null_mut();
    // SAFETY: the borrowed handle remains live; GetSecurityInfo allocates the
    // descriptor returned through this initialized output pointer.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    struct Descriptor(*mut core::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo returned this unique LocalAlloc buffer.
            unsafe { LocalFree(self.0) };
        }
    }
    let descriptor = Descriptor(descriptor);
    // SAFETY: this is a live security descriptor returned by GetSecurityInfo.
    let length = unsafe { GetSecurityDescriptorLength(descriptor.0) } as usize;
    if length == 0 || length > MAX_SECURITY_DESCRIPTOR {
        return Err(io::Error::other(
            "MCP security descriptor exceeds its bound",
        ));
    }
    // SAFETY: the API-reported descriptor length is checked before borrowing;
    // no descriptor bytes or SIDs are logged, stored as authority, or exposed.
    let bytes = unsafe { std::slice::from_raw_parts(descriptor.0.cast::<u8>(), length) };
    let mut hash = Sha256::new();
    hash.update(b"mini-agent.mcp-security.v1\0");
    hash.update(bytes);
    Ok(hash.finalize().into())
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let count = path.as_os_str().encode_wide().count();
    if count == 0 || count > MAX_WINDOWS_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid MCP path length",
        ));
    }
    let mut value = Vec::with_capacity(count + 1);
    for unit in path.as_os_str().encode_wide() {
        if unit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP path contains NUL",
            ));
        }
        value.push(unit);
    }
    value.push(0);
    Ok(value)
}

fn held_path(file: &File) -> io::Result<PathBuf> {
    // SAFETY: the live borrowed handle is queried without resolving a caller
    // pathname. The zero-size first call only obtains the required capacity.
    let needed = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    if needed > MAX_WINDOWS_PATH + 1 {
        return Err(io::Error::other(
            "retained MCP directory path exceeds its bound",
        ));
    }
    let mut buffer = vec![0u16; needed];
    // SAFETY: writable initialized buffer of the supplied size and live handle.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    if written >= buffer.len() {
        return Err(io::Error::other(
            "retained MCP directory path changed length",
        ));
    }
    buffer.truncate(written);
    // TxF is local-NTFS only. Never turn an unsupported UNC route into a new
    // network access or a path-based nontransactional fallback.
    if buffer.len() < 7
        || buffer[..4] != [92, 92, 63, 92]
        || buffer[5] != 58
        || buffer[6] != 92
        || !matches!(buffer[4], 65..=90 | 97..=122)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "MCP transactions require a local drive",
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

struct Transaction {
    handle: OwnedHandle,
    committed: bool,
}

impl Transaction {
    fn new() -> io::Result<Self> {
        // SAFETY: optional security/GUID/description pointers are null. The
        // transaction is synchronous and remains owned by this blocking job.
        let handle = unsafe { CreateTransaction(null_mut(), null_mut(), 0, 0, 0, 5_000, null()) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: successful CreateTransaction transferred unique ownership.
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
            committed: false,
        })
    }

    fn open(&self, path: &Path, create: bool, directory: bool) -> io::Result<File> {
        let path = wide(path)?;
        let access = GENERIC_READ
            | GENERIC_WRITE
            | DELETE
            | READ_CONTROL
            | if create { WRITE_DAC | WRITE_OWNER } else { 0 };
        // SAFETY: terminated path, null optional pointers, live exact transaction.
        // No backup/restore privilege is acquired or enabled by this operation.
        let handle = unsafe {
            CreateFileTransactedW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                if create { CREATE_NEW } else { OPEN_EXISTING },
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
                null_mut(),
                self.handle.as_raw_handle(),
                null(),
                null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful CreateFileTransactedW returned this unique file.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    fn replace(&self, source: &Path, target: &Path) -> io::Result<()> {
        let source = wide(source)?;
        let target = wide(target)?;
        // SAFETY: exact paths were validated against retained directory handles
        // under this transaction. All transacted file handles are now closed;
        // the SAME transaction retains write/namespace exclusion until commit.
        if unsafe {
            MoveFileTransactedW(
                source.as_ptr(),
                target.as_ptr(),
                None,
                null(),
                MOVEFILE_REPLACE_EXISTING,
                self.handle.as_raw_handle(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn commit(&mut self) -> io::Result<()> {
        // SAFETY: this transaction is live, and all transacted files are closed.
        if unsafe { CommitTransaction(self.handle.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: the owned transaction handle outlives this rollback.
            // A failed/aborted transaction may already be rolled back; never
            // attempt a nontransactional restoration over a newer checkpoint.
            unsafe { RollbackTransaction(self.handle.as_raw_handle()) };
        }
    }
}

fn check_shape(file: &File, directory: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.is_dir() != directory
        || (!directory && (!metadata.is_file() || crate::fs::windows_file_link_count(file)? != 1))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "MCP transaction target has unsafe shape",
        ));
    }
    Ok(())
}

fn check_directory(file: &File, expected: &CheckedMetadata, security: &[u8; 32]) -> io::Result<()> {
    check_shape(file, true)?;
    crate::fs::ensure_same_file(
        Path::new("MCP directory"),
        expected,
        &crate::fs::checked_file_metadata(file)?,
    )?;
    if security_fingerprint(file)? != *security {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "MCP directory security changed",
        ));
    }
    Ok(())
}

pub(crate) fn replace(
    workspace: &WorkspaceBinding,
    path: &Path,
    content: &[u8],
    expected: &CheckedMetadata,
    base: &ContentDigest,
    expected_security: &[u8; 32],
) -> io::Result<()> {
    replace_with_hook(
        workspace,
        path,
        content,
        expected,
        base,
        expected_security,
        || Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn replace_with_hook(
    workspace: &WorkspaceBinding,
    path: &Path,
    content: &[u8],
    expected: &CheckedMetadata,
    base: &ContentDigest,
    expected_security: &[u8; 32],
    before_commit: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if content.len() > crate::extras::mcp::project::MAX_PROJECT_MCP_BYTES {
        return Err(io::Error::other("MCP configuration exceeds 1 MiB"));
    }
    workspace.validate().map_err(io::Error::other)?;
    let relative = workspace.normalize_relative(path)?;
    let (parent, name) = workspace.open_parent(&relative, false)?;
    let parent = parent.into_std_file();
    let parent_identity = crate::fs::checked_file_metadata(&parent)?;
    let parent_security = security_fingerprint(&parent)?;
    let root_security = security_fingerprint(&workspace.directory)?;
    let root_path = held_path(&workspace.directory)?;
    let parent_path = held_path(&parent)?;
    let target_path = parent_path.join(&name);
    let candidate_path = parent_path.join(super::sibling_temp_name(&name));
    let mut transaction = Transaction::new()?;
    {
        // Exact directory writer locks exclude both namespace and security
        // mutation, including preheld WRITE_DAC/WRITE_OWNER interference.
        let root = transaction.open(&root_path, false, true)?;
        check_directory(&root, &workspace.identity, &root_security)?;
        let parent = transaction.open(&parent_path, false, true)?;
        check_directory(&parent, &parent_identity, &parent_security)?;
        workspace.validate().map_err(io::Error::other)?;
        let mut target = transaction.open(&target_path, false, false)?;
        check_shape(&target, false)?;
        crate::fs::ensure_same_file(
            &relative,
            expected,
            &crate::fs::checked_file_metadata(&target)?,
        )?;
        if security_fingerprint(&target)? != *expected_security
            || !base.matches_reader(&mut target)?
        {
            return Err(crate::fs::stale_content_error(&relative));
        }
        let mut candidate = transaction.open(&candidate_path, true, false)?;
        // Use the SAME existing private-image initializer and strict validator.
        // If it is unsupported on a transacted handle, refuse; no path fallback.
        crate::fs::prepare_new_private_file(&candidate)?;
        candidate.write_all(content)?;
        candidate.flush()?;
        crate::fs::validate_private_file(&candidate)?;
        check_directory(&root, &workspace.identity, &root_security)?;
        check_directory(&parent, &parent_identity, &parent_security)?;
        workspace.validate().map_err(io::Error::other)?;
        // All transacted File and temporary CheckedMetadata clones drop HERE.
    }
    transaction.replace(&candidate_path, &target_path)?;
    before_commit()?;
    workspace.validate().map_err(io::Error::other)?;
    transaction.commit()?;
    workspace.validate().map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    struct Fixture {
        workspace: WorkspaceBinding,
        target: PathBuf,
        _owned: crate::tests::OwnedTestDirectory,
    }
    impl Fixture {
        fn new(label: &str) -> Self {
            let owned = crate::tests::OwnedTestDirectory::new(label);
            let state = owned.path().join("private");
            crate::fs::ensure_private_directory(&state).unwrap();
            let nested = state.join("nested");
            crate::fs::ensure_private_directory(&nested).unwrap();
            let target = nested.join(".mcp.json");
            crate::fs::private_atomic_write_sync(&target, b"{\"mcpServers\":{}}").unwrap();
            Self {
                workspace: WorkspaceBinding::capture(&state).unwrap(),
                target,
                _owned: owned,
            }
        }
        fn relative(&self) -> &Path {
            Path::new("nested/.mcp.json")
        }
    }

    fn metadata_writer(path: &Path, access: u32) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .access_mode(access | READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    #[test]
    fn windows_mcp_txf_actual_snapshot_keeps_reader_and_private_candidate() {
        use crate::extras::mcp::project::FileSnapshot;
        let fixture = Fixture::new("mcp-txf-reader");
        let mut survivor = fixture
            .workspace
            .open_regular_relative(fixture.relative())
            .unwrap();
        let snapshot = FileSnapshot::read(&fixture.workspace, fixture.relative()).unwrap();
        let replacement = "{\"mcpServers\":{},\"future\":true}";
        snapshot.publish(&fixture.workspace, replacement).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fixture.target).unwrap(),
            replacement
        );
        crate::fs::validate_private_file(&File::open(&fixture.target).unwrap()).unwrap();
        // Both the explicit survivor and FileSnapshot's CheckedMetadata handle
        // remain live. A surviving view is complete old/new, never partial.
        let mut survived = String::new();
        std::io::Read::read_to_string(&mut survivor, &mut survived).unwrap();
        assert!(survived == snapshot.content || survived == replacement);
        assert!(snapshot.publish(&fixture.workspace, "{}").is_err());
        assert_eq!(
            std::fs::read_to_string(&fixture.target).unwrap(),
            replacement
        );
    }

    #[test]
    fn windows_mcp_txf_preheld_parent_and_root_metadata_writers_refuse_without_change() {
        use crate::extras::mcp::project::FileSnapshot;
        for root in [true, false] {
            for access in [WRITE_DAC, WRITE_OWNER] {
                let fixture = Fixture::new("mcp-txf-parent-authority");
                let snapshot = FileSnapshot::read(&fixture.workspace, fixture.relative()).unwrap();
                let path = if root {
                    fixture.workspace.root()
                } else {
                    fixture.target.parent().unwrap()
                };
                let held = metadata_writer(path, access).unwrap();
                let error = snapshot.publish(&fixture.workspace, "{}").unwrap_err();
                assert!(
                    matches!(error.raw_os_error(), Some(32 | 6800)),
                    "unexpected native refusal: {error}"
                );
                assert_eq!(
                    std::fs::read_to_string(&fixture.target).unwrap(),
                    snapshot.content
                );
                drop(held);
                snapshot.publish(&fixture.workspace, "{}").unwrap();
                assert_eq!(std::fs::read_to_string(&fixture.target).unwrap(), "{}");
            }
        }
    }

    #[test]
    fn windows_mcp_txf_root_replacement_never_targets_new_namespace() {
        use crate::extras::mcp::project::FileSnapshot;
        let fixture = Fixture::new("mcp-txf-root-replacement");
        let snapshot = FileSnapshot::read(&fixture.workspace, fixture.relative()).unwrap();
        let original = fixture.workspace.root().to_owned();
        let retained = original.with_extension("retained");
        std::fs::rename(&original, &retained).unwrap();
        crate::fs::ensure_private_directory(&original).unwrap();
        crate::fs::ensure_private_directory(&original.join("nested")).unwrap();
        std::fs::write(&fixture.target, "NEW_NAMESPACE_UNCHANGED").unwrap();
        assert!(snapshot.publish(&fixture.workspace, "{}").is_err());
        assert_eq!(
            std::fs::read_to_string(&fixture.target).unwrap(),
            "NEW_NAMESPACE_UNCHANGED"
        );
        assert_eq!(
            std::fs::read_to_string(retained.join(fixture.relative())).unwrap(),
            snapshot.content
        );
    }

    #[test]
    fn windows_mcp_txf_error_before_commit_rolls_back_and_keeps_directory_exclusion() {
        let fixture = Fixture::new("mcp-txf-rollback");
        let original = std::fs::read(&fixture.target).unwrap();
        let file = fixture
            .workspace
            .open_regular_relative(fixture.relative())
            .unwrap();
        let expected = crate::fs::checked_file_metadata(&file).unwrap();
        let security = security_fingerprint(&file).unwrap();
        let error = replace_with_hook(
            &fixture.workspace,
            fixture.relative(),
            b"{}",
            &expected,
            &ContentDigest::of(&original),
            &security,
            || {
                assert_eq!(std::fs::read(&fixture.target).unwrap(), original);
                for path in [fixture.workspace.root(), fixture.target.parent().unwrap()] {
                    for access in [WRITE_DAC, WRITE_OWNER] {
                        let error = metadata_writer(path, access).unwrap_err();
                        assert!(
                            matches!(error.raw_os_error(), Some(32 | 6800)),
                            "unexpected native exclusion: {error}"
                        );
                    }
                }
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "owned publication cancelled",
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(fixture.target.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn windows_mcp_txf_cancelled_waiter_retains_blocking_transaction_until_rollback() {
        use std::sync::Arc;
        let fixture = Arc::new(Fixture::new("mcp-txf-owned-retirement"));
        let original = std::fs::read(&fixture.target).unwrap();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cancellation = scope.cancellation_handle();
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let worker_fixture = fixture.clone();
        let worker = scope
            .run(async {
                // Construct INSIDE the actual owner task-local. The worker's Arc
                // retains fixture cleanup even if the test unwinds before release.
                crate::agent::runner::spawn_blocking_scoped(move || {
                    let fixture = worker_fixture;
                    let file = fixture
                        .workspace
                        .open_regular_relative(fixture.relative())?;
                    let expected = crate::fs::checked_file_metadata(&file)?;
                    let security = security_fingerprint(&file)?;
                    let base = ContentDigest::of(&std::fs::read(&fixture.target)?);
                    replace_with_hook(
                        &fixture.workspace,
                        fixture.relative(),
                        b"{}",
                        &expected,
                        &base,
                        &security,
                        || {
                            let _ = entered.send(());
                            release_rx.recv().map_err(io::Error::other)?;
                            if cancellation.is_cancelled() {
                                return Err(io::Error::new(
                                    io::ErrorKind::Interrupted,
                                    "owned publication cancelled",
                                ));
                            }
                            Ok(())
                        },
                    )
                })
            })
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .unwrap()
            .unwrap();
        scope.cancellation_handle().cancel();
        drop(worker); // Dropping the awaiter never drops its running IO owner.
        let idle = scope.wait_idle();
        tokio::pin!(idle);
        assert!(futures::poll!(&mut idle).is_pending());
        assert_eq!(std::fs::read(&fixture.target).unwrap(), original);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut idle)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&fixture.target).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(fixture.target.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }
}
