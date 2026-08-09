use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

mod private;

#[cfg(all(test, unix))]
pub(crate) use private::atomic_write_with_failure as private_atomic_write_with_failure_sync;
#[cfg(all(test, windows))]
pub(crate) use private::dacl_sddl as private_dacl_sddl;
pub(crate) use private::{
    atomic_create as private_atomic_create_sync, atomic_write as private_atomic_write_sync,
    ensure_directory as ensure_private_directory, open_existing as open_private_file,
};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) unsafe fn linux_renameat2(
    old_directory: libc::c_int,
    old_path: *const libc::c_char,
    new_directory: libc::c_int,
    new_path: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_int {
    // Call the kernel directly: glibc exports renameat2, but musl does not
    // provide that symbol even though Linux supports the syscall.
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_directory,
            old_path,
            new_directory,
            new_path,
            flags,
        ) as libc::c_int
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WindowsFileIdentity {
    pub(crate) volume_serial_number: u64,
    pub(crate) file_id: [u8; 16],
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MacOsFileIdentity {
    pub(crate) volume_uuid: [u8; 16],
    pub(crate) file_id: u64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MacOsAttrList {
    bitmap_count: u16,
    reserved: u16,
    common_attributes: u32,
    volume_attributes: u32,
    directory_attributes: u32,
    file_attributes: u32,
    fork_attributes: u32,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn macos_common_attribute<T, const N: usize>(
    file: &T,
    common_attributes: u32,
) -> std::io::Result<[u8; N]>
where
    T: std::os::fd::AsRawFd,
{
    use std::ffi::c_void;

    unsafe extern "C" {
        fn fgetattrlist(
            descriptor: std::ffi::c_int,
            attributes: *const c_void,
            buffer: *mut c_void,
            buffer_size: usize,
            options: std::ffi::c_uint,
        ) -> std::ffi::c_int;
    }

    const ATTRIBUTE_BITMAP_COUNT: u16 = 5;
    let attributes = MacOsAttrList {
        bitmap_count: ATTRIBUTE_BITMAP_COUNT,
        reserved: 0,
        common_attributes,
        volume_attributes: 0,
        directory_attributes: 0,
        file_attributes: 0,
        fork_attributes: 0,
    };
    let mut buffer = vec![0u8; size_of::<u32>() + N];
    // SAFETY: `file` owns a live descriptor, both pointers reference initialized
    // storage of the supplied sizes, and `fgetattrlist` does not retain them.
    if unsafe {
        fgetattrlist(
            file.as_raw_fd(),
            (&attributes as *const MacOsAttrList).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let returned_size = u32::from_ne_bytes(buffer[..size_of::<u32>()].try_into().unwrap());
    if returned_size as usize != buffer.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "macOS returned an invalid file-identity attribute size",
        ));
    }
    Ok(buffer[size_of::<u32>()..].try_into().unwrap())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn macos_volume_uuid<T>(file: &T) -> std::io::Result<[u8; 16]>
where
    T: std::os::fd::AsRawFd,
{
    use std::ffi::{c_char, c_void};

    unsafe extern "C" {
        fn getattrlist(
            path: *const c_char,
            attributes: *const c_void,
            buffer: *mut c_void,
            buffer_size: usize,
            options: std::ffi::c_uint,
        ) -> std::ffi::c_int;
    }

    const ATTRIBUTE_BITMAP_COUNT: u16 = 5;
    const ATTR_VOL_UUID: u32 = 0x0004_0000;
    const ATTR_VOL_INFO: u32 = 0x8000_0000;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: the descriptor remains live and `filesystem` points to writable
    // storage of the exact structure expected by fstatfs.
    if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful fstatfs initialized the complete structure, including
    // its NUL-terminated mounted-on path.
    let filesystem = unsafe { filesystem.assume_init() };
    if !filesystem.f_mntonname.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "macOS returned an unterminated volume mount path",
        ));
    }
    let attributes = MacOsAttrList {
        bitmap_count: ATTRIBUTE_BITMAP_COUNT,
        reserved: 0,
        common_attributes: 0,
        volume_attributes: ATTR_VOL_INFO | ATTR_VOL_UUID,
        directory_attributes: 0,
        file_attributes: 0,
        fork_attributes: 0,
    };
    let mut buffer = [0u8; size_of::<u32>() + 16];
    // Darwin requires volume attributes to be requested against the mounted
    // volume's root. The mount path comes from the already-open descriptor,
    // and the returned UUID is paired with that descriptor's file ID below.
    // SAFETY: both buffers remain live for the synchronous call and the mount
    // path is the NUL-terminated array returned by fstatfs.
    if unsafe {
        getattrlist(
            filesystem.f_mntonname.as_ptr(),
            (&attributes as *const MacOsAttrList).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let returned_size = u32::from_ne_bytes(buffer[..size_of::<u32>()].try_into().unwrap());
    if returned_size as usize != buffer.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "macOS returned an invalid volume-identity attribute size",
        ));
    }
    Ok(buffer[size_of::<u32>()..].try_into().unwrap())
}

#[cfg(target_os = "macos")]
pub(crate) fn macos_file_identity<T>(file: &T) -> std::io::Result<MacOsFileIdentity>
where
    T: std::os::fd::AsRawFd,
{
    const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    let volume_uuid = macos_volume_uuid(file)?;
    let file_id = u64::from_ne_bytes(macos_common_attribute(file, ATTR_CMN_FILEID)?);
    if volume_uuid == [0; 16] || file_id == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the filesystem does not expose a stable volume and file identity",
        ));
    }
    Ok(MacOsFileIdentity {
        volume_uuid,
        file_id,
    })
}

#[cfg(windows)]
fn validated_windows_file_identity(
    volume_serial_number: u64,
    file_id: [u8; 16],
) -> std::io::Result<WindowsFileIdentity> {
    if file_id == [0; 16] {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the filesystem does not expose a stable 128-bit file identity",
        ));
    }
    Ok(WindowsFileIdentity {
        volume_serial_number,
        file_id,
    })
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn windows_file_identity<T>(file: &T) -> std::io::Result<WindowsFileIdentity>
where
    T: std::os::windows::io::AsRawHandle,
{
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_ID_INFO::default();
    // SAFETY: `file` owns a live Windows handle for the duration of the call and
    // `information` points to initialized, writable storage of the required type.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    validated_windows_file_identity(
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    )
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn windows_file_link_count<T>(file: &T) -> std::io::Result<u32>
where
    T: std::os::windows::io::AsRawHandle,
{
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_STANDARD_INFO::default();
    // SAFETY: `file` owns a live Windows handle for the duration of the call and
    // `information` points to initialized, writable storage of the required type.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileStandardInfo,
            (&mut information as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(information.NumberOfLinks)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial_number: u64,
        file_id: [u8; 16],
    },
}

#[derive(Debug, Clone)]
pub(crate) struct CheckedMetadata {
    metadata: std::fs::Metadata,
    identity: FileIdentity,
    // Keeping this handle alive prevents the approved object's identity from
    // being recycled while a permission prompt or asynchronous operation is
    // outstanding.
    handle: Arc<std::fs::File>,
}

impl std::ops::Deref for CheckedMetadata {
    type Target = std::fs::Metadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

fn checked_owned_file(
    file: std::fs::File,
    metadata: std::fs::Metadata,
) -> std::io::Result<CheckedMetadata> {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;

        FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    };
    #[cfg(windows)]
    let identity = windows_file_identity(&file)?;
    #[cfg(windows)]
    let identity = FileIdentity::Windows {
        volume_serial_number: identity.volume_serial_number,
        file_id: identity.file_id,
    };
    #[cfg(any(unix, windows))]
    {
        Ok(CheckedMetadata {
            metadata,
            identity,
            handle: Arc::new(file),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, metadata);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "stable file identity is unavailable on this platform",
        ))
    }
}

#[allow(unsafe_code)]
pub(crate) fn checked_path_metadata(path: &Path) -> std::io::Result<CheckedMetadata> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;

        let path_bytes = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
        })?;
        #[cfg(target_os = "linux")]
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        #[cfg(target_os = "macos")]
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        // SAFETY: `path_bytes` is NUL-terminated and a successful descriptor
        // is immediately transferred into an owned `File`.
        let descriptor = unsafe { libc::open(path_bytes.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `open` returned a new owned descriptor.
        let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
        let metadata = file.metadata()?;
        checked_owned_file(file, metadata)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let metadata = file.metadata()?;
        checked_owned_file(file, metadata)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "stable file identity is unavailable on this platform",
        ))
    }
}

pub(crate) fn checked_file_metadata(file: &std::fs::File) -> std::io::Result<CheckedMetadata> {
    let file = file.try_clone()?;
    let metadata = file.metadata()?;
    checked_owned_file(file, metadata)
}

pub(crate) async fn checked_tokio_file_metadata(
    file: &tokio::fs::File,
) -> std::io::Result<CheckedMetadata> {
    let metadata = file.metadata().await?;
    let file = file.try_clone().await?.into_std().await;
    #[cfg(windows)]
    {
        return tokio::task::spawn_blocking(move || checked_owned_file(file, metadata))
            .await
            .map_err(std::io::Error::other)?;
    }
    #[cfg(not(windows))]
    {
        checked_owned_file(file, metadata)
    }
}

#[derive(Debug)]
struct PathChangedError(PathBuf);

impl std::fmt::Display for PathChangedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Path changed after permission check: {}",
            self.0.display()
        )
    }
}

impl std::error::Error for PathChangedError {}

fn path_changed_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        PathChangedError(path.to_path_buf()),
    )
}

pub(crate) fn is_path_changed_error(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<PathChangedError>().is_some())
}

#[cfg(unix)]
pub(crate) fn is_symlink_loop_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(not(unix))]
pub(crate) fn is_symlink_loop_error(_error: &std::io::Error) -> bool {
    false
}

fn non_regular_file_error(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("Path is not a regular file: {}", path.display()),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AtomicWriteFailure {
    None,
    #[cfg(test)]
    Write,
    #[cfg(test)]
    Rename,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AtomicWriteMode {
    Replace,
    CreateNew,
}

/// Cooperative stop signal for descriptor-relative atomic writes. Cancellation and the final
/// publication-start decision share one atomic state transition: cancellation which wins prevents
/// rename, while an already-approved syscall may finish under an ambiguous caller result.
const ATOMIC_WRITE_ACTIVE: u8 = 0;
const ATOMIC_WRITE_CANCELLED: u8 = 1;
const ATOMIC_WRITE_PUBLISHING: u8 = 2;
const ATOMIC_WRITE_FINISHED: u8 = 3;

#[derive(Debug, Default)]
struct AtomicWriteCancellationState {
    publication: AtomicU8,
    #[cfg(test)]
    publication_probe: Option<AtomicWritePublicationProbe>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AtomicWriteCancellation(Arc<AtomicWriteCancellationState>);

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct AtomicWritePublicationProbe {
    reached: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
    point: AtomicWriteProbePoint,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AtomicWriteProbePoint {
    BeforeTempCreation,
    BeforeDecision,
    AfterDecision,
}

#[cfg(test)]
impl AtomicWritePublicationProbe {
    pub(crate) fn wait_until_reached(&self) {
        self.reached.wait();
    }

    pub(crate) fn resume(&self) {
        self.resume.wait();
    }
}

impl AtomicWriteCancellation {
    pub(crate) fn cancel(&self) {
        let _ = self.0.publication.compare_exchange(
            ATOMIC_WRITE_ACTIVE,
            ATOMIC_WRITE_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn check(&self) -> std::io::Result<()> {
        if self.0.publication.load(Ordering::Acquire) == ATOMIC_WRITE_CANCELLED {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "atomic write cancelled before publication",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn probe_before_temp_creation(&self) {
        if let Some(probe) = &self.0.publication_probe
            && probe.point == AtomicWriteProbePoint::BeforeTempCreation
        {
            probe.reached.wait();
            probe.resume.wait();
        }
    }

    fn publish<T>(&self, operation: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        #[cfg(test)]
        if let Some(probe) = &self.0.publication_probe
            && probe.point == AtomicWriteProbePoint::BeforeDecision
        {
            probe.reached.wait();
            probe.resume.wait();
        }
        self.0
            .publication
            .compare_exchange(
                ATOMIC_WRITE_ACTIVE,
                ATOMIC_WRITE_PUBLISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "atomic write cancelled before publication",
                )
            })?;
        #[cfg(test)]
        if let Some(probe) = &self.0.publication_probe
            && probe.point == AtomicWriteProbePoint::AfterDecision
        {
            probe.reached.wait();
            probe.resume.wait();
        }
        // CAS is the publication-start decision. Cancellation never waits for OS I/O: if it wins,
        // this operation cannot begin; if publication wins, the already-approved syscall may
        // finish and the caller truthfully reports OutcomeUnknown.
        let result = operation();
        self.0
            .publication
            .store(ATOMIC_WRITE_FINISHED, Ordering::Release);
        result
    }

    #[cfg(test)]
    pub(crate) fn with_temp_creation_probe_for_test() -> (Self, AtomicWritePublicationProbe) {
        let probe = AtomicWritePublicationProbe {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
            point: AtomicWriteProbePoint::BeforeTempCreation,
        };
        let cancellation = Self(Arc::new(AtomicWriteCancellationState {
            publication: AtomicU8::new(ATOMIC_WRITE_ACTIVE),
            publication_probe: Some(probe.clone()),
        }));
        (cancellation, probe)
    }

    #[cfg(test)]
    pub(crate) fn with_publication_probe_for_test() -> (Self, AtomicWritePublicationProbe) {
        let probe = AtomicWritePublicationProbe {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
            point: AtomicWriteProbePoint::BeforeDecision,
        };
        let cancellation = Self(Arc::new(AtomicWriteCancellationState {
            publication: AtomicU8::new(ATOMIC_WRITE_ACTIVE),
            publication_probe: Some(probe.clone()),
        }));
        (cancellation, probe)
    }

    #[cfg(test)]
    pub(crate) fn with_blocking_publication_probe_for_test() -> (Self, AtomicWritePublicationProbe)
    {
        let probe = AtomicWritePublicationProbe {
            reached: Arc::new(std::sync::Barrier::new(2)),
            resume: Arc::new(std::sync::Barrier::new(2)),
            point: AtomicWriteProbePoint::AfterDecision,
        };
        let cancellation = Self(Arc::new(AtomicWriteCancellationState {
            publication: AtomicU8::new(ATOMIC_WRITE_ACTIVE),
            publication_probe: Some(probe.clone()),
        }));
        (cancellation, probe)
    }
}

pub(crate) async fn stable_path_metadata(path: &Path) -> std::io::Result<CheckedMetadata> {
    let path = path.to_path_buf();
    let checked_path = path.clone();
    let metadata = tokio::task::spawn_blocking(move || checked_path_metadata(&checked_path))
        .await
        .map_err(std::io::Error::other)?
        .map_err(|error| {
            if is_symlink_loop_error(&error) {
                path_changed_error(&path)
            } else {
                error
            }
        })?;
    if metadata.file_type().is_symlink() {
        return Err(path_changed_error(&path));
    }
    Ok(metadata)
}

pub(crate) fn ensure_same_file(
    path: &Path,
    checked: &CheckedMetadata,
    current: &CheckedMetadata,
) -> std::io::Result<()> {
    if checked.identity == current.identity {
        Ok(())
    } else {
        Err(path_changed_error(path))
    }
}

/// Open a regular path after permission approval and verify that it was not
/// replaced while the open was in progress.
pub(crate) async fn open_stable_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    let before = stable_path_metadata(path).await?;
    // Opening a FIFO can block forever. Reject every non-regular node from
    // metadata before attempting the open, then verify the descriptor too so
    // a replacement raced between those operations cannot be consumed.
    if !before.is_file() {
        return Err(non_regular_file_error(path));
    }
    #[cfg(unix)]
    let file = {
        let mut options = tokio::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        options.open(path).await?
    };
    #[cfg(not(unix))]
    let file = tokio::fs::File::open(path).await?;
    let opened = checked_tokio_file_metadata(&file).await?;
    if !opened.is_file() {
        return Err(non_regular_file_error(path));
    }
    let after = stable_path_metadata(path).await?;
    if !after.is_file() {
        return Err(non_regular_file_error(path));
    }
    ensure_same_file(path, &before, &opened)?;
    ensure_same_file(path, &opened, &after)?;
    Ok(file)
}

/// Atomically write `contents` to `path`.
///
/// On Linux and macOS, every destination-directory operation is relative to an
/// identity-verified open directory descriptor. Descendants use `O_NOFOLLOW`;
/// the final target must be absent or a regular file, so symlinks and
/// directories are rejected. The temporary file is created exclusively with a
/// random name and mode `0600`, then renamed over the target in the same
/// directory.
///
/// The helper intentionally flushes userspace buffers but does not `fsync`;
/// this preserves the existing atomicity (not power-loss durability) contract.
///
/// Other platforms use a conservative no-symlink fallback. Because their
/// standard library APIs cannot provide equivalent descriptor-relative replace
/// semantics, replacing an existing file fails closed with `Unsupported`.
#[cfg(test)]
pub async fn atomic_write(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    atomic_write_resolved(path, contents).await
}

/// Atomically write to a path that the caller has already resolved and
/// permission-checked.
///
/// The parent directory is treated as the approved root. Path containment is
/// component-aware, so a sibling such as `/safe-root-evil` never satisfies an
/// approval for `/safe-root`.
#[cfg(test)]
pub(crate) async fn atomic_write_resolved(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    atomic_write_resolved_inner(
        path.as_ref(),
        contents.as_ref(),
        None,
        AtomicWriteCancellation::default(),
    )
    .await
}

/// Variant used when a permission-gated caller captured the approved parent
/// before waiting for user input. The opened directory descriptor must still
/// identify that exact directory or the write fails.
pub(crate) async fn atomic_write_resolved_checked(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    approved_parent: CheckedMetadata,
) -> std::io::Result<()> {
    atomic_write_resolved_inner(
        path.as_ref(),
        contents.as_ref(),
        Some(approved_parent),
        AtomicWriteCancellation::default(),
    )
    .await
}

pub(crate) async fn atomic_write_resolved_checked_cancellable(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    approved_parent: CheckedMetadata,
    cancellation: AtomicWriteCancellation,
) -> std::io::Result<()> {
    atomic_write_resolved_inner(
        path.as_ref(),
        contents.as_ref(),
        Some(approved_parent),
        cancellation,
    )
    .await
}

async fn atomic_write_resolved_inner(
    path: &Path,
    contents: &[u8],
    approved_parent: Option<CheckedMetadata>,
    cancellation: AtomicWriteCancellation,
) -> std::io::Result<()> {
    tracing::debug!(
        "atomic_write: {} ({} bytes)",
        path.display(),
        contents.len(),
    );
    let root = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::new(),
    };
    let path = path.to_path_buf();
    let contents = contents.to_vec();
    crate::agent::runner::spawn_blocking_scoped(move || {
        atomic_write_within_sync_impl(
            &root,
            &path,
            &contents,
            approved_parent.as_ref(),
            AtomicWriteMode::Replace,
            AtomicWriteFailure::None,
            &cancellation,
        )
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Create a new file atomically after permission approval. If any entry appears
/// at the final name before the descriptor-relative rename, the operation fails
/// instead of replacing it.
pub(crate) async fn atomic_create_resolved_checked(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    approved_parent: CheckedMetadata,
) -> std::io::Result<()> {
    atomic_create_resolved_checked_cancellable(
        path,
        contents,
        approved_parent,
        AtomicWriteCancellation::default(),
    )
    .await
}

pub(crate) async fn atomic_create_resolved_checked_cancellable(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    approved_parent: CheckedMetadata,
    cancellation: AtomicWriteCancellation,
) -> std::io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let contents = contents.as_ref().to_vec();
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    crate::agent::runner::spawn_blocking_scoped(move || {
        atomic_write_within_sync_impl(
            &root,
            &path,
            &contents,
            Some(&approved_parent),
            AtomicWriteMode::CreateNew,
            AtomicWriteFailure::None,
            &cancellation,
        )
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Synchronous entry point for config/session/memory persistence.
pub(crate) fn atomic_write_sync(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    atomic_write_within_sync_impl(
        root,
        path,
        contents,
        None,
        AtomicWriteMode::Replace,
        AtomicWriteFailure::None,
        &AtomicWriteCancellation::default(),
    )
}

/// Synchronous create-only variant used for randomly named tool output.
pub(crate) fn atomic_create_sync(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    atomic_write_within_sync_impl(
        root,
        path,
        contents,
        None,
        AtomicWriteMode::CreateNew,
        AtomicWriteFailure::None,
        &AtomicWriteCancellation::default(),
    )
}

#[cfg(test)]
pub(crate) fn atomic_write_within_sync(
    approved_root: &Path,
    path: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    atomic_write_within_sync_impl(
        approved_root,
        path,
        contents,
        None,
        AtomicWriteMode::Replace,
        AtomicWriteFailure::None,
        &AtomicWriteCancellation::default(),
    )
}

#[cfg(test)]
pub(crate) fn atomic_write_with_failure_sync(
    approved_root: &Path,
    path: &Path,
    contents: &[u8],
    fail_rename: bool,
) -> std::io::Result<()> {
    atomic_write_within_sync_impl(
        approved_root,
        path,
        contents,
        None,
        AtomicWriteMode::Replace,
        if fail_rename {
            AtomicWriteFailure::Rename
        } else {
            AtomicWriteFailure::Write
        },
        &AtomicWriteCancellation::default(),
    )
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

fn relative_target<'a>(
    approved_root: &Path,
    path: &'a Path,
) -> std::io::Result<(PathBuf, &'a std::ffi::OsStr)> {
    use std::path::Component;

    let relative = path.strip_prefix(approved_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is outside approved root {}",
                path.display(),
                approved_root.display()
            ),
        )
    })?;
    let leaf = relative.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic write target must name a file",
        )
    })?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic write target contains an invalid path component",
        ));
    }
    Ok((
        relative
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf(),
        leaf,
    ))
}

fn atomic_write_within_sync_impl(
    approved_root: &Path,
    path: &Path,
    contents: &[u8],
    approved_parent: Option<&CheckedMetadata>,
    mode: AtomicWriteMode,
    failure: AtomicWriteFailure,
    cancellation: &AtomicWriteCancellation,
) -> std::io::Result<()> {
    cancellation.check()?;
    let approved_root = absolute_lexical(approved_root)?;
    let path = absolute_lexical(path)?;
    let (relative_parent, leaf) = relative_target(&approved_root, &path)?;
    let approved_root_metadata = checked_path_metadata(&approved_root)?;
    if approved_root_metadata.file_type().is_symlink() || !approved_root_metadata.is_dir() {
        return Err(path_changed_error(&approved_root));
    }
    let canonical_root = std::fs::canonicalize(&approved_root)?;
    ensure_same_file(
        &approved_root,
        &approved_root_metadata,
        &checked_path_metadata(&canonical_root)?,
    )?;

    atomic_write_platform(
        &canonical_root,
        &relative_parent,
        leaf,
        contents,
        &approved_root_metadata,
        approved_parent,
        mode,
        failure,
        cancellation,
    )
}

#[cfg(target_os = "linux")]
const OPEN_DIRECTORY: std::os::raw::c_int = 0x1_0000;
#[cfg(target_os = "linux")]
const OPEN_NOFOLLOW: std::os::raw::c_int = 0x2_0000;
#[cfg(target_os = "linux")]
const OPEN_CLOEXEC: std::os::raw::c_int = 0x8_0000;
#[cfg(target_os = "linux")]
const OPEN_CREATE: std::os::raw::c_int = 0x40;
#[cfg(target_os = "linux")]
const OPEN_EXCLUSIVE: std::os::raw::c_int = 0x80;

#[cfg(target_os = "macos")]
const OPEN_DIRECTORY: std::os::raw::c_int = 0x10_0000;
#[cfg(target_os = "macos")]
const OPEN_NOFOLLOW: std::os::raw::c_int = 0x100;
#[cfg(target_os = "macos")]
const OPEN_CLOEXEC: std::os::raw::c_int = 0x100_0000;
#[cfg(target_os = "macos")]
const OPEN_CREATE: std::os::raw::c_int = 0x200;
#[cfg(target_os = "macos")]
const OPEN_EXCLUSIVE: std::os::raw::c_int = 0x800;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn atomic_write_platform(
    canonical_root: &Path,
    relative_parent: &Path,
    leaf: &std::ffi::OsStr,
    contents: &[u8],
    approved_root: &CheckedMetadata,
    approved_parent: Option<&CheckedMetadata>,
    mode: AtomicWriteMode,
    failure: AtomicWriteFailure,
    cancellation: &AtomicWriteCancellation,
) -> std::io::Result<()> {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn openat(directory: c_int, path: *const c_char, flags: c_int, mode: c_uint) -> c_int;
        fn renameat(
            old_directory: c_int,
            old_path: *const c_char,
            new_directory: c_int,
            new_path: *const c_char,
        ) -> c_int;
        #[cfg(target_os = "macos")]
        fn renameatx_np(
            old_directory: c_int,
            old_path: *const c_char,
            new_directory: c_int,
            new_path: *const c_char,
            flags: c_uint,
        ) -> c_int;
        fn unlinkat(directory: c_int, path: *const c_char, flags: c_int) -> c_int;
    }

    let _ = failure;

    fn c_name(name: &OsStr) -> std::io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path component contains NUL",
            )
        })
    }

    fn open_at(
        directory: &File,
        name: &OsStr,
        flags: c_int,
        mode: c_uint,
    ) -> std::io::Result<File> {
        let name = c_name(name)?;
        // SAFETY: `name` is NUL-terminated for this call and `directory` owns a
        // valid descriptor. A successful descriptor is transferred to `File`.
        let descriptor = unsafe { openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
        if descriptor < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: `openat` returned a new owned descriptor.
            Ok(unsafe { File::from_raw_fd(descriptor) })
        }
    }

    fn open_directory_at(directory: &File, name: &OsStr) -> std::io::Result<File> {
        open_at(
            directory,
            name,
            OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )
    }

    fn open_absolute_directory(path: &Path) -> std::io::Result<File> {
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::OpenOptionsExt;

            // APFS firmlinks (notably /Users) can report a different st_dev
            // when walked one component at a time from `/`. Opening the
            // canonical path directly avoids that alias; the caller verifies
            // this live descriptor against the retained approved identity
            // before any relative operation.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(OPEN_DIRECTORY | OPEN_NOFOLLOW | OPEN_CLOEXEC)
                .open(path)
        }

        #[cfg(target_os = "linux")]
        {
            use std::path::Component;

            let mut directory = File::open(Path::new("/"))?;
            for component in path.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        directory = open_directory_at(&directory, name)?;
                    }
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "canonical root contains an invalid component",
                        ));
                    }
                }
            }
            Ok(directory)
        }
    }

    fn open_parent(
        canonical_root: &Path,
        relative_parent: &Path,
        approved_root: &CheckedMetadata,
    ) -> std::io::Result<File> {
        let mut directory = open_absolute_directory(canonical_root)?;
        ensure_same_file(
            canonical_root,
            approved_root,
            &checked_file_metadata(&directory)?,
        )?;
        for component in relative_parent.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "relative parent contains an invalid component",
                ));
            };
            directory = open_directory_at(&directory, name)?;
        }
        Ok(directory)
    }

    fn inspect_target(directory: &File, name: &OsStr) -> std::io::Result<Option<CheckedMetadata>> {
        match open_at(directory, name, OPEN_NOFOLLOW | OPEN_CLOEXEC, 0) {
            Ok(file) => {
                let metadata = checked_file_metadata(&file)?;
                if !metadata.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "atomic write target is not a regular file",
                    ));
                }
                Ok(Some(metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn same_optional_target(
        path: &Path,
        before: Option<&CheckedMetadata>,
        after: Option<&CheckedMetadata>,
    ) -> std::io::Result<()> {
        match (before, after) {
            (None, None) => Ok(()),
            (Some(before), Some(after)) => ensure_same_file(path, before, after),
            _ => Err(path_changed_error(path)),
        }
    }

    #[cfg(target_os = "linux")]
    fn unlink_owned_temp(directory: &File, name: &CString, identity: &CheckedMetadata) {
        let still_ours = open_at(
            directory,
            OsStr::from_bytes(name.as_bytes()),
            OPEN_NOFOLLOW,
            0,
        )
        .and_then(|file| checked_file_metadata(&file))
        .is_ok_and(|metadata| identity.identity == metadata.identity);
        if still_ours {
            // SAFETY: both the directory descriptor and C string are valid.
            let _ = unsafe { unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
        }
    }

    #[cfg(target_os = "macos")]
    fn unlink_owned_temp(directory: &File, name: &CString, identity: &std::fs::Metadata) {
        if temp_entry_matches(directory, name, identity).unwrap_or(false) {
            // SAFETY: both the directory descriptor and C string are valid,
            // and the descriptor-relative identity check selected this entry.
            let _ = unsafe { unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
        }
    }

    #[cfg(target_os = "macos")]
    fn temp_entry_matches(
        directory: &File,
        name: &CString,
        identity: &std::fs::Metadata,
    ) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let mut entry = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `directory` and `name` remain live for the call, and `entry`
        // points to writable storage of the exact stat structure. NOFOLLOW
        // makes a substituted symlink observable instead of traversing it.
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                entry.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized the complete structure.
        let entry = unsafe { entry.assume_init() };
        Ok(entry.st_mode & libc::S_IFMT == libc::S_IFREG
            && u64::try_from(entry.st_dev).ok() == Some(identity.dev())
            && entry.st_ino == identity.ino())
    }

    fn rename_entry(
        directory: &File,
        old_name: &CString,
        new_name: &CString,
        mode: AtomicWriteMode,
    ) -> c_int {
        if mode == AtomicWriteMode::Replace {
            // SAFETY: both names are valid C strings and the descriptor remains
            // open for the duration of the call.
            return unsafe {
                renameat(
                    directory.as_raw_fd(),
                    old_name.as_ptr(),
                    directory.as_raw_fd(),
                    new_name.as_ptr(),
                )
            };
        }

        #[cfg(target_os = "linux")]
        {
            // `RENAME_NOREPLACE` makes create-only publication atomic with the
            // non-existence check instead of relying on a racy final `stat`.
            unsafe {
                linux_renameat2(
                    directory.as_raw_fd(),
                    old_name.as_ptr(),
                    directory.as_raw_fd(),
                    new_name.as_ptr(),
                    1,
                )
            }
        }
        #[cfg(target_os = "macos")]
        {
            // Darwin's `RENAME_EXCL` is the create-only equivalent.
            unsafe {
                renameatx_np(
                    directory.as_raw_fd(),
                    old_name.as_ptr(),
                    directory.as_raw_fd(),
                    new_name.as_ptr(),
                    0x4,
                )
            }
        }
    }

    cancellation.check()?;
    let directory = open_parent(canonical_root, relative_parent, approved_root)?;
    let directory_metadata = checked_file_metadata(&directory)?;
    if let Some(approved) = approved_parent {
        ensure_same_file(canonical_root, approved, &directory_metadata)?;
    }

    let target_path = canonical_root.join(relative_parent).join(leaf);
    let initial_target = inspect_target(&directory, leaf)?;
    if mode == AtomicWriteMode::CreateNew && initial_target.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "atomic create target already exists",
        ));
    }

    let (temp_name, mut temp, temp_identity) = {
        let mut result = None;
        for _ in 0..128 {
            let candidate = CString::new(format!(".zswrite.{}.tmp", uuid::Uuid::new_v4().simple()))
                .expect("UUID temp name never contains NUL");
            match open_at(
                &directory,
                OsStr::from_bytes(candidate.as_bytes()),
                1 | OPEN_CREATE | OPEN_EXCLUSIVE | OPEN_NOFOLLOW | OPEN_CLOEXEC,
                0o600,
            ) {
                Ok(file) => {
                    #[cfg(target_os = "macos")]
                    let identity = file.metadata()?;
                    #[cfg(target_os = "linux")]
                    let identity = checked_file_metadata(&file)?;
                    result = Some((candidate, file, identity));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        result.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a unique atomic-write temp file",
            )
        })?
    };

    let write_result = (|| {
        #[cfg(test)]
        if failure == AtomicWriteFailure::Write {
            return Err(std::io::Error::other("injected atomic-write failure"));
        }
        temp.write_all(contents)?;
        temp.flush()
    })();
    if let Err(error) = write_result {
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(error);
    }

    if let Err(error) = cancellation.check() {
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(error);
    }

    let current_target = match inspect_target(&directory, leaf) {
        Ok(target) => target,
        Err(error) => {
            drop(temp);
            unlink_owned_temp(&directory, &temp_name, &temp_identity);
            return Err(error);
        }
    };
    if let Err(error) = same_optional_target(
        &target_path,
        initial_target.as_ref(),
        current_target.as_ref(),
    ) {
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(error);
    }

    let current_directory_metadata =
        match open_parent(canonical_root, relative_parent, approved_root)
            .and_then(|directory| checked_file_metadata(&directory))
        {
            Ok(metadata) => metadata,
            Err(error) => {
                drop(temp);
                unlink_owned_temp(&directory, &temp_name, &temp_identity);
                return Err(error);
            }
        };
    if let Err(error) = ensure_same_file(
        &target_path,
        &directory_metadata,
        &current_directory_metadata,
    ) {
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(error);
    }

    #[cfg(target_os = "macos")]
    match temp_entry_matches(&directory, &temp_name, &temp_identity) {
        Ok(true) => {}
        Ok(false) => {
            drop(temp);
            return Err(path_changed_error(&target_path));
        }
        Err(error) => {
            drop(temp);
            return Err(error);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let temp_still_ours = open_at(
            &directory,
            OsStr::from_bytes(temp_name.as_bytes()),
            OPEN_NOFOLLOW | OPEN_CLOEXEC,
            0,
        )
        .and_then(|file| checked_file_metadata(&file))
        .and_then(|metadata| ensure_same_file(&target_path, &temp_identity, &metadata));
        if let Err(error) = temp_still_ours {
            drop(temp);
            return Err(error);
        }
    }

    #[cfg(test)]
    if failure == AtomicWriteFailure::Rename {
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(std::io::Error::other("injected atomic-rename failure"));
    }

    if let Some(metadata) = initial_target.as_ref()
        && let Err(error) = temp.set_permissions(metadata.permissions())
    {
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(error);
    }

    let leaf = match c_name(leaf) {
        Ok(leaf) => leaf,
        Err(error) => {
            drop(temp);
            unlink_owned_temp(&directory, &temp_name, &temp_identity);
            return Err(error);
        }
    };
    let rename_result =
        match cancellation.publish(|| Ok(rename_entry(&directory, &temp_name, &leaf, mode))) {
            Ok(result) => result,
            Err(error) => {
                drop(temp);
                unlink_owned_temp(&directory, &temp_name, &temp_identity);
                return Err(error);
            }
        };
    if rename_result < 0 {
        let error = std::io::Error::last_os_error();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = temp.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        drop(temp);
        unlink_owned_temp(&directory, &temp_name, &temp_identity);
        return Err(error);
    }
    drop(temp);
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments, unsafe_code)]
fn atomic_write_platform(
    canonical_root: &Path,
    relative_parent: &Path,
    leaf: &std::ffi::OsStr,
    contents: &[u8],
    approved_root: &CheckedMetadata,
    approved_parent: Option<&CheckedMetadata>,
    mode: AtomicWriteMode,
    failure: AtomicWriteFailure,
    cancellation: &AtomicWriteCancellation,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        GENERIC_WRITE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_INFO, FILE_READ_ATTRIBUTES,
        FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
        FileDispositionInfo, FileRenameInfo, SYNCHRONIZE, SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    fn delete_open_file(file: &std::fs::File) {
        let information = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: `file` owns a live handle with DELETE access and
        // `information` has the layout required by FileDispositionInfo.
        let _ = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FileDispositionInfo,
                (&information as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
    }

    fn rename_open_file(file: &std::fs::File, name: &std::ffi::OsStr) -> std::io::Result<()> {
        let name: Vec<u16> = name.encode_wide().collect();
        if name.is_empty() || name.len() > (u32::MAX as usize / 2) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "atomic write target has an invalid file name",
            ));
        }
        let name_bytes = name.len() * std::mem::size_of::<u16>();
        let required = std::mem::size_of::<FILE_RENAME_INFO>() + name_bytes;
        let words = required.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; words];
        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // FileRenameInfo with ReplaceIfExists=false provides create-only
        // publication across supported Windows versions. For a simple leaf
        // name, a null RootDirectory resolves the destination relative to the
        // source file's current parent directory. The descriptor-relative
        // create above therefore remains authoritative even if that directory's
        // pathname is concurrently replaced.
        // SAFETY: `storage` is aligned, large enough for the header and UTF-16
        // name, and both handles remain live for the call.
        let renamed = unsafe {
            (*information).Anonymous.ReplaceIfExists = false;
            (*information).RootDirectory = std::ptr::null_mut();
            (*information).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                (*information).FileName.as_mut_ptr(),
                name.len(),
            );
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FileRenameInfo,
                information.cast(),
                required as u32,
            )
        };
        if renamed == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn open_verified_directory(
        path: &Path,
        expected: &CheckedMetadata,
    ) -> std::io::Result<std::fs::File> {
        let directory = std::fs::OpenOptions::new()
            .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let opened = checked_file_metadata(&directory)?;
        if !opened.is_dir() {
            return Err(path_changed_error(path));
        }
        ensure_same_file(path, expected, &opened)?;
        Ok(directory)
    }

    fn create_relative_temp(
        directory: &std::fs::File,
        name: &std::ffi::OsStr,
    ) -> std::io::Result<std::fs::File> {
        let mut name: Vec<u16> = name.encode_wide().collect();
        let name_bytes = name
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "atomic temp name is too long",
                )
            })?;
        if name.is_empty() || name_bytes > u16::MAX as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "atomic temp name is invalid",
            ));
        }
        let object_name = UNICODE_STRING {
            Length: name_bytes as u16,
            MaximumLength: name_bytes as u16,
            Buffer: name.as_mut_ptr(),
        };
        let object_attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: directory.as_raw_handle().cast(),
            ObjectName: &object_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut io_status = IO_STATUS_BLOCK::default();
        let mut handle = std::ptr::null_mut();
        // SAFETY: all structures and the UTF-16 name remain live for the call;
        // RootDirectory is an identity-verified directory handle; FILE_CREATE
        // provides exclusive creation relative to that handle.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                GENERIC_WRITE | DELETE | SYNCHRONIZE,
                &object_attributes,
                &mut io_status,
                std::ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                0,
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            // SAFETY: the conversion accepts any NTSTATUS returned by NtCreateFile.
            let code = unsafe { RtlNtStatusToDosError(status) };
            return Err(std::io::Error::from_raw_os_error(code as i32));
        }
        if handle.is_null() {
            return Err(std::io::Error::other(
                "relative temp creation returned no handle",
            ));
        }
        // SAFETY: successful NtCreateFile returned a new owned handle.
        Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
    }

    cancellation.check()?;
    ensure_same_file(
        canonical_root,
        approved_root,
        &checked_path_metadata(canonical_root)?,
    )?;
    let parent = canonical_root.join(relative_parent);
    let parent_metadata = checked_path_metadata(&parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(path_changed_error(&parent));
    }
    if let Some(approved) = approved_parent {
        ensure_same_file(&parent, approved, &parent_metadata)?;
    }
    let directory = open_verified_directory(&parent, &parent_metadata)?;
    if let Some(approved) = approved_parent {
        ensure_same_file(&parent, approved, &checked_file_metadata(&directory)?)?;
    }

    let target = parent.join(leaf);
    let initial_target: Option<CheckedMetadata> = match checked_path_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(path_changed_error(&target));
        }
        Ok(_) if mode == AtomicWriteMode::CreateNew => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "atomic create target already exists",
            ));
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "atomic replacement is unsupported on Windows",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    ensure_same_file(&parent, &parent_metadata, &checked_path_metadata(&parent)?)?;

    #[cfg(test)]
    cancellation.probe_before_temp_creation();

    // NtCreateFile binds exclusive temp creation directly to the held,
    // identity-verified directory handle. No mutable pathname or attacker
    // directory ACL participates in the temp file's creation.
    let mut bound_file = None;
    for _ in 0..128 {
        let temp_name = format!(".zswrite.{}.tmp", uuid::Uuid::new_v4().simple());
        let file = match create_relative_temp(&directory, std::ffi::OsStr::new(&temp_name)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        bound_file = Some(file);
        break;
    }
    let mut file = bound_file.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique atomic-write temp file",
        )
    })?;

    #[cfg(test)]
    if failure == AtomicWriteFailure::Write {
        delete_open_file(&file);
        return Err(std::io::Error::other("injected atomic-write failure"));
    }
    if let Err(error) = file.write_all(contents).and_then(|()| file.flush()) {
        delete_open_file(&file);
        return Err(error);
    }
    if let Err(error) = cancellation.check() {
        delete_open_file(&file);
        return Err(error);
    }
    #[cfg(test)]
    if failure == AtomicWriteFailure::Rename {
        delete_open_file(&file);
        return Err(std::io::Error::other("injected atomic-rename failure"));
    }

    // When the directory is still reachable at its approved path, preserve
    // the same-target check used by the Unix implementation. If the directory
    // itself was renamed, the created file's current parent remains authoritative.
    let parent_still_reachable = checked_path_metadata(&parent)
        .is_ok_and(|current| current.identity == parent_metadata.identity);
    if parent_still_reachable {
        let current_target = match checked_path_metadata(&target) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                delete_open_file(&file);
                return Err(error);
            }
        };
        let same_target = match (initial_target.as_ref(), current_target.as_ref()) {
            (None, None) => true,
            (Some(before), Some(after)) => before.identity == after.identity,
            _ => false,
        };
        if !same_target {
            delete_open_file(&file);
            return Err(path_changed_error(&target));
        }
    }

    let publish_result = cancellation.publish(|| rename_open_file(&file, leaf));
    if let Err(error) = publish_result {
        delete_open_file(&file);
        return Err(error);
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[allow(clippy::too_many_arguments)]
fn atomic_write_platform(
    canonical_root: &Path,
    relative_parent: &Path,
    leaf: &std::ffi::OsStr,
    contents: &[u8],
    approved_root: &CheckedMetadata,
    approved_parent: Option<&CheckedMetadata>,
    mode: AtomicWriteMode,
    failure: AtomicWriteFailure,
    cancellation: &AtomicWriteCancellation,
) -> std::io::Result<()> {
    use std::io::Write;

    let _ = failure;
    fn remove_if_owned(path: &Path, identity: &CheckedMetadata) {
        let still_ours = checked_path_metadata(path)
            .is_ok_and(|metadata| identity.identity == metadata.identity);
        if still_ours {
            let _ = std::fs::remove_file(path);
        }
    }

    cancellation.check()?;
    let parent = canonical_root.join(relative_parent);
    ensure_same_file(
        canonical_root,
        approved_root,
        &checked_path_metadata(canonical_root)?,
    )?;
    let parent_metadata = checked_path_metadata(&parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(path_changed_error(&parent));
    }
    if let Some(approved) = approved_parent {
        ensure_same_file(&parent, approved, &parent_metadata)?;
    }

    let target = parent.join(leaf);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(path_changed_error(&target));
        }
        Ok(_) if mode == AtomicWriteMode::CreateNew => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "atomic create target already exists",
            ));
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "atomic replacement is unsupported on this platform",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let temp = parent.join(format!(".zswrite.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let temp_identity = checked_file_metadata(&file)?;
    #[cfg(test)]
    let write_result = if failure == AtomicWriteFailure::Write {
        Err(std::io::Error::other("injected atomic-write failure"))
    } else {
        file.write_all(contents).and_then(|()| file.flush())
    };
    #[cfg(not(test))]
    let write_result = file.write_all(contents).and_then(|()| file.flush());
    if let Err(error) = write_result {
        drop(file);
        remove_if_owned(&temp, &temp_identity);
        return Err(error);
    }
    if let Err(error) = cancellation.check() {
        drop(file);
        remove_if_owned(&temp, &temp_identity);
        return Err(error);
    }
    drop(file);

    let current_parent = match checked_path_metadata(&parent) {
        Ok(metadata) => metadata,
        Err(error) => {
            remove_if_owned(&temp, &temp_identity);
            return Err(error);
        }
    };
    if current_parent.file_type().is_symlink()
        || ensure_same_file(&parent, &parent_metadata, &current_parent).is_err()
    {
        remove_if_owned(&temp, &temp_identity);
        return Err(path_changed_error(&target));
    }
    match std::fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            remove_if_owned(&temp, &temp_identity);
            return Err(path_changed_error(&target));
        }
        Err(error) => {
            remove_if_owned(&temp, &temp_identity);
            return Err(error);
        }
    }
    if !checked_path_metadata(&temp)
        .is_ok_and(|metadata| temp_identity.identity == metadata.identity)
    {
        return Err(path_changed_error(&target));
    }
    #[cfg(test)]
    if failure == AtomicWriteFailure::Rename {
        remove_if_owned(&temp, &temp_identity);
        return Err(std::io::Error::other("injected atomic-rename failure"));
    }
    if let Err(error) = cancellation.publish(|| std::fs::rename(&temp, &target)) {
        remove_if_owned(&temp, &temp_identity);
        return Err(error);
    }
    Ok(())
}

/// Follow a symlink (or chain of symlinks) to the file it ultimately points at,
/// so an atomic write replaces that file rather than the link.
///
/// Relative link targets are resolved against the directory of the link that
/// produced them, matching POSIX semantics. The number of hops is bounded (as
/// the kernel bounds `MAXSYMLINKS`) to avoid looping on a cyclic link. If `path`
/// is not a symlink, or a link is broken/unreadable, the best path resolved so
/// far is returned — so a plain file, a new file, or a broken link all behave
/// sensibly (we just write to that path).
pub(crate) async fn resolve_symlink_target(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match tokio::fs::symlink_metadata(&current).await {
            Ok(meta) if meta.file_type().is_symlink() => match tokio::fs::read_link(&current).await
            {
                Ok(target) => {
                    current = if target.is_absolute() {
                        target
                    } else if let Some(parent) = current.parent() {
                        parent.join(target)
                    } else {
                        target
                    };
                }
                Err(_) => break,
            },
            _ => break,
        }
    }
    current
}

pub fn expand_tilde(s: &str) -> String {
    let Some(home) = crate::paths::process_home_dir() else {
        return s.to_string();
    };

    if s == "~" || s == "$HOME" {
        return home.to_string_lossy().to_string();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return expand_home_relative(&home, rest)
            .to_string_lossy()
            .to_string();
    }
    if let Some(rest) = s.strip_prefix("$HOME/") {
        return expand_home_relative(&home, rest)
            .to_string_lossy()
            .to_string();
    }
    s.to_string()
}

/// Expand `~` and resolve a relative path against an explicit workspace.
pub(crate) fn resolve_workspace_path(workspace: &Path, value: &str) -> PathBuf {
    let expanded = PathBuf::from(expand_tilde(value));
    if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    }
}

/// Resolve a path relative to `home` without allowing parent components to
/// traverse above it.
fn expand_home_relative(home: &Path, relative: &str) -> PathBuf {
    let mut expanded = home.to_path_buf();
    let mut depth = 0;

    for component in Path::new(relative).components() {
        match component {
            std::path::Component::Normal(part) => {
                expanded.push(part);
                depth += 1;
            }
            std::path::Component::ParentDir if depth > 0 => {
                expanded.pop();
                depth -= 1;
            }
            std::path::Component::ParentDir
            | std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }

    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mini_agent_fs_{label}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&path).expect("create temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn windows_file_identity_uses_stable_handle_information() {
        let source = include_str!("fs.rs");
        assert!(source.contains("GetFileInformationByHandleEx"));
        assert!(source.contains("FileIdInfo"));
        assert!(source.contains("file_id: [u8; 16]"));
        assert!(source.contains("file_id == [0; 16]"));
        assert!(source.contains("access_mode(0)"));
        assert!(source.contains("spawn_blocking(move || checked_owned_file(file, metadata))"));
        assert!(source.contains("struct CheckedMetadata"));
        let stable_identity = source
            .split("pub(crate) fn windows_file_identity")
            .nth(1)
            .and_then(|source| source.split("enum FileIdentity").next())
            .expect("Windows stable-handle identity implementation missing");
        assert!(!stable_identity.contains(concat!(".", "volume_serial_number()")));
        assert!(!stable_identity.contains(concat!(".", "file_index()")));
        assert!(!stable_identity.contains(concat!(".", "number_of_links()")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_identity_preserves_full_kernel_identity_and_link_count() {
        let directory = TestDirectory::new("windows_identity");
        let original = directory.path().join("original.txt");
        let linked = directory.path().join("linked.txt");
        std::fs::write(&original, b"identity").expect("write original");
        std::fs::hard_link(&original, &linked).expect("create hard link");

        let original = std::fs::File::open(original).expect("open original");
        let linked = std::fs::File::open(linked).expect("open hard link");
        let original_identity = windows_file_identity(&original).expect("query original identity");
        let linked_identity = windows_file_identity(&linked).expect("query linked identity");

        assert_eq!(
            original_identity.volume_serial_number,
            linked_identity.volume_serial_number
        );
        assert_eq!(original_identity.file_id, linked_identity.file_id);
        assert_ne!(original_identity.file_id, [0; 16]);
        assert_eq!(windows_file_link_count(&original).unwrap(), 2);
        assert_eq!(windows_file_link_count(&linked).unwrap(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_identity_rejects_unsupported_zero_identifier() {
        assert!(validated_windows_file_identity(7, [0; 16]).is_err());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn checked_metadata_keeps_unlinked_approved_object_alive_and_rejects_replacement() {
        let directory = TestDirectory::new("retained_handle");
        let path = directory.path().join("checked.txt");
        let moved = directory.path().join("moved.txt");
        std::fs::write(&path, b"approved object").expect("write original");
        let checked = checked_path_metadata(&path).expect("capture original identity");

        std::fs::rename(&path, &moved).expect("move original");
        std::fs::remove_file(&moved).expect("unlink original while retained handle is live");
        std::fs::write(&path, b"replacement").expect("write replacement");
        let replacement = checked_path_metadata(&path).expect("capture replacement identity");

        assert!(ensure_same_file(&path, &checked, &replacement).is_err());
        assert!(checked.handle.metadata().is_ok());
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CheckedMetadata>();
    }

    #[cfg(unix)]
    #[test]
    fn checked_path_and_open_file_metadata_use_the_same_identity() {
        let directory = TestDirectory::new("path_and_file_identity");
        let path = directory.path().join("checked.txt");
        std::fs::write(&path, b"identity").expect("write checked file");

        let path_metadata = checked_path_metadata(&path).expect("capture path identity");
        let file = std::fs::File::open(&path).expect("open checked file");
        let file_metadata = checked_file_metadata(&file).expect("capture file identity");

        ensure_same_file(&path, &path_metadata, &file_metadata)
            .expect("path and open handle must identify the same file");
    }

    #[cfg(unix)]
    #[test]
    #[allow(unsafe_code)]
    fn checked_path_metadata_is_nonblocking_for_fifo() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;
        #[cfg(target_os = "linux")]
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("metadata_handle");
        let fifo = directory.path().join("event.fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated and points into a live CString.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let fifo_metadata = checked_path_metadata(&fifo).expect("open FIFO metadata handle");
        assert!(fifo_metadata.file_type().is_fifo());

        #[cfg(target_os = "linux")]
        {
            let unreadable = directory.path().join("unreadable.txt");
            std::fs::write(&unreadable, b"metadata only").unwrap();
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0)).unwrap();
            let metadata = checked_path_metadata(&unreadable).expect("O_PATH needs no read access");
            assert!(metadata.is_file());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stable_path_metadata_reports_nofollow_symlink_as_path_changed() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("stable_symlink_error");
        let target = directory.path().join("target.txt");
        let link = directory.path().join("link.txt");
        std::fs::write(&target, b"target").expect("write symlink target");
        symlink(&target, &link).expect("create symlink");

        let error = stable_path_metadata(&link)
            .await
            .expect_err("stable metadata must reject symlinks");
        assert!(is_path_changed_error(&error));
        assert!(error.to_string().contains("Path changed"));
    }

    #[cfg(unix)]
    #[test]
    fn private_file_symlink_rejection_preserves_owned_regular_file_error() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("private_symlink_error");
        let target = directory.path().join("target.txt");
        let link = directory.path().join("link.txt");
        std::fs::write(&target, b"target").expect("write symlink target");
        symlink(&target, &link).expect("create symlink");

        let error = open_private_file(&link).expect_err("private open must reject symlinks");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("owned regular file"));
    }

    #[test]
    fn expand_tilde_keeps_parent_traversal_within_home() {
        let home = dirs::home_dir().expect("test requires a home directory");

        assert_eq!(
            PathBuf::from(expand_tilde("~/../../etc/passwd")),
            home.join("etc/passwd")
        );
        assert_eq!(
            PathBuf::from(expand_tilde("~/../../../root/.ssh/id_rsa")),
            home.join("root/.ssh/id_rsa")
        );
        assert_eq!(
            PathBuf::from(expand_tilde("$HOME/../../../root/.ssh/id_rsa")),
            home.join("root/.ssh/id_rsa")
        );
    }

    #[test]
    fn expand_tilde_normalizes_parent_components_below_home() {
        let home = dirs::home_dir().expect("test requires a home directory");

        assert_eq!(
            PathBuf::from(expand_tilde("~/projects/mini-agent/../notes")),
            home.join("projects/notes")
        );
    }

    #[test]
    fn expand_tilde_does_not_treat_repeated_separator_as_absolute() {
        let home = dirs::home_dir().expect("test requires a home directory");

        assert_eq!(
            PathBuf::from(expand_tilde("~//etc/passwd")),
            home.join("etc/passwd")
        );
    }
}
