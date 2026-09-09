use std::collections::{BinaryHeap, HashMap};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use globset::{GlobBuilder, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::Regex;
use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, FindFilesArgs, PermCheck, ToolError, check_perm, check_perm_bound_path,
    check_perm_path, combine_coaching, is_skip_dir, is_vcs_metadata,
};

fn path_changed_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("Path changed after permission check: {}", path.display()),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
mod bound_platform {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    const OPEN_NOFOLLOW: c_int = libc::O_NOFOLLOW;
    const OPEN_CLOEXEC: c_int = libc::O_CLOEXEC;
    const OPEN_NONBLOCK: c_int = libc::O_NONBLOCK;

    #[repr(C)]
    struct DirectoryStream {
        _private: [u8; 0],
    }

    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct DirectoryEntry {
        inode: u64,
        offset: i64,
        record_length: u16,
        file_type: u8,
        name: [c_char; 256],
    }

    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct DirectoryEntry {
        inode: u64,
        seek_offset: u64,
        record_length: u16,
        name_length: u16,
        file_type: u8,
        name: [c_char; 1024],
    }

    unsafe extern "C" {
        fn openat(directory: c_int, path: *const c_char, flags: c_int, mode: c_uint) -> c_int;
        #[cfg_attr(
            all(target_os = "macos", target_arch = "x86_64"),
            link_name = "fdopendir$INODE64"
        )]
        #[cfg_attr(
            all(target_os = "macos", target_arch = "x86"),
            link_name = "fdopendir$INODE64$UNIX2003"
        )]
        fn fdopendir(descriptor: c_int) -> *mut DirectoryStream;
        #[cfg_attr(
            all(target_os = "macos", not(target_arch = "aarch64")),
            link_name = "readdir$INODE64"
        )]
        fn readdir(directory: *mut DirectoryStream) -> *mut DirectoryEntry;
        #[cfg_attr(
            all(target_os = "macos", target_arch = "x86"),
            link_name = "closedir$UNIX2003"
        )]
        fn closedir(directory: *mut DirectoryStream) -> c_int;
    }

    struct DirectoryStreamGuard(*mut DirectoryStream);

    // SAFETY: the stream is owned by one walker and is never accessed
    // concurrently; moving that owner between executor threads is safe.
    unsafe impl Send for DirectoryStreamGuard {}

    impl Drop for DirectoryStreamGuard {
        fn drop(&mut self) {
            // SAFETY: fdopendir returned this stream and it is closed exactly once.
            let _ = unsafe { closedir(self.0) };
        }
    }

    pub(super) fn open_root(path: &Path) -> std::io::Result<File> {
        File::open(path)
    }

    pub(super) fn open_child(directory: &File, name: &OsStr) -> std::io::Result<File> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path component contains NUL",
            )
        })?;
        // SAFETY: `name` is NUL-terminated and `directory` owns a valid descriptor.
        let descriptor = unsafe {
            openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                OPEN_NOFOLLOW | OPEN_CLOEXEC | OPEN_NONBLOCK,
                0,
            )
        };
        if descriptor < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: openat returned a new owned descriptor.
            Ok(unsafe { File::from_raw_fd(descriptor) })
        }
    }

    pub(super) fn is_link(directory: &File, name: &OsStr) -> bool {
        let Ok(name) = CString::new(name.as_bytes()) else {
            return false;
        };
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is NUL-terminated, the directory descriptor remains
        // live, and fstatat initializes `metadata` on success without following
        // the entry. This classifies a link but never opens its target.
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return false;
        }
        // SAFETY: fstatat succeeded and initialized the value.
        let metadata = unsafe { metadata.assume_init() };
        metadata.st_mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub(super) fn read_link(
        directory: &File,
        name: &OsStr,
        _approved_root: &Path,
    ) -> std::io::Result<OsString> {
        let name = CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path component contains NUL",
            )
        })?;
        let mut capacity = 256usize;
        loop {
            let mut buffer = vec![0u8; capacity];
            // SAFETY: the directory descriptor and NUL-terminated name are
            // live, and `buffer` is writable for its declared capacity.
            let length = unsafe {
                libc::readlinkat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if length < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let length = length as usize;
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(OsString::from_vec(buffer));
            }
            capacity = capacity
                .checked_mul(2)
                .filter(|size| *size <= 64 * 1024)
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "link target is too long")
                })?;
        }
    }

    pub(super) struct DirectoryReader {
        stream: DirectoryStreamGuard,
    }

    impl Iterator for DirectoryReader {
        type Item = OsString;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                // SAFETY: the stream remains valid for the lifetime of the guard.
                let entry = unsafe { readdir(self.stream.0) };
                if entry.is_null() {
                    return None;
                }
                // SAFETY: readdir returns a NUL-terminated name within a live entry.
                let name = unsafe { CStr::from_ptr((*entry).name.as_ptr()) }.to_bytes();
                if name != b"." && name != b".." {
                    return Some(OsString::from_vec(name.to_vec()));
                }
            }
        }
    }

    pub(super) fn read_directory(directory: &File) -> std::io::Result<DirectoryReader> {
        let descriptor = directory.try_clone()?.into_raw_fd();
        // SAFETY: ownership of `descriptor` is transferred to fdopendir on success.
        let stream = unsafe { fdopendir(descriptor) };
        if stream.is_null() {
            // SAFETY: fdopendir failed, so ownership of the descriptor remains here.
            drop(unsafe { File::from_raw_fd(descriptor) });
            return Err(std::io::Error::last_os_error());
        }
        Ok(DirectoryReader {
            stream: DirectoryStreamGuard(stream),
        })
    }

    pub(super) fn is_safe_entry(_metadata: &std::fs::Metadata) -> bool {
        true
    }

    pub(super) fn is_link_metadata(metadata: &std::fs::Metadata) -> bool {
        metadata.file_type().is_symlink()
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod bound_platform {
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::{File, OpenOptions};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr;

    type Handle = *mut c_void;
    type NtStatus = i32;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_GENERIC_READ: u32 = 0x0012_0089;
    const FILE_SHARE_ALL: u32 = 0x7;
    const FILE_OPEN: u32 = 0x1;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    const OBJECT_CASE_INSENSITIVE: u32 = 0x40;
    const STATUS_NO_MORE_FILES: NtStatus = 0x8000_0006_u32 as NtStatus;

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: Handle,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status: isize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut Handle,
            desired_access: u32,
            object_attributes: *mut ObjectAttributes,
            io_status_block: *mut IoStatusBlock,
            allocation_size: *mut i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *mut c_void,
            ea_length: u32,
        ) -> NtStatus;
        fn NtQueryDirectoryFile(
            file_handle: Handle,
            event: Handle,
            apc_routine: *mut c_void,
            apc_context: *mut c_void,
            io_status_block: *mut IoStatusBlock,
            file_information: *mut c_void,
            length: u32,
            file_information_class: u32,
            return_single_entry: u8,
            file_name: *mut UnicodeString,
            restart_scan: u8,
        ) -> NtStatus;
    }

    fn nt_error(status: NtStatus) -> std::io::Error {
        std::io::Error::other(format!("Windows native filesystem error: {status:#x}"))
    }

    pub(super) fn open_root(path: &Path) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    pub(super) fn open_child(directory: &File, name: &OsStr) -> std::io::Result<File> {
        let mut wide: Vec<u16> = name.encode_wide().collect();
        let byte_length = wide
            .len()
            .checked_mul(2)
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path component is too long",
                )
            })?;
        let mut name = UnicodeString {
            length: byte_length,
            maximum_length: byte_length,
            buffer: wide.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root_directory: directory.as_raw_handle().cast(),
            object_name: &mut name,
            attributes: OBJECT_CASE_INSENSITIVE,
            security_descriptor: ptr::null_mut(),
            security_quality_of_service: ptr::null_mut(),
        };
        let mut io = IoStatusBlock {
            status: 0,
            information: 0,
        };
        let mut handle = ptr::null_mut();
        // SAFETY: all native structures point to live storage for the duration
        // of the call, and a successful handle is transferred to `File`.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_GENERIC_READ,
                &mut attributes,
                &mut io,
                ptr::null_mut(),
                0,
                FILE_SHARE_ALL,
                FILE_OPEN,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                ptr::null_mut(),
                0,
            )
        };
        if status < 0 {
            Err(nt_error(status))
        } else {
            // SAFETY: NtCreateFile returned a new owned handle.
            Ok(unsafe { File::from_raw_handle(handle.cast()) })
        }
    }

    pub(super) struct DirectoryReader {
        directory: File,
        restart: u8,
        finished: bool,
    }

    impl Iterator for DirectoryReader {
        type Item = OsString;

        fn next(&mut self) -> Option<Self::Item> {
            if self.finished {
                return None;
            }
            loop {
                let mut storage = vec![0_u64; 512];
                let mut io = IoStatusBlock {
                    status: 0,
                    information: 0,
                };
                // SAFETY: the output buffer and status block remain valid for the call.
                let status = unsafe {
                    NtQueryDirectoryFile(
                        self.directory.as_raw_handle().cast(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut io,
                        storage.as_mut_ptr().cast(),
                        (storage.len() * std::mem::size_of::<u64>()) as u32,
                        1,
                        1,
                        ptr::null_mut(),
                        self.restart,
                    )
                };
                self.restart = 0;
                if status == STATUS_NO_MORE_FILES {
                    self.finished = true;
                    return None;
                }
                if status < 0 {
                    self.finished = true;
                    return None;
                }
                if io.information < 64 {
                    self.finished = true;
                    return None;
                }

                let bytes = storage.as_ptr().cast::<u8>();
                // FILE_DIRECTORY_INFORMATION stores FileNameLength at byte 60
                // and the UTF-16 filename at byte 64.
                let name_length =
                    unsafe { ptr::read_unaligned(bytes.add(60).cast::<u32>()) } as usize;
                if name_length % 2 != 0
                    || name_length > io.information - 64
                    || name_length > storage.len() * std::mem::size_of::<u64>() - 64
                {
                    self.finished = true;
                    return None;
                }
                let name = unsafe {
                    std::slice::from_raw_parts(bytes.add(64).cast::<u16>(), name_length / 2)
                };
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    return Some(OsString::from_wide(name));
                }
            }
        }
    }

    pub(super) fn read_directory(directory: &File) -> std::io::Result<DirectoryReader> {
        Ok(DirectoryReader {
            directory: directory.try_clone()?,
            restart: 1,
            finished: false,
        })
    }

    pub(super) fn is_safe_entry(metadata: &std::fs::Metadata) -> bool {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }

    pub(super) fn is_link(_directory: &File, _name: &OsStr) -> bool {
        false
    }

    pub(super) fn read_link(
        _directory: &File,
        name: &OsStr,
        approved_root: &Path,
    ) -> std::io::Result<OsString> {
        std::fs::read_link(approved_root.join(name)).map(|target| target.into_os_string())
    }

    pub(super) fn is_link_metadata(metadata: &std::fs::Metadata) -> bool {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod bound_platform {
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::path::Path;

    fn unsupported() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "descriptor-bound directory traversal is unsupported on this platform",
        )
    }

    pub(super) fn open_root(_path: &Path) -> std::io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn open_child(_directory: &File, _name: &OsStr) -> std::io::Result<File> {
        Err(unsupported())
    }

    pub(super) struct DirectoryReader;

    impl Iterator for DirectoryReader {
        type Item = OsString;

        fn next(&mut self) -> Option<Self::Item> {
            None
        }
    }

    pub(super) fn read_directory(_directory: &File) -> std::io::Result<DirectoryReader> {
        Err(unsupported())
    }

    pub(super) fn is_safe_entry(_metadata: &std::fs::Metadata) -> bool {
        false
    }

    pub(super) fn is_link(_directory: &File, _name: &OsStr) -> bool {
        false
    }

    pub(super) fn read_link(
        _directory: &File,
        _name: &OsStr,
        _approved_root: &Path,
    ) -> std::io::Result<OsString> {
        Err(unsupported())
    }

    pub(super) fn is_link_metadata(_metadata: &std::fs::Metadata) -> bool {
        false
    }
}

pub(crate) struct BoundFile {
    pub(crate) path: PathBuf,
    pub(crate) file_name: OsString,
    pub(crate) file: File,
    pub(crate) metadata: std::fs::Metadata,
}

pub(crate) struct BoundDirectory {
    approved_root: PathBuf,
    root: File,
}

#[derive(Clone, Default)]
struct IgnoreChain(Option<Arc<IgnoreChainNode>>);

struct IgnoreChainNode {
    parent: Option<Arc<IgnoreChainNode>>,
    matchers: Arc<[Gitignore]>,
}

impl IgnoreChain {
    fn append(self, matchers: impl Into<Arc<[Gitignore]>>) -> Self {
        let matchers = matchers.into();
        if matchers.is_empty() {
            return self;
        }
        Self(Some(Arc::new(IgnoreChainNode {
            parent: self.0,
            matchers,
        })))
    }
}

#[derive(Clone, Eq, PartialEq)]
struct IgnoreSourceStamp {
    source: PathBuf,
    length: Option<u64>,
    modified: Option<SystemTime>,
    is_file: bool,
}

struct ParentIgnoreCacheEntry {
    stamps: Vec<IgnoreSourceStamp>,
    matchers: Arc<[Gitignore]>,
}

const MAX_PARENT_IGNORE_CACHE_ENTRIES: usize = 64;
static PARENT_IGNORE_CACHE: LazyLock<Mutex<HashMap<PathBuf, ParentIgnoreCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl BoundDirectory {
    pub(crate) fn approved_root(&self) -> &Path {
        &self.approved_root
    }

    pub(crate) fn open(
        approved_root: &Path,
        approved_metadata: &crate::fs::CheckedMetadata,
    ) -> std::io::Result<Self> {
        if !approved_metadata.is_dir() || !bound_platform::is_safe_entry(approved_metadata) {
            return Err(path_changed_error(approved_root));
        }
        let root = bound_platform::open_root(approved_root)?;
        let opened_metadata = crate::fs::checked_file_metadata(&root)?;
        let current_metadata = std::fs::symlink_metadata(approved_root)?;
        if current_metadata.file_type().is_symlink()
            || !bound_platform::is_safe_entry(&opened_metadata)
        {
            return Err(path_changed_error(approved_root));
        }
        crate::fs::ensure_same_file(approved_root, approved_metadata, &opened_metadata)?;
        let current = bound_platform::open_root(approved_root)?;
        let current_identity = crate::fs::checked_file_metadata(&current)?;
        crate::fs::ensure_same_file(approved_root, &opened_metadata, &current_identity)?;
        Self::from_file(approved_root, root)
    }

    pub(crate) fn from_file(approved_root: &Path, root: File) -> std::io::Result<Self> {
        let metadata = root.metadata()?;
        if !metadata.is_dir() || !bound_platform::is_safe_entry(&metadata) {
            return Err(path_changed_error(approved_root));
        }
        Ok(Self {
            approved_root: approved_root.to_path_buf(),
            root,
        })
    }

    pub(crate) fn walker(&self) -> std::io::Result<BoundWalker> {
        BoundWalker::new(self.root.try_clone()?, self.approved_root.clone())
    }

    pub(super) fn list_entries(&self) -> std::io::Result<Vec<BoundListEntry>> {
        let mut chain = IgnoreChain::default();
        let (global, _) = GitignoreBuilder::new(&self.approved_root).build_global();
        if !global.is_empty() {
            chain = chain.append(vec![global]);
        }
        chain = chain.append(parent_ignore_matchers(&self.approved_root));
        let mut local_matchers = Vec::new();
        for ignore_name in [".gitignore", ".ignore"] {
            if let Some(matcher) =
                local_ignore_matcher(&self.root, Path::new(""), &self.approved_root, ignore_name)
            {
                local_matchers.push(matcher);
            }
        }
        chain = chain.append(local_matchers);
        if let Ok(exclude) = open_relative(&self.root, Path::new(".git/info/exclude"))
            && let Some(matcher) = ignore_matcher(
                exclude,
                &self.approved_root,
                self.approved_root.join(".git/info/exclude"),
            )
        {
            chain = chain.append(vec![matcher]);
        }

        let mut entries = Vec::new();
        for name in bound_platform::read_directory(&self.root)? {
            if is_vcs_metadata(name.to_str().unwrap_or("")) {
                continue;
            }
            let child = match bound_platform::open_child(&self.root, &name) {
                Ok(child) => child,
                Err(_) if bound_platform::is_link(&self.root, &name) => {
                    let path = self.approved_root.join(&name);
                    if !is_ignored(&chain, &path, false) {
                        entries.push(BoundListEntry {
                            link_target: bound_platform::read_link(
                                &self.root,
                                &name,
                                &self.approved_root,
                            )
                            .ok(),
                            file_name: name,
                            is_directory: false,
                            is_link: true,
                            size: 0,
                            child_count: 0,
                        });
                    }
                    continue;
                }
                Err(_) => continue,
            };
            let metadata = match child.metadata() {
                Ok(metadata) if bound_platform::is_link_metadata(&metadata) => {
                    let path = self.approved_root.join(&name);
                    if !is_ignored(&chain, &path, false) {
                        entries.push(BoundListEntry {
                            link_target: bound_platform::read_link(
                                &self.root,
                                &name,
                                &self.approved_root,
                            )
                            .ok(),
                            file_name: name,
                            is_directory: false,
                            is_link: true,
                            size: 0,
                            child_count: 0,
                        });
                    }
                    continue;
                }
                Ok(metadata) if bound_platform::is_safe_entry(&metadata) => metadata,
                _ => continue,
            };
            let path = self.approved_root.join(&name);
            let is_directory = metadata.is_dir();
            if is_directory && is_skip_dir(name.to_str().unwrap_or("")) {
                continue;
            }
            if is_ignored(&chain, &path, is_directory) {
                continue;
            }
            let child_count = if is_directory {
                bound_platform::read_directory(&child)
                    .map(|reader| reader.count() as u64)
                    .unwrap_or(0)
            } else {
                0
            };
            entries.push(BoundListEntry {
                link_target: None,
                file_name: name,
                is_directory,
                is_link: false,
                size: metadata.len(),
                child_count,
            });
        }
        Ok(entries)
    }

    #[cfg(feature = "js")]
    pub(crate) fn list_entries_bounded(
        &self,
        max_entries: usize,
    ) -> std::io::Result<(Vec<BoundDiscoveryEntry>, bool)> {
        let mut chain = IgnoreChain::default();
        let (global, _) = GitignoreBuilder::new(&self.approved_root).build_global();
        if !global.is_empty() {
            chain = chain.append(vec![global]);
        }
        chain = chain.append(parent_ignore_matchers(&self.approved_root));
        let mut local_matchers = Vec::new();
        for ignore_name in [".gitignore", ".ignore"] {
            if let Some(matcher) =
                local_ignore_matcher(&self.root, Path::new(""), &self.approved_root, ignore_name)
            {
                local_matchers.push(matcher);
            }
        }
        chain = chain.append(local_matchers);

        let mut entries = Vec::with_capacity(max_entries.min(64));
        for name in bound_platform::read_directory(&self.root)? {
            if is_vcs_metadata(name.to_str().unwrap_or("")) {
                continue;
            }
            let child = match bound_platform::open_child(&self.root, &name) {
                Ok(child) => child,
                Err(_) => continue,
            };
            let metadata = match child.metadata() {
                Ok(metadata) if bound_platform::is_safe_entry(&metadata) => metadata,
                _ => continue,
            };
            let path = self.approved_root.join(&name);
            let is_directory = metadata.is_dir();
            if (!is_directory && !metadata.is_file())
                || (is_directory && is_skip_dir(name.to_str().unwrap_or("")))
                || is_ignored(&chain, &path, is_directory)
            {
                continue;
            }
            if entries.len() == max_entries {
                return Ok((entries, true));
            }
            entries.push(BoundDiscoveryEntry {
                file_name: name,
                is_directory,
                size: if is_directory { 0 } else { metadata.len() },
            });
        }
        Ok((entries, false))
    }
}

#[cfg(feature = "js")]
pub(crate) struct BoundDiscoveryEntry {
    pub(crate) file_name: OsString,
    pub(crate) is_directory: bool,
    pub(crate) size: u64,
}

pub(super) struct BoundListEntry {
    pub(super) file_name: OsString,
    pub(super) link_target: Option<OsString>,
    pub(super) is_directory: bool,
    pub(super) is_link: bool,
    pub(super) size: u64,
    pub(super) child_count: u64,
}

struct DirectoryFrame {
    directory: File,
    relative_path: PathBuf,
    names: bound_platform::DirectoryReader,
    matchers: IgnoreChain,
}

impl DirectoryFrame {
    fn new(
        directory: File,
        relative_path: PathBuf,
        mut matchers: IgnoreChain,
        approved_root: &Path,
    ) -> std::io::Result<Self> {
        let mut local_matchers = Vec::new();
        for ignore_name in [".gitignore", ".ignore"] {
            if let Some(matcher) =
                local_ignore_matcher(&directory, &relative_path, approved_root, ignore_name)
            {
                local_matchers.push(matcher);
            }
        }
        matchers = matchers.append(local_matchers);
        let names = bound_platform::read_directory(&directory)?;
        Ok(Self {
            directory,
            relative_path,
            names,
            matchers,
        })
    }
}

pub(crate) struct BoundWalker {
    approved_root: PathBuf,
    stack: Vec<DirectoryFrame>,
}

impl BoundWalker {
    fn new(root: File, approved_root: PathBuf) -> std::io::Result<Self> {
        let mut matchers = IgnoreChain::default();
        let (global, _) = GitignoreBuilder::new(&approved_root).build_global();
        if !global.is_empty() {
            matchers = matchers.append(vec![global]);
        }
        matchers = matchers.append(parent_ignore_matchers(&approved_root));
        if let Ok(exclude) = open_relative(&root, Path::new(".git/info/exclude"))
            && let Some(matcher) = ignore_matcher(
                exclude,
                &approved_root,
                approved_root.join(".git/info/exclude"),
            )
        {
            matchers = matchers.append(vec![matcher]);
        }
        let frame = DirectoryFrame::new(root, PathBuf::new(), matchers, &approved_root)?;
        Ok(Self {
            approved_root,
            stack: vec![frame],
        })
    }
}

impl Iterator for BoundWalker {
    type Item = BoundFile;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            let Some(name) = frame.names.next() else {
                self.stack.pop();
                continue;
            };
            if is_vcs_metadata(name.to_str().unwrap_or("")) {
                continue;
            }
            let relative_path = frame.relative_path.join(&name);
            let child = match bound_platform::open_child(&frame.directory, &name) {
                Ok(child) => child,
                Err(_) => continue,
            };
            let metadata = match child.metadata() {
                Ok(metadata) if bound_platform::is_safe_entry(&metadata) => metadata,
                _ => continue,
            };
            let approved_path = self.approved_root.join(&relative_path);
            let is_directory = metadata.is_dir();
            if is_directory && is_skip_dir(name.to_str().unwrap_or("")) {
                continue;
            }
            if is_ignored(&frame.matchers, &approved_path, is_directory) {
                continue;
            }
            if is_directory {
                let matchers = frame.matchers.clone();
                if let Ok(child_frame) =
                    DirectoryFrame::new(child, relative_path, matchers, &self.approved_root)
                {
                    self.stack.push(child_frame);
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            return Some(BoundFile {
                path: approved_path,
                file_name: name,
                file: child,
                metadata,
            });
        }
    }
}

fn open_relative(root: &File, path: &Path) -> std::io::Result<File> {
    let mut current = root.try_clone()?;
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "relative path contains an invalid component",
            ));
        };
        current = bound_platform::open_child(&current, name)?;
    }
    Ok(current)
}

fn local_ignore_matcher(
    directory: &File,
    relative_path: &Path,
    approved_root: &Path,
    ignore_name: &str,
) -> Option<Gitignore> {
    let file = bound_platform::open_child(directory, ignore_name.as_ref()).ok()?;
    ignore_matcher(
        file,
        &approved_root.join(relative_path),
        approved_root.join(relative_path).join(ignore_name),
    )
}

fn parent_ignore_sources(approved_root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut directories: Vec<&Path> = approved_root.ancestors().skip(1).collect();
    directories.reverse();
    let mut sources = Vec::with_capacity(directories.len().saturating_mul(3));
    for directory in directories {
        sources.push((directory.to_path_buf(), directory.join(".git/info/exclude")));
        for ignore_name in [".gitignore", ".ignore"] {
            sources.push((directory.to_path_buf(), directory.join(ignore_name)));
        }
    }
    sources
}

fn parent_ignore_stamps(sources: &[(PathBuf, PathBuf)]) -> Vec<IgnoreSourceStamp> {
    sources
        .iter()
        .map(|(_, source)| match std::fs::metadata(source) {
            Ok(metadata) => IgnoreSourceStamp {
                source: source.clone(),
                length: Some(metadata.len()),
                modified: metadata.modified().ok(),
                is_file: metadata.is_file(),
            },
            Err(_) => IgnoreSourceStamp {
                source: source.clone(),
                length: None,
                modified: None,
                is_file: false,
            },
        })
        .collect()
}

fn parse_parent_ignore_matchers(sources: &[(PathBuf, PathBuf)]) -> Arc<[Gitignore]> {
    sources
        .iter()
        .filter_map(|(directory, source)| {
            let file = File::open(source).ok()?;
            ignore_matcher(file, directory, source.clone())
        })
        .collect::<Vec<_>>()
        .into()
}

fn parent_ignore_matchers(approved_root: &Path) -> Arc<[Gitignore]> {
    let sources = parent_ignore_sources(approved_root);
    let mut stamps = parent_ignore_stamps(&sources);
    if let Some(matchers) = PARENT_IGNORE_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(approved_root)
        .filter(|entry| entry.stamps == stamps)
        .map(|entry| Arc::clone(&entry.matchers))
    {
        return matchers;
    }

    let mut matchers = parse_parent_ignore_matchers(&sources);
    let after_parse = parent_ignore_stamps(&sources);
    if after_parse != stamps {
        stamps = after_parse;
        matchers = parse_parent_ignore_matchers(&sources);
    }

    let mut cache = PARENT_IGNORE_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if cache.len() >= MAX_PARENT_IGNORE_CACHE_ENTRIES && !cache.contains_key(approved_root) {
        cache.clear();
    }
    cache.insert(
        approved_root.to_path_buf(),
        ParentIgnoreCacheEntry {
            stamps,
            matchers: Arc::clone(&matchers),
        },
    );
    matchers
}

fn ignore_matcher(mut file: File, root: &Path, source: PathBuf) -> Option<Gitignore> {
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    let mut builder = GitignoreBuilder::new(root);
    for line in contents.lines() {
        let _ = builder.add_line(Some(source.clone()), line);
    }
    builder.build().ok()
}

fn is_ignored(matchers: &IgnoreChain, path: &Path, is_directory: bool) -> bool {
    let mut node = matchers.0.as_deref();
    while let Some(current) = node {
        for matcher in current.matchers.iter().rev() {
            let matched = matcher.matched(path, is_directory);
            if matched.is_ignore() {
                return true;
            }
            if matched.is_whitelist() {
                return false;
            }
        }
        node = current.parent.as_deref();
    }
    false
}

pub struct FindFilesTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_results: u64,
    workspace: Option<std::sync::Arc<crate::paths::WorkspaceBinding>>,
}

enum FindFilesPattern {
    FileNameRegex(Regex),
    RelativePathGlob(GlobMatcher),
}

impl FindFilesPattern {
    fn compile(pattern: &str) -> Result<Self, ToolError> {
        match Regex::new(pattern) {
            Ok(regex) => Ok(Self::FileNameRegex(regex)),
            Err(regex_error) => GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map(|glob| Self::RelativePathGlob(glob.compile_matcher()))
                .map_err(|glob_error| {
                    ToolError::Msg(format!(
                        "Invalid filename regex or relative-path glob: regex: {regex_error}; glob: {glob_error}"
                    ))
                }),
        }
    }

    fn is_match(&self, file_name: &str, relative_path: &Path) -> bool {
        match self {
            Self::FileNameRegex(regex) => regex.is_match(file_name),
            Self::RelativePathGlob(glob) => glob.is_match(relative_path),
        }
    }
}

impl FindFilesTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, max_results: u64) -> Self {
        FindFilesTool {
            permission,
            ask_tx,
            max_results,
            workspace: None,
        }
    }

    pub(crate) fn with_workspace_binding(
        mut self,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    ) -> Self {
        self.workspace = Some(workspace);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_workspace(self, root: impl Into<std::path::PathBuf>) -> Self {
        self.with_workspace_binding(crate::agent::tools::capture_workspace_binding(root.into()))
    }
}

impl Tool for FindFilesTool {
    const NAME: &'static str = "find_files";

    type Error = ToolError;
    type Args = FindFilesArgs;
    type Output = String;

    fn description(&self) -> String {
        "Recursively find files using a filename regex or, when the value is not valid regex, a relative-path glob such as `**/*.rs`. Respects .gitignore. Returns the deterministic lexicographically first results when capped. Skips dependency/build directories and VCS metadata unless that directory is requested explicitly.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Filename regex, or a relative-path glob such as **/*.rs when not valid regex"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: FindFilesArgs) -> Result<String, ToolError> {
        tracing::debug!(
            "tool find_files start: pattern={}, path={}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
        );
        let coaching =
            check_perm(&self.permission, &self.ask_tx, "find_files", &args.pattern).await?;

        let pattern = FindFilesPattern::compile(&args.pattern)?;

        let requested_path = args.path.as_deref().unwrap_or(".");
        if requested_path.is_empty() {
            return Err(ToolError::Msg("Search path cannot be empty".to_string()));
        }
        let workspace_root =
            crate::agent::tools::validate_workspace_binding(self.workspace.as_ref())?;
        let search_path =
            crate::agent::tools::resolve_tool_path(workspace_root.as_deref(), requested_path);
        let relative = Path::new(requested_path);
        let (bound_directory, path_coaching) = if !relative.is_absolute()
            && !requested_path.starts_with('~')
            && let Some(workspace) = &self.workspace
        {
            let logical = workspace.logical_relative_path(relative)?;
            let directory = workspace.open_relative_directory_file(relative)?;
            let bound = BoundDirectory::from_file(&logical, directory)?;
            let coaching = check_perm_bound_path(
                &self.permission,
                &self.ask_tx,
                "find_files",
                workspace,
                relative,
            )
            .await?;
            (bound, coaching)
        } else {
            let traversal_root = tokio::fs::canonicalize(&search_path).await?;
            let authorized_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
            let bound = BoundDirectory::open(&traversal_root, &authorized_metadata)?;
            let coaching = check_perm_path(
                &self.permission,
                &self.ask_tx,
                "find_files",
                &traversal_root.to_string_lossy(),
            )
            .await?;
            (bound, coaching)
        };
        let coaching = combine_coaching(coaching, path_coaching);

        let max_results = usize::try_from(self.max_results).unwrap_or(usize::MAX);
        let (first_results, total_matches) =
            crate::agent::runner::spawn_blocking_scoped(move || -> Result<_, ToolError> {
                let traversal_root = bound_directory.approved_root().to_path_buf();
                let walker = bound_directory.walker()?;
                let mut first_results = BinaryHeap::with_capacity(max_results.min(64));
                let mut total_matches = 0_usize;

                for entry in walker {
                    let fname = entry.file_name.to_string_lossy();
                    let relative_path = entry
                        .path
                        .strip_prefix(&traversal_root)
                        .unwrap_or(&entry.path);
                    if pattern.is_match(&fname, relative_path) {
                        total_matches = total_matches.saturating_add(1);
                        let path = entry.path.to_string_lossy().to_string();
                        if first_results.len() < max_results {
                            first_results.push(path);
                        } else if let Some(current_last) = first_results.peek()
                            && path < *current_last
                        {
                            first_results.pop();
                            first_results.push(path);
                        }
                    }
                }
                Ok((first_results, total_matches))
            })
            .await
            .map_err(|error| {
                ToolError::Msg(format!("find_files directory walker failed: {error}"))
            })??;
        if total_matches == 0 {
            let msg = "No files found matching the pattern.".to_string();
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        let limit_hit = total_matches > max_results;
        let mut results = first_results.into_vec();
        results.sort();

        let result = if limit_hit {
            format!(
                "{} files found (showing first {}):\n{}\n\n[truncated after {} entries — {} additional entries; narrow the pattern or path]",
                total_matches,
                max_results,
                results.join("\n"),
                max_results,
                total_matches - max_results,
            )
        } else {
            format!("{} files found:\n{}", total_matches, results.join("\n"))
        };

        tracing::debug!(
            "tool find_files done: results={}, truncated={}",
            total_matches,
            limit_hit,
        );
        Ok(match coaching {
            Some(c) => format!("{}\n\n{}", c, result),
            None => result,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::permission::ask::UserDecision;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            Self::new_in(&std::env::temp_dir(), tag)
        }

        fn new_in(parent: &Path, tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = parent.join(format!(
                "zerostack_find_files_test_{}_{}_{}",
                tag,
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
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

    fn restrictive_permission_allowing_pattern() -> PermCheck {
        let config = PermissionConfig {
            find_files: Some(ToolPerm::Granular(
                [("needle".to_string(), Action::Allow)].into(),
            )),
            ..PermissionConfig::default()
        };
        Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::from(config),
                SecurityMode::Restrictive,
                Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
                Some(vec!["restrictive".to_string()]),
            )
            .expect("valid permission test configuration"),
        ))
    }

    fn standard_permission(working_dir: &Path) -> PermCheck {
        Arc::new(Mutex::new(
            PermissionChecker::new(
                &PermissionConfigs::default(),
                SecurityMode::Standard,
                Some(working_dir.to_path_buf()),
                None,
            )
            .expect("valid permission test configuration"),
        ))
    }

    async fn call_answering_path_permission(
        permission: PermCheck,
        args: FindFilesArgs,
        expected_path: &Path,
        decision: UserDecision,
    ) -> Result<String, ToolError> {
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(permission), Some(ask_tx), 10);
        let call = tool.call(args);
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("find_files did not request path permission")
                .expect("find_files permission channel closed");
            assert_eq!(request.tool.as_str(), "find_files");
            assert_eq!(
                PathBuf::from(request.input.as_str()),
                expected_path.to_path_buf()
            );
            request
                .reply
                .send(decision)
                .expect("find_files dropped the permission reply");
        };

        let (result, ()) = tokio::join!(call, respond);
        result
    }

    #[tokio::test]
    async fn prompts_before_searching_external_path() {
        let external = TempDir::new("restrictive_external");
        let canonical_external = std::fs::canonicalize(external.path()).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(
            Some(restrictive_permission_allowing_pattern()),
            Some(ask_tx),
            10,
        );

        let call = tool.call(FindFilesArgs {
            pattern: "needle".to_string(),
            path: Some(external.path().to_string_lossy().into_owned()),
        });
        let respond = async {
            let request = tokio::time::timeout(Duration::from_secs(1), ask_rx.recv())
                .await
                .expect("find_files did not request path permission")
                .expect("find_files permission channel closed");
            assert_eq!(request.tool.as_str(), "find_files");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_external);
            request
                .reply
                .send(UserDecision::Deny)
                .expect("find_files dropped the permission reply");
        };

        let (result, ()) = tokio::join!(call, respond);
        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission denied by user"
        ));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_keeps_local_relative_searches() {
        let cwd = std::env::current_dir().unwrap();
        let dir = TempDir::new_in(&cwd, "local_relative");
        let marker = "find_files_local_relative_marker.txt";
        std::fs::write(dir.path().join(marker), "").unwrap();
        let relative_root = dir.path().strip_prefix(&cwd).unwrap();

        let output = FindFilesTool::new(Some(standard_permission(&cwd)), None, 10)
            .call(FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: Some(relative_root.to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.contains(marker));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_uses_canonical_absolute_root() {
        let container = TempDir::new("absolute_external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "absolute_external_marker.txt";
        std::fs::write(external.join(marker), "").unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: Some(external.to_string_lossy().into_owned()),
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission denied by user"
        ));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_resolves_traversal_before_asking() {
        let container = TempDir::new("traversal_external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let requested = workspace.join("..").join("external");
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some(requested.to_string_lossy().into_owned()),
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn find_files_external_path_permission_expands_tilde_before_asking() {
        let home = PathBuf::from(crate::fs::expand_tilde("~"));
        assert_ne!(home, PathBuf::from("~"), "test requires a home directory");
        let workspace = TempDir::new("tilde_workspace");
        let canonical_home = std::fs::canonicalize(&home).unwrap();

        let result = call_answering_path_permission(
            standard_permission(workspace.path()),
            FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some("~".to_string()),
            },
            &canonical_home,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_files_external_path_permission_resolves_symlink_escape_before_asking() {
        let container = TempDir::new("symlink_external");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        let link = workspace.join("escaped");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, &link).unwrap();
        let canonical_external = std::fs::canonicalize(&external).unwrap();

        let result = call_answering_path_permission(
            standard_permission(&workspace),
            FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some(link.to_string_lossy().into_owned()),
            },
            &canonical_external,
            UserDecision::Deny,
        )
        .await;

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_files_external_path_permission_binds_walker_to_authorized_symlink_target() {
        let container = TempDir::new("symlink_binding");
        let workspace = container.path().join("workspace");
        let authorized = container.path().join("authorized");
        let swapped = container.path().join("swapped");
        let link = workspace.join("root");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&swapped).unwrap();
        std::fs::write(authorized.join("authorized_marker.txt"), "").unwrap();
        std::fs::write(swapped.join("swapped_marker.txt"), "").unwrap();
        std::os::unix::fs::symlink(&authorized, &link).unwrap();
        let canonical_authorized = std::fs::canonicalize(&authorized).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let call = tool.call(FindFilesArgs {
            pattern: "marker".to_string(),
            path: Some(link.to_string_lossy().into_owned()),
        });
        let swap = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_authorized);
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&swapped, &link).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, swap);
        let output = result.unwrap();
        assert!(output.contains("authorized_marker.txt"));
        assert!(!output.contains("swapped_marker.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_files_external_path_permission_retains_authorized_root_on_replacement() {
        let container = TempDir::new("root_replacement");
        let workspace = container.path().join("workspace");
        let authorized = container.path().join("authorized");
        let moved = container.path().join("moved");
        let swapped = container.path().join("swapped");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&swapped).unwrap();
        std::fs::write(swapped.join("must_not_be_returned.txt"), "").unwrap();
        let canonical_authorized = std::fs::canonicalize(&authorized).unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let call = tool.call(FindFilesArgs {
            pattern: "must_not_be_returned".to_string(),
            path: Some(authorized.to_string_lossy().into_owned()),
        });
        let replace = async {
            let request = ask_rx.recv().await.expect("permission request");
            assert_eq!(PathBuf::from(request.input.as_str()), canonical_authorized);
            std::fs::rename(&authorized, &moved).unwrap();
            std::os::unix::fs::symlink(&swapped, &authorized).unwrap();
            request.reply.send(UserDecision::AllowOnce).unwrap();
        };

        let (result, ()) = tokio::join!(call, replace);
        let output = result.expect("descriptor-bound search must retain the authorized root");
        assert!(output.contains("No files found"));
        assert!(!output.contains("must_not_be_returned.txt"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn bound_walker_never_observes_an_aba_root_replacement() {
        let container = TempDir::new("aba_root_replacement");
        let authorized = container.path().join("authorized");
        let moved = container.path().join("moved");
        let replacement = container.path().join("replacement");
        std::fs::create_dir_all(&authorized).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        std::fs::write(authorized.join("authorized_one.txt"), "").unwrap();
        std::fs::write(authorized.join("authorized_two.txt"), "").unwrap();
        let secret = "aba_secret_marker.txt";
        std::fs::write(replacement.join(secret), "").unwrap();

        let approved_metadata = crate::fs::checked_path_metadata(&authorized).unwrap();
        let bound = BoundDirectory::open(&authorized, &approved_metadata).unwrap();
        std::fs::rename(&authorized, &moved).unwrap();
        std::fs::rename(&replacement, &authorized).unwrap();

        let mut walker = bound.walker().unwrap();
        let first = walker.next().expect("approved directory has two files");
        let mut names = vec![first.file_name.to_string_lossy().into_owned()];

        std::fs::rename(&authorized, &replacement).unwrap();
        std::fs::rename(&moved, &authorized).unwrap();
        names.extend(walker.map(|entry| entry.file_name.to_string_lossy().into_owned()));

        assert_eq!(names.len(), 2);
        assert!(!names.iter().any(|name| name == secret));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_pattern_cannot_widen_root() {
        let container = TempDir::new("pattern_root");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "pattern_must_not_escape_marker.txt";
        std::fs::write(external.join(marker), "").unwrap();

        let output = FindFilesTool::new(Some(standard_permission(&workspace)), None, 10)
            .call(FindFilesArgs {
                pattern: format!(".*{marker}$"),
                path: Some(workspace.to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert_eq!(output, "No files found matching the pattern.");
    }

    #[tokio::test]
    async fn find_files_external_path_permission_omitted_root_searches_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let dir = TempDir::new_in(&cwd, "omitted_root");
        let marker = "find_files_omitted_root_marker.txt";
        std::fs::write(dir.path().join(marker), "").unwrap();

        let output = FindFilesTool::new(Some(standard_permission(&cwd)), None, 10)
            .call(FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: None,
            })
            .await
            .unwrap();

        assert!(output.contains(marker));
    }

    #[tokio::test]
    async fn find_files_external_path_permission_rejects_empty_root_before_asking() {
        let cwd = std::env::current_dir().unwrap();
        let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
        let tool = FindFilesTool::new(Some(standard_permission(&cwd)), Some(ask_tx), 10);

        let result = tool
            .call(FindFilesArgs {
                pattern: "needle".to_string(),
                path: Some(String::new()),
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Search path cannot be empty"
        ));
        assert!(ask_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn find_files_external_path_permission_fails_closed_on_permission_channel_failure() {
        let container = TempDir::new("closed_permission_channel");
        let workspace = container.path().join("workspace");
        let external = container.path().join("external");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let marker = "closed_permission_channel_marker.txt";
        std::fs::write(external.join(marker), "").unwrap();
        let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(1);
        drop(ask_rx);
        let tool = FindFilesTool::new(Some(standard_permission(&workspace)), Some(ask_tx), 10);

        let result = tool
            .call(FindFilesArgs {
                pattern: format!("^{marker}$"),
                path: Some(external.to_string_lossy().into_owned()),
            })
            .await;

        assert!(matches!(
            result,
            Err(ToolError::Msg(ref msg)) if msg == "Permission system unavailable"
        ));
    }

    #[tokio::test]
    async fn reports_exact_remaining_count_when_result_limit_is_hit() {
        let dir = TempDir::new("truncation");
        for index in 0..101 {
            std::fs::write(dir.path().join(format!("match_{index:03}.txt")), "").unwrap();
        }

        let output = FindFilesTool::new(None, None, 100)
            .call(FindFilesArgs {
                pattern: r"^match_\d+\.txt$".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.contains("truncated after 100 entries"));
        assert!(output.contains("1 additional entries"));
    }

    #[tokio::test]
    async fn does_not_report_truncation_when_walker_is_exhausted_at_result_limit() {
        let dir = TempDir::new("exact_limit");
        for index in 0..100 {
            std::fs::write(dir.path().join(format!("match_{index:03}.txt")), "").unwrap();
        }

        let output = FindFilesTool::new(None, None, 100)
            .call(FindFilesArgs {
                pattern: r"^match_\d+\.txt$".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.starts_with("100 files found:\n"));
        assert!(!output.contains("[truncated"));
    }

    #[tokio::test]
    async fn path_glob_matches_nested_and_root_files() {
        let dir = TempDir::new("path_glob");
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();
        std::fs::write(dir.path().join("nested").join("child.rs"), "").unwrap();
        std::fs::write(dir.path().join("nested").join("child.txt"), "").unwrap();

        let output = FindFilesTool::new(None, None, 10)
            .call(FindFilesArgs {
                pattern: "**/*.rs".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(
            output.contains(dir.path().join("root.rs").to_string_lossy().as_ref()),
            "{output}"
        );
        assert!(
            output.contains(
                dir.path()
                    .join("nested")
                    .join("child.rs")
                    .to_string_lossy()
                    .as_ref()
            ),
            "{output}"
        );
        assert!(!output.contains("child.txt"), "{output}");
    }

    #[tokio::test]
    async fn capped_results_are_lexicographically_first_independent_of_walk_order() {
        let dir = TempDir::new("deterministic_cap");
        for name in ["z.txt", "m.txt", "a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }

        let output = FindFilesTool::new(None, None, 2)
            .call(FindFilesArgs {
                pattern: r"^[a-z]\.txt$".to_string(),
                path: Some(dir.path().to_string_lossy().into_owned()),
            })
            .await
            .unwrap();

        assert!(output.contains("a.txt"), "{output}");
        assert!(output.contains("b.txt"), "{output}");
        assert!(!output.contains("m.txt"), "{output}");
        assert!(!output.contains("z.txt"), "{output}");
        assert!(output.starts_with("4 files found (showing first 2):"));
    }

    #[test]
    fn parent_ignore_matchers_are_cached_and_invalidated_by_source_changes() {
        let container = TempDir::new("parent_ignore_cache");
        let workspace = container.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let ignore = container.path().join(".gitignore");
        std::fs::write(&ignore, "first.txt\n").unwrap();

        let first = parent_ignore_matchers(&workspace);
        let cached = parent_ignore_matchers(&workspace);
        assert!(Arc::ptr_eq(&first, &cached));

        std::fs::write(&ignore, "different-length-name.txt\n").unwrap();
        let refreshed = parent_ignore_matchers(&workspace);
        assert!(!Arc::ptr_eq(&cached, &refreshed));
    }

    #[test]
    fn nested_ignore_whitelist_overrides_parent_matcher_without_cloning_chain() {
        let dir = TempDir::new("ignore_chain_precedence");
        let nested = dir.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.txt\n").unwrap();
        std::fs::write(nested.join(".gitignore"), "!keep.txt\n").unwrap();
        std::fs::write(nested.join("keep.txt"), "visible").unwrap();
        std::fs::write(nested.join("drop.txt"), "hidden").unwrap();

        let metadata = crate::fs::checked_path_metadata(dir.path()).unwrap();
        let bound = BoundDirectory::open(dir.path(), &metadata).unwrap();
        let names: Vec<_> = bound
            .walker()
            .unwrap()
            .map(|entry| entry.file_name.to_string_lossy().into_owned())
            .collect();

        assert!(names.iter().any(|name| name == "keep.txt"));
        assert!(!names.iter().any(|name| name == "drop.txt"));
    }
}
