#[cfg(not(windows))]
use std::path::Path;

/// Existing-file access without permission repair. Append never creates,
/// truncates, replaces or widens an existing file's permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivateFileAccess {
    Read,
    Append,
}

fn private_file_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "private file must be owned, private, singly linked and regular",
    )
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy)]
struct PrivateAce {
    kind: u8,
    flags: u8,
    mask: u32,
    current_user: bool,
    system: bool,
}

/// The closed policy is structural. Trustee spellings in SDDL are not identity:
/// Windows can render the current user's numeric SID using an account alias.
#[cfg(any(windows, test))]
fn closed_private_acl(protected: bool, aces: &[PrivateAce]) -> bool {
    const ACCESS_ALLOWED: u8 = 0;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
    protected
        && aces.len() == 2
        && aces.iter().all(|ace| {
            ace.kind == ACCESS_ALLOWED
                && ace.flags == 0
                && ace.mask == FILE_ALL_ACCESS
                && (ace.current_user || ace.system)
        })
        && aces.iter().any(|ace| ace.current_user)
        && aces.iter().any(|ace| ace.system)
}

#[cfg(not(any(unix, windows)))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private persistence is unsupported on this platform",
    )
}

#[cfg(unix)]
const OPEN_NOFOLLOW: std::os::raw::c_int = libc::O_NOFOLLOW;

#[cfg(unix)]
const OPEN_DIRECTORY: std::os::raw::c_int = libc::O_DIRECTORY;

#[cfg(unix)]
const OPEN_CLOEXEC: std::os::raw::c_int = libc::O_CLOEXEC;

#[cfg(unix)]
fn stage_error(stage: &'static str, error: std::io::Error) -> std::io::Error {
    #[cfg(test)]
    eprintln!("PRIVATE_PERSISTENCE_FAILED={stage}");
    std::io::Error::new(
        error.kind(),
        format!("private persistence failed at {stage}: {error}"),
    )
}

#[cfg(unix)]
fn open_existing_stage<T>(_stage: &'static str, result: std::io::Result<T>) -> std::io::Result<T> {
    #[cfg(test)]
    if result.is_err() {
        eprintln!("PRIVATE_OPEN_FAILED={_stage}");
    }
    result
}

#[cfg(unix)]
pub(crate) fn ensure_directory(path: &Path) -> std::io::Result<()> {
    ensure_directory_inner(path, true)
}

/// Create the parent of a user-selected export destination.
///
/// Application-private state is created and repaired at 0700, but an operator's
/// own output directory is not application state: an export must not silently
/// narrow the permissions or ACLs of a directory that already exists. Missing
/// components are still created privately, and the no-follow/ownership checks
/// are unchanged.
#[cfg(all(unix, feature = "skills"))]
pub(crate) fn ensure_export_directory(path: &Path) -> std::io::Result<()> {
    ensure_directory_inner(path, false)
}

#[cfg(unix)]
fn ensure_directory_inner(path: &Path, repair_existing: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    if path.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private directory must not be a filesystem root",
        ));
    }

    let existed = match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(stage_error("directory_inspection", error)),
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|error| stage_error("directory_create", error))?;

    let before = super::checked_path_metadata(path)
        .map_err(|error| stage_error("directory_initial_identity", error))?;
    if before.file_type().is_symlink() || !before.is_dir() || before.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private directory is not an owned real directory",
        ));
    }
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC)
        .open(path)
        .map_err(|error| stage_error("directory_open", error))?;
    let opened = super::checked_file_metadata(&directory)
        .map_err(|error| stage_error("directory_open_identity", error))?;
    super::ensure_same_file(path, &before, &opened)
        .map_err(|error| stage_error("directory_initial_revalidation", error))?;
    if repair_existing || !existed {
        directory
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|error| stage_error("directory_permissions", error))?;
    }
    let after = super::checked_path_metadata(path)
        .map_err(|error| stage_error("directory_final_identity", error))?;
    super::ensure_same_file(path, &opened, &after)
        .map_err(|error| stage_error("directory_final_revalidation", error))
}

#[cfg(unix)]
fn same_open_file_identity(
    left_file: &std::fs::File,
    left: &std::fs::Metadata,
    right_file: &std::fs::File,
    right: &std::fs::Metadata,
) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        // APFS can present different logical device/file IDs even to two
        // independently opened descriptors for the same path. Compare the
        // physical filesystem and 64-bit file identity instead.
        let _ = (left, right);
        Ok(super::macos_real_file_identity(left_file)?
            == super::macos_real_file_identity(right_file)?)
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::fs::MetadataExt;

        let _ = (left_file, right_file);
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
}

#[cfg(all(test, unix))]
type OpenTestHook = Box<dyn FnMut(&'static str)>;

#[cfg(all(test, unix))]
thread_local! {
    static OPEN_TEST_HOOK: std::cell::RefCell<Option<OpenTestHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn run_open_test_hook(stage: &'static str) {
    OPEN_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(stage);
        }
    });
}

#[cfg(all(test, unix))]
pub(crate) fn with_open_test_hook<T>(
    hook: impl FnMut(&'static str) + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<OpenTestHook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPEN_TEST_HOOK.with(|hook| *hook.borrow_mut() = self.0.take());
        }
    }
    let previous = OPEN_TEST_HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
    let _restore = Restore(previous);
    operation()
}

#[cfg(unix)]
pub(crate) fn open_existing(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    // Use lstat only to reject an initial symlink before opening. On APFS,
    // especially across the /Users firmlink, path and descriptor metadata can
    // legitimately report different device IDs for the same inode. The final
    // identity proof therefore compares the physical filesystem and 64-bit
    // file identity of two independently opened descriptors while `file`
    // keeps the selected inode alive throughout the check.
    let before = open_existing_stage("initial_metadata", std::fs::symlink_metadata(path))?;
    if before.file_type().is_symlink()
        || !before.is_file()
        || before.uid() != current_uid()
        || before.nlink() != 1
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private file is not an owned regular file",
        ));
    }
    #[cfg(test)]
    run_open_test_hook("initial_open");
    let file = open_existing_stage(
        "initial_open",
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC | libc::O_NONBLOCK)
            .open(path),
    )?;
    let opened = open_existing_stage("initial_open_metadata", file.metadata())?;
    if !opened.is_file() || opened.uid() != current_uid() || opened.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private file is not an owned regular file",
        ));
    }
    open_existing_stage(
        "permissions",
        file.set_permissions(std::fs::Permissions::from_mode(0o600)),
    )?;
    #[cfg(test)]
    run_open_test_hook("revalidation_open");
    let current = open_existing_stage(
        "revalidation_open",
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC | libc::O_NONBLOCK)
            .open(path),
    )?;
    let after = open_existing_stage("revalidation_metadata", current.metadata())?;
    if !after.is_file() || after.uid() != current_uid() || after.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private file is not an owned singly linked regular file",
        ));
    }
    if !open_existing_stage(
        "revalidation_identity",
        same_open_file_identity(&file, &opened, &current, &after),
    )? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("Path changed after permission check: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn validate_checked(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(private_file_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn open_checked(
    path: &Path,
    access: PrivateFileAccess,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let private = |metadata: &std::fs::Metadata| {
        metadata.is_file()
            && metadata.uid() == current_uid()
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0
    };
    let before = std::fs::symlink_metadata(path)?;
    if !private(&before) || before.file_type().is_symlink() {
        return Err(private_file_error());
    }
    #[cfg(test)]
    run_open_test_hook("checked_before_initial_open");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(access == PrivateFileAccess::Append)
        .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    if !private(&opened) {
        return Err(private_file_error());
    }
    #[cfg(test)]
    run_open_test_hook("checked_before_revalidation_open");
    let current = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let after = current.metadata()?;
    if !private(&after) || !same_open_file_identity(&file, &opened, &current, &after)? {
        return Err(private_file_error());
    }
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    prepare_write(path).map_err(|error| stage_error("write_prepare", error))?;
    super::atomic_write_sync(path, bytes)
        .map_err(|error| stage_error("write_publication", error))?;
    drop(open_existing(path).map_err(|error| stage_error("write_final_revalidation", error))?);
    Ok(())
}

#[cfg(unix)]
pub(crate) fn atomic_create(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_create_with_parent_policy(path, bytes, true)
}

/// Create a private, create-only file at a user-selected export destination.
///
/// The file itself keeps 0600 and the atomic no-follow publication; only the
/// treatment of an already existing parent differs from `atomic_create`.
#[cfg(all(unix, feature = "skills"))]
pub(crate) fn atomic_create_export(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_create_with_parent_policy(path, bytes, false)
}

#[cfg(unix)]
fn atomic_create_with_parent_policy(
    path: &Path,
    bytes: &[u8],
    repair_existing_parent: bool,
) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private file must have a parent directory",
        )
    })?;
    ensure_directory_inner(parent, repair_existing_parent)
        .map_err(|error| stage_error("create_parent", error))?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private create target is not a regular file",
            ));
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "private create target already exists",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(stage_error("create_target_inspection", error)),
    }
    super::atomic_create_sync(path, bytes)
        .map_err(|error| stage_error("create_publication", error))?;
    drop(open_existing(path).map_err(|error| stage_error("create_final_revalidation", error))?);
    Ok(())
}

#[cfg(all(test, unix))]
pub(crate) fn atomic_write_with_failure(
    path: &Path,
    bytes: &[u8],
    fail_rename: bool,
) -> std::io::Result<()> {
    prepare_write(path)?;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private file must have a parent directory",
        )
    })?;
    super::atomic_write_with_failure_sync(parent, path, bytes, fail_rename)
}

#[cfg(unix)]
fn prepare_write(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private file must have a parent directory",
        )
    })?;
    ensure_directory(parent).map_err(|error| stage_error("write_parent", error))?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => drop(
            open_existing(path).map_err(|error| stage_error("write_target_revalidation", error))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(stage_error("write_target_inspection", error)),
    }
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn current_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: `getuid` takes no arguments and has no failure mode.
    unsafe { getuid() }
}

#[cfg(windows)]
pub(crate) use windows::{
    atomic_create, atomic_write, ensure_directory, open_checked, open_existing, validate_checked,
};

#[cfg(all(windows, feature = "skills"))]
pub(crate) use windows::{atomic_create_export, ensure_export_directory};

#[cfg(all(test, windows))]
pub(crate) use windows::dacl_sddl;

#[cfg(not(any(unix, windows)))]
pub(crate) fn ensure_directory(_path: &Path) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(all(not(any(unix, windows)), feature = "skills"))]
pub(crate) fn ensure_export_directory(_path: &Path) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(all(not(any(unix, windows)), feature = "skills"))]
pub(crate) fn atomic_create_export(_path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_existing(_path: &Path) -> std::io::Result<std::fs::File> {
    Err(unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn validate_checked(_file: &std::fs::File) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_checked(
    _path: &Path,
    _access: PrivateFileAccess,
) -> std::io::Result<std::fs::File> {
    Err(unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn atomic_write(_path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn atomic_create(_path: &Path, _bytes: &[u8]) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use std::ffi::{OsStr, c_void};
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    type LocalHandle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_READ: Dword = 0x8000_0000;
    const GENERIC_WRITE: Dword = 0x4000_0000;
    const DELETE: Dword = 0x0001_0000;
    const READ_CONTROL: Dword = 0x0002_0000;
    const WRITE_DAC: Dword = 0x0004_0000;
    const FILE_SHARE_READ: Dword = 0x0000_0001;
    const FILE_SHARE_WRITE: Dword = 0x0000_0002;
    const FILE_SHARE_DELETE: Dword = 0x0000_0004;
    const CREATE_NEW: Dword = 1;
    const OPEN_EXISTING: Dword = 3;
    const FILE_ATTRIBUTE_NORMAL: Dword = 0x0000_0080;
    const FILE_ATTRIBUTE_DIRECTORY: Dword = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: Dword = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: Dword = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: Dword = 0x0200_0000;
    const FILE_FLAG_WRITE_THROUGH: Dword = 0x8000_0000;
    const MOVEFILE_REPLACE_EXISTING: Dword = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: Dword = 0x0000_0008;
    const SDDL_REVISION_1: Dword = 1;
    const SE_FILE_OBJECT: Dword = 1;
    const OWNER_SECURITY_INFORMATION: Dword = 0x0000_0001;
    const DACL_SECURITY_INFORMATION: Dword = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: Dword = 0x8000_0000;
    const TOKEN_QUERY: Dword = 0x0000_0008;
    const TOKEN_USER_CLASS: Dword = 1;
    const ERROR_ACCESS_DENIED: Dword = 5;
    const ERROR_SHARING_VIOLATION: Dword = 32;
    const ERROR_LOCK_VIOLATION: Dword = 33;
    const ERROR_ALREADY_EXISTS: Dword = 183;

    #[repr(C)]
    struct SecurityAttributes {
        _length: Dword,
        _security_descriptor: *mut c_void,
        _inherit_handle: Bool,
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
        #[cfg(test)]
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
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: Dword) -> Bool;
    }

    struct SecurityDescriptor(*mut c_void);

    impl SecurityDescriptor {
        fn private() -> std::io::Result<Self> {
            let current_user = current_user_sid_string()?;
            let sddl = wide(OsStr::new(&format!(
                "O:{current_user}D:P(A;;FA;;;SY)(A;;FA;;;{current_user})"
            )));
            let mut descriptor = null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            } == 0
            {
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
            if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
                == 0
                || present == 0
                || dacl.is_null()
            {
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
            let value =
                String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, length) })
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "current-user SID is not valid UTF-16",
                        )
                    });
            let _ = unsafe { LocalFree(string_sid.cast()) };
            value
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
                    "private path is not owned by the current user",
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

    #[cfg(feature = "mcp")]
    pub(crate) fn prepare_new_private_file(file: &std::fs::File) -> std::io::Result<()> {
        if file.metadata()?.len() != 0 {
            return Err(super::private_file_error());
        }
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_READ_ATTRIBUTES, READ_CONTROL, ReOpenFile, WRITE_DAC, WRITE_OWNER,
        };
        // SAFETY: the live File owns this regular temporary image. ReOpenFile
        // retains that exact object rather than resolving an ambient path;
        // the new handle is transferred exactly once into File for cleanup.
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle() as Handle,
                READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let reopened = unsafe { std::fs::File::from_raw_handle(handle as _) };
        let information = information(handle)?;
        if information.file_attributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
            != 0
            || information._number_of_links != 1
            || information._file_size_high != 0
            || information._file_size_low != 0
        {
            return Err(super::private_file_error());
        }
        // Only this newly created, empty, single-link temporary image may be
        // initialized. Elevated tokens can default its owner to Administrators;
        // install the explicit user owner before writing any private bytes.
        // Existing destination readers/validators still reject foreign owners.
        let descriptor = SecurityDescriptor::private()?;
        with_current_user_sid(|owner| {
            let status = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION,
                    owner,
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
        })?;
        // Recheck the same object, including its explicit owner, after setup.
        apply_private_dacl(reopened.as_raw_handle() as Handle, false)
    }

    fn apply_private_dacl(handle: Handle, expect_directory: bool) -> std::io::Result<()> {
        let information = information(handle)?;
        if information.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (information.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != expect_directory
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private path is a reparse point or has the wrong type",
            ));
        }
        if !expect_directory && information._number_of_links != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private file is hardlinked",
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

    pub(crate) fn ensure_directory(path: &Path) -> std::io::Result<()> {
        ensure_directory_inner(path, true)
    }

    /// See the Unix `ensure_export_directory`: an operator's existing output
    /// directory keeps its ACL, while missing components are created private.
    #[cfg(feature = "skills")]
    pub(crate) fn ensure_export_directory(path: &Path) -> std::io::Result<()> {
        ensure_directory_inner(path, false)
    }

    fn ensure_directory_inner(path: &Path, repair_existing: bool) -> std::io::Result<()> {
        if path.file_name().is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "private directory must not be a filesystem root",
            ));
        }
        if let Some(parent) = path.parent() {
            match std::fs::symlink_metadata(parent) {
                Ok(metadata)
                    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                        || !metadata.is_dir() =>
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "private directory parent is a reparse point or has the wrong type",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ensure_directory_inner(parent, repair_existing)?;
                }
                Err(error) => return Err(error),
            }
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                    && metadata.is_dir() =>
            {
                if repair_existing {
                    repair_path(path, true)
                } else {
                    Ok(())
                }
            }
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private directory is a reparse point or has the wrong type",
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

    fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
        let descriptor = SecurityDescriptor::private()?;
        let attributes = descriptor.attributes();
        let handle = open_handle(
            path,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
            Some(&attributes),
        )?;
        if let Err(error) = apply_private_dacl(handle, false) {
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    fn replace_with_retry(
        temp_wide: &[u16],
        target_wide: &[u16],
        target: &Path,
        flags: Dword,
    ) -> std::io::Result<()> {
        super::super::retry_windows_lock_violation(
            || {
                if unsafe { MoveFileExW(temp_wide.as_ptr(), target_wide.as_ptr(), flags) } != 0 {
                    Ok(())
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
                        return Err(error);
                    }

                    if delete_share_is_locked(target) {
                        return Err(std::io::Error::from_raw_os_error(
                            ERROR_SHARING_VIOLATION as i32,
                        ));
                    }

                    // MoveFileEx reports ACCESS_DENIED for a target opened
                    // without delete sharing. The blocking handle can close
                    // between that failure and the diagnostic open above, so
                    // retry publication once before treating ACCESS_DENIED as
                    // a stable permission failure. A still-live blocker is
                    // converted to SHARING_VIOLATION for the bounded retry.
                    if unsafe { MoveFileExW(temp_wide.as_ptr(), target_wide.as_ptr(), flags) } != 0
                    {
                        return Ok(());
                    }
                    let retry_error = std::io::Error::last_os_error();
                    if retry_error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
                        && delete_share_is_locked(target)
                    {
                        Err(std::io::Error::from_raw_os_error(
                            ERROR_SHARING_VIOLATION as i32,
                        ))
                    } else {
                        Err(retry_error)
                    }
                }
            },
            || false,
        )
    }

    fn delete_share_is_locked(path: &Path) -> bool {
        match open_handle(
            path,
            DELETE,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        ) {
            Ok(handle) => {
                let _ = unsafe { CloseHandle(handle) };
                false
            }
            Err(error) => matches!(
                error.raw_os_error(),
                Some(code)
                    if code == ERROR_SHARING_VIOLATION as i32
                        || code == ERROR_LOCK_VIOLATION as i32
            ),
        }
    }

    pub(crate) fn open_existing(path: &Path) -> std::io::Result<std::fs::File> {
        repair_path(path, false)?;
        let before = super::super::checked_path_metadata(path)?;
        if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !before.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private file is a reparse point or has the wrong type",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if super::super::windows_file_link_count(&file)? != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private file is hardlinked",
            ));
        }
        let opened = super::super::checked_file_metadata(&file)?;
        let after = super::super::checked_path_metadata(path)?;
        super::super::ensure_same_file(path, &before, &opened)?;
        super::super::ensure_same_file(path, &opened, &after)?;
        Ok(file)
    }

    pub(crate) fn validate_checked(file: &std::fs::File) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        let handle = file.as_raw_handle();
        let information = information(handle)?;
        if information.file_attributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
            != 0
            || information._number_of_links != 1
        {
            return Err(super::private_file_error());
        }
        ensure_current_owner(handle)?;
        struct LocalAllocation(*mut c_void);
        impl Drop for LocalAllocation {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe {
                        LocalFree(self.0);
                    }
                }
            }
        }
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
        let _descriptor = LocalAllocation(descriptor);
        validate_private_dacl(descriptor)
    }

    fn validate_private_dacl(descriptor: *mut c_void) -> std::io::Result<()> {
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, GetAce, GetSecurityDescriptorControl, IsValidAcl, IsValidSid,
            IsWellKnownSid, SE_DACL_PROTECTED, WinLocalSystemSid,
        };
        let mut control = 0;
        let mut revision = 0;
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = null_mut();
        // SAFETY: descriptor is the live allocation returned by GetSecurityInfo
        // or ConvertStringSecurityDescriptorToSecurityDescriptorW. Both native
        // queries borrow it; it remains retained for all ACL/ACE/SID reads below.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            } == 0
            || present == 0
            || dacl.is_null()
            || unsafe { IsValidAcl(dacl.cast()) } == 0
        {
            return Err(super::private_file_error());
        }
        let dacl = dacl.cast::<windows_sys::Win32::Security::ACL>();
        if unsafe { (*dacl).AceCount } != 2 {
            return Err(super::private_file_error());
        }
        with_current_user_sid(|current| {
            let mut aces = Vec::with_capacity(2);
            for index in 0..2 {
                let mut raw = null_mut();
                if unsafe { GetAce(dacl, index, &mut raw) } == 0 || raw.is_null() {
                    return Err(super::private_file_error());
                }
                // IsValidAcl/GetAce establish this ACE's bounded allocation.
                // Check the complete SID extent before any SID API may read it;
                // callback/object ACEs and appended data are not our closed policy.
                let header = unsafe { &*raw.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
                let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
                let size = usize::from(header.AceSize);
                if header.AceType != 0 || size < sid_offset + 8 {
                    return Err(super::private_file_error());
                }
                let sid = unsafe { raw.cast::<u8>().add(sid_offset) };
                let subauthorities = unsafe { *sid.add(1) };
                if subauthorities > 15 || size != sid_offset + 8 + usize::from(subauthorities) * 4 {
                    return Err(super::private_file_error());
                }
                if unsafe { IsValidSid(sid.cast()) } == 0 {
                    return Err(super::private_file_error());
                }
                let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
                aces.push(super::PrivateAce {
                    kind: header.AceType,
                    flags: header.AceFlags,
                    mask: ace.Mask,
                    current_user: unsafe { EqualSid(sid.cast(), current) } != 0,
                    system: unsafe { IsWellKnownSid(sid.cast(), WinLocalSystemSid) } != 0,
                });
            }
            if super::closed_private_acl(control & SE_DACL_PROTECTED != 0, &aces) {
                Ok(())
            } else {
                Err(super::private_file_error())
            }
        })
    }

    pub(crate) fn open_checked(
        path: &Path,
        access: super::PrivateFileAccess,
    ) -> std::io::Result<std::fs::File> {
        let before = std::fs::symlink_metadata(path)?;
        if !before.is_file() || before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(super::private_file_error());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(access == super::PrivateFileAccess::Append)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        validate_checked(&file)?;
        let current = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        validate_checked(&current)?;
        let opened = super::super::checked_file_metadata(&file)?;
        let after = super::super::checked_file_metadata(&current)?;
        super::super::ensure_same_file(path, &opened, &after)?;
        Ok(file)
    }

    pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        atomic_write_mode(path, bytes, false)
    }

    pub(crate) fn atomic_create(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        atomic_write_mode(path, bytes, true)
    }

    #[cfg(feature = "skills")]
    pub(crate) fn atomic_create_export(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        // The Windows publication path creates the file with a private
        // descriptor and never rewrites an existing parent's ACL, so the export
        // variant differs only in the directory policy applied by the caller.
        atomic_write_mode(path, bytes, true)
    }

    fn atomic_write_mode(path: &Path, bytes: &[u8], create_only: bool) -> std::io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || !metadata.is_file()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "private target is a reparse point or has the wrong type",
                    ));
                }
                if create_only {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "private create target already exists",
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
                "private target has no parent",
            )
        })?;
        ensure_directory(parent)?;
        let temp = parent.join(format!(".zsconfig.{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut temp_identity = None;
        let result = (|| {
            let mut file = create_private_file(&temp)?;
            temp_identity = Some(super::super::checked_file_metadata(&file)?);
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);

            let target_wide = wide(path.as_os_str());
            let temp_wide = wide(temp.as_os_str());
            let flags = MOVEFILE_WRITE_THROUGH
                | if create_only {
                    0
                } else {
                    MOVEFILE_REPLACE_EXISTING
                };
            if create_only {
                if unsafe { MoveFileExW(temp_wide.as_ptr(), target_wide.as_ptr(), flags) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else {
                replace_with_retry(&temp_wide, &target_wide, path, flags)?;
            }
            repair_path(path, false)
        })();
        if result.is_err()
            && let (Some(identity), Ok(current)) = (
                temp_identity.as_ref(),
                super::super::checked_path_metadata(&temp),
            )
            && super::super::ensure_same_file(&temp, identity, &current).is_ok()
        {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// Native falsification experiments, not a production publication protocol.
    #[cfg(all(test, feature = "mcp"))]
    mod operator_config_lease_probe {
        use super::*;
        use std::io::{Read, Seek, SeekFrom};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        };
        use windows_sys::Win32::Foundation::{
            OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_READ_ATTRIBUTES, FILE_TRAVERSE, SYNCHRONIZE,
        };
        use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

        const LEAF: &str = "inert-owned-record";
        const OLD: &[u8] = b"{\"future\":\"keep\",\"servers\":{}}";
        const ALL_SHARES: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

        struct Fixture {
            // Directory handle closes before the owning root is removed.
            directory: std::fs::File,
            root: crate::tests::OwnedTestDirectory,
            expected: crate::fs::CheckedMetadata,
        }
        impl Fixture {
            fn new() -> Self {
                let root = crate::tests::OwnedTestDirectory::new("mcp-operator-lease-native");
                let directory = std::fs::OpenOptions::new()
                    .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(root.path())
                    .unwrap();
                crate::fs::ensure_same_file(
                    root.path(),
                    &crate::fs::checked_path_metadata(root.path()).unwrap(),
                    &crate::fs::checked_file_metadata(&directory).unwrap(),
                )
                .unwrap();
                let mut file = create_private_file(&root.path().join(LEAF)).unwrap();
                prepare_new_private_file(&file).unwrap();
                validate_checked(&file).unwrap();
                file.write_all(OLD).unwrap();
                file.sync_all().unwrap();
                let expected = crate::fs::checked_file_metadata(&file).unwrap();
                drop(file);
                Self {
                    directory,
                    root,
                    expected,
                }
            }
            fn open(&self, access: u32, sharing: u32) -> std::io::Result<std::fs::File> {
                relative_open(&self.directory, LEAF, access, sharing)
            }
            fn lease(&self) -> std::io::Result<std::fs::File> {
                self.open(GENERIC_READ | GENERIC_WRITE | DELETE | READ_CONTROL, 0)
            }
            fn validate(&self, file: &std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
                validate_checked(file)?;
                crate::fs::ensure_same_file(
                    Path::new(LEAF),
                    &self.expected,
                    &crate::fs::checked_file_metadata(file)?,
                )?;
                if held_bytes(file)? != bytes {
                    return Err(std::io::Error::other("inert content conflict"));
                }
                Ok(())
            }
        }
        fn held_bytes(file: &std::fs::File) -> std::io::Result<Vec<u8>> {
            let mut file = file.try_clone()?;
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            file.take(4097).read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                return Err(std::io::Error::other("inert probe bounds"));
            }
            Ok(bytes)
        }
        fn relative_open(
            directory: &std::fs::File,
            leaf: &str,
            access: u32,
            sharing: u32,
        ) -> std::io::Result<std::fs::File> {
            if leaf.is_empty()
                || leaf.len() > 64
                || matches!(leaf, "." | "..")
                || leaf.contains(['\\', '/', ':', '\0'])
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "inert leaf invalid",
                ));
            }
            let mut name: Vec<u16> = leaf.encode_utf16().collect();
            let unicode = UNICODE_STRING {
                Length: (name.len() * 2) as u16,
                MaximumLength: (name.len() * 2) as u16,
                Buffer: name.as_mut_ptr(),
            };
            let attrs = OBJECT_ATTRIBUTES {
                Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
                RootDirectory: directory.as_raw_handle().cast(),
                ObjectName: &unicode,
                Attributes: OBJ_CASE_INSENSITIVE,
                SecurityDescriptor: null_mut(),
                SecurityQualityOfService: null_mut(),
            };
            let mut handle = null_mut();
            let mut status = IO_STATUS_BLOCK::default();
            // SAFETY: both retained handles and initialized leaf/buffers live through FILE_OPEN;
            // no create/truncate disposition or ambient path is admitted by this test helper.
            let code = unsafe {
                NtCreateFile(
                    &mut handle,
                    access | SYNCHRONIZE,
                    &attrs,
                    &mut status,
                    null_mut(),
                    FILE_ATTRIBUTE_NORMAL,
                    sharing,
                    FILE_OPEN,
                    FILE_NON_DIRECTORY_FILE
                        | FILE_OPEN_REPARSE_POINT
                        | FILE_SYNCHRONOUS_IO_NONALERT,
                    null_mut(),
                    0,
                )
            };
            if code < 0 {
                return Err(std::io::Error::from_raw_os_error(
                    unsafe { RtlNtStatusToDosError(code) } as i32,
                ));
            }
            Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
        }
        fn expect_sharing_conflict(result: std::io::Result<std::fs::File>) {
            match result {
                Err(error) => assert_eq!(
                    error.raw_os_error(),
                    Some(ERROR_SHARING_VIOLATION as i32),
                    "{error}"
                ),
                Ok(file) => {
                    drop(file);
                    panic!("native candidate rejected: conflicting data access admitted");
                }
            }
        }
        fn observe_writable_view(fixture: &Fixture) {
            let observed = match fixture.lease() {
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                    CandidateObservation::WritableViewExcluded
                }
                Ok(lease) => {
                    fixture.validate(&lease, OLD).unwrap();
                    drop(lease);
                    // A retained PAGE_READWRITE/FILE_MAP_WRITE capability on
                    // this exact object refutes universal writer exclusion.
                    // No candidate data write is attempted in this branch.
                    CandidateObservation::WritableViewAdmittedRejectCandidate
                }
            };
            eprintln!("MCP57_1_WRITABLE_VIEW={observed:?}");
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateFileMappingW(
                file: Handle,
                attrs: *const SecurityAttributes,
                protection: Dword,
                high: Dword,
                low: Dword,
                name: *const u16,
            ) -> Handle;
            fn MapViewOfFile(
                mapping: Handle,
                access: Dword,
                high: Dword,
                low: Dword,
                bytes: usize,
            ) -> *mut c_void;
            fn UnmapViewOfFile(view: *const c_void) -> Bool;
        }
        struct Mapping {
            view: *mut c_void,
            handle: Option<Handle>,
        }
        impl Mapping {
            fn new(file: &std::fs::File, writable: bool) -> Self {
                // SAFETY: fresh owned regular inert file; view length is four initialized bytes.
                let handle = unsafe {
                    CreateFileMappingW(
                        file.as_raw_handle().cast(),
                        null(),
                        if writable { 4 } else { 2 },
                        0,
                        0,
                        null(),
                    )
                };
                assert!(!handle.is_null(), "{}", std::io::Error::last_os_error());
                let mut owned = Self {
                    view: null_mut(),
                    handle: Some(handle),
                };
                owned.view =
                    unsafe { MapViewOfFile(handle, if writable { 2 } else { 4 }, 0, 0, 4) };
                assert!(!owned.view.is_null(), "{}", std::io::Error::last_os_error());
                owned
            }
            fn close_mapping_handle(&mut self) {
                let handle = self.handle.take().unwrap();
                assert_ne!(unsafe { CloseHandle(handle) }, 0);
            }
            fn first_four(&self) -> Vec<u8> {
                unsafe { std::slice::from_raw_parts(self.view.cast::<u8>(), 4) }.to_vec()
            }
        }
        impl Drop for Mapping {
            fn drop(&mut self) {
                if !self.view.is_null() {
                    assert_ne!(unsafe { UnmapViewOfFile(self.view) }, 0);
                }
                if let Some(handle) = self.handle.take() {
                    assert_ne!(unsafe { CloseHandle(handle) }, 0);
                }
            }
        }
        #[derive(Debug, PartialEq, Eq)]
        enum CandidateObservation {
            ReadViewExcluded,
            ReadViewSurvivesRejectUniversalAtomicity,
            AliasExcluded,
            AliasAdmittedRejectCandidate,
            NameAdmittedRejectCandidate,
            MetadataMutableRejectUnvalidatedCandidate,
            WritableViewExcluded,
            WritableViewAdmittedRejectCandidate,
        }

        #[test]
        fn exact_relative_lease_surviving_mapping_and_data_conflicts() {
            let fixture = Fixture::new();
            for access in [GENERIC_READ, GENERIC_WRITE] {
                let held = fixture.open(access, ALL_SHARES).unwrap();
                expect_sharing_conflict(fixture.lease());
                drop(held);
                let lease = fixture.lease().unwrap();
                fixture.validate(&lease, OLD).unwrap();
                drop(lease);
            }
            let file = fixture
                .open(GENERIC_READ | GENERIC_WRITE, ALL_SHARES)
                .unwrap();
            let mut mapping = Mapping::new(&file, true);
            drop(file);
            assert_eq!(mapping.first_four(), OLD[..4]);
            observe_writable_view(&fixture);
            mapping.close_mapping_handle();
            observe_writable_view(&fixture);
            drop(mapping);
            let mut lease = fixture.lease().unwrap();
            fixture.validate(&lease, OLD).unwrap();
            // Stable owned positive only: same descriptor grows, shrinks, syncs and validates.
            lease.seek(SeekFrom::Start(0)).unwrap();
            let new = b"{\"future\":\"keep\",\"servers\":{\"inert\":1}}";
            lease.write_all(new).unwrap();
            lease.set_len(1024).unwrap();
            lease.set_len(new.len() as u64).unwrap();
            lease.sync_all().unwrap();
            fixture.validate(&lease, new).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&held_bytes(&lease).unwrap()).unwrap()
                    ["future"],
                "keep"
            );
            crate::fs::ensure_same_file(
                Path::new(LEAF),
                &fixture.expected,
                &crate::fs::checked_file_metadata(&lease).unwrap(),
            )
            .unwrap();
            lease.seek(SeekFrom::Start(0)).unwrap();
            lease.write_all(OLD).unwrap();
            lease.set_len(OLD.len() as u64).unwrap();
            lease.sync_all().unwrap();
            fixture.validate(&lease, OLD).unwrap();
            drop(lease);
            let file = fixture.open(GENERIC_READ, ALL_SHARES).unwrap();
            let mapping = Mapping::new(&file, false);
            drop(file);
            let observed = match fixture.lease() {
                Ok(lease) => {
                    fixture.validate(&lease, OLD).unwrap();
                    drop(lease);
                    CandidateObservation::ReadViewSurvivesRejectUniversalAtomicity
                }
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
                    CandidateObservation::ReadViewExcluded
                }
            };
            assert_eq!(mapping.first_four(), OLD[..4]);
            drop(mapping);
            eprintln!("MCP57_1_READ_VIEW={observed:?}");
            let lease = fixture.lease().unwrap();
            fixture.validate(&lease, OLD).unwrap();
            drop(lease);
            std::fs::write(fixture.root.path().join(LEAF), b"inert concurrent content").unwrap();
            let lease = fixture.lease().unwrap();
            assert!(fixture.validate(&lease, OLD).is_err());
            assert_eq!(held_bytes(&lease).unwrap(), b"inert concurrent content");
        }

        #[test]
        fn namespace_alias_and_private_metadata_are_observed_before_any_candidate_write() {
            let fixture = Fixture::new();
            let lease = fixture.lease().unwrap();
            fixture.validate(&lease, OLD).unwrap();
            expect_sharing_conflict(fixture.open(DELETE, ALL_SHARES));
            let mut replacement =
                create_private_file(&fixture.root.path().join("replacement")).unwrap();
            prepare_new_private_file(&replacement).unwrap();
            validate_checked(&replacement).unwrap();
            replacement.write_all(b"inert replacement").unwrap();
            replacement.sync_all().unwrap();
            drop(replacement);
            let replacement = unsafe {
                std::fs::File::from_raw_handle(
                    open_handle(
                        &fixture.root.path().join("replacement"),
                        DELETE | READ_CONTROL,
                        OPEN_EXISTING,
                        FILE_FLAG_OPEN_REPARSE_POINT,
                        None,
                    )
                    .unwrap(),
                )
            };
            let attempt = crate::fs::windows_rename_open_file_relative(
                &fixture.directory,
                &replacement,
                std::ffi::OsStr::new(LEAF),
                true,
            );
            match attempt {
                Ok(()) => {
                    // The retained old object remains intact, but its name now
                    // identifies the replacement. Classify/reject this protocol;
                    // never write through either object to make the test green.
                    assert_eq!(held_bytes(&lease).unwrap(), OLD);
                    let named = relative_open(
                        &fixture.directory,
                        LEAF,
                        GENERIC_READ | READ_CONTROL,
                        ALL_SHARES,
                    )
                    .unwrap();
                    assert!(
                        crate::fs::ensure_same_file(
                            Path::new(LEAF),
                            &fixture.expected,
                            &crate::fs::checked_file_metadata(&named).unwrap()
                        )
                        .is_err()
                    );
                    eprintln!(
                        "MCP57_1_NAME={:?}",
                        CandidateObservation::NameAdmittedRejectCandidate
                    );
                    drop(named);
                }
                Err(error) => {
                    assert!(
                        matches!(error.raw_os_error(), Some(5 | 32 | 33)),
                        "unexpected replacement result: {error}"
                    );
                    fixture.validate(&lease, OLD).unwrap();
                    eprintln!("MCP57_1_NAME=Excluded");
                }
            }
            // The identical source handle/directory must support a native
            // no-replace rename to a free inert name, ruling out a vacuous
            // target rejection caused by invalid helper access or ABI.
            crate::fs::windows_rename_open_file_relative(
                &fixture.directory,
                &replacement,
                std::ffi::OsStr::new("renamed-inert"),
                false,
            )
            .unwrap();
            assert_eq!(
                std::fs::read(fixture.root.path().join("renamed-inert")).unwrap(),
                b"inert replacement"
            );
            drop(replacement);
            drop(lease);
            drop(fixture);
            let fixture = Fixture::new();
            let lease = fixture.lease().unwrap();
            fixture.validate(&lease, OLD).unwrap();
            let alias = fixture.root.path().join("alias");
            let observation = match std::fs::hard_link(fixture.root.path().join(LEAF), &alias) {
                Ok(()) => {
                    assert_eq!(
                        fixture.validate(&lease, OLD).unwrap_err().kind(),
                        std::io::ErrorKind::PermissionDenied
                    );
                    assert_eq!(held_bytes(&lease).unwrap(), OLD);
                    CandidateObservation::AliasAdmittedRejectCandidate
                }
                Err(error) => {
                    assert!(matches!(error.raw_os_error(), Some(5 | 32 | 33)));
                    CandidateObservation::AliasExcluded
                }
            };
            eprintln!("MCP57_1_ALIAS={observation:?}");
            drop(lease);
            if alias.exists() {
                std::fs::remove_file(alias).unwrap();
            }
            let lease = fixture.lease().unwrap();
            fixture.validate(&lease, OLD).unwrap();
            let metadata = fixture
                .open(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES, ALL_SHARES)
                .unwrap();
            let user = current_user_sid_string().unwrap();
            let text = wide(OsStr::new(&format!(
                "D:P(A;;FA;;;SY)(A;;FA;;;{user})(A;;FR;;;WD)"
            )));
            let mut raw = null_mut();
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        text.as_ptr(),
                        SDDL_REVISION_1,
                        &mut raw,
                        null_mut(),
                    )
                },
                0
            );
            let descriptor = SecurityDescriptor(raw);
            assert_eq!(
                unsafe {
                    SetSecurityInfo(
                        metadata.as_raw_handle().cast(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        null_mut(),
                        null_mut(),
                        descriptor.dacl().unwrap(),
                        null_mut(),
                    )
                },
                0
            );
            assert_eq!(
                validate_checked(&lease).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(held_bytes(&lease).unwrap(), OLD);
            let observed = CandidateObservation::MetadataMutableRejectUnvalidatedCandidate;
            eprintln!("MCP57_1_PRIVATE_METADATA={observed:?}");
            // Restore only this deliberately changed owned inert image for cleanup.
            apply_private_dacl(metadata.as_raw_handle().cast(), false).unwrap();
            drop(metadata);
            fixture.validate(&lease, OLD).unwrap();
            drop(lease);
            let other = Fixture::new();
            let other_lease = other.lease().unwrap();
            assert!(fixture.validate(&other_lease, OLD).is_err());
            assert_eq!(std::fs::read(fixture.root.path().join(LEAF)).unwrap(), OLD);
        }

        #[test]
        fn reader_lease_precedes_marker_and_late_marker_refuses_admission() {
            let fixture = Fixture::new();
            let marker = fixture.root.path().join("inert-unresolved-marker");
            let reader = fixture
                .open(GENERIC_READ | READ_CONTROL, FILE_SHARE_READ)
                .unwrap();
            assert!(!marker.exists());
            expect_sharing_conflict(fixture.lease());
            drop(reader);
            // This old observation would be unsafe if reused after a later open.
            let marker_checked_before_open = marker.exists();
            let writer = fixture.lease().unwrap();
            let mut pending = create_private_file(&marker).unwrap();
            prepare_new_private_file(&pending).unwrap();
            validate_checked(&pending).unwrap();
            pending.write_all(b"INERT_UNRESOLVED").unwrap();
            pending.sync_all().unwrap();
            drop(pending);
            expect_sharing_conflict(fixture.open(GENERIC_READ | READ_CONTROL, FILE_SHARE_READ));
            drop(writer);
            let reader = fixture
                .open(GENERIC_READ | READ_CONTROL, FILE_SHARE_READ)
                .unwrap();
            fixture.validate(&reader, OLD).unwrap();
            assert!(
                !marker_checked_before_open && marker.exists(),
                "late marker must falsify check-before-open admission"
            );
            let retained_marker = relative_open(
                &fixture.directory,
                "inert-unresolved-marker",
                GENERIC_READ | READ_CONTROL,
                FILE_SHARE_READ,
            )
            .unwrap();
            validate_checked(&retained_marker).unwrap();
            assert_eq!(held_bytes(&retained_marker).unwrap(), b"INERT_UNRESOLVED");
            // The late-reader gate returns a terminal conflict; it never parses config/defaults.
            let admission: Result<(), &str> =
                if held_bytes(&retained_marker).unwrap() == b"INERT_UNRESOLVED" {
                    Err("unresolved operator transaction")
                } else {
                    panic!("unexpected owned marker")
                };
            assert_eq!(admission, Err("unresolved operator transaction"));
            assert_eq!(held_bytes(&reader).unwrap(), OLD);
            drop(retained_marker);
            drop(reader);
            std::fs::remove_file(marker).unwrap();
            let lease = fixture.lease().unwrap();
            fixture.validate(&lease, OLD).unwrap();
        }
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_native_windows_new_temp_is_user_owned_private_and_nonempty_or_linked_files_are_unchanged()
     {
        use std::os::windows::io::AsRawHandle;
        let root = crate::tests::OwnedTestDirectory::new("mcp-new-private-owner");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        workspace
            .create_relative_private_atomic(Path::new("published.json"), b"{}")
            .unwrap();
        {
            // Observe only this inert fixture's fixed target before the
            // unchanged nonrepairing assertion. No security is repaired and
            // no diagnostic observation is used to admit the file.
            let published_path = root.path().join("published.json");
            let diagnostic = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&published_path)
                .unwrap();
            let observed = information(diagnostic.as_raw_handle() as Handle).unwrap();
            let owner = ensure_current_owner(diagnostic.as_raw_handle() as Handle).is_ok();
            let dacl = dacl_sddl(&published_path, false).unwrap();
            eprintln!(
                "MCP_PRIVATE_TEMP_DIAGNOSTIC attributes={:#x} regular={} reparse={} links={} current_owner={} closed_dacl={} dacl={}",
                observed.file_attributes,
                diagnostic.metadata().unwrap().is_file(),
                observed.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                observed._number_of_links,
                owner,
                validate_checked(&diagnostic).is_ok(),
                dacl.chars().take(2048).collect::<String>(),
            );
        }
        let published = open_checked(
            &root.path().join("published.json"),
            super::PrivateFileAccess::Read,
        )
        .unwrap();
        ensure_current_owner(published.as_raw_handle() as Handle).unwrap();
        // Exercise the same binary descriptor gate with actual native SIDs.
        // SDDL rendering may alias a numeric current-user SID; admission still
        // compares the actual trustee identity rather than accepting an alias.
        let user = current_user_sid_string().unwrap();
        for (sddl, expected) in [
            (format!("D:P(A;;FA;;;SY)(A;;FA;;;{user})"), true),
            (format!("D:P(A;;FA;;;{user})(A;;FA;;;SY)"), true),
            (format!("D:(A;;FA;;;SY)(A;;FA;;;{user})"), false),
            (format!("D:P(A;;FA;;;WD)(A;;FA;;;SY)"), false),
            (format!("D:P(A;ID;FA;;;SY)(A;;FA;;;{user})"), false),
            (format!("D:P(A;;FR;;;SY)(A;;FA;;;{user})"), false),
            (
                format!("D:P(A;;FA;;;SY)(A;;FA;;;{user})(A;;FR;;;BU)"),
                false,
            ),
        ] {
            let mut raw = null_mut();
            let text = wide(OsStr::new(&sddl));
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        text.as_ptr(),
                        SDDL_REVISION_1,
                        &mut raw,
                        null_mut(),
                    )
                },
                0
            );
            let descriptor = SecurityDescriptor(raw);
            assert_eq!(validate_private_dacl(descriptor.0).is_ok(), expected);
        }
        assert_eq!(
            std::fs::read(root.path().join("published.json")).unwrap(),
            b"{}"
        );
        drop(published);
        let fresh = root.path().join("fresh");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&fresh)
            .unwrap();
        prepare_new_private_file(&file).unwrap();
        ensure_current_owner(file.as_raw_handle() as Handle).unwrap();
        super::validate_checked(&file).unwrap();
        file.write_all(b"private bytes").unwrap();
        let before = dacl_sddl(&fresh, false).unwrap();
        assert!(prepare_new_private_file(&file).is_err());
        assert_eq!(dacl_sddl(&fresh, false).unwrap(), before);
        assert_eq!(std::fs::read(&fresh).unwrap(), b"private bytes");
        drop(file);
        let linked = root.path().join("linked");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&linked)
            .unwrap();
        let before = dacl_sddl(&linked, false).unwrap();
        std::fs::hard_link(&linked, root.path().join("alias")).unwrap();
        assert!(prepare_new_private_file(&file).is_err());
        assert_eq!(dacl_sddl(&linked, false).unwrap(), before);
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn usd_budget_native_windows_checked_open_refuses_public_dacl_and_hardlinks_without_repair() {
        use std::os::windows::io::FromRawHandle;
        let root = crate::tests::OwnedTestDirectory::new("cost-private-dacl");
        let path = root.path().join("public");
        atomic_create(&path, b"unchanged").unwrap();
        let user = current_user_sid_string().unwrap();
        let text = wide(OsStr::new(&format!(
            "D:P(A;;FA;;;SY)(A;;FA;;;{user})(A;;FR;;;WD)"
        )));
        let mut descriptor = null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            0
        );
        let descriptor = SecurityDescriptor(descriptor);
        let handle = open_handle(
            &path,
            READ_CONTROL | WRITE_DAC,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .unwrap();
        // File owns the native handle even if a later assertion unwinds.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        assert_eq!(
            unsafe {
                SetSecurityInfo(
                    handle,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    descriptor.dacl().unwrap(),
                    null_mut(),
                )
            },
            0
        );
        drop(file);
        let before = dacl_sddl(&path, false).unwrap();
        for access in [
            super::PrivateFileAccess::Read,
            super::PrivateFileAccess::Append,
        ] {
            assert!(open_checked(&path, access).is_err());
            assert_eq!(dacl_sddl(&path, false).unwrap(), before);
            assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
        }
        let private = root.path().join("linked");
        atomic_create(&private, b"linked unchanged").unwrap();
        let before = dacl_sddl(&private, false).unwrap();
        std::fs::hard_link(&private, root.path().join("alias")).unwrap();
        for access in [
            super::PrivateFileAccess::Read,
            super::PrivateFileAccess::Append,
        ] {
            assert!(open_checked(&private, access).is_err());
            assert_eq!(dacl_sddl(&private, false).unwrap(), before);
            assert_eq!(std::fs::read(&private).unwrap(), b"linked unchanged");
        }
    }

    #[cfg(test)]
    pub(crate) fn dacl_sddl(path: &Path, directory: bool) -> std::io::Result<String> {
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
                            "private DACL is not valid UTF-16",
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

#[cfg(test)]
mod checked_tests {
    use super::*;
    #[test]
    fn spend_private_dacl_requires_closed_current_user_system_protected_access() {
        let user = PrivateAce {
            kind: 0,
            flags: 0,
            mask: 0x001f_01ff,
            current_user: true,
            system: false,
        };
        let system = PrivateAce {
            current_user: false,
            system: true,
            ..user
        };
        assert!(closed_private_acl(true, &[user, system]));
        assert!(closed_private_acl(true, &[system, user]));
        assert!(!closed_private_acl(false, &[user, system]));
        for bad in [
            PrivateAce { kind: 1, ..user },
            PrivateAce {
                flags: 0x10,
                ..user
            },
            PrivateAce { flags: 1, ..user },
            PrivateAce {
                mask: 0x0012_0089,
                ..user
            },
            PrivateAce {
                mask: 0x1000_0000,
                ..user
            },
            PrivateAce {
                current_user: false,
                system: false,
                ..user
            },
        ] {
            assert!(!closed_private_acl(true, &[system, bad]));
        }
        assert!(!closed_private_acl(true, &[]));
        assert!(!closed_private_acl(true, &[user]));
        assert!(!closed_private_acl(true, &[user, system, user]));
        assert!(!closed_private_acl(true, &[user, user]));
        assert!(!closed_private_acl(true, &[system, system]));
        // A System-owned process still has exactly its user and System rights.
        let same = PrivateAce {
            system: true,
            ..user
        };
        assert!(closed_private_acl(true, &[same, same]));
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn spend_checked_private_file_read_and_append_preserve_existing_security() {
        use std::io::{Read, Write};
        let root = crate::tests::OwnedTestDirectory::new("spend-private-file");
        let path = root.path().join("private");
        atomic_create(&path, b"one").unwrap();
        let mut reader = open_checked(&path, PrivateFileAccess::Read).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"one");
        let mut writer = open_checked(&path, PrivateFileAccess::Append).unwrap();
        writer.write_all(b"two").unwrap();
        validate_checked(&writer).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"onetwo");
        #[cfg(windows)]
        assert!(dacl_sddl(&path, false).unwrap().contains("D:P"));
    }
    #[cfg(unix)]
    #[test]
    fn spend_checked_private_file_refuses_public_mode_and_hardlinks_without_repair() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::tests::OwnedTestDirectory::new("spend-private-refusal");
        let path = root.path().join("public");
        std::fs::write(&path, b"original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        for access in [PrivateFileAccess::Read, PrivateFileAccess::Append] {
            assert!(open_checked(&path, access).is_err());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(open_checked(&path, PrivateFileAccess::Append).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }
}

#[cfg(all(test, unix))]
mod checked_private_race_tests {
    use super::*;
    #[test]
    fn usd_budget_private_checked_open_rejects_replacements_without_permission_repair() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for stage in [
            "checked_before_initial_open",
            "checked_before_revalidation_open",
        ] {
            for access in [PrivateFileAccess::Read, PrivateFileAccess::Append] {
                let root = crate::tests::OwnedTestDirectory::new("private-ledger-race");
                let path = root.path().join("usage.jsonl");
                let target = root.path().join("public-victim");
                atomic_create(&path, b"trusted\n").unwrap();
                std::fs::write(&target, b"victim\n").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
                let attack_path = path.clone();
                let attack_target = target.clone();
                let outcome = with_open_test_hook(
                    move |point| {
                        if point == stage {
                            std::fs::remove_file(&attack_path).unwrap();
                            symlink(&attack_target, &attack_path).unwrap();
                        }
                    },
                    || open_checked(&path, access),
                );
                assert!(outcome.is_err());
                assert_eq!(std::fs::read(&target).unwrap(), b"victim\n");
                use std::os::unix::fs::MetadataExt;
                assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o644);
            }
        }
    }
}

/// Restrict an exclusively created, still-empty owned temporary file before
/// writing secret config bytes. This never repairs an existing destination.
#[cfg(all(feature = "mcp", unix))]
pub(crate) fn prepare_new_private_file(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(private_file_error());
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(all(feature = "mcp", not(any(unix, windows))))]
pub(crate) fn prepare_new_private_file(_: &std::fs::File) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(all(feature = "mcp", windows))]
pub(crate) use windows::prepare_new_private_file;
