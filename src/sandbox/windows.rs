#![allow(unsafe_code)]

//! General-process Windows sandbox.
//!
//! This is deliberately separate from the broker-only LPAC worker launcher. A small copy of the
//! current executable receives an authenticated-by-inheritance request on stdin, creates a
//! workspace-capable AppContainer, and starts the requested program in a creation-time Job. The
//! request never appears in a command line, environment variable, or temporary file. The
//! AppContainer receives no capabilities: in particular, network access is denied by default.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io::{Read, Write};
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf, Prefix};
use std::process::{Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FILETIME, GENERIC_ALL, GENERIC_READ,
    GetHandleInformation, HANDLE, LocalFree, TRUE, WAIT_ABANDONED_0, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::NetworkManagement::WindowsFirewall::NetworkIsolationGetAppContainerConfig;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetExplicitEntriesFromAclW, GetSecurityInfo, REVOKE_ACCESS, SE_FILE_OBJECT, SE_OBJECT_TYPE,
    SE_WINDOW_OBJECT, SET_ACCESS, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
    DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, DuplicateTokenEx, EqualSid, FreeSid,
    GetTokenInformation, ImpersonateLoggedOnUser, InitializeSecurityDescriptor, OBJECT_INHERIT_ACE,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, RevertToSelf,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SECURITY_DESCRIPTOR, SID_AND_ATTRIBUTES,
    SecurityImpersonation, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TOKEN_APPCONTAINER_INFORMATION, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    TOKEN_QUERY, TOKEN_USER, TokenAppContainerSid, TokenCapabilities, TokenImpersonation,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CREATEWINDOW, DESKTOP_READOBJECTS,
    GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, HDESK, UOI_NAME,
};
use windows_sys::Win32::System::SystemServices::{
    JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE, JOB_OBJECT_UILIMIT_ALL, SECURITY_DESCRIPTOR_REVISION,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateMutexW,
    CreateProcessAsUserW, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentProcessId,
    GetCurrentThreadId, GetExitCodeProcess, GetProcessId, GetProcessTimes,
    InitializeProcThreadAttributeList, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, ReleaseMutex,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForMultipleObjects, WaitForSingleObject,
};

use crate::process_creation::StdCommandCreationExt;
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_REMOTE, PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
};

const HELPER_ARG: &str = "--mini-agent-windows-sandbox-helper-v1";
const PROBE_ARG: &str = "--mini-agent-windows-sandbox-runtime-check";
const PARENT_PROBE_ARG: &str = "--mini-agent-windows-sandbox-parent-probe";
const AUTHORITY_PROBE_ARG: &str = "--mini-agent-windows-sandbox-authority-probe";
const DESCENDANT_PROBE_ARG: &str = "--mini-agent-windows-appcontainer-descendant-probe";
const HELPER_PID_PLACEHOLDER: &str = "helper-pid";
const DESKTOP_NAME_PLACEHOLDER: &str = "desktop-name";
const OMITTED_HANDLE_PLACEHOLDER: &str = "omitted-handle";
const DESCENDANT_READY_PLACEHOLDER: &str = "descendant-ready";
const DESCENDANT_RELEASE_PLACEHOLDER: &str = "descendant-release";
const CONTROL_ROOT_PLACEHOLDER: &str = "control-root";
const REQUEST_VERSION: u32 = 1;
// This stays below the requested anonymous-pipe buffer, so request creation cannot block before
// the helper exists to read it. It also leaves room below CreateProcessW's UTF-16 argv bound.
const REQUEST_MAX_BYTES: usize = 24 * 1024;
const APPCONTAINER_PROFILE_PREFIX: &str = "mini-agent.general.";
const REQUEST_PIPE_BUFFER: u32 = 512;
const MAX_REQUEST_FEEDERS: usize = 16;
const MAX_ACL_ENTRIES: usize = 250_000;
const MAX_ACCESS_ROOTS: usize = 128;
const MAX_STALE_PROFILE_JOURNALS: usize = 64;
const PROFILE_JOURNAL_VERSION: u32 = 2;
const JOB_NAME_PREFIX: &str = "Global\\mini-agent-general-job-";
const STALE_JOB_CLEANUP_EXIT_CODE: u32 = 126;
const DESCENDANT_IDENTITY_MAX_BYTES: usize = 128;
const MAX_JOB_PROCESSES: u32 = 64;
const PROCESS_MEMORY_BYTES: usize = 512 * 1024 * 1024;
const JOB_MEMORY_BYTES: usize = 1024 * 1024 * 1024;
const PROCESS_CPU_100NS: i64 = 60 * 10_000_000;
const ACL_MUTEX_WAIT_MS: u32 = 5_000;
// ACL snapshots must be serialized across terminal/RDP/service sessions. A Local\\ mutex would
// allow two same-user helpers in different sessions to overwrite each other's read/modify/write
// transaction. The object manager applies the creator token's default DACL; a pre-created object
// that we cannot open therefore fails the launch closed.
const ACL_MUTEX_NAME: &str = "Global\\mini-agent-general-sandbox-acl-v1";

static ACTIVE_REQUEST_FEEDERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct LaunchRequest {
    version: u32,
    program: PathBuf,
    program_proof: FileProof,
    arguments: Vec<String>,
    cwd: PathBuf,
    cache: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    configured_read_roots: Vec<PathBuf>,
    configured_write_roots: Vec<PathBuf>,
    parent_pid: u32,
    parent_created: u64,
    ready_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Eq, PartialEq)]
struct FileProof {
    volume_serial_number: u64,
    file_id: [u8; 16],
    sha256: [u8; 32],
}

struct Handle(OwnedHandle);

impl Handle {
    fn created(raw: HANDLE, context: &str) -> Result<Self, String> {
        if raw.is_null() || raw == (-1isize as HANDLE) {
            return Err(last_error(context));
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
}

struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

struct AclMutationGuard(Handle);

impl AclMutationGuard {
    fn acquire() -> Result<Self, String> {
        let name = wide_string(ACL_MUTEX_NAME);
        let mutex = Handle::created(
            unsafe { CreateMutexW(null(), 0, name.as_ptr()) },
            "open cross-process ACL mutation mutex",
        )?;
        match unsafe { WaitForSingleObject(mutex.raw(), ACL_MUTEX_WAIT_MS) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED_0 => Ok(Self(mutex)),
            WAIT_TIMEOUT => Err("sandbox: timed out serializing ACL mutation".into()),
            _ => Err(last_error("wait for cross-process ACL mutation mutex")),
        }
    }
}

impl Drop for AclMutationGuard {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0.raw()) };
    }
}

struct AppContainerProfile {
    sid: PSID,
    name: Vec<u16>,
    name_text: String,
    text: String,
    storage: PathBuf,
    journal_path: PathBuf,
    journal_lease: Option<File>,
}

impl AppContainerProfile {
    fn raw(&self) -> PSID {
        self.sid
    }
}

impl Drop for AppContainerProfile {
    fn drop(&mut self) {
        if self.journal_path.as_os_str().is_empty() {
            let _ = delete_appcontainer_profile(&self.name);
        }
        unsafe {
            FreeSid(self.sid);
        }
    }
}

impl AppContainerProfile {
    fn finalize_cleanup(&mut self) -> Result<(), String> {
        delete_appcontainer_profile(&self.name)?;
        self.journal_lease.take();
        std::fs::remove_file(&self.journal_path)
            .map_err(|error| format!("sandbox: remove completed AppContainer journal: {error}"))?;
        self.journal_path.clear();
        Ok(())
    }

    fn rollback_unjournaled(&mut self) -> Result<(), String> {
        if !self.journal_path.as_os_str().is_empty() {
            return Err("sandbox: refused unjournaled rollback after journal publication".into());
        }
        delete_appcontainer_profile(&self.name)?;
        if !self.storage.as_os_str().is_empty() && self.storage.exists() {
            std::fs::remove_dir_all(&self.storage).map_err(|error| {
                format!(
                    "sandbox: remove unjournaled AppContainer storage {}: {error}",
                    self.storage.display()
                )
            })?;
        }
        Ok(())
    }
}

fn delete_appcontainer_profile(name: &[u16]) -> Result<(), String> {
    let result = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
    const HRESULT_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;
    if result == 0 || result == HRESULT_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!(
            "sandbox: delete AppContainer profile: HRESULT {result:#x}"
        ))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ProfileJournal {
    version: u32,
    profile_name: String,
    sid: String,
    job_name: String,
    roots: Vec<PathBuf>,
}

#[derive(Debug)]
struct CleanupProof {
    sid: String,
    profile_name: String,
    storage: PathBuf,
    journal: PathBuf,
    job_name: Option<String>,
}

struct PrivateDesktop {
    handle: HDESK,
    name: String,
    startup_name: Vec<u16>,
}

impl Drop for PrivateDesktop {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseDesktop(self.handle) };
        }
    }
}

pub(crate) fn is_available() -> bool {
    // Availability is an operational claim, not a compile-time one. The current backend needs a
    // real restricted launch to prove its token, desktop, and creation-time Job boundary, and the
    // production startup path has no side-effect-free way to do that. Keep it unavailable until a
    // cached production preflight exists; the hidden hosted probe remains directly invokable.
    false
}

pub(crate) fn build_shell_helper(
    shell: &str,
    command_arg: &str,
    script: &str,
    cwd: &Path,
    cache: &Path,
    configured_read_roots: &[PathBuf],
    configured_write_roots: &[PathBuf],
) -> Result<tokio::process::Command, String> {
    let program = resolve_program(shell, cwd)?;
    build_helper_configured(
        program,
        vec![command_arg.to_string(), script.to_string()],
        cwd,
        cache,
        configured_read_roots,
        configured_write_roots,
    )
}

pub(crate) fn build_direct_helper(
    program: &Path,
    arguments: &[String],
    cwd: &Path,
    cache: &Path,
    configured_read_roots: &[PathBuf],
    configured_write_roots: &[PathBuf],
) -> Result<tokio::process::Command, String> {
    let program = canonical_file(program, "direct executable")?;
    build_helper_configured(
        program,
        arguments.to_vec(),
        cwd,
        cache,
        configured_read_roots,
        configured_write_roots,
    )
}

fn build_helper(
    program: PathBuf,
    arguments: Vec<String>,
    cwd: &Path,
    cache: &Path,
) -> Result<tokio::process::Command, String> {
    build_helper_configured(program, arguments, cwd, cache, &[], &[])
}

fn build_helper_configured(
    program: PathBuf,
    arguments: Vec<String>,
    cwd: &Path,
    cache: &Path,
    configured_read_roots: &[PathBuf],
    configured_write_roots: &[PathBuf],
) -> Result<tokio::process::Command, String> {
    build_helper_with_ready_and_roots(
        program,
        arguments,
        cwd,
        cache,
        None,
        configured_read_roots,
        configured_write_roots,
    )
}

fn build_helper_with_ready(
    program: PathBuf,
    arguments: Vec<String>,
    cwd: &Path,
    cache: &Path,
    ready_path: Option<PathBuf>,
) -> Result<tokio::process::Command, String> {
    build_helper_with_ready_and_roots(program, arguments, cwd, cache, ready_path, &[], &[])
}

fn build_helper_with_ready_and_roots(
    program: PathBuf,
    arguments: Vec<String>,
    cwd: &Path,
    cache: &Path,
    ready_path: Option<PathBuf>,
    configured_read_roots: &[PathBuf],
    configured_write_roots: &[PathBuf],
) -> Result<tokio::process::Command, String> {
    let cwd = canonical_root(cwd, "workspace")?;
    let cache = canonical_root(cache, "application cache")?;
    let program = canonical_file(&program, "sandbox executable")?;
    let program_proof = prove_executable(&program)?;
    // The workspace is the sole implicit write root. The application cache and toolchain/cache
    // roots are read/execute only; adding another writable root requires explicit future config
    // plumbing rather than silently broadening this policy.
    let configured_read_roots =
        canonicalize_access_roots(configured_read_roots.iter().map(PathBuf::as_path), &cwd)?;
    let configured_write_roots =
        canonicalize_access_roots(configured_write_roots.iter().map(PathBuf::as_path), &cwd)?;
    let write_roots = collect_write_roots(&cwd, &configured_write_roots)?;
    let read_roots = collect_read_roots(&program, &cwd, &cache, &configured_read_roots)?;
    validate_explicit_root_policy(
        &program,
        &cwd,
        &cache,
        &configured_read_roots,
        &configured_write_roots,
        &read_roots,
        &write_roots,
    )?;
    let request = LaunchRequest {
        version: REQUEST_VERSION,
        program,
        program_proof,
        arguments,
        cwd,
        cache,
        read_roots,
        write_roots,
        configured_read_roots,
        configured_write_roots,
        parent_pid: unsafe { GetCurrentProcessId() },
        parent_created: process_creation_time(unsafe { GetCurrentProcess() })?,
        ready_path,
    };
    let payload = serde_json::to_vec(&request)
        .map_err(|error| format!("sandbox: encode Windows launch request: {error}"))?;
    if payload.len() > REQUEST_MAX_BYTES {
        return Err("sandbox: Windows launch request exceeds the 24 KiB transport bound".into());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    let mut read = null_mut();
    let mut write = null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &mut attributes, REQUEST_PIPE_BUFFER) } == 0 {
        return Err(last_error("create private Windows sandbox request pipe"));
    }
    let mut reader = Some(unsafe { File::from_raw_handle(read) });
    let mut writer = unsafe { File::from_raw_handle(write) };
    ACTIVE_REQUEST_FEEDERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_REQUEST_FEEDERS).then_some(active + 1)
        })
        .map_err(|_| "sandbox: too many pending Windows request feeders".to_string())?;
    std::thread::Builder::new()
        .name("windows-sandbox-request".into())
        .spawn(move || {
            struct FeederPermit;
            impl Drop for FeederPermit {
                fn drop(&mut self) {
                    ACTIVE_REQUEST_FEEDERS.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _permit = FeederPermit;
            let _ = writer
                .write_all(&(payload.len() as u32).to_le_bytes())
                .and_then(|_| writer.write_all(&payload));
        })
        .map_err(|error| {
            ACTIVE_REQUEST_FEEDERS.fetch_sub(1, Ordering::AcqRel);
            format!("sandbox: start Windows request feeder: {error}")
        })?;

    let executable = std::env::current_exe()
        .map_err(|error| format!("sandbox: locate Windows helper executable: {error}"))?;
    let mut helper = Command::new(executable);
    helper
        .arg(HELPER_ARG)
        .env_clear()
        .stdin(Stdio::from(reader.take().expect("request reader is owned")));
    for (name, value) in essential_windows_environment() {
        helper.env(name, value);
    }
    let mut helper = tokio::process::Command::from(helper);
    crate::sandbox::configure_child_lifetime(&mut helper);
    Ok(helper)
}

fn collect_read_roots(
    program: &Path,
    cwd: &Path,
    cache: &Path,
    configured: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut candidates = vec![
        cwd.to_path_buf(),
        cache.to_path_buf(),
        program.to_path_buf(),
    ];
    candidates.extend(configured.iter().cloned());
    canonicalize_access_roots(candidates.iter().map(PathBuf::as_path), cwd)
}

fn collect_write_roots(cwd: &Path, configured: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut candidates = vec![cwd.to_path_buf()];
    candidates.extend(configured.iter().cloned());
    canonicalize_access_roots(candidates.iter().map(PathBuf::as_path), cwd)
}

fn canonicalize_access_roots<'a>(
    roots: impl IntoIterator<Item = &'a Path>,
    cwd: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut canonical = Vec::new();
    for root in roots {
        if canonical.len() >= MAX_ACCESS_ROOTS {
            return Err("sandbox: Windows access-root count exceeds 128".into());
        }
        let root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            cwd.join(root)
        };
        reject_remote_access_path(&root)?;
        canonical.push(canonical_access_path(&root)?);
    }
    canonical.sort();
    canonical.dedup();
    Ok(canonical)
}

fn canonical_access_path(path: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("sandbox: inspect AppContainer access path: {error}"))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("sandbox: AppContainer access path is a reparse point".into());
    }
    if metadata.is_dir() {
        canonical_root(path, "AppContainer access root")
    } else if metadata.is_file() {
        canonical_file(path, "AppContainer access file")
    } else {
        Err("sandbox: AppContainer access path has unsupported type".into())
    }
}

fn reject_remote_access_path(path: &Path) -> Result<(), String> {
    let drive = match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                format!("{}:\\", letter as char)
            }
            Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) | Prefix::DeviceNS(_) => {
                return Err("sandbox: UNC/device AppContainer roots are denied".into());
            }
            Prefix::Verbatim(_) => {
                return Err("sandbox: unsupported verbatim AppContainer root".into());
            }
        },
        _ => return Err("sandbox: AppContainer access root must be drive-absolute".into()),
    };
    let drive = wide_string(&drive);
    if unsafe { GetDriveTypeW(drive.as_ptr()) } == DRIVE_REMOTE {
        return Err("sandbox: remote AppContainer roots are denied".into());
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_explicit_root_policy(
    program: &Path,
    cwd: &Path,
    cache: &Path,
    configured_read_roots: &[PathBuf],
    configured_write_roots: &[PathBuf],
    read_roots: &[PathBuf],
    write_roots: &[PathBuf],
) -> Result<(), String> {
    if paths_overlap(cwd, cache) {
        return Err("sandbox: workspace and read-only cache roots overlap".into());
    }
    let mut expected_reads = vec![
        cwd.to_path_buf(),
        cache.to_path_buf(),
        program.to_path_buf(),
    ];
    expected_reads.extend(configured_read_roots.iter().cloned());
    expected_reads.sort();
    expected_reads.dedup();
    let mut expected_writes = vec![cwd.to_path_buf()];
    expected_writes.extend(configured_write_roots.iter().cloned());
    expected_writes.sort();
    expected_writes.dedup();
    if read_roots != expected_reads || write_roots != expected_writes {
        return Err(
            "sandbox: AppContainer request violated the closed explicit root policy".into(),
        );
    }
    let protected_reads =
        std::iter::once(cache).chain(configured_read_roots.iter().map(PathBuf::as_path));
    if protected_reads.clone().any(|read| {
        expected_writes
            .iter()
            .any(|write| paths_overlap(read, write))
    }) {
        return Err("sandbox: configured writable root overlaps a read-only root".into());
    }
    for (index, write) in expected_writes.iter().enumerate() {
        if expected_writes[index + 1..]
            .iter()
            .any(|other| paths_overlap(write, other))
        {
            return Err("sandbox: configured writable roots overlap".into());
        }
    }
    Ok(())
}

fn resolve_program(program: &str, cwd: &Path) -> Result<PathBuf, String> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return canonical_file(candidate, "shell executable");
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        for suffix in ["", ".exe", ".cmd", ".bat"] {
            let candidate = directory.join(format!("{program}{suffix}"));
            if let Ok(path) = canonical_file(&candidate, "shell executable") {
                return Ok(path);
            }
        }
    }
    canonical_file(&cwd.join(program), "shell executable")
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    reject_reparse_components(path)?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("sandbox: canonicalize {label} {}: {error}", path.display()))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("sandbox: inspect {label} {}: {error}", canonical.display()))?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!(
            "sandbox: {label} must be a non-reparse regular file"
        ));
    }
    Ok(canonical)
}

fn prove_executable(path: &Path) -> Result<FileProof, String> {
    let (_, proof) = lock_executable(path)?;
    Ok(proof)
}

fn lock_executable(path: &Path) -> Result<(File, FileProof), String> {
    let canonical = canonical_file(path, "executable proof")?;
    let mut file = open_stable_path(
        &canonical,
        false,
        GENERIC_READ | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ,
    )?;
    // Installed executables may legitimately have multiple names. The proof
    // binds volume/file identity and digest, while this live handle denies
    // writes and deletion through every alias until process creation.
    let identity = crate::fs::windows_file_identity(&file)
        .map_err(|error| format!("sandbox: inspect executable identity: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("sandbox: hash executable: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let proof = FileProof {
        volume_serial_number: identity.volume_serial_number,
        file_id: identity.file_id,
        sha256: hasher.finalize().into(),
    };
    Ok((file, proof))
}

fn open_stable_path(path: &Path, directory: bool, access: u32, share: u32) -> Result<File, String> {
    let wide = wide_null(path.as_os_str())?;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            share,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    let handle = Handle::created(raw, "open stable no-follow sandbox path")?;
    let file = unsafe { File::from_raw_handle(handle.0.into_raw_handle()) };
    let metadata = file
        .metadata()
        .map_err(|error| format!("sandbox: inspect stable path {}: {error}", path.display()))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.is_dir() != directory
    {
        return Err(format!(
            "sandbox: stable path has a reparse point or wrong type: {}",
            path.display()
        ));
    }
    Ok(file)
}

pub(crate) fn maybe_run_from_args() -> Option<i32> {
    let mut args = std::env::args_os();
    let _ = args.next();
    match args.next().as_deref() {
        Some(value) if value == OsStr::new(HELPER_ARG) => Some(run_helper().unwrap_or_else(|e| {
            eprintln!("Windows AppContainer sandbox helper failed: {e}");
            126
        })),
        Some(value) if value == OsStr::new(PROBE_ARG) => {
            Some(run_runtime_probe().unwrap_or_else(|e| {
                eprintln!("Windows AppContainer sandbox probe failed: {e}");
                1
            }))
        }
        Some(value) if value == OsStr::new(PARENT_PROBE_ARG) => {
            let marker = args.next().map(PathBuf::from);
            Some(run_parent_probe(marker.as_deref()).unwrap_or_else(|e| {
                eprintln!("Windows sandbox parent probe failed: {e}");
                1
            }))
        }
        Some(value) if value == OsStr::new(AUTHORITY_PROBE_ARG) => {
            Some(run_authority_probe(args).unwrap_or(97))
        }
        Some(value) if value == OsStr::new(DESCENDANT_PROBE_ARG) => {
            Some(run_descendant_probe(args).unwrap_or(98))
        }
        _ => None,
    }
}

fn run_helper() -> Result<i32, String> {
    let request = read_request()?;
    if request.version != REQUEST_VERSION {
        return Err("unsupported Windows sandbox request version".into());
    }
    let cwd = canonical_root(&request.cwd, "workspace")?;
    let cache = canonical_root(&request.cache, "application cache")?;
    let read_roots =
        canonicalize_access_roots(request.read_roots.iter().map(PathBuf::as_path), &cwd)?;
    let write_roots =
        canonicalize_access_roots(request.write_roots.iter().map(PathBuf::as_path), &cwd)?;
    let configured_read_roots = canonicalize_access_roots(
        request.configured_read_roots.iter().map(PathBuf::as_path),
        &cwd,
    )?;
    let configured_write_roots = canonicalize_access_roots(
        request.configured_write_roots.iter().map(PathBuf::as_path),
        &cwd,
    )?;
    let program = canonical_file(&request.program, "requested executable")?;
    validate_explicit_root_policy(
        &program,
        &cwd,
        &cache,
        &configured_read_roots,
        &configured_write_roots,
        &read_roots,
        &write_roots,
    )?;
    let (_program_lock, observed_proof) = lock_executable(&program)?;
    if observed_proof != request.program_proof {
        return Err("requested executable identity or digest changed before launch".into());
    }
    let ready_path = request
        .ready_path
        .as_deref()
        .map(|path| validate_ready_path(path, &cwd))
        .transpose()?;
    let parent = open_and_verify_parent(request.parent_pid, request.parent_created)?;
    ensure_parent_alive(&parent)?;
    let (job, job_name) = bounded_job()?;
    let profile = create_appcontainer_profile(&cache, &read_roots, &write_roots, &job_name)?;
    let mut grants = AccessGrants::new(profile, read_roots, write_roots);
    for root in grants.write_roots.clone() {
        grant_write_root(&root, &mut grants, &parent)?;
    }
    for root in grants.read_roots.clone() {
        if !grants
            .write_roots
            .iter()
            .any(|write| root.starts_with(write))
        {
            grant_read_root(&root, &mut grants, &parent)?;
        }
    }
    ensure_parent_alive(&parent)?;
    let token = primary_token()?;
    let desktop = private_desktop(grants.sid())?;
    let omitted_handle = inheritable_omitted_canary()?;
    let mut arguments = request.arguments;
    let mut descendant_rendezvous = None;
    if arguments.first().map(String::as_str) == Some(AUTHORITY_PROBE_ARG) {
        let helper_pid = arguments
            .get_mut(1)
            .filter(|value| value.as_str() == HELPER_PID_PLACEHOLDER)
            .ok_or("invalid authority-probe helper PID placeholder")?;
        *helper_pid = unsafe { GetCurrentProcessId() }.to_string();
        let desktop_name = arguments
            .get_mut(4)
            .filter(|value| value.as_str() == DESKTOP_NAME_PLACEHOLDER)
            .ok_or("invalid authority-probe desktop placeholder")?;
        *desktop_name = desktop.name.clone();
        let omitted = arguments
            .get_mut(5)
            .filter(|value| value.as_str() == OMITTED_HANDLE_PLACEHOLDER)
            .ok_or("invalid omitted-handle placeholder")?;
        *omitted = (omitted_handle.raw() as usize).to_string();
        let ready = cwd.join(format!(
            ".mini-agent-descendant-ready-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let release = cwd.join(format!(
            ".mini-agent-descendant-release-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let descendant_ready = arguments
            .get_mut(6)
            .filter(|value| value.as_str() == DESCENDANT_READY_PLACEHOLDER)
            .ok_or("invalid descendant-ready placeholder")?;
        *descendant_ready = ready.to_string_lossy().into_owned();
        let control_root = arguments
            .get_mut(7)
            .filter(|value| value.as_str() == CONTROL_ROOT_PLACEHOLDER)
            .ok_or("invalid control-root placeholder")?;
        *control_root = grants
            .profile
            .journal_path
            .parent()
            .ok_or("AppContainer journal control root missing")?
            .to_string_lossy()
            .into_owned();
        let descendant_release = arguments
            .get_mut(8)
            .filter(|value| value.as_str() == DESCENDANT_RELEASE_PLACEHOLDER)
            .ok_or("invalid descendant-release placeholder")?;
        *descendant_release = release.to_string_lossy().into_owned();
        descendant_rendezvous = Some((ready, release));
    }
    grants.disarm_for_launch();
    let child = launch_appcontainer(
        &token,
        &job,
        &desktop,
        grants.sid(),
        &program,
        &arguments,
        &cwd,
        &cache,
        &grants.profile.storage,
    )?;
    if let Err(error) = verify_job_membership_and_limits(&job, &child) {
        terminate_and_drain_job(&job, 126)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Err(error);
    }
    if let Some((ready, release)) = descendant_rendezvous
        && let Err(error) = verify_descendant_rendezvous(&job, &child, &ready, &release)
    {
        terminate_and_drain_job(&job, 126)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Err(error);
    }
    if let Some(path) = ready_path
        && let Err(error) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| {
                file.write_all(
                    format!(
                        "TARGET_READY\n{}\n{}\n{}\n{}\n",
                        grants.sid_text(),
                        grants.profile.name_text,
                        grants.profile.storage.display(),
                        grants.profile.journal_path.display(),
                    )
                    .as_bytes(),
                )
            })
    {
        terminate_and_drain_job(&job, 126)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Err(format!("publish AppContainer target readiness: {error}"));
    }
    let waits = [child.raw(), parent.raw()];
    let result = unsafe { WaitForMultipleObjects(waits.len() as u32, waits.as_ptr(), 0, u32::MAX) };
    if result == WAIT_OBJECT_0 + 1 {
        terminate_and_drain_job(&job, 125)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Ok(125);
    }
    if result != WAIT_OBJECT_0 {
        terminate_and_drain_job(&job, 126)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Err(last_error("wait for restricted child or parent death"));
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(child.raw(), &mut code) } == 0 {
        terminate_and_drain_job(&job, 126)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Err(last_error("read restricted child exit code"));
    }
    terminate_and_drain_job(&job, code)?;
    grants.mark_job_quiescent();
    grants.cleanup()?;
    Ok(code as i32)
}

fn validate_ready_path(path: &Path, root: &Path) -> Result<PathBuf, String> {
    if path.exists() || path.file_name().is_none() {
        return Err("sandbox readiness path must be an absent file".into());
    }
    let parent = path
        .parent()
        .ok_or("sandbox readiness path parent is missing")?;
    reject_reparse_components(parent)?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| format!("canonicalize readiness parent: {error}"))?;
    if parent != root {
        return Err("sandbox readiness path must be directly inside the workspace".into());
    }
    Ok(parent.join(path.file_name().expect("validated file name")))
}

fn read_request() -> Result<LaunchRequest, String> {
    let mut length = [0u8; 4];
    std::io::stdin()
        .read_exact(&mut length)
        .map_err(|error| format!("read Windows launch request length: {error}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > REQUEST_MAX_BYTES {
        return Err("invalid Windows sandbox request length".into());
    }
    let mut payload = vec![0u8; length];
    std::io::stdin()
        .read_exact(&mut payload)
        .map_err(|error| format!("read Windows launch request: {error}"))?;
    serde_json::from_slice(&payload)
        .map_err(|error| format!("decode Windows launch request: {error}"))
}

fn canonical_root(path: &Path, label: &str) -> Result<PathBuf, String> {
    reject_reparse_components(path)?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("sandbox: canonicalize {label} {}: {error}", path.display()))?;
    if canonical.parent().is_none() || !canonical.is_dir() {
        return Err(format!("sandbox: {label} must be a non-root directory"));
    }
    reject_reparse_components(&canonical)?;
    Ok(canonical)
}

fn reject_reparse_components(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if current.as_os_str().is_empty() || !current.exists() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            format!(
                "sandbox: inspect path component {}: {error}",
                current.display()
            )
        })?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "sandbox: reparse-point path component denied: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

struct GrantedObject {
    file: File,
    directory: bool,
    acl_mutated: bool,
}

struct AccessGrants {
    profile: AppContainerProfile,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    acl_roots: Vec<PathBuf>,
    objects: Vec<GrantedObject>,
    cleaned: bool,
    cleanup_attempted: bool,
    cleanup_allowed: bool,
}

impl AccessGrants {
    fn new(
        profile: AppContainerProfile,
        mut read_roots: Vec<PathBuf>,
        mut write_roots: Vec<PathBuf>,
    ) -> Self {
        read_roots.sort();
        read_roots.dedup();
        write_roots.sort();
        write_roots.dedup();
        let mut acl_roots = read_roots.clone();
        acl_roots.extend(write_roots.iter().cloned());
        acl_roots.sort();
        acl_roots.dedup();
        Self {
            profile,
            read_roots,
            write_roots,
            acl_roots,
            objects: Vec::new(),
            cleaned: false,
            cleanup_attempted: false,
            cleanup_allowed: true,
        }
    }

    fn sid(&self) -> PSID {
        self.profile.raw()
    }

    fn sid_text(&self) -> &str {
        &self.profile.text
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        if !self.cleanup_allowed {
            return Err("sandbox: cleanup deferred until exact Job quiescence is proven".into());
        }
        if self.cleanup_attempted {
            return Err("sandbox: cleanup previously failed; recovery journal retained".into());
        }
        self.cleanup_attempted = true;
        let mut first_error = None;
        for root in &self.acl_roots {
            if let Err(error) = revoke_tree(root, self.sid())
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        for object in self.objects.iter().rev() {
            if object.acl_mutated
                && let Err(error) =
                    update_access_ace(&object.file, object.directory, self.sid(), REVOKE_ACCESS, 0)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.profile.finalize_cleanup()?;
        self.cleaned = true;
        Ok(())
    }

    fn disarm_for_launch(&mut self) {
        self.cleanup_allowed = false;
    }

    fn mark_job_quiescent(&mut self) {
        self.cleanup_allowed = true;
    }
}

impl Drop for AccessGrants {
    fn drop(&mut self) {
        if self.cleanup_allowed {
            let _ = self.cleanup();
        }
    }
}

fn grant_read_root(root: &Path, grants: &mut AccessGrants, parent: &Handle) -> Result<(), String> {
    if trusted_system_read_file(root)? {
        // Windows system executables already carry the application-package
        // read/execute grant and are commonly owned by TrustedInstaller. Keep
        // a non-share-write/delete handle live to bind their identity without
        // attempting an unauthorized DACL mutation.
        let file = open_stable_path(
            root,
            false,
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ,
        )?;
        crate::fs::windows_file_identity(&file)
            .map_err(|error| format!("sandbox: inspect trusted system identity: {error}"))?;
        grants.acl_roots.retain(|candidate| candidate != root);
        grants.objects.push(GrantedObject {
            file,
            directory: false,
            acl_mutated: false,
        });
        return Ok(());
    }
    grant_access_root(
        root,
        grants,
        parent,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
    )
}

fn trusted_system_read_file(path: &Path) -> Result<bool, String> {
    let Some(system_root) = std::env::var_os("SystemRoot") else {
        return Ok(false);
    };
    let system_root = canonical_root(Path::new(&system_root), "Windows system root")?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("sandbox: inspect trusted system path: {error}"))?;
    Ok(metadata.is_file()
        && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
        && path.starts_with(system_root))
}

fn grant_write_root(root: &Path, grants: &mut AccessGrants, parent: &Handle) -> Result<(), String> {
    grant_access_root(
        root,
        grants,
        parent,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD,
    )
}

fn grant_access_root(
    root: &Path,
    grants: &mut AccessGrants,
    parent: &Handle,
    permissions: u32,
) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        ensure_parent_alive(parent)?;
        if grants.objects.len() >= MAX_ACL_ENTRIES {
            return Err("sandbox: writable-root ACL traversal exceeded 250000 entries".into());
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            format!("sandbox: inspect writable root {}: {error}", path.display())
        })?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "sandbox: writable root contains reparse point: {}",
                path.display()
            ));
        }
        let directory = metadata.is_dir();
        let resolved = std::fs::canonicalize(&path).map_err(|error| {
            format!(
                "sandbox: revalidate writable path {}: {error}",
                path.display()
            )
        })?;
        if !resolved.starts_with(root) {
            return Err(format!(
                "sandbox: writable path escaped canonical root: {}",
                path.display()
            ));
        }
        let file = open_stable_path(
            &resolved,
            directory,
            READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        if !directory
            && crate::fs::windows_file_link_count(&file)
                .map_err(|error| format!("sandbox: inspect writable link count: {error}"))?
                != 1
        {
            return Err(format!(
                "sandbox: multi-link writable file denied: {}",
                resolved.display()
            ));
        }
        let identity = crate::fs::windows_file_identity(&file)
            .map_err(|error| format!("sandbox: inspect writable identity: {error}"))?;
        let permissions = permissions
            & if directory {
                u32::MAX
            } else {
                !FILE_DELETE_CHILD
            };
        update_access_ace(&file, directory, grants.sid(), GRANT_ACCESS, permissions)?;
        if crate::fs::windows_file_identity(&file)
            .map_err(|error| format!("sandbox: recheck writable identity: {error}"))?
            != identity
        {
            return Err("sandbox: writable identity changed while applying ACL".into());
        }
        grants.objects.push(GrantedObject {
            file,
            directory,
            acl_mutated: true,
        });
        if directory {
            for entry in std::fs::read_dir(&resolved).map_err(|error| {
                format!(
                    "sandbox: enumerate writable root {}: {error}",
                    resolved.display()
                )
            })? {
                pending.push(
                    entry
                        .map_err(|error| format!("sandbox: enumerate writable root: {error}"))?
                        .path(),
                );
            }
        }
    }
    Ok(())
}

fn revoke_tree(root: &Path, sid: PSID) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(path) = pending.pop() {
        seen += 1;
        if seen > MAX_ACL_ENTRIES {
            return Err("sandbox: cleanup ACL traversal exceeded 250000 entries".into());
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("sandbox: cleanup inspect {}: {error}", path.display()))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "sandbox: cleanup encountered reparse point: {}",
                path.display()
            ));
        }
        let directory = metadata.is_dir();
        let file = open_stable_path(
            &path,
            directory,
            READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        update_access_ace(&file, directory, sid, REVOKE_ACCESS, 0)?;
        if directory {
            for entry in std::fs::read_dir(&path)
                .map_err(|error| format!("sandbox: cleanup enumerate: {error}"))?
            {
                pending.push(
                    entry
                        .map_err(|error| format!("sandbox: cleanup enumerate: {error}"))?
                        .path(),
                );
            }
        }
    }
    Ok(())
}

fn update_access_ace(
    file: &File,
    directory: bool,
    sid: PSID,
    mode: windows_sys::Win32::Security::Authorization::ACCESS_MODE,
    permissions: u32,
) -> Result<(), String> {
    let permissions = if mode == REVOKE_ACCESS {
        0
    } else {
        permissions
    };
    let inheritance = if directory {
        (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u32
    } else {
        0
    };
    update_handle_ace(
        file.as_raw_handle(),
        SE_FILE_OBJECT,
        sid,
        mode,
        permissions,
        inheritance,
    )
}

fn update_handle_ace(
    handle: HANDLE,
    object_type: SE_OBJECT_TYPE,
    sid: PSID,
    mode: windows_sys::Win32::Security::Authorization::ACCESS_MODE,
    permissions: u32,
    inheritance: u32,
) -> Result<(), String> {
    // SetEntriesInAclW operates on a DACL snapshot. Serialize the complete read/modify/write
    // transaction across helper processes so concurrent launch and cleanup cannot lose an ACE.
    let _mutation = AclMutationGuard::acquire()?;
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle,
            object_type,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 || descriptor.is_null() || dacl.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor) };
        }
        return Err(format!("sandbox: read handle-bound DACL: code {result}"));
    }
    let _descriptor = Local(descriptor);
    let trustee = TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid.cast(),
    };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: permissions,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: trustee,
    };
    let mut updated = null_mut();
    let result = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut updated) };
    if result != 0 || updated.is_null() {
        if !updated.is_null() {
            unsafe { LocalFree(updated.cast()) };
        }
        return Err(format!(
            "sandbox: construct handle-bound write DACL: code {result}"
        ));
    }
    let _updated = Local(updated.cast());
    let result = unsafe {
        SetSecurityInfo(
            handle,
            object_type,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            updated,
            null_mut(),
        )
    };
    if result != 0 {
        return Err(format!(
            "sandbox: commit handle-bound write DACL: code {result}"
        ));
    }
    Ok(())
}

fn private_desktop(sid: PSID) -> Result<PrivateDesktop, String> {
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return Err(last_error("get sandbox helper window station"));
    }
    let station_name = user_object_name(station, "sandbox window-station")?;
    let desktop_name = format!("mini-agent-{}", uuid::Uuid::new_v4());
    let desktop_name_wide = wide_string(&desktop_name);
    let child_desired = DESKTOP_CREATEWINDOW | DESKTOP_READOBJECTS;
    let creator_desired = child_desired | READ_CONTROL | WRITE_DAC;
    let handle = unsafe {
        CreateDesktopW(
            desktop_name_wide.as_ptr(),
            null(),
            null(),
            0,
            creator_desired,
            null(),
        )
    };
    if handle.is_null() {
        return Err(last_error("create private sandbox desktop"));
    }
    let desktop = PrivateDesktop {
        handle,
        name: desktop_name.clone(),
        startup_name: wide_string(&format!("{station_name}\\{desktop_name}")),
    };
    update_handle_ace(
        desktop.handle,
        SE_WINDOW_OBJECT,
        sid,
        GRANT_ACCESS,
        child_desired,
        0,
    )?;
    Ok(desktop)
}

fn user_object_name(handle: HANDLE, label: &str) -> Result<String, String> {
    let mut bytes = 0u32;
    unsafe {
        GetUserObjectInformationW(handle, UOI_NAME, null_mut(), 0, &mut bytes);
    }
    if bytes < 2 {
        return Err(last_error(&format!("size {label} name")));
    }
    let mut name = vec![0u16; (bytes as usize).div_ceil(size_of::<u16>())];
    if unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(last_error(&format!("read {label} name")));
    }
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..end]))
}

fn create_appcontainer_profile(
    cache: &Path,
    read_roots: &[PathBuf],
    write_roots: &[PathBuf],
    job_name: &str,
) -> Result<AppContainerProfile, String> {
    let journal_root = profile_journal_root(cache, read_roots, write_roots)?;
    sweep_stale_profiles(&journal_root)?;
    let name_text = format!(
        "{APPCONTAINER_PROFILE_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    );
    let name = wide_string(&name_text);
    let display = wide_string("mini-agent general sandbox");
    let description = wide_string("Ephemeral workspace-capable mini-agent sandbox");
    let mut sid = null_mut();
    let result = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if result < 0 || sid.is_null() {
        return Err(format!(
            "sandbox: create unique AppContainer profile: HRESULT {result:#x}"
        ));
    }
    let mut profile = AppContainerProfile {
        sid,
        name,
        name_text: name_text.clone(),
        text: String::new(),
        storage: PathBuf::new(),
        journal_path: PathBuf::new(),
        journal_lease: None,
    };
    let setup = (|| -> Result<(PathBuf, File), String> {
        let mut derived = null_mut();
        let derived_result = unsafe {
            DeriveAppContainerSidFromAppContainerName(profile.name.as_ptr(), &mut derived)
        };
        if derived_result < 0 || derived.is_null() {
            return Err(format!(
                "sandbox: derive created AppContainer SID: HRESULT {derived_result:#x}"
            ));
        }
        struct DerivedSid(PSID);
        impl Drop for DerivedSid {
            fn drop(&mut self) {
                unsafe { FreeSid(self.0) };
            }
        }
        let derived = DerivedSid(derived);
        let derived_text = sid_text(derived.0)?;
        profile.text = sid_text(profile.sid)?;
        if derived_text != profile.text {
            return Err(
                "sandbox: created AppContainer SID differed from derived profile SID".into(),
            );
        }
        profile.storage = appcontainer_storage_path(&profile.text)?;
        std::fs::create_dir_all(&profile.storage).map_err(|error| {
            format!(
                "sandbox: create private AppContainer storage {}: {error}",
                profile.storage.display()
            )
        })?;
        let mut roots = read_roots.to_vec();
        roots.extend(write_roots.iter().cloned());
        roots.sort();
        roots.dedup();
        let journal = ProfileJournal {
            version: PROFILE_JOURNAL_VERSION,
            profile_name: name_text,
            sid: profile.text.clone(),
            job_name: job_name.to_string(),
            roots,
        };
        let payload = serde_json::to_vec(&journal)
            .map_err(|error| format!("sandbox: encode AppContainer cleanup journal: {error}"))?;
        let journal_path = journal_root.join(format!("{}.json", uuid::Uuid::new_v4().simple()));
        let mut lease = create_profile_journal(&journal_path)?;
        let durable = lease
            .write_all(&payload)
            .map_err(|error| format!("sandbox: write AppContainer cleanup journal: {error}"))
            .and_then(|_| {
                lease
                    .sync_all()
                    .map_err(|error| format!("sandbox: sync AppContainer cleanup journal: {error}"))
            });
        if let Err(error) = durable {
            drop(lease);
            let _ = std::fs::remove_file(&journal_path);
            return Err(error);
        }
        Ok((journal_path, lease))
    })();
    match setup {
        Ok((journal_path, lease)) => {
            profile.journal_path = journal_path;
            profile.journal_lease = Some(lease);
            Ok(profile)
        }
        Err(error) => {
            let rollback = profile.rollback_unjournaled();
            Err(match rollback {
                Ok(()) => error,
                Err(rollback) => format!("{error}; unjournaled rollback failed: {rollback}"),
            })
        }
    }
}

fn appcontainer_storage_path(sid: &str) -> Result<PathBuf, String> {
    let sid = wide_string(sid);
    let mut path = null_mut();
    let result = unsafe { GetAppContainerFolderPath(sid.as_ptr(), &mut path) };
    if result < 0 || path.is_null() {
        return Err(format!(
            "sandbox: resolve private AppContainer storage: HRESULT {result:#x}"
        ));
    }
    let mut length = 0usize;
    while unsafe { *path.add(length) } != 0 {
        length += 1;
        if length > 32_767 {
            unsafe { CoTaskMemFree(path.cast()) };
            return Err("sandbox: private AppContainer storage path exceeded bound".into());
        }
    }
    let storage = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(path, length)
    }));
    unsafe { CoTaskMemFree(path.cast()) };
    Ok(storage)
}

fn profile_journal_root(
    cache: &Path,
    read_roots: &[PathBuf],
    write_roots: &[PathBuf],
) -> Result<PathBuf, String> {
    let parent = cache
        .parent()
        .ok_or("sandbox: application cache has no private control parent")?;
    let root = parent.join(".mini-agent-appcontainer-control-v1");
    std::fs::create_dir_all(&root).map_err(|error| {
        format!(
            "sandbox: create AppContainer cleanup journal root {}: {error}",
            root.display()
        )
    })?;
    let root = canonical_root(&root, "AppContainer cleanup journal root")?;
    if read_roots
        .iter()
        .chain(write_roots)
        .any(|granted| paths_overlap(&root, granted))
    {
        return Err("sandbox: AppContainer cleanup journal overlaps a granted root".into());
    }
    protect_and_attest_control_root(&root)?;
    Ok(root)
}

fn current_user_sid_buffer() -> Result<Vec<usize>, String> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(last_error("open current token for control-root owner"));
    }
    let token = Handle::created(raw, "open current token for control-root owner")?;
    let mut bytes = 0u32;
    unsafe { GetTokenInformation(token.raw(), TokenUser, null_mut(), 0, &mut bytes) };
    if bytes < size_of::<TOKEN_USER>() as u32 || bytes > 64 * 1024 {
        return Err("sandbox: current token user SID size was invalid".into());
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(last_error("read current token user SID"));
    }
    Ok(buffer)
}

fn token_user_sid(buffer: &[usize]) -> PSID {
    unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
}

fn protect_and_attest_control_root(root: &Path) -> Result<(), String> {
    let directory = open_stable_path(
        root,
        true,
        READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    let user = current_user_sid_buffer()?;
    let user_sid = token_user_sid(&user);
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u32,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: user_sid.cast(),
        },
    };
    let mut dacl = null_mut();
    let result = unsafe { SetEntriesInAclW(1, &entry, null(), &mut dacl) };
    if result != 0 || dacl.is_null() {
        return Err(format!(
            "sandbox: construct private AppContainer control DACL: code {result}"
        ));
    }
    let dacl_allocation = Local(dacl.cast());
    let result = unsafe {
        SetSecurityInfo(
            directory.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            user_sid,
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if result != 0 {
        return Err(format!(
            "sandbox: commit private AppContainer control DACL: code {result}"
        ));
    }
    drop(dacl_allocation);

    let mut owner = null_mut();
    let mut observed_dacl = null_mut();
    let mut descriptor = null_mut();
    let result = unsafe {
        GetSecurityInfo(
            directory.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut observed_dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 || owner.is_null() || observed_dacl.is_null() || descriptor.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor) };
        }
        return Err(format!(
            "sandbox: attest private AppContainer control DACL: code {result}"
        ));
    }
    let _descriptor = Local(descriptor);
    if unsafe { EqualSid(owner, user_sid) } == 0 {
        return Err("sandbox: AppContainer control root owner differed from current user".into());
    }
    let mut count = 0u32;
    let mut entries = null_mut();
    let result = unsafe { GetExplicitEntriesFromAclW(observed_dacl, &mut count, &mut entries) };
    if result != 0 || count != 1 || entries.is_null() {
        if !entries.is_null() {
            unsafe { LocalFree(entries.cast()) };
        }
        return Err("sandbox: AppContainer control root DACL was not owner-only".into());
    }
    let entries_allocation = Local(entries.cast());
    let observed = unsafe { &*entries };
    let valid = observed.grfAccessPermissions == FILE_ALL_ACCESS
        && observed.Trustee.TrusteeForm == TRUSTEE_IS_SID
        && !observed.Trustee.ptstrName.is_null()
        && unsafe { EqualSid(observed.Trustee.ptstrName.cast(), user_sid) } != 0;
    drop(entries_allocation);
    if !valid {
        return Err("sandbox: AppContainer control root DACL attestation failed".into());
    }
    Ok(())
}

fn create_profile_journal(path: &Path) -> Result<File, String> {
    let wide = wide_null(path.as_os_str())?;
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    let handle = Handle::created(raw, "create exclusive AppContainer cleanup journal")?;
    Ok(unsafe { File::from_raw_handle(handle.0.into_raw_handle()) })
}

fn open_stale_profile_journal(path: &Path) -> Result<Option<File>, String> {
    let wide = wide_null(path.as_os_str())?;
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | FILE_READ_ATTRIBUTES,
            0,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw.is_null() || raw == (-1isize as HANDLE) {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(32) {
            return Ok(None);
        }
        return Err(format!(
            "sandbox: open stale AppContainer cleanup journal: {error}"
        ));
    }
    Ok(Some(unsafe { File::from_raw_handle(raw) }))
}

fn sweep_stale_profiles(journal_root: &Path) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(journal_root)
        .map_err(|error| format!("sandbox: enumerate AppContainer cleanup journals: {error}"))?
    {
        if entries.len() >= MAX_STALE_PROFILE_JOURNALS {
            return Err("sandbox: stale AppContainer journal count exceeds 64".into());
        }
        entries.push(
            entry
                .map_err(|error| format!("sandbox: enumerate AppContainer journal: {error}"))?
                .path(),
        );
    }
    entries.sort();
    for path in entries {
        if path.extension() != Some(OsStr::new("json")) {
            return Err("sandbox: unexpected entry in AppContainer cleanup journal root".into());
        }
        let Some(mut lease) = open_stale_profile_journal(&path)? else {
            continue;
        };
        let metadata = lease
            .metadata()
            .map_err(|error| format!("sandbox: inspect AppContainer cleanup journal: {error}"))?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || crate::fs::windows_file_link_count(&lease)
                .map_err(|error| format!("sandbox: inspect cleanup-journal links: {error}"))?
                != 1
            || metadata.len() > 64 * 1024
        {
            return Err("sandbox: invalid AppContainer cleanup journal object".into());
        }
        let mut payload = Vec::with_capacity(metadata.len() as usize);
        lease
            .read_to_end(&mut payload)
            .map_err(|error| format!("sandbox: read AppContainer cleanup journal: {error}"))?;
        let journal: ProfileJournal = serde_json::from_slice(&payload)
            .map_err(|error| format!("sandbox: decode AppContainer cleanup journal: {error}"))?;
        if journal.version != PROFILE_JOURNAL_VERSION
            || !journal
                .profile_name
                .starts_with(APPCONTAINER_PROFILE_PREFIX)
            || journal.profile_name.len() != APPCONTAINER_PROFILE_PREFIX.len() + 32
            || !journal.job_name.starts_with(JOB_NAME_PREFIX)
            || journal.job_name.len() != JOB_NAME_PREFIX.len() + 32
            || journal.roots.is_empty()
            || journal.roots.len() > MAX_ACCESS_ROOTS
        {
            return Err("sandbox: invalid AppContainer cleanup journal policy".into());
        }
        let name = wide_string(&journal.profile_name);
        let mut sid = null_mut();
        let result = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if result < 0 || sid.is_null() {
            return Err(format!(
                "sandbox: derive stale AppContainer SID: HRESULT {result:#x}"
            ));
        }
        struct Sid(PSID);
        impl Drop for Sid {
            fn drop(&mut self) {
                unsafe { FreeSid(self.0) };
            }
        }
        let sid = Sid(sid);
        if sid_text(sid.0)? != journal.sid {
            return Err("sandbox: stale AppContainer journal SID mismatch".into());
        }
        wait_for_stale_job_quiescence(&journal.job_name)?;
        let roots =
            canonicalize_access_roots(journal.roots.iter().map(PathBuf::as_path), journal_root)?;
        for root in roots {
            revoke_tree(&root, sid.0)?;
        }
        delete_appcontainer_profile(&name)?;
        drop(lease);
        std::fs::remove_file(&path)
            .map_err(|error| format!("sandbox: remove stale AppContainer journal: {error}"))?;
    }
    Ok(())
}

fn sid_text(sid: PSID) -> Result<String, String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return Err(last_error("render AppContainer SID"));
    }
    let allocation = Local(text.cast());
    let mut length = 0usize;
    while unsafe { *text.add(length) } != 0 {
        length += 1;
        if length > 256 {
            return Err("sandbox: AppContainer SID text exceeded bound".into());
        }
    }
    let rendered = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    drop(allocation);
    Ok(rendered)
}

fn primary_token() -> Result<Handle, String> {
    let mut raw = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut raw,
        )
    } == 0
    {
        return Err(last_error(
            "open current primary token for AppContainer launch",
        ));
    }
    Handle::created(raw, "open current primary token for AppContainer launch")
}

fn bounded_job() -> Result<(Handle, String), String> {
    let name = format!("{JOB_NAME_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let wide_name = wide_string(&name);
    let user = current_user_sid_buffer()?;
    let user_sid = token_user_sid(&user);
    if user_sid.is_null() {
        return Err("sandbox: current token user SID was null".into());
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: SET_ACCESS,
        grfInheritance: 0,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: user_sid.cast(),
        },
    };
    let mut dacl = null_mut();
    let result = unsafe { SetEntriesInAclW(1, &entry, null(), &mut dacl) };
    if result != 0 || dacl.is_null() {
        return Err(format!(
            "sandbox: construct owner-only named Job DACL: code {result}"
        ));
    }
    let _dacl = Local(dacl.cast());
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    if unsafe {
        InitializeSecurityDescriptor(
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            SECURITY_DESCRIPTOR_REVISION,
        )
    } == 0
        || unsafe {
            SetSecurityDescriptorOwner(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                user_sid,
                0,
            )
        } == 0
        || unsafe {
            SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                TRUE,
                dacl,
                0,
            )
        } == 0
    {
        return Err(last_error(
            "construct owner-only named Job security descriptor",
        ));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    let raw = unsafe { CreateJobObjectW(&attributes, wide_name.as_ptr()) };
    let creation_error = std::io::Error::last_os_error();
    let job = Handle::created(raw, "create sandbox Job")?;
    if creation_error.raw_os_error() == Some(183) {
        return Err("sandbox: unique Job name unexpectedly already existed".into());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME;
    limits.BasicLimitInformation.ActiveProcessLimit = MAX_JOB_PROCESSES;
    limits.BasicLimitInformation.PerProcessUserTimeLimit = PROCESS_CPU_100NS;
    limits.ProcessMemoryLimit = PROCESS_MEMORY_BYTES;
    limits.JobMemoryLimit = JOB_MEMORY_BYTES;
    if unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(last_error("configure bounded sandbox Job"));
    }
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_ALL,
    };
    if unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectBasicUIRestrictions,
            (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
            size_of_val(&ui) as u32,
        )
    } == 0
    {
        return Err(last_error("configure sandbox Job UI restrictions"));
    }
    Ok((job, name))
}

fn verify_job_membership_and_limits(job: &Handle, child: &Handle) -> Result<(), String> {
    let mut in_job = 0;
    if unsafe { IsProcessInJob(child.raw(), job.raw(), &mut in_job) } == 0 {
        return Err(last_error("query exact restricted process Job membership"));
    }
    if in_job == 0 {
        return Err("sandbox: restricted process escaped its exact creation-time Job".into());
    }

    verify_job_limits(job)
}

fn verify_descendant_rendezvous(
    job: &Handle,
    target: &Handle,
    ready: &Path,
    release: &Path,
) -> Result<(), String> {
    wait_for_descendant_identity(ready)?;
    let metadata = std::fs::symlink_metadata(ready)
        .map_err(|error| format!("inspect descendant identity proof: {error}"))?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.len() > DESCENDANT_IDENTITY_MAX_BYTES as u64
    {
        return Err("sandbox: descendant identity proof object was invalid".into());
    }
    let mut proof_file = open_stable_path(
        ready,
        false,
        GENERIC_READ | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ,
    )?;
    if crate::fs::windows_file_link_count(&proof_file)
        .map_err(|error| format!("inspect descendant identity proof links: {error}"))?
        != 1
    {
        return Err("sandbox: descendant identity proof was multiply linked".into());
    }
    let mut proof = Vec::with_capacity(DESCENDANT_IDENTITY_MAX_BYTES + 1);
    Read::by_ref(&mut proof_file)
        .take((DESCENDANT_IDENTITY_MAX_BYTES + 1) as u64)
        .read_to_end(&mut proof)
        .map_err(|error| format!("read descendant identity proof: {error}"))?;
    if proof.len() > DESCENDANT_IDENTITY_MAX_BYTES {
        return Err("sandbox: descendant identity proof exceeded bound".into());
    }
    let (pid, created) = parse_descendant_identity(&proof)?;
    if pid == unsafe { GetProcessId(target.raw()) } {
        return Err("descendant identity proof did not identify a distinct process".into());
    }
    let descendant = Handle::created(
        unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) },
        "open exact descendant identity",
    )?;
    if process_creation_time(descendant.raw())? != created {
        return Err("descendant process identity changed before exact Job proof".into());
    }
    let mut in_job = 0;
    if unsafe { IsProcessInJob(descendant.raw(), job.raw(), &mut in_job) } == 0 || in_job == 0 {
        return Err("sandbox: descendant escaped the exact launcher Job".into());
    }
    verify_job_limits(job)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(release)
        .and_then(|mut file| file.write_all(b"release\n"))
        .map_err(|error| format!("release exact-Job descendant probe: {error}"))?;
    Ok(())
}

fn parse_descendant_identity(proof: &[u8]) -> Result<(u32, u64), String> {
    if proof.len() > DESCENDANT_IDENTITY_MAX_BYTES {
        return Err("sandbox: descendant identity proof exceeded bound".into());
    }
    let proof = std::str::from_utf8(proof)
        .map_err(|_| "sandbox: descendant identity proof was not UTF-8".to_string())?;
    let mut lines = proof.lines();
    let pid = lines
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .ok_or("descendant identity PID was invalid")?;
    let created = lines
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|created| *created != 0)
        .ok_or("descendant creation identity was invalid")?;
    if lines.next().is_some() {
        return Err("descendant identity proof contained trailing data".into());
    }
    Ok((pid, created))
}

fn wait_for_descendant_identity(path: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if bounded_probe_contents(path, DESCENDANT_IDENTITY_MAX_BYTES)
            .is_some_and(|contents| parse_descendant_identity(&contents).is_ok())
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(format!(
        "timed out waiting for descendant identity: {}",
        path.display()
    ))
}

fn verify_job_limits(job: &Handle) -> Result<(), String> {
    let expected_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(last_error("query restricted process Job limits"));
    }
    if limits.BasicLimitInformation.LimitFlags != expected_flags
        || limits.BasicLimitInformation.ActiveProcessLimit != MAX_JOB_PROCESSES
        || limits.BasicLimitInformation.PerProcessUserTimeLimit != PROCESS_CPU_100NS
        || limits.ProcessMemoryLimit != PROCESS_MEMORY_BYTES
        || limits.JobMemoryLimit != JOB_MEMORY_BYTES
    {
        return Err("sandbox: restricted process Job limits differ from policy".into());
    }

    let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
    if unsafe {
        QueryInformationJobObject(
            job.raw(),
            JobObjectBasicUIRestrictions,
            (&mut ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
            size_of_val(&ui) as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(last_error("query restricted process Job UI limits"));
    }
    if ui.UIRestrictionsClass != JOB_OBJECT_UILIMIT_ALL {
        return Err("sandbox: restricted process Job UI limits differ from policy".into());
    }
    Ok(())
}

fn wait_for_stale_job_quiescence(name: &str) -> Result<(), String> {
    if !name.starts_with(JOB_NAME_PREFIX) || name.len() != JOB_NAME_PREFIX.len() + 32 {
        return Err("sandbox: invalid stale AppContainer Job name".into());
    }
    let name = wide_string(name);
    let raw = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(2) {
            // A named Job ceases to be openable only after its final handle is closed. With
            // KILL_ON_JOB_CLOSE and no target Job handle inheritance, that final close starts and
            // owns whole-tree termination; if the object is already gone, no assigned process
            // remains whose lifetime could race ACL/profile cleanup.
            return Ok(());
        }
        return Err(format!(
            "sandbox: open exact stale AppContainer Job: {error}"
        ));
    }
    let job = Handle::created(raw, "open exact stale AppContainer Job")?;
    verify_job_limits(&job)?;
    if unsafe { TerminateJobObject(job.raw(), STALE_JOB_CLEANUP_EXIT_CODE) } == 0 {
        return Err(last_error("terminate exact stale AppContainer Job"));
    }
    wait_for_job_zero(&job, "stale AppContainer Job")
}

fn wait_for_job_zero(job: &Handle, label: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of_val(&accounting) as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error(&format!("query drained {label}")));
        }
        if accounting.ActiveProcesses == 0 {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("sandbox: {label} did not drain all descendants"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn terminate_and_drain_job(job: &Handle, exit_code: u32) -> Result<(), String> {
    if unsafe { TerminateJobObject(job.raw(), exit_code) } == 0 {
        return Err(last_error("terminate exact AppContainer Job"));
    }
    wait_for_job_zero(job, "AppContainer Job")
}

fn launch_appcontainer(
    token: &Handle,
    job: &Handle,
    desktop: &PrivateDesktop,
    appcontainer_sid: PSID,
    program: &Path,
    arguments: &[String],
    cwd: &Path,
    cache: &Path,
    private_storage: &Path,
) -> Result<Handle, String> {
    let _creation = crate::process_creation::creation_guard()
        .map_err(|error| format!("lock Windows process creation: {error}"))?;
    let stdout = inheritable_duplicate(std::io::stdout().as_raw_handle())?;
    let stderr = inheritable_duplicate(std::io::stderr().as_raw_handle())?;
    let stdin = inheritable_null_input()?;
    let handles = [stdin.raw(), stdout.raw(), stderr.raw()];
    let jobs = [job.raw()];
    let mut bytes = 0usize;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 4, 0, &mut bytes) };
    if bytes == 0 {
        return Err(last_error("size restricted process attribute list"));
    }
    let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let list = storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(list, 4, 0, &mut bytes) } == 0 {
        return Err(last_error("initialize restricted process attribute list"));
    }
    struct DeleteList(windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for DeleteList {
        fn drop(&mut self) {
            unsafe { windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _list = DeleteList(list);
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast_mut().cast(),
            size_of_val(&handles),
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err(last_error("set exact restricted process handle list"));
    }
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr().cast_mut().cast(),
            size_of_val(&jobs),
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err(last_error("set creation-time restricted process Job"));
    }
    // An empty capability list is intentional. Do not add internetClient (or any other network
    // capability): the AppContainer network boundary is default-deny.
    let mut security_capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: appcontainer_sid,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&mut security_capabilities as *mut SECURITY_CAPABILITIES).cast(),
            size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err(last_error("set AppContainer security capabilities"));
    }
    let mut all_application_packages_policy = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY as usize,
            (&mut all_application_packages_policy as *mut u32).cast(),
            size_of::<u32>(),
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err(last_error("opt out of ALL APPLICATION PACKAGES authority"));
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.raw();
    startup.StartupInfo.hStdOutput = stdout.raw();
    startup.StartupInfo.hStdError = stderr.raw();
    startup.StartupInfo.lpDesktop = desktop.startup_name.as_ptr().cast_mut();
    startup.lpAttributeList = list;
    let application = wide_null(program.as_os_str())?;
    let command_line = windows_command_line(program, arguments);
    if command_line.encode_utf16().count() >= 32_767 {
        return Err(
            "sandbox: restricted process command line exceeds the Windows UTF-16 bound".into(),
        );
    }
    let mut command_line = wide_string(&command_line);
    let cwd = wide_null(cwd.as_os_str())?;
    let environment = appcontainer_environment(cache, private_storage);
    let mut information = PROCESS_INFORMATION::default();
    if unsafe {
        CreateProcessAsUserW(
            token.raw(),
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut information,
        )
    } == 0
    {
        return Err(last_error("launch creation-time-Job AppContainer process"));
    }
    unsafe { CloseHandle(information.hThread) };
    Handle::created(information.hProcess, "own AppContainer process")
}

fn inheritable_duplicate(source: *mut c_void) -> Result<Handle, String> {
    let mut duplicate = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            TRUE,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(last_error("duplicate restricted child standard handle"));
    }
    Handle::created(duplicate, "duplicate restricted child standard handle")
}

fn inheritable_null_input() -> Result<Handle, String> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: TRUE,
    };
    let nul = wide_string("NUL");
    Handle::created(
        unsafe {
            CreateFileW(
                nul.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &mut attributes,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        },
        "open restricted child NUL stdin",
    )
}

fn inheritable_omitted_canary() -> Result<Handle, String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: TRUE,
    };
    Handle::created(
        unsafe { CreateEventW(&attributes, 1, 0, null()) },
        "create omitted inheritable-handle canary",
    )
}

fn appcontainer_environment(cache: &Path, private_storage: &Path) -> Vec<u16> {
    let mut entries = essential_windows_environment();
    let private_storage = private_storage.as_os_str().to_string_lossy().into_owned();
    let cache = cache.as_os_str().to_string_lossy().into_owned();
    entries.push(("TEMP".into(), private_storage.clone()));
    entries.push(("TMP".into(), private_storage));
    entries.push(("ZS_CACHE_DIR".into(), cache));
    entries.sort_by(|a, b| a.0.to_ascii_uppercase().cmp(&b.0.to_ascii_uppercase()));
    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(wide_string(&format!("{name}={value}")));
    }
    block.push(0);
    block
}

fn essential_windows_environment() -> Vec<(String, String)> {
    [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "LANG",
        "LC_ALL",
        "TERM",
        "NO_COLOR",
    ]
    .iter()
    .filter_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| ((*name).to_string(), value))
    })
    .collect()
}

fn windows_command_line(program: &Path, arguments: &[String]) -> String {
    std::iter::once(program.as_os_str().to_string_lossy().into_owned())
        .chain(arguments.iter().cloned())
        .map(|argument| quote_windows_argument(&argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return argument.to_string();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0usize;
    for ch in argument.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            quoted.push_str(&"\\".repeat(slashes * 2 + 1));
            quoted.push('"');
        } else {
            quoted.push_str(&"\\".repeat(slashes));
            quoted.push(ch);
        }
        slashes = 0;
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}

fn open_and_verify_parent(pid: u32, created: u64) -> Result<Handle, String> {
    let parent = Handle::created(
        unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) },
        "open sandbox parent process",
    )?;
    if process_creation_time(parent.raw())? != created {
        return Err("sandbox parent PID identity changed before helper startup".into());
    }
    Ok(parent)
}

fn ensure_parent_alive(parent: &Handle) -> Result<(), String> {
    match unsafe { WaitForSingleObject(parent.raw(), 0) } {
        WAIT_TIMEOUT => Ok(()),
        WAIT_OBJECT_0 => Err("sandbox parent exited before restricted launch completed".into()),
        _ => Err(last_error("check sandbox parent liveness")),
    }
}

fn process_creation_time(process: HANDLE) -> Result<u64, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(last_error("read process creation identity"));
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

pub(crate) fn terminate_helper(pid: u32) {
    let process = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return;
    }
    if let Ok(process) = Handle::created(process, "open sandbox helper for termination") {
        unsafe { TerminateProcess(process.raw(), 125) };
        let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
    }
}

fn run_runtime_probe() -> Result<i32, String> {
    let base = std::env::temp_dir().join(format!(
        "mini-agent-windows-sandbox-{}",
        uuid::Uuid::new_v4()
    ));
    let workspace = base.join("workspace");
    let workspace_b = base.join("workspace-b");
    let cache = base.join("cache");
    let outside = base.join("outside");
    std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&workspace_b).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&outside).map_err(|e| e.to_string())?;
    let inside_file = workspace.join("inside.txt");
    let outside_file = outside.join("outside.txt");
    let outside_secret = outside.join("secret.txt");
    let cache_fixture = cache.join("read-only-cache.txt");
    std::fs::write(&outside_secret, b"outside-secret").map_err(|e| e.to_string())?;
    std::fs::write(&cache_fixture, b"cache-readable").map_err(|e| e.to_string())?;
    let script = format!(
        "$ErrorActionPreference='Stop'; Set-Content -LiteralPath {} -Value inside; if ((Get-Content -LiteralPath {}).Trim() -ne 'cache-readable') {{ exit 40 }}; try {{ Get-Content -LiteralPath {} | Out-Null; exit 41 }} catch {{}}; try {{ Set-Content -LiteralPath {} -Value outside; exit 42 }} catch {{ exit 0 }}",
        powershell_literal(&inside_file)?,
        powershell_literal(&cache_fixture)?,
        powershell_literal(&outside_secret)?,
        powershell_literal(&outside_file)?
    );
    let powershell = resolve_program("powershell.exe", &workspace)?;
    let cleanup_ready = workspace.join("cleanup-ready.txt");
    let mut command = build_helper_with_ready(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        &workspace,
        &cache,
        Some(cleanup_ready.clone()),
    )?;
    let output = command
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run write-boundary probe: {e}"))?;
    if !output.status.success() || !inside_file.exists() || outside_file.exists() {
        return Err(format!(
            "explicit read/write boundary probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    attest_completed_cleanup(&cleanup_ready, [&workspace, &cache, powershell.as_path()])?;

    let configured_tool = resolve_program("cmd.exe", &workspace)?;
    let configured_read = base.join("configured-read");
    let configured_write = base.join("configured-write");
    std::fs::create_dir_all(&configured_read).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&configured_write).map_err(|e| e.to_string())?;
    let configured_fixture = configured_read.join("fixture.txt");
    let configured_output = configured_write.join("output.txt");
    let configured_cleanup_ready = workspace.join("configured-cleanup-ready.txt");
    std::fs::write(&configured_fixture, b"configured-read").map_err(|e| e.to_string())?;
    let configured_script = format!(
        "$ErrorActionPreference='Stop'; if ((Get-Content -LiteralPath {}).Trim() -ne 'configured-read') {{ exit 51 }}; & {} /c exit 0; if ($LASTEXITCODE -ne 0) {{ exit 52 }}; Set-Content -LiteralPath {} -Value configured-write",
        powershell_literal(&configured_fixture)?,
        powershell_literal(&configured_tool)?,
        powershell_literal(&configured_output)?,
    );
    let mut configured_launch = build_helper_with_ready_and_roots(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            configured_script,
        ],
        &workspace,
        &cache,
        Some(configured_cleanup_ready.clone()),
        &[configured_read.clone(), configured_tool.clone()],
        std::slice::from_ref(&configured_write),
    )?;
    if !configured_launch
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run configured AppContainer tool/root probe: {e}"))?
        .status
        .success()
        || !configured_output.exists()
    {
        return Err("configured AppContainer tool/root probe failed".into());
    }
    attest_completed_cleanup(
        &configured_cleanup_ready,
        [
            workspace.as_path(),
            cache.as_path(),
            powershell.as_path(),
            configured_read.as_path(),
            configured_write.as_path(),
            configured_tool.as_path(),
        ],
    )?;

    let hardlink_source = workspace.join("hardlink-source.txt");
    let hardlink_alias = workspace.join("hardlink-alias.txt");
    std::fs::write(&hardlink_source, b"hardlink").map_err(|e| e.to_string())?;
    std::fs::hard_link(&hardlink_source, &hardlink_alias).map_err(|e| e.to_string())?;
    let mut hardlink = build_helper(
        powershell.clone(),
        vec!["-NoProfile".into(), "-Command".into(), "exit 0".into()],
        &workspace,
        &cache,
    )?;
    if hardlink
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run hardlink probe: {e}"))?
        .status
        .success()
    {
        return Err("multi-link writable file was not rejected".into());
    }
    std::fs::remove_file(&hardlink_alias).map_err(|e| e.to_string())?;
    std::fs::remove_file(&hardlink_source).map_err(|e| e.to_string())?;

    let swap_victim = workspace.join("swap-victim.txt");
    let swap_ready = workspace.join("swap-ready.txt");
    std::fs::write(&swap_victim, b"stable").map_err(|e| e.to_string())?;
    let mut swap_command = build_helper_with_ready(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 2; exit 0".into(),
        ],
        &workspace,
        &cache,
        Some(swap_ready.clone()),
    )?;
    let mut swap = swap_command
        .as_std_mut()
        .spawn_guarded()
        .map_err(|e| format!("start stable-handle swap probe: {e}"))?;
    wait_for_probe_file(&swap_ready)?;
    if std::fs::rename(&swap_victim, workspace.join("swap-moved.txt")).is_ok() {
        let _ = swap.kill();
        return Err("stable ACL handle allowed an in-flight path swap".into());
    }
    if !swap
        .wait()
        .map_err(|e| format!("wait stable-handle swap probe: {e}"))?
        .success()
    {
        return Err("stable-handle swap probe target failed".into());
    }

    let mut max_request = build_helper(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!("exit 0; #{}", "x".repeat(18_000)),
        ],
        &workspace,
        &cache,
    )?;
    if !max_request
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run bounded request-pipe probe: {e}"))?
        .status
        .success()
    {
        return Err("bounded request feeder failed with a near-limit request".into());
    }

    let concurrent_root = workspace.join("concurrent-acl");
    std::fs::create_dir_all(&concurrent_root).map_err(|e| e.to_string())?;
    for index in 0..64 {
        std::fs::write(concurrent_root.join(format!("seed-{index}.txt")), b"seed")
            .map_err(|e| e.to_string())?;
    }
    let concurrent_a = concurrent_root.join("a.txt");
    let concurrent_b = concurrent_root.join("b.txt");
    let concurrent_script = |path: &Path| -> Result<String, String> {
        Ok(format!(
            "Set-Content -LiteralPath {} -Value concurrent",
            powershell_literal(path)?
        ))
    };
    let mut concurrent_a_command = build_helper(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            concurrent_script(&concurrent_a)?,
        ],
        &workspace,
        &cache,
    )?;
    let mut concurrent_b_command = build_helper(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            concurrent_script(&concurrent_b)?,
        ],
        &workspace,
        &cache,
    )?;
    let mut concurrent_a_child = concurrent_a_command
        .as_std_mut()
        .spawn_guarded()
        .map_err(|e| format!("start first concurrent ACL probe: {e}"))?;
    let mut concurrent_b_child = concurrent_b_command
        .as_std_mut()
        .spawn_guarded()
        .map_err(|e| format!("start second concurrent ACL probe: {e}"))?;
    let concurrent_a_status = concurrent_a_child
        .wait()
        .map_err(|e| format!("wait first concurrent ACL probe: {e}"))?;
    let concurrent_b_status = concurrent_b_child
        .wait()
        .map_err(|e| format!("wait second concurrent ACL probe: {e}"))?;
    if !concurrent_a_status.success()
        || !concurrent_b_status.success()
        || !concurrent_a.exists()
        || !concurrent_b.exists()
    {
        return Err("cross-process ACL serialization probe failed".into());
    }

    let crash_ready = workspace.join("crash-ready.txt");
    let mut crashed_command = build_helper_with_ready(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 10".into(),
        ],
        &workspace,
        &cache,
        Some(crash_ready.clone()),
    )?;
    let mut crashed_helper = crashed_command
        .as_std_mut()
        .spawn_guarded()
        .map_err(|e| format!("start unique-SID crash probe: {e}"))?;
    wait_for_probe_file(&crash_ready)?;
    crashed_helper
        .kill()
        .map_err(|e| format!("crash unique-SID helper: {e}"))?;
    let _ = crashed_helper.wait();
    let crash_proof = parse_crash_cleanup_proof(&crash_ready)?;
    if !crash_proof.storage.exists() || !crash_proof.journal.exists() {
        return Err("crashed launch did not retain recoverable profile artifacts".into());
    }

    let workspace_b_file = workspace_b.join("inside-b.txt");
    let escaped_a = workspace.join("escaped-from-b.txt");
    let script_b = format!(
        "$ErrorActionPreference='Stop'; Set-Content -LiteralPath {} -Value inside; try {{ Set-Content -LiteralPath {} -Value escaped; exit 42 }} catch {{ exit 0 }}",
        powershell_literal(&workspace_b_file)?,
        powershell_literal(&escaped_a)?
    );
    let mut second_launch = build_helper(
        powershell.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script_b,
        ],
        &workspace_b,
        &cache,
    )?;
    if !second_launch
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run unique-SID sequential probe: {e}"))?
        .status
        .success()
        || !workspace_b_file.exists()
        || escaped_a.exists()
    {
        return Err("a crashed launch SID authorized a later workspace".into());
    }
    attest_cleanup_proof(&crash_proof, [&workspace, &cache, powershell.as_path()])?;

    let authority_escape = outside.join("authority-escape.txt");
    let mut authority_probe = build_helper(
        canonical_file(
            &std::env::current_exe().map_err(|e| e.to_string())?,
            "authority-probe executable",
        )?,
        vec![
            AUTHORITY_PROBE_ARG.into(),
            HELPER_PID_PLACEHOLDER.into(),
            unsafe { GetCurrentProcessId() }.to_string(),
            authority_escape.to_string_lossy().into_owned(),
            DESKTOP_NAME_PLACEHOLDER.into(),
            OMITTED_HANDLE_PLACEHOLDER.into(),
            DESCENDANT_READY_PLACEHOLDER.into(),
            CONTROL_ROOT_PLACEHOLDER.into(),
            DESCENDANT_RELEASE_PLACEHOLDER.into(),
        ],
        &workspace_b,
        &cache,
    )?;
    if !authority_probe
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run restricted authority probe: {e}"))?
        .status
        .success()
        || authority_escape.exists()
        || escaped_a.exists()
    {
        return Err("AppContainer target acquired launcher authority".into());
    }

    let marker = workspace.join("parent-death-marker.txt");
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut parent = Command::new(executable)
        .arg(PARENT_PROBE_ARG)
        .arg(&marker)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_guarded()
        .map_err(|e| format!("start parent-death probe: {e}"))?;
    let mut ready = [0u8; 6];
    parent
        .stdout
        .as_mut()
        .ok_or("parent probe stdout missing")?
        .read_exact(&mut ready)
        .map_err(|e| format!("wait for parent probe readiness: {e}"))?;
    if &ready != b"READY\n" {
        return Err("parent probe emitted invalid readiness".into());
    }
    parent
        .kill()
        .map_err(|e| format!("kill parent probe: {e}"))?;
    let _ = parent.wait();
    std::thread::sleep(std::time::Duration::from_secs(3));
    if marker.exists() {
        return Err("parent death did not kill the restricted Job tree".into());
    }
    let _ = std::fs::remove_dir_all(&base);
    println!(
        "WINDOWS_GENERAL_SANDBOX_PASS appcontainer=pass explicit_reads=pass configured_tool=pass workspace_write=pass outside_read=denied outside_write=denied hardlink=denied stable_handle_swap=denied unique_profile_crash=pass authority_escape=denied omitted_handle=denied descendant=contained breakaway=denied control_journal=denied bounded_pipe=pass acl_serialization=pass parent_death_job=pass private_desktop=pass ui_job=restricted network=denied registry=not_isolated"
    );
    Ok(0)
}

fn attest_completed_cleanup<'a>(
    ready: &Path,
    roots: impl IntoIterator<Item = &'a Path>,
) -> Result<(), String> {
    let proof = parse_cleanup_proof(ready)?;
    attest_cleanup_proof(&proof, roots)
}

fn parse_cleanup_proof(ready: &Path) -> Result<CleanupProof, String> {
    let proof = std::fs::read_to_string(ready)
        .map_err(|error| format!("read AppContainer cleanup proof: {error}"))?;
    let mut lines = proof.lines();
    if lines.next() != Some("TARGET_READY") {
        return Err("invalid AppContainer cleanup proof marker".into());
    }
    let sid = lines.next().ok_or("cleanup proof SID missing")?.to_string();
    if !sid.starts_with("S-1-15-2-") {
        return Err("cleanup proof SID was invalid".into());
    }
    let profile_name = lines
        .next()
        .ok_or("cleanup proof profile missing")?
        .to_string();
    let storage = PathBuf::from(lines.next().ok_or("cleanup proof storage missing")?);
    let journal = PathBuf::from(lines.next().ok_or("cleanup proof journal missing")?);
    if lines.next().is_some() {
        return Err("cleanup proof contained trailing data".into());
    }
    Ok(CleanupProof {
        sid,
        profile_name,
        storage,
        journal,
        job_name: None,
    })
}

fn parse_crash_cleanup_proof(ready: &Path) -> Result<CleanupProof, String> {
    let mut proof = parse_cleanup_proof(ready)?;
    let payload = std::fs::read(&proof.journal)
        .map_err(|error| format!("read crashed AppContainer recovery journal: {error}"))?;
    if payload.len() > 64 * 1024 {
        return Err("crashed AppContainer recovery journal exceeded bound".into());
    }
    let journal: ProfileJournal = serde_json::from_slice(&payload)
        .map_err(|error| format!("decode crashed AppContainer recovery journal: {error}"))?;
    if journal.version != PROFILE_JOURNAL_VERSION
        || journal.sid != proof.sid
        || journal.profile_name != proof.profile_name
        || !journal.job_name.starts_with(JOB_NAME_PREFIX)
    {
        return Err("crashed AppContainer recovery journal mismatched readiness proof".into());
    }
    proof.job_name = Some(journal.job_name);
    Ok(proof)
}

fn attest_cleanup_proof<'a>(
    proof: &CleanupProof,
    roots: impl IntoIterator<Item = &'a Path>,
) -> Result<(), String> {
    if proof.storage.exists() || proof.journal.exists() {
        return Err("AppContainer profile storage or recovery journal survived clean exit".into());
    }
    if let Some(job_name) = &proof.job_name {
        wait_for_stale_job_quiescence(job_name)?;
    }
    let sid_wide = wide_string(&proof.sid);
    let mut sid = null_mut();
    if unsafe { ConvertStringSidToSidW(sid_wide.as_ptr(), &mut sid) } == 0 || sid.is_null() {
        return Err("cleanup proof SID was invalid".into());
    }
    let sid = Local(sid);
    for root in roots {
        attest_tree_has_no_explicit_sid(root, sid.0)?;
    }
    let name = wide_string(&proof.profile_name);
    let display = wide_string("mini-agent cleanup attestation");
    let description = wide_string("temporary cleanup attestation profile");
    let mut sid = null_mut();
    let result = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if result < 0 || sid.is_null() {
        return Err(format!(
            "AppContainer profile survived clean exit: HRESULT {result:#x}"
        ));
    }
    unsafe { FreeSid(sid) };
    delete_appcontainer_profile(&name)?;
    Ok(())
}

fn attest_tree_has_no_explicit_sid(root: &Path, sid: PSID) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(path) = pending.pop() {
        seen += 1;
        if seen > MAX_ACL_ENTRIES {
            return Err("cleanup attestation traversal exceeded bound".into());
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect cleanup-attestation path: {error}"))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("cleanup attestation encountered reparse point".into());
        }
        if access_path_has_explicit_sid(&path, sid)? {
            return Err(format!(
                "AppContainer ACE survived clean exit on {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)
                .map_err(|error| format!("enumerate cleanup-attestation tree: {error}"))?
            {
                pending.push(
                    entry
                        .map_err(|error| format!("enumerate cleanup-attestation entry: {error}"))?
                        .path(),
                );
            }
        }
    }
    Ok(())
}

fn access_path_has_explicit_sid(path: &Path, sid: PSID) -> Result<bool, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect cleanup-attestation path: {error}"))?;
    let file = open_stable_path(
        path,
        metadata.is_dir(),
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )?;
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 || dacl.is_null() || descriptor.is_null() {
        return Err(format!("read cleanup-attestation DACL: code {result}"));
    }
    let _descriptor = Local(descriptor);
    let mut count = 0u32;
    let mut entries = null_mut();
    let result = unsafe { GetExplicitEntriesFromAclW(dacl, &mut count, &mut entries) };
    if result != 0 || count > 65_536 || (count != 0 && entries.is_null()) {
        return Err(format!("enumerate cleanup-attestation ACEs: code {result}"));
    }
    let _entries = Local(entries.cast());
    for index in 0..count as usize {
        let trustee = unsafe { &(*entries.add(index)).Trustee };
        if trustee.TrusteeForm == TRUSTEE_IS_SID
            && !trustee.ptstrName.is_null()
            && unsafe { EqualSid(trustee.ptstrName.cast(), sid) } != 0
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn run_parent_probe(marker: Option<&Path>) -> Result<i32, String> {
    let marker = marker.ok_or("parent probe marker missing")?;
    let workspace = marker.parent().ok_or("parent probe workspace missing")?;
    let base = workspace.parent().ok_or("parent probe root missing")?;
    let cache = base.join("cache");
    let ready_path = workspace.join("parent-target-ready.txt");
    let tree_ready = workspace.join("parent-tree-ready.txt");
    let powershell = resolve_program("powershell.exe", &workspace)?;
    let child_script = format!(
        "[System.IO.File]::WriteAllText({}, \"TARGET_READY`n\", [System.Text.UTF8Encoding]::new($false)); Start-Sleep -Seconds 2; Set-Content -LiteralPath {} -Value leaked",
        powershell_literal(&tree_ready)?,
        powershell_literal(marker)?
    );
    let script = format!(
        "$exe=(Get-Process -Id $PID).Path; $child={}; $encoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($child)); Start-Process -FilePath $exe -ArgumentList @('-NoProfile','-NonInteractive','-EncodedCommand',$encoded) | Out-Null; Start-Sleep -Seconds 10",
        powershell_literal(Path::new(&child_script))?
    );
    let mut helper = build_helper_with_ready(
        powershell,
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        &workspace,
        &cache,
        Some(ready_path.clone()),
    )?;
    let mut child = helper
        .as_std_mut()
        .spawn_guarded()
        .map_err(|e| format!("spawn parent-death helper: {e}"))?;
    wait_for_probe_file(&ready_path)?;
    wait_for_exact_probe_file(&tree_ready)?;
    print!("READY\n");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let status = child
        .wait()
        .map_err(|e| format!("wait parent-death helper: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

fn run_authority_probe(mut args: std::env::ArgsOs) -> Result<i32, String> {
    let helper_pid = parse_probe_pid(args.next(), "helper")?;
    let parent_pid = parse_probe_pid(args.next(), "parent")?;
    let outside = args
        .next()
        .map(PathBuf::from)
        .ok_or("outside marker missing")?;
    let expected_desktop = args
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("expected private desktop name missing")?;
    let omitted_handle = args
        .next()
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value != 0)
        .ok_or("omitted handle missing")?;
    let descendant_ready = args
        .next()
        .map(PathBuf::from)
        .ok_or("descendant readiness path missing")?;
    let control_root = args
        .next()
        .map(PathBuf::from)
        .ok_or("AppContainer control root missing")?;
    let descendant_release = args
        .next()
        .map(PathBuf::from)
        .ok_or("descendant release path missing")?;
    if args.next().is_some() || outside.exists() {
        return Err("invalid authority-probe arguments".into());
    }
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    if desktop.is_null()
        || user_object_name(desktop, "authority-probe desktop")? != expected_desktop
        || !expected_desktop.starts_with("mini-agent-")
    {
        return Ok(93);
    }
    if try_probe_write(&outside) {
        return Ok(91);
    }
    let mut flags = 0u32;
    if unsafe { GetHandleInformation(omitted_handle as HANDLE, &mut flags) } != 0 {
        return Ok(97);
    }
    if std::fs::read_dir(&control_root).is_ok()
        || std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(control_root.join("child-access-canary"))
            .is_ok()
    {
        return Ok(100);
    }
    if process_token_is_acquirable(helper_pid, &outside)
        || process_token_is_acquirable(parent_pid, &outside)
    {
        return Ok(90);
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let descendant = Command::new(executable)
        .arg(DESCENDANT_PROBE_ARG)
        .arg(&descendant_ready)
        .arg(&descendant_release)
        .status_guarded()
        .map_err(|error| format!("run AppContainer descendant probe: {error}"))?;
    if !descendant.success() {
        return Ok(99);
    }
    let mut breakaway = Command::new(
        std::env::current_exe().map_err(|error| format!("locate breakaway probe: {error}"))?,
    );
    breakaway
        .arg("--help")
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    match breakaway.status_guarded() {
        Err(error) if error.raw_os_error() == Some(5) => {}
        Ok(_) => return Ok(101),
        Err(error) => {
            return Err(format!(
                "breakaway denial returned unexpected error: {error}"
            ));
        }
    }
    if !current_token_has_zero_capabilities()? {
        return Ok(92);
    }
    if !current_appcontainer_has_no_loopback_exemption()? {
        return Ok(96);
    }
    if !tcp_attempt_denied("127.0.0.1:9")
        || !tcp_attempt_denied("1.1.1.1:9")
        || !tcp_attempt_denied("[::1]:9")
        || !tcp_attempt_denied("[2606:4700:4700::1111]:9")
    {
        return Ok(94);
    }
    if !udp_attempt_denied("127.0.0.1:9")
        || !udp_attempt_denied("1.1.1.1:9")
        || !udp_attempt_denied("[::1]:9")
        || !udp_attempt_denied("[2606:4700:4700::1111]:9")
    {
        return Ok(95);
    }
    Ok(0)
}

fn run_descendant_probe(mut args: std::env::ArgsOs) -> Result<i32, String> {
    let ready = args
        .next()
        .map(PathBuf::from)
        .ok_or("descendant readiness path missing")?;
    let release = args
        .next()
        .map(PathBuf::from)
        .ok_or("descendant release path missing")?;
    if args.next().is_some() {
        return Err("unexpected descendant probe argument".into());
    }
    if !current_token_has_zero_capabilities()? || !current_token_is_appcontainer()? {
        return Ok(2);
    }
    let proof = format!(
        "{}\n{}\n",
        unsafe { GetCurrentProcessId() },
        process_creation_time(unsafe { GetCurrentProcess() })?
    );
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&ready)
        .and_then(|mut file| file.write_all(proof.as_bytes()))
        .map_err(|error| format!("publish descendant process identity: {error}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !release.exists() {
        if std::time::Instant::now() >= deadline {
            return Ok(3);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(0)
}

fn current_token_is_appcontainer() -> Result<bool, String> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(last_error("open descendant AppContainer token"));
    }
    let token = Handle::created(raw, "open descendant AppContainer token")?;
    let mut information = TOKEN_APPCONTAINER_INFORMATION::default();
    let mut bytes = 0u32;
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenAppContainerSid,
            (&mut information as *mut TOKEN_APPCONTAINER_INFORMATION).cast(),
            size_of::<TOKEN_APPCONTAINER_INFORMATION>() as u32,
            &mut bytes,
        )
    } == 0
    {
        return Err(last_error("read descendant AppContainer SID"));
    }
    Ok(!information.TokenAppContainer.is_null())
}

fn current_appcontainer_has_no_loopback_exemption() -> Result<bool, String> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(last_error("open AppContainer token for loopback proof"));
    }
    let token = Handle::created(raw, "open AppContainer token for loopback proof")?;
    let mut information = TOKEN_APPCONTAINER_INFORMATION::default();
    let mut bytes = 0u32;
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenAppContainerSid,
            (&mut information as *mut TOKEN_APPCONTAINER_INFORMATION).cast(),
            size_of::<TOKEN_APPCONTAINER_INFORMATION>() as u32,
            &mut bytes,
        )
    } == 0
        || information.TokenAppContainer.is_null()
    {
        return Err(last_error("read current AppContainer SID"));
    }
    let mut count = 0u32;
    let mut entries: *mut SID_AND_ATTRIBUTES = null_mut();
    let result = unsafe { NetworkIsolationGetAppContainerConfig(&mut count, &mut entries) };
    if result != 0 || count > 4096 || (count != 0 && entries.is_null()) {
        return Err(format!(
            "sandbox: query AppContainer loopback exemptions: code {result}"
        ));
    }
    struct LoopbackEntries {
        count: u32,
        entries: *mut SID_AND_ATTRIBUTES,
    }
    impl Drop for LoopbackEntries {
        fn drop(&mut self) {
            if self.entries.is_null() {
                return;
            }
            for index in 0..self.count as usize {
                let sid = unsafe { (*self.entries.add(index)).Sid };
                if !sid.is_null() {
                    unsafe { HeapFree(GetProcessHeap(), 0, sid) };
                }
            }
            unsafe { HeapFree(GetProcessHeap(), 0, self.entries.cast()) };
        }
    }
    let entries = LoopbackEntries { count, entries };
    for index in 0..count as usize {
        let candidate = unsafe { (*entries.entries.add(index)).Sid };
        if !candidate.is_null()
            && unsafe { EqualSid(information.TokenAppContainer, candidate) } != 0
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn current_token_has_zero_capabilities() -> Result<bool, String> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(last_error("open AppContainer token for capability proof"));
    }
    let token = Handle::created(raw, "open AppContainer token for capability proof")?;
    let mut bytes = 0u32;
    unsafe {
        GetTokenInformation(token.raw(), TokenCapabilities, null_mut(), 0, &mut bytes);
    }
    if bytes < size_of::<u32>() as u32 || bytes > 64 * 1024 {
        return Err("sandbox: invalid TokenCapabilities size".into());
    }
    let mut storage = vec![0u8; bytes as usize];
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenCapabilities,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(last_error("read AppContainer TokenCapabilities"));
    }
    Ok(u32::from_ne_bytes(
        storage[..size_of::<u32>()]
            .try_into()
            .expect("bounded capability header"),
    ) == 0)
}

fn network_access_denied(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(10013)
}

fn tcp_attempt_denied(address: &str) -> bool {
    let Ok(address) = address.parse() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(750))
        .is_err_and(|error| network_access_denied(&error))
}

fn udp_attempt_denied(address: &str) -> bool {
    let bind = if address.starts_with('[') {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = match std::net::UdpSocket::bind(bind) {
        Ok(socket) => socket,
        Err(error) => return network_access_denied(&error),
    };
    socket
        .connect(address)
        .and_then(|_| socket.send(b"mini-agent-network-denial-probe"))
        .is_err_and(|error| network_access_denied(&error))
}

fn parse_probe_pid(value: Option<std::ffi::OsString>, label: &str) -> Result<u32, String> {
    value
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| format!("invalid {label} probe PID"))
}

fn try_probe_write(path: &Path) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(b"denied\n"))
        .is_ok()
}

fn process_token_is_acquirable(pid: u32, outside: &Path) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    let Ok(process) = Handle::created(process, "probe trusted process") else {
        return false;
    };
    let mut token = null_mut();
    if unsafe {
        OpenProcessToken(
            process.raw(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut token,
        )
    } == 0
    {
        return false;
    }
    let Ok(token) = Handle::created(token, "probe trusted token") else {
        return true;
    };
    let mut duplicate = null_mut();
    if unsafe {
        DuplicateTokenEx(
            token.raw(),
            TOKEN_QUERY | TOKEN_IMPERSONATE,
            null(),
            SecurityImpersonation,
            TokenImpersonation,
            &mut duplicate,
        )
    } != 0
        && let Ok(duplicate) = Handle::created(duplicate, "probe duplicate token")
        && unsafe { ImpersonateLoggedOnUser(duplicate.raw()) } != 0
    {
        let _ = try_probe_write(outside);
        unsafe { RevertToSelf() };
    }
    // Obtaining TOKEN_DUPLICATE authority over a trusted token is itself a containment failure,
    // even if a later API happens to reject this particular impersonation attempt.
    true
}

fn wait_for_probe_file(path: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::fs::read(path).is_ok_and(|contents| contents.starts_with(b"TARGET_READY\n")) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(format!(
        "timed out waiting for probe readiness: {}",
        path.display()
    ))
}

fn wait_for_exact_probe_file(path: &Path) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if bounded_probe_contents(path, b"TARGET_READY\n".len())
            .is_some_and(|contents| contents == b"TARGET_READY\n")
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(format!(
        "timed out waiting for exact child readiness: {}",
        path.display()
    ))
}

fn bounded_probe_contents(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut contents = Vec::with_capacity(max_bytes + 1);
    Read::by_ref(&mut file)
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut contents)
        .ok()?;
    (contents.len() <= max_bytes).then_some(contents)
}

fn powershell_literal(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or("PowerShell probe path is not valid Unicode")?;
    Ok(format!("'{}'", path.replace('\'', "''")))
}

fn wide_string(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn wide_null(value: &OsStr) -> Result<Vec<u16>, String> {
    if value.encode_wide().any(|unit| unit == 0) {
        return Err("sandbox: Windows path contains NUL".into());
    }
    Ok(value.encode_wide().chain(Some(0)).collect())
}

fn last_error(context: &str) -> String {
    format!("sandbox: {context}: {}", std::io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_argument_quoting_handles_empty_quotes_and_trailing_slashes() {
        assert_eq!(quote_windows_argument("plain"), "plain");
        assert_eq!(quote_windows_argument(""), "\"\"");
        assert_eq!(quote_windows_argument("a b"), "\"a b\"");
        assert_eq!(quote_windows_argument("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_windows_argument("a b\\"), "\"a b\\\\\"");
    }

    #[test]
    fn windows_system_executables_use_the_preexisting_package_acl() {
        let cwd = std::env::current_dir().expect("current directory");
        let command = resolve_program("cmd.exe", &cwd).expect("resolve system command");
        assert!(
            trusted_system_read_file(&command).expect("classify system command"),
            "{}",
            command.display()
        );

        let ordinary = std::env::temp_dir().join(format!(
            "mini-agent-nonsystem-read-root-{}-{}.exe",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&ordinary, b"not an executable").expect("write ordinary fixture");
        assert!(!trusted_system_read_file(&ordinary).expect("classify ordinary file"));
        std::fs::remove_file(ordinary).expect("remove ordinary fixture");
    }
}
