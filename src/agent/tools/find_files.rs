use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use regex::Regex;
use rig::tool::Tool;

use crate::agent::tools::{
    AskSender, FindFilesArgs, PermCheck, ToolError, check_perm, check_perm_path, is_skip_dir,
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

    #[cfg(target_os = "linux")]
    const OPEN_NOFOLLOW: c_int = 0x2_0000;
    #[cfg(target_os = "linux")]
    const OPEN_CLOEXEC: c_int = 0x8_0000;
    #[cfg(target_os = "linux")]
    const OPEN_NONBLOCK: c_int = 0x800;

    #[cfg(target_os = "macos")]
    const OPEN_NOFOLLOW: c_int = 0x100;
    #[cfg(target_os = "macos")]
    const OPEN_CLOEXEC: c_int = 0x100_0000;
    #[cfg(target_os = "macos")]
    const OPEN_NONBLOCK: c_int = 0x4;

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
        fn fdopendir(descriptor: c_int) -> *mut DirectoryStream;
        fn readdir(directory: *mut DirectoryStream) -> *mut DirectoryEntry;
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
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
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
}

pub(super) struct BoundFile {
    pub(super) path: PathBuf,
    pub(super) file_name: OsString,
    pub(super) file: File,
    pub(super) metadata: std::fs::Metadata,
}

pub(super) struct BoundDirectory {
    approved_root: PathBuf,
    root: File,
}

impl BoundDirectory {
    pub(super) fn open(
        approved_root: &Path,
        approved_metadata: &std::fs::Metadata,
    ) -> std::io::Result<Self> {
        if !approved_metadata.is_dir() || !bound_platform::is_safe_entry(approved_metadata) {
            return Err(path_changed_error(approved_root));
        }
        let root = bound_platform::open_root(approved_root)?;
        let opened_metadata = root.metadata()?;
        let current_metadata = std::fs::symlink_metadata(approved_root)?;
        if current_metadata.file_type().is_symlink()
            || !bound_platform::is_safe_entry(&opened_metadata)
        {
            return Err(path_changed_error(approved_root));
        }
        crate::fs::ensure_same_file(approved_root, approved_metadata, &opened_metadata)?;
        crate::fs::ensure_same_file(approved_root, &opened_metadata, &current_metadata)?;
        Ok(Self {
            approved_root: approved_root.to_path_buf(),
            root,
        })
    }

    pub(super) fn walker(&self) -> std::io::Result<BoundWalker> {
        BoundWalker::new(self.root.try_clone()?, self.approved_root.clone())
    }
}

struct DirectoryFrame {
    directory: File,
    relative_path: PathBuf,
    names: bound_platform::DirectoryReader,
    matchers: Vec<Gitignore>,
}

impl DirectoryFrame {
    fn new(
        directory: File,
        relative_path: PathBuf,
        mut matchers: Vec<Gitignore>,
        approved_root: &Path,
    ) -> std::io::Result<Self> {
        for ignore_name in [".gitignore", ".ignore"] {
            if let Some(matcher) =
                local_ignore_matcher(&directory, &relative_path, approved_root, ignore_name)
            {
                matchers.push(matcher);
            }
        }
        let names = bound_platform::read_directory(&directory)?;
        Ok(Self {
            directory,
            relative_path,
            names,
            matchers,
        })
    }
}

pub(super) struct BoundWalker {
    approved_root: PathBuf,
    stack: Vec<DirectoryFrame>,
}

impl BoundWalker {
    fn new(root: File, approved_root: PathBuf) -> std::io::Result<Self> {
        let mut matchers = Vec::new();
        let (global, _) = GitignoreBuilder::new(&approved_root).build_global();
        if !global.is_empty() {
            matchers.push(global);
        }
        matchers.extend(parent_ignore_matchers(&approved_root));
        if let Ok(exclude) = open_relative(&root, Path::new(".git/info/exclude"))
            && let Some(matcher) = ignore_matcher(
                exclude,
                &approved_root,
                approved_root.join(".git/info/exclude"),
            )
        {
            matchers.push(matcher);
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

fn parent_ignore_matchers(approved_root: &Path) -> Vec<Gitignore> {
    let mut directories: Vec<&Path> = approved_root.ancestors().skip(1).collect();
    directories.reverse();
    let mut matchers = Vec::new();
    for directory in directories {
        let exclude_path = directory.join(".git/info/exclude");
        if let Ok(file) = File::open(&exclude_path)
            && let Some(matcher) = ignore_matcher(file, directory, exclude_path)
        {
            matchers.push(matcher);
        }
        for ignore_name in [".gitignore", ".ignore"] {
            let source = directory.join(ignore_name);
            if let Ok(file) = File::open(&source)
                && let Some(matcher) = ignore_matcher(file, directory, source)
            {
                matchers.push(matcher);
            }
        }
    }
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

fn is_ignored(matchers: &[Gitignore], path: &Path, is_directory: bool) -> bool {
    let mut ignored = false;
    for matcher in matchers {
        let matched = matcher.matched(path, is_directory);
        if matched.is_ignore() {
            ignored = true;
        } else if matched.is_whitelist() {
            ignored = false;
        }
    }
    ignored
}

pub struct FindFilesTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub max_results: u64,
}

impl FindFilesTool {
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, max_results: u64) -> Self {
        FindFilesTool {
            permission,
            ask_tx,
            max_results,
        }
    }
}

impl Tool for FindFilesTool {
    const NAME: &'static str = "find_files";

    type Error = ToolError;
    type Args = FindFilesArgs;
    type Output = String;

    fn description(&self) -> String {
        "Recursively find files matching a regex pattern in their filename. Respects .gitignore. Skips node_modules and target.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to match file names against"
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

        let re = Regex::new(&args.pattern)
            .map_err(|e| ToolError::Msg(format!("Invalid regex: {}", e)))?;

        let requested_path = args.path.as_deref().unwrap_or(".");
        if requested_path.is_empty() {
            return Err(ToolError::Msg("Search path cannot be empty".to_string()));
        }
        let search_path = crate::fs::expand_tilde(requested_path);
        let traversal_root = tokio::fs::canonicalize(&search_path).await?;
        let authorized_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        let bound_directory = BoundDirectory::open(&traversal_root, &authorized_metadata)?;
        let permission_path = traversal_root.to_string_lossy();
        let _ = check_perm_path(
            &self.permission,
            &self.ask_tx,
            "find_files",
            &permission_path,
        )
        .await?;
        let traversal_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        crate::fs::ensure_same_file(&traversal_root, &authorized_metadata, &traversal_metadata)?;

        let walker = bound_directory.walker()?;

        let max_results = self.max_results as usize;
        let mut results: Vec<String> = Vec::with_capacity(max_results.saturating_add(1).min(64));
        let mut limit_hit = false;

        for entry in walker {
            let fname = entry.file_name.to_string_lossy();
            if re.is_match(&fname) {
                results.push(entry.path.to_string_lossy().to_string());
                if results.len() > max_results {
                    limit_hit = true;
                    break;
                }
            }
        }
        let current_metadata = crate::fs::stable_path_metadata(&traversal_root).await?;
        crate::fs::ensure_same_file(&traversal_root, &authorized_metadata, &current_metadata)?;

        if results.is_empty() {
            let msg = "No files found matching the pattern.".to_string();
            return Ok(match coaching {
                Some(c) => format!("{}\n\n{}", c, msg),
                None => msg,
            });
        }

        if limit_hit {
            results.truncate(max_results);
        }
        results.sort();

        let total = results.len();
        let result = if limit_hit {
            format!(
                "{} files found (showing first {}):\n{}\n\n[truncated after {} entries — unknown number of additional entries; narrow the pattern or path]",
                total,
                max_results,
                results[..max_results].join("\n"),
                max_results
            )
        } else {
            format!("{} files found:\n{}", total, results.join("\n"))
        };

        tracing::debug!(
            "tool find_files done: results={}, truncated={}",
            total,
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
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Restrictive,
            Some(std::path::PathBuf::from("/workspace")),
            Some(vec!["restrictive".to_string()]),
        )))
    }

    fn standard_permission(working_dir: &Path) -> PermCheck {
        Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(working_dir.to_path_buf()),
            None,
        )))
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
    async fn find_files_external_path_permission_rejects_authorized_root_replacement() {
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
        let error = result.expect_err("find_files must reject a replaced traversal root");
        assert!(error.to_string().contains("Path changed"));
        assert!(!error.to_string().contains("must_not_be_returned.txt"));
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

        let approved_metadata = std::fs::symlink_metadata(&authorized).unwrap();
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
    async fn reports_unknown_remaining_count_when_result_limit_is_hit() {
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
        assert!(output.contains("unknown number of additional entries"));
        assert!(!output.contains("0 more"));
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
}
