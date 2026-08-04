//! Bounded recovery of crashed one-time macOS worker publications.
//!
//! The publisher and sweeper share the exact private-root, canonical-directory, lease, and image
//! protocol. Recovery is conservative: malformed or replaced state fails closed before mutation.

use std::ffi::{CString, OsStr, c_char, c_int, c_void};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const CANDIDATE_PREFIX: &str = ".mini-agent-js-worker-";
const LEASE_NAME: &str = "lease";
const IMAGE_NAME: &str = "worker-image";

const OPEN_NOFOLLOW: i32 = 0x100;
const OPEN_DIRECTORY: i32 = 0x10_0000;
const OPEN_CLOEXEC: i32 = 0x100_0000;
const OPEN_READ_ONLY: i32 = 0;
const OPEN_READ_WRITE: i32 = 2;
const AT_REMOVE_DIRECTORY: i32 = 0x80;
const LOCK_EXCLUSIVE: c_int = 2;
const LOCK_NONBLOCKING: c_int = 4;
const INTERRUPTED_ERRNO: c_int = 4;
const WOULD_BLOCK_ERRNO: c_int = 35;
const FILE_DESCRIPTOR_CLOEXEC: i32 = 1;
const FCNTL_GET_DESCRIPTOR_FLAGS: i32 = 1;
const ACL_TYPE_EXTENDED: c_int = 0x100;
const ACL_FIRST_ENTRY: c_int = 0;
const INVALID_ARGUMENT_ERRNO: c_int = 22;
const NO_ENTRY_ERRNO: c_int = 2;
const DARWIN_DIRENT_NAME_CAPACITY: usize = 1_024;
const MAX_ROOT_ENTRIES: usize = 1_024;
const MAX_CANDIDATES: usize = 64;
const MAX_CANDIDATE_ENTRIES: usize = 2;
const MAX_CONSECUTIVE_FLOCK_INTERRUPTS: usize = 8;

#[derive(Clone, Copy, Debug)]
struct SweepPolicy {
    now: SystemTime,
    min_age: Duration,
    max_root_entries: usize,
    max_candidates: usize,
    max_candidate_entries: usize,
}

impl SweepPolicy {
    #[cfg(test)]
    fn test(now: SystemTime) -> Self {
        Self {
            now,
            min_age: Duration::from_secs(60 * 60),
            max_root_entries: MAX_ROOT_ENTRIES,
            max_candidates: MAX_CANDIDATES,
            max_candidate_entries: MAX_CANDIDATE_ENTRIES,
        }
    }

    fn validate(self) -> io::Result<Self> {
        if self.min_age.is_zero()
            || self.max_root_entries == 0
            || self.max_root_entries > MAX_ROOT_ENTRIES
            || self.max_candidates == 0
            || self.max_candidates > MAX_CANDIDATES
            || self.max_candidate_entries != MAX_CANDIDATE_ENTRIES
            || self.max_candidates > self.max_root_entries
        {
            return Err(invalid_data("invalid stale-sweep policy bounds"));
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SweepSummary {
    removed: usize,
    skipped_live: usize,
    skipped_young: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SweepOutcome {
    RootBusy,
    Completed(SweepSummary),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SweepFaultStage {
    BeforeImageUnlink,
    BeforeDirectoryRemove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InodeIdentity {
    device: u64,
    inode: u64,
}

impl InodeIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn matches(self, metadata: &std::fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }
}

#[derive(Debug)]
struct HeldLock {
    file: std::fs::File,
    identity: InodeIdentity,
}

#[derive(Debug)]
enum TryLock {
    Acquired(HeldLock),
    Busy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlockDisposition {
    Acquired,
    Retry,
    Busy,
}

#[derive(Debug)]
enum CandidatePlan {
    Remove(StaleCandidate),
    Live,
    Young,
}

#[derive(Debug)]
struct StaleCandidate {
    name: Vec<u8>,
    path: PathBuf,
    directory: std::fs::File,
    directory_identity: InodeIdentity,
    lease: Option<HeldLock>,
    image: Option<(std::fs::File, InodeIdentity)>,
}

#[repr(C)]
struct DarwinDirectoryStream {
    _private: [u8; 0],
}

#[repr(C)]
struct DarwinDirectoryEntry {
    inode: u64,
    seek_offset: u64,
    record_length: u16,
    name_length: u16,
    file_type: u8,
    name: [c_char; DARWIN_DIRENT_NAME_CAPACITY],
}

#[repr(C)]
struct DarwinAcl {
    _private: [u8; 0],
}

#[repr(C)]
struct DarwinAclEntry {
    _private: [u8; 0],
}

type DarwinOpenAt = unsafe extern "C" fn(c_int, *const c_char, c_int, ...) -> c_int;

#[allow(clashing_extern_declarations, unsafe_code)]
unsafe extern "C" {
    #[link_name = "openat"]
    fn stale_sweep_openat(directory: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
}

const _: DarwinOpenAt = stale_sweep_openat;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn permission_denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| invalid_data("stale-sweep path component contains NUL"))
}

#[allow(unsafe_code)]
fn open_at(
    directory: &std::fs::File,
    name: &OsStr,
    flags: i32,
    mode: u16,
) -> io::Result<std::fs::File> {
    let name = c_name(name)?;
    // SAFETY: the directory descriptor and C string remain live for this Darwin variadic call.
    let descriptor = unsafe {
        stale_sweep_openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            c_int::from(mode),
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `openat` returned a new owned descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }
}

fn open_directory(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC)
        .open(path)
}

fn open_directory_at(directory: &std::fs::File, name: &OsStr) -> io::Result<std::fs::File> {
    open_at(
        directory,
        name,
        OPEN_READ_ONLY | OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
        0,
    )
}

#[allow(unsafe_code)]
fn unlink_at(directory: &std::fs::File, name: &OsStr, flags: i32) -> io::Result<()> {
    unsafe extern "C" {
        #[link_name = "unlinkat"]
        fn stale_sweep_unlinkat(directory: c_int, path: *const c_char, flags: c_int) -> c_int;
    }
    let name = c_name(name)?;
    // SAFETY: the directory descriptor and C string remain live for this call.
    if unsafe { stale_sweep_unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[allow(unsafe_code, clashing_extern_declarations)]
fn directory_entry_names(directory: &std::fs::File, limit: usize) -> io::Result<Vec<Vec<u8>>> {
    unsafe extern "C" {
        #[cfg_attr(target_arch = "x86_64", link_name = "fdopendir$INODE64")]
        #[cfg_attr(target_arch = "aarch64", link_name = "fdopendir")]
        fn stale_sweep_fdopendir(descriptor: c_int) -> *mut DarwinDirectoryStream;
        #[cfg_attr(target_arch = "x86_64", link_name = "readdir$INODE64")]
        #[cfg_attr(target_arch = "aarch64", link_name = "readdir")]
        fn stale_sweep_readdir(directory: *mut DarwinDirectoryStream) -> *mut DarwinDirectoryEntry;
        #[link_name = "closedir"]
        fn stale_sweep_closedir(directory: *mut DarwinDirectoryStream) -> c_int;
        fn __error() -> *mut c_int;
    }

    let copy = open_at(
        directory,
        OsStr::new("."),
        OPEN_READ_ONLY | OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
        0,
    )?;
    let descriptor = copy.into_raw_fd();
    // SAFETY: the descriptor is newly owned. On success fdopendir consumes it.
    let stream = unsafe { stale_sweep_fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: failed fdopendir did not consume the descriptor.
        drop(unsafe { std::fs::File::from_raw_fd(descriptor) });
        return Err(error);
    }

    let result = (|| {
        let mut names = Vec::new();
        loop {
            // SAFETY: `__error` returns this thread's errno storage.
            unsafe { *__error() = 0 };
            // SAFETY: the directory stream remains live until closed below.
            let entry = unsafe { stale_sweep_readdir(stream) };
            if entry.is_null() {
                // SAFETY: errno storage remains live.
                let errno = unsafe { *__error() };
                if errno == 0 {
                    break;
                }
                return Err(io::Error::from_raw_os_error(errno));
            }
            // SAFETY: readdir returned a live Darwin dirent.
            let name_length = unsafe { (*entry).name_length as usize };
            if name_length >= DARWIN_DIRENT_NAME_CAPACITY {
                return Err(invalid_data(
                    "directory entry name exceeds Darwin ABI bound",
                ));
            }
            // SAFETY: the validated name length lies within the fixed array.
            let name = unsafe {
                std::slice::from_raw_parts((*entry).name.as_ptr().cast::<u8>(), name_length)
            };
            if name != b"." && name != b".." {
                if names.len() == limit {
                    return Err(invalid_data("stale-sweep directory entry bound exceeded"));
                }
                names.push(name.to_vec());
            }
        }
        Ok(names)
    })();
    // SAFETY: the stream is owned and closed exactly once.
    let close_result = unsafe { stale_sweep_closedir(stream) };
    match result {
        Err(error) => Err(error),
        Ok(_) if close_result != 0 => Err(io::Error::last_os_error()),
        Ok(names) => Ok(names),
    }
}

#[allow(unsafe_code)]
fn current_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid has no arguments or failure mode.
    unsafe { getuid() }
}

fn validate_private_directory(metadata: &std::fs::Metadata, label: &str) -> io::Result<()> {
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o7777 != 0o700
        || metadata.nlink() == 0
    {
        return Err(permission_denied(label));
    }
    Ok(())
}

fn validate_regular_file(
    file: &std::fs::File,
    metadata: &std::fs::Metadata,
    expected_modes: &[u32],
    label: &str,
) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != current_uid()
        || metadata.nlink() != 1
        || !expected_modes.contains(&(metadata.mode() & 0o7777))
    {
        return Err(permission_denied(label));
    }
    ensure_no_extended_acl(file, label)
}

#[allow(unsafe_code)]
fn ensure_no_extended_acl(file: &std::fs::File, label: &str) -> io::Result<()> {
    unsafe extern "C" {
        fn acl_get_fd_np(descriptor: c_int, acl_type: c_int) -> *mut DarwinAcl;
        fn acl_get_entry(
            acl: *mut DarwinAcl,
            entry_id: c_int,
            entry: *mut *mut DarwinAclEntry,
        ) -> c_int;
        fn acl_free(object: *mut c_void) -> c_int;
        fn __error() -> *mut c_int;
    }
    // SAFETY: file owns a live descriptor and the ACL type is Darwin's extended ACL.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(NO_ENTRY_ERRNO) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: errno storage and ACL pointers remain valid for the calls.
    unsafe { *__error() = 0 };
    // SAFETY: ACL is live and entry points to writable storage.
    let result = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
    // SAFETY: errno storage remains live.
    let errno = unsafe { *__error() };
    // SAFETY: acl_get_fd_np returned the allocation, freed exactly once.
    let free_result = unsafe { acl_free(acl.cast::<c_void>()) };
    if free_result != 0 {
        return Err(io::Error::last_os_error());
    }
    match (result, errno) {
        (-1, INVALID_ARGUMENT_ERRNO) => Ok(()),
        (0, _) => Err(permission_denied(format!(
            "{label} has extended ACL entries"
        ))),
        _ => Err(io::Error::from_raw_os_error(errno)),
    }
}

#[allow(unsafe_code)]
fn descriptor_has_close_on_exec(descriptor: RawFd) -> io::Result<bool> {
    unsafe extern "C" {
        fn fcntl(descriptor: c_int, command: c_int, ...) -> c_int;
    }
    // SAFETY: descriptor is live and F_GETFD takes no variadic argument.
    let flags = unsafe { fcntl(descriptor, FCNTL_GET_DESCRIPTOR_FLAGS) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags & FILE_DESCRIPTOR_CLOEXEC != 0)
    }
}

fn classify_flock_result(result: c_int, errno: c_int) -> io::Result<FlockDisposition> {
    if result == 0 {
        return Ok(FlockDisposition::Acquired);
    }
    match errno {
        INTERRUPTED_ERRNO => Ok(FlockDisposition::Retry),
        WOULD_BLOCK_ERRNO => Ok(FlockDisposition::Busy),
        _ => Err(io::Error::from_raw_os_error(errno)),
    }
}

fn bounded_flock_disposition(
    result: c_int,
    errno: c_int,
    consecutive_interrupts: usize,
) -> io::Result<FlockDisposition> {
    let disposition = classify_flock_result(result, errno)?;
    if disposition == FlockDisposition::Retry
        && consecutive_interrupts >= MAX_CONSECUTIVE_FLOCK_INTERRUPTS
    {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "stale-sweep flock interrupt retry bound exceeded",
        ));
    }
    Ok(disposition)
}

#[allow(unsafe_code)]
fn try_lock_file(file: std::fs::File) -> io::Result<TryLock> {
    unsafe extern "C" {
        #[link_name = "flock"]
        fn stale_sweep_flock(descriptor: c_int, operation: c_int) -> c_int;
    }
    let metadata = file.metadata()?;
    let identity = InodeIdentity::from_metadata(&metadata);
    let mut consecutive_interrupts = 0;
    loop {
        // SAFETY: file owns a live descriptor and Darwin accepts this flag combination.
        let result =
            unsafe { stale_sweep_flock(file.as_raw_fd(), LOCK_EXCLUSIVE | LOCK_NONBLOCKING) };
        let errno = if result == 0 {
            0
        } else {
            io::Error::last_os_error().raw_os_error().unwrap_or(0)
        };
        match bounded_flock_disposition(result, errno, consecutive_interrupts)? {
            FlockDisposition::Acquired => {
                return Ok(TryLock::Acquired(HeldLock { file, identity }));
            }
            FlockDisposition::Retry => consecutive_interrupts += 1,
            FlockDisposition::Busy => return Ok(TryLock::Busy),
        }
    }
}

fn revalidate_file_entry(
    parent: &std::fs::File,
    name: &OsStr,
    expected: InodeIdentity,
    flags: i32,
    label: &str,
) -> io::Result<std::fs::File> {
    let file = open_at(parent, name, flags | OPEN_NOFOLLOW | OPEN_CLOEXEC, 0)?;
    if !expected.matches(&file.metadata()?) {
        return Err(permission_denied(format!("{label} was replaced")));
    }
    Ok(file)
}

fn canonical_candidate_name(name: &[u8]) -> io::Result<Option<String>> {
    if !name.starts_with(CANDIDATE_PREFIX.as_bytes()) {
        return Ok(None);
    }
    let name = std::str::from_utf8(name)
        .map_err(|_| invalid_data("prefixed stale-sweep candidate is not UTF-8"))?;
    let suffix = name
        .strip_prefix(CANDIDATE_PREFIX)
        .ok_or_else(|| invalid_data("candidate prefix disappeared"))?;
    let uuid = uuid::Uuid::parse_str(suffix)
        .map_err(|_| invalid_data("prefixed stale-sweep candidate has an invalid UUID"))?;
    if uuid.to_string() != suffix {
        return Err(invalid_data(
            "prefixed stale-sweep candidate UUID is not canonical",
        ));
    }
    Ok(Some(name.to_string()))
}

fn metadata_is_stale(metadata: &[&std::fs::Metadata], policy: SweepPolicy) -> io::Result<bool> {
    let mut latest = SystemTime::UNIX_EPOCH;
    for metadata in metadata {
        latest = latest.max(metadata.modified()?);
    }
    Ok(policy
        .now
        .duration_since(latest)
        .is_ok_and(|age| age >= policy.min_age))
}

fn preflight_candidate(
    root: &std::fs::File,
    root_path: &Path,
    name: String,
    policy: SweepPolicy,
) -> io::Result<CandidatePlan> {
    let name_os = OsStr::new(&name);
    let path = root_path.join(name_os);
    let directory = open_directory_at(root, name_os)?;
    let directory_metadata = directory.metadata()?;
    validate_private_directory(&directory_metadata, "stale publication directory")?;
    ensure_no_extended_acl(&directory, "stale publication directory")?;
    if !descriptor_has_close_on_exec(directory.as_raw_fd())? {
        return Err(permission_denied(
            "stale publication directory is not close-on-exec",
        ));
    }
    let directory_identity = InodeIdentity::from_metadata(&directory_metadata);
    revalidate_file_entry(
        root,
        name_os,
        directory_identity,
        OPEN_READ_ONLY | OPEN_DIRECTORY,
        "stale publication directory",
    )?;

    let entries = directory_entry_names(&directory, policy.max_candidate_entries)?;
    if entries.is_empty() {
        return if metadata_is_stale(&[&directory_metadata], policy)? {
            Ok(CandidatePlan::Remove(StaleCandidate {
                name: name.into_bytes(),
                path,
                directory,
                directory_identity,
                lease: None,
                image: None,
            }))
        } else {
            Ok(CandidatePlan::Young)
        };
    }

    if !entries
        .iter()
        .any(|entry| entry.as_slice() == LEASE_NAME.as_bytes())
    {
        return Err(invalid_data(
            "non-empty stale publication has no lease entry",
        ));
    }
    let lease_file = open_at(
        &directory,
        OsStr::new(LEASE_NAME),
        OPEN_READ_WRITE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
        0,
    )?;
    let lease_metadata = lease_file.metadata()?;
    validate_regular_file(
        &lease_file,
        &lease_metadata,
        &[0o600],
        "stale publication lease",
    )?;
    if !descriptor_has_close_on_exec(lease_file.as_raw_fd())? {
        return Err(permission_denied(
            "stale publication lease is not close-on-exec",
        ));
    }
    let lease = match try_lock_file(lease_file)? {
        TryLock::Busy => return Ok(CandidatePlan::Live),
        TryLock::Acquired(lease) => lease,
    };
    revalidate_file_entry(
        &directory,
        OsStr::new(LEASE_NAME),
        lease.identity,
        OPEN_READ_WRITE,
        "stale publication lease",
    )?;

    for entry in &entries {
        if entry.as_slice() != LEASE_NAME.as_bytes() && entry.as_slice() != IMAGE_NAME.as_bytes() {
            return Err(invalid_data(
                "stale publication contains an unrecognized entry",
            ));
        }
    }
    let image = if entries
        .iter()
        .any(|entry| entry.as_slice() == IMAGE_NAME.as_bytes())
    {
        let image = open_at(
            &directory,
            OsStr::new(IMAGE_NAME),
            OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )?;
        let metadata = image.metadata()?;
        validate_regular_file(
            &image,
            &metadata,
            &[0o600, 0o500],
            "stale publication image",
        )?;
        if !descriptor_has_close_on_exec(image.as_raw_fd())? {
            return Err(permission_denied(
                "stale publication image is not close-on-exec",
            ));
        }
        let identity = InodeIdentity::from_metadata(&metadata);
        revalidate_file_entry(
            &directory,
            OsStr::new(IMAGE_NAME),
            identity,
            OPEN_READ_ONLY,
            "stale publication image",
        )?;
        Some((image, identity))
    } else {
        None
    };

    let image_metadata = image
        .as_ref()
        .map(|(image, _)| image.metadata())
        .transpose()?;
    let mut ages = vec![&directory_metadata, &lease_metadata];
    if let Some(metadata) = image_metadata.as_ref() {
        ages.push(metadata);
    }
    if !metadata_is_stale(&ages, policy)? {
        return Ok(CandidatePlan::Young);
    }

    Ok(CandidatePlan::Remove(StaleCandidate {
        name: name.into_bytes(),
        path,
        directory,
        directory_identity,
        lease: Some(lease),
        image,
    }))
}

fn remove_candidate<F>(
    root: &std::fs::File,
    mut candidate: StaleCandidate,
    hook: &mut F,
) -> io::Result<()>
where
    F: FnMut(SweepFaultStage, &Path) -> io::Result<()>,
{
    let name = OsStr::from_bytes(&candidate.name);
    if let Some((image, identity)) = candidate.image.as_ref() {
        let image_path = candidate.path.join(IMAGE_NAME);
        hook(SweepFaultStage::BeforeImageUnlink, &image_path)?;
        let path_file = revalidate_file_entry(
            &candidate.directory,
            OsStr::new(IMAGE_NAME),
            *identity,
            OPEN_READ_ONLY,
            "stale publication image",
        )?;
        drop(path_file);
        unlink_at(&candidate.directory, OsStr::new(IMAGE_NAME), 0)?;
        if image.metadata()?.nlink() != 0 {
            return Err(permission_denied(
                "stale publication image retained a link after unlink",
            ));
        }
        candidate.directory.sync_all()?;
    }
    candidate.image.take();

    if let Some(lease) = candidate.lease.as_ref() {
        let path_file = revalidate_file_entry(
            &candidate.directory,
            OsStr::new(LEASE_NAME),
            lease.identity,
            OPEN_READ_WRITE,
            "stale publication lease",
        )?;
        drop(path_file);
        unlink_at(&candidate.directory, OsStr::new(LEASE_NAME), 0)?;
        if lease.file.metadata()?.nlink() != 0 {
            return Err(permission_denied(
                "stale publication lease retained a link after unlink",
            ));
        }
        candidate.directory.sync_all()?;
    }

    if !directory_entry_names(&candidate.directory, 1)?.is_empty() {
        return Err(permission_denied(
            "stale publication directory was not empty before removal",
        ));
    }
    hook(SweepFaultStage::BeforeDirectoryRemove, &candidate.path)?;
    let path_directory = revalidate_file_entry(
        root,
        name,
        candidate.directory_identity,
        OPEN_READ_ONLY | OPEN_DIRECTORY,
        "stale publication directory",
    )?;
    drop(path_directory);
    unlink_at(root, name, AT_REMOVE_DIRECTORY)?;
    root.sync_all()?;
    Ok(())
}

fn sweep_stale_publications(root_path: &Path, policy: SweepPolicy) -> io::Result<SweepOutcome> {
    sweep_stale_publications_with_hook(root_path, policy, |_, _| Ok(()))
}

/// Recover publications whose owning parent died at least one day ago.
pub(super) fn sweep_production_publications(root_path: &Path) -> io::Result<()> {
    let policy = SweepPolicy {
        now: SystemTime::now(),
        min_age: Duration::from_secs(24 * 60 * 60),
        max_root_entries: MAX_ROOT_ENTRIES,
        max_candidates: MAX_CANDIDATES,
        max_candidate_entries: MAX_CANDIDATE_ENTRIES,
    };
    match sweep_stale_publications(root_path, policy)? {
        SweepOutcome::RootBusy => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "one-time worker publication root is busy",
        )),
        SweepOutcome::Completed(_) => Ok(()),
    }
}

/// Exercise the production one-day recovery policy after the hosted parent-death canary.
/// The controlled probe advances only the policy clock; it does not weaken classification,
/// lease locking, identity checks, or removal durability.
pub(super) fn sweep_hosted_parent_death_publications(root_path: &Path) -> io::Result<()> {
    let now = SystemTime::now()
        .checked_add(Duration::from_secs(24 * 60 * 60 + 1))
        .ok_or_else(|| invalid_data("hosted stale-sweep clock overflow"))?;
    let policy = SweepPolicy {
        now,
        min_age: Duration::from_secs(24 * 60 * 60),
        max_root_entries: MAX_ROOT_ENTRIES,
        max_candidates: MAX_CANDIDATES,
        max_candidate_entries: MAX_CANDIDATE_ENTRIES,
    };
    match sweep_stale_publications(root_path, policy)? {
        SweepOutcome::RootBusy => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "one-time worker publication root is busy",
        )),
        SweepOutcome::Completed(_) => Ok(()),
    }
}

fn sweep_stale_publications_with_hook<F>(
    root_path: &Path,
    policy: SweepPolicy,
    mut hook: F,
) -> io::Result<SweepOutcome>
where
    F: FnMut(SweepFaultStage, &Path) -> io::Result<()>,
{
    let policy = policy.validate()?;
    let supplied = std::fs::symlink_metadata(root_path)?;
    validate_private_directory(&supplied, "stale publication root")?;
    let root = open_directory(root_path)?;
    let opened = root.metadata()?;
    validate_private_directory(&opened, "stale publication root")?;
    if !InodeIdentity::from_metadata(&supplied).matches(&opened) {
        return Err(permission_denied("stale publication root was replaced"));
    }
    ensure_no_extended_acl(&root, "stale publication root")?;
    if !descriptor_has_close_on_exec(root.as_raw_fd())? {
        return Err(permission_denied(
            "stale publication root is not close-on-exec",
        ));
    }

    let root_lock = match try_lock_file(root)? {
        TryLock::Busy => return Ok(SweepOutcome::RootBusy),
        TryLock::Acquired(lock) => lock,
    };
    let revalidated_root = std::fs::symlink_metadata(root_path)?;
    if !root_lock.identity.matches(&revalidated_root) {
        return Err(permission_denied(
            "stale publication root was replaced after locking",
        ));
    }
    let root = &root_lock.file;

    let entries = directory_entry_names(root, policy.max_root_entries)?;
    let mut names = Vec::new();
    for entry in entries {
        if let Some(name) = canonical_candidate_name(&entry)? {
            if names.len() == policy.max_candidates {
                return Err(invalid_data("stale-sweep candidate bound exceeded"));
            }
            names.push(name);
        }
    }

    // Preflight every candidate while holding the root lock. No deletion occurs until all
    // malformed, over-bound, and replacement states have failed closed.
    let mut plans = Vec::with_capacity(names.len());
    for name in names {
        plans.push(preflight_candidate(root, root_path, name, policy)?);
    }

    let mut summary = SweepSummary::default();
    for plan in plans {
        match plan {
            CandidatePlan::Live => summary.skipped_live += 1,
            CandidatePlan::Young => summary.skipped_young += 1,
            CandidatePlan::Remove(candidate) => {
                remove_candidate(root, candidate, &mut hook)?;
                summary.removed += 1;
            }
        }
    }
    Ok(SweepOutcome::Completed(summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    #[derive(Clone, Copy)]
    enum FixtureState {
        Empty,
        LeaseOnly,
        Image(u32),
    }

    struct TestRoot {
        path: PathBuf,
    }

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mini-agent-stale-sweep-test-{}",
                uuid::Uuid::new_v4()
            ));
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(&path).unwrap();
            Self { path }
        }

        fn candidate(&self, state: FixtureState) -> PathBuf {
            let path = self
                .path
                .join(format!("{CANDIDATE_PREFIX}{}", uuid::Uuid::new_v4()));
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(&path).unwrap();
            match state {
                FixtureState::Empty => {}
                FixtureState::LeaseOnly => {
                    create_file(&path.join(LEASE_NAME), 0o600, b"");
                }
                FixtureState::Image(mode) => {
                    create_file(&path.join(LEASE_NAME), 0o600, b"");
                    create_file(&path.join(IMAGE_NAME), 0o600, b"partial worker image");
                    std::fs::set_permissions(
                        path.join(IMAGE_NAME),
                        std::fs::Permissions::from_mode(mode),
                    )
                    .unwrap();
                }
            }
            path
        }

        fn old_policy(&self) -> SweepPolicy {
            SweepPolicy::test(SystemTime::now() + Duration::from_secs(2 * 60 * 60))
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn create_file(path: &Path, mode: u32, contents: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .unwrap();
        file.write_all(contents).unwrap();
        file.sync_all().unwrap();
    }

    fn completed(outcome: SweepOutcome) -> SweepSummary {
        match outcome {
            SweepOutcome::Completed(summary) => summary,
            SweepOutcome::RootBusy => panic!("unexpected busy root"),
        }
    }

    #[test]
    fn stale_sweep_recognizes_all_crash_partial_states() {
        let root = TestRoot::new();
        let empty = root.candidate(FixtureState::Empty);
        let lease_only = root.candidate(FixtureState::LeaseOnly);
        let writable = root.candidate(FixtureState::Image(0o600));
        let sealed = root.candidate(FixtureState::Image(0o500));

        let summary = completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap());
        assert_eq!(summary.removed, 4);
        assert_eq!(summary.skipped_live, 0);
        assert_eq!(summary.skipped_young, 0);
        for path in [empty, lease_only, writable, sealed] {
            assert!(!path.exists());
        }
    }

    #[test]
    fn stale_sweep_skips_young_candidates() {
        let root = TestRoot::new();
        let candidate = root.candidate(FixtureState::Image(0o500));
        let policy = SweepPolicy::test(SystemTime::now());

        let summary = completed(sweep_stale_publications(&root.path, policy).unwrap());
        assert_eq!(summary.skipped_young, 1);
        assert!(candidate.exists());
    }

    #[test]
    fn stale_sweep_skips_a_live_lease() {
        let root = TestRoot::new();
        let candidate = root.candidate(FixtureState::Image(0o500));
        let directory = open_directory(&candidate).unwrap();
        let lease = open_at(
            &directory,
            OsStr::new(LEASE_NAME),
            OPEN_READ_WRITE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )
        .unwrap();
        let held = match try_lock_file(lease).unwrap() {
            TryLock::Acquired(lock) => lock,
            TryLock::Busy => panic!("fixture lease unexpectedly busy"),
        };

        let summary = completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap());
        assert_eq!(summary.skipped_live, 1);
        assert!(candidate.exists());
        drop(held);
    }

    #[test]
    fn stale_sweep_reports_a_busy_root_without_mutation() {
        let root = TestRoot::new();
        let candidate = root.candidate(FixtureState::LeaseOnly);
        let root_directory = open_directory(&root.path).unwrap();
        let held = match try_lock_file(root_directory).unwrap() {
            TryLock::Acquired(lock) => lock,
            TryLock::Busy => panic!("fixture root unexpectedly busy"),
        };

        assert_eq!(
            sweep_stale_publications(&root.path, root.old_policy()).unwrap(),
            SweepOutcome::RootBusy
        );
        assert!(candidate.exists());
        drop(held);
    }

    #[test]
    fn stale_sweep_preflight_rejects_malformed_and_extra_entries_before_deletion() {
        let root = TestRoot::new();
        let valid = root.candidate(FixtureState::LeaseOnly);
        let malformed = root.path.join(format!("{CANDIDATE_PREFIX}not-a-uuid"));
        std::fs::create_dir(&malformed).unwrap();
        assert!(sweep_stale_publications(&root.path, root.old_policy()).is_err());
        assert!(valid.exists());
        std::fs::remove_dir(malformed).unwrap();

        let invalid = root.candidate(FixtureState::LeaseOnly);
        create_file(&invalid.join("unexpected"), 0o600, b"unexpected");
        assert!(sweep_stale_publications(&root.path, root.old_policy()).is_err());
        assert!(valid.exists());
    }

    #[test]
    fn stale_sweep_rejects_symlinks_hard_links_and_wrong_modes() {
        let root = TestRoot::new();
        let valid = root.candidate(FixtureState::LeaseOnly);

        let external = root.path.join("external");
        std::fs::create_dir(&external).unwrap();
        let symlink_candidate = root
            .path
            .join(format!("{CANDIDATE_PREFIX}{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink(&external, &symlink_candidate).unwrap();
        assert!(sweep_stale_publications(&root.path, root.old_policy()).is_err());
        assert!(valid.exists());
        std::fs::remove_file(symlink_candidate).unwrap();

        let hard_linked = root.candidate(FixtureState::Image(0o500));
        std::fs::hard_link(
            hard_linked.join(IMAGE_NAME),
            root.path.join("image-hard-link"),
        )
        .unwrap();
        assert!(sweep_stale_publications(&root.path, root.old_policy()).is_err());
        assert!(valid.exists());
        std::fs::remove_file(root.path.join("image-hard-link")).unwrap();

        let wrong_mode = root.candidate(FixtureState::Image(0o500));
        std::fs::set_permissions(
            wrong_mode.join(IMAGE_NAME),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(sweep_stale_publications(&root.path, root.old_policy()).is_err());
        assert!(valid.exists());

        let special_mode_root = TestRoot::new();
        let special_mode = special_mode_root.candidate(FixtureState::Image(0o500));
        std::fs::set_permissions(
            special_mode.join(IMAGE_NAME),
            std::fs::Permissions::from_mode(0o1500),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(special_mode.join(IMAGE_NAME))
                .unwrap()
                .mode()
                & 0o7777,
            0o1500
        );
        assert!(
            sweep_stale_publications(&special_mode_root.path, special_mode_root.old_policy())
                .is_err()
        );
    }

    #[test]
    fn stale_sweep_bounds_abort_before_any_deletion() {
        let root = TestRoot::new();
        let first = root.candidate(FixtureState::Empty);
        let second = root.candidate(FixtureState::Empty);
        let mut policy = root.old_policy();
        policy.max_candidates = 1;
        assert!(sweep_stale_publications(&root.path, policy).is_err());
        assert!(first.exists() && second.exists());

        let mut policy = root.old_policy();
        policy.max_root_entries = 2;
        policy.max_candidates = 1;
        create_file(&root.path.join("unrelated"), 0o600, b"entry");
        assert!(sweep_stale_publications(&root.path, policy).is_err());
        assert!(first.exists() && second.exists());

        let bounded = TestRoot::new();
        let candidate = bounded.candidate(FixtureState::Image(0o500));
        create_file(&candidate.join("third"), 0o600, b"entry");
        assert!(sweep_stale_publications(&bounded.path, bounded.old_policy()).is_err());
        assert!(candidate.exists());
    }

    #[test]
    fn stale_sweep_preserves_an_image_replacement() {
        let root = TestRoot::new();
        let candidate = root.candidate(FixtureState::Image(0o500));
        let mut injected = false;
        let result = sweep_stale_publications_with_hook(
            &root.path,
            root.old_policy(),
            |stage, image_path| {
                if stage == SweepFaultStage::BeforeImageUnlink && !injected {
                    injected = true;
                    std::fs::rename(image_path, image_path.with_file_name("original-image"))?;
                    create_file(image_path, 0o500, b"replacement");
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(candidate.join(IMAGE_NAME)).unwrap(),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(candidate.join("original-image")).unwrap(),
            b"partial worker image"
        );
    }

    #[test]
    fn stale_sweep_preserves_a_directory_replacement() {
        let root = TestRoot::new();
        let candidate = root.candidate(FixtureState::Empty);
        let relocated = root.path.join("relocated-original");
        let result = sweep_stale_publications_with_hook(
            &root.path,
            root.old_policy(),
            |stage, directory_path| {
                if stage == SweepFaultStage::BeforeDirectoryRemove {
                    std::fs::rename(directory_path, &relocated)?;
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700).create(directory_path)?;
                    create_file(&directory_path.join("marker"), 0o600, b"replacement");
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(candidate.join("marker")).unwrap(),
            b"replacement"
        );
        assert!(relocated.exists());
    }

    #[test]
    fn stale_sweep_is_idempotent_and_reclassifies_crash_states() {
        let root = TestRoot::new();
        root.candidate(FixtureState::Image(0o500));
        assert_eq!(
            completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap()).removed,
            1
        );
        assert_eq!(
            completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap()).removed,
            0
        );

        let lease_only = root.candidate(FixtureState::Image(0o600));
        std::fs::remove_file(lease_only.join(IMAGE_NAME)).unwrap();
        assert_eq!(
            completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap()).removed,
            1
        );

        let empty = root.candidate(FixtureState::LeaseOnly);
        std::fs::remove_file(empty.join(LEASE_NAME)).unwrap();
        assert_eq!(
            completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap()).removed,
            1
        );
    }

    #[test]
    fn stale_sweep_owned_descriptors_are_close_on_exec() {
        let root = TestRoot::new();
        let candidate = root.candidate(FixtureState::Image(0o500));
        let root_directory = open_directory(&root.path).unwrap();
        let root_lock = match try_lock_file(root_directory).unwrap() {
            TryLock::Acquired(lock) => lock,
            TryLock::Busy => panic!("fixture root unexpectedly busy"),
        };
        let directory = open_directory(&candidate).unwrap();
        let lease = open_at(
            &directory,
            OsStr::new(LEASE_NAME),
            OPEN_READ_WRITE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )
        .unwrap();
        let image = open_at(
            &directory,
            OsStr::new(IMAGE_NAME),
            OPEN_READ_ONLY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )
        .unwrap();

        for descriptor in [
            root_lock.file.as_raw_fd(),
            directory.as_raw_fd(),
            lease.as_raw_fd(),
            image.as_raw_fd(),
        ] {
            assert!(descriptor_has_close_on_exec(descriptor).unwrap());
        }
    }

    #[test]
    fn flock_result_classification_retries_interrupts_and_preserves_busy() {
        assert_eq!(
            classify_flock_result(-1, 4).unwrap(),
            FlockDisposition::Retry
        );
        assert_eq!(
            classify_flock_result(-1, WOULD_BLOCK_ERRNO).unwrap(),
            FlockDisposition::Busy
        );
        assert_eq!(
            classify_flock_result(0, 0).unwrap(),
            FlockDisposition::Acquired
        );
        assert!(classify_flock_result(-1, 5).is_err());
        assert_eq!(
            bounded_flock_disposition(-1, INTERRUPTED_ERRNO, MAX_CONSECUTIVE_FLOCK_INTERRUPTS - 1,)
                .unwrap(),
            FlockDisposition::Retry
        );
        let exhausted =
            bounded_flock_disposition(-1, INTERRUPTED_ERRNO, MAX_CONSECUTIVE_FLOCK_INTERRUPTS)
                .unwrap_err();
        assert_eq!(exhausted.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn stale_sweep_requires_exact_root_and_candidate_directory_modes() {
        let wrong_root = TestRoot::new();
        std::fs::set_permissions(&wrong_root.path, std::fs::Permissions::from_mode(0o1700))
            .unwrap();
        assert!(sweep_stale_publications(&wrong_root.path, wrong_root.old_policy()).is_err());

        let wrong_candidate = TestRoot::new();
        let candidate = wrong_candidate.candidate(FixtureState::Empty);
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o1700)).unwrap();
        assert!(
            sweep_stale_publications(&wrong_candidate.path, wrong_candidate.old_policy()).is_err()
        );
        assert!(candidate.exists());
    }

    #[test]
    fn stale_sweep_policy_rejects_requests_above_structural_bounds() {
        let now = SystemTime::now();
        let mut policy = SweepPolicy::test(now);
        policy.max_root_entries = 1_025;
        assert!(policy.validate().is_err());

        let mut policy = SweepPolicy::test(now);
        policy.max_candidates = 65;
        assert!(policy.validate().is_err());

        let mut policy = SweepPolicy::test(now);
        policy.max_candidate_entries = 3;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn separately_opened_root_directory_fds_contend_without_a_lock_artifact() {
        let root = TestRoot::new();
        let first = open_directory(&root.path).unwrap();
        let second = open_directory(&root.path).unwrap();
        let held = match try_lock_file(first).unwrap() {
            TryLock::Acquired(lock) => lock,
            TryLock::Busy => panic!("first root directory descriptor unexpectedly busy"),
        };
        assert!(matches!(try_lock_file(second).unwrap(), TryLock::Busy));
        assert_eq!(std::fs::read_dir(&root.path).unwrap().count(), 0);

        drop(held);
        let third = open_directory(&root.path).unwrap();
        let reacquired = match try_lock_file(third).unwrap() {
            TryLock::Acquired(lock) => lock,
            TryLock::Busy => panic!("released root directory lock remained busy"),
        };
        drop(reacquired);
        assert_eq!(
            completed(sweep_stale_publications(&root.path, root.old_policy()).unwrap()),
            SweepSummary::default()
        );
        assert_eq!(std::fs::read_dir(&root.path).unwrap().count(), 0);
    }
}
