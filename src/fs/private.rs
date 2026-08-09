#[cfg(not(windows))]
use std::path::Path;

#[cfg(not(any(unix, windows)))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private persistence is unsupported on this platform",
    )
}

#[cfg(unix)]
const OPEN_NOFOLLOW: std::os::raw::c_int = if cfg!(target_os = "macos") {
    0x100
} else {
    0x2_0000
};

#[cfg(target_os = "linux")]
const OPEN_DIRECTORY: std::os::raw::c_int = 0x1_0000;
#[cfg(target_os = "macos")]
const OPEN_DIRECTORY: std::os::raw::c_int = 0x10_0000;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const OPEN_DIRECTORY: std::os::raw::c_int = 0;

#[cfg(target_os = "linux")]
const OPEN_CLOEXEC: std::os::raw::c_int = 0x8_0000;
#[cfg(target_os = "macos")]
const OPEN_CLOEXEC: std::os::raw::c_int = 0x100_0000;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
const OPEN_CLOEXEC: std::os::raw::c_int = 0;

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
pub(crate) fn ensure_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    if path.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private directory must not be a filesystem root",
        ));
    }

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
    directory
        .set_permissions(std::fs::Permissions::from_mode(0o700))
        .map_err(|error| stage_error("directory_permissions", error))?;
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
        use std::os::unix::fs::MetadataExt;

        // Both descriptors were opened independently through the same exact
        // path, so they share the same APFS/firmlink metadata view. Comparing
        // their descriptor metadata detects replacement without querying the
        // protected volume root for ATTR_VOL_UUID, which hosted macOS runners
        // can deny even though the selected file itself is accessible.
        let _ = (left_file, right_file);
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::fs::MetadataExt;

        let _ = (left_file, right_file);
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
}

#[cfg(unix)]
pub(crate) fn open_existing(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    // Use lstat only to reject an initial symlink before opening. On APFS,
    // especially across the /Users firmlink, path and descriptor metadata can
    // legitimately report different device IDs for the same inode. The final
    // identity proof therefore compares two independently opened descriptors,
    // which use the same metadata view while `file` keeps the selected inode
    // alive throughout the check.
    let before = std::fs::symlink_metadata(path)?;
    if before.file_type().is_symlink() || !before.is_file() || before.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private file is not an owned regular file",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC)
        .open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private file is not an owned regular file",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let current = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_NOFOLLOW | OPEN_CLOEXEC)
        .open(path)?;
    let after = current.metadata()?;
    if !same_open_file_identity(&file, &opened, &current, &after)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("Path changed after permission check: {}", path.display()),
        ));
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
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private file must have a parent directory",
        )
    })?;
    ensure_directory(parent).map_err(|error| stage_error("create_parent", error))?;
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
fn current_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: `getuid` takes no arguments and has no failure mode.
    unsafe { getuid() }
}

#[cfg(windows)]
pub(crate) use windows::{atomic_create, atomic_write, ensure_directory, open_existing};

#[cfg(all(test, windows))]
pub(crate) use windows::dacl_sddl;

#[cfg(not(any(unix, windows)))]
pub(crate) fn ensure_directory(_path: &Path) -> std::io::Result<()> {
    Err(unsupported())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_existing(_path: &Path) -> std::io::Result<std::fs::File> {
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
    const MOVEFILE_WRITE_THROUGH: Dword = 0x0000_0008;
    const REPLACEFILE_WRITE_THROUGH: Dword = 0x0000_0001;
    const SDDL_REVISION_1: Dword = 1;
    const SE_FILE_OBJECT: Dword = 1;
    const OWNER_SECURITY_INFORMATION: Dword = 0x0000_0001;
    const DACL_SECURITY_INFORMATION: Dword = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: Dword = 0x8000_0000;
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
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: Dword) -> Bool;
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: Dword,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> Bool;
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
                    ensure_directory(parent)?;
                }
                Err(error) => return Err(error),
            }
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
        let opened = super::super::checked_file_metadata(&file)?;
        let after = super::super::checked_path_metadata(path)?;
        super::super::ensure_same_file(path, &before, &opened)?;
        super::super::ensure_same_file(path, &opened, &after)?;
        Ok(file)
    }

    pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        atomic_write_mode(path, bytes, false)
    }

    pub(crate) fn atomic_create(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
            let replaced = if !create_only && std::fs::symlink_metadata(path).is_ok() {
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
