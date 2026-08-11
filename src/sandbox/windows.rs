#![allow(unsafe_code)]

//! General-process Windows sandbox.
//!
//! This is deliberately separate from the broker-only LPAC worker launcher. A small copy of the
//! current executable receives an authenticated-by-inheritance request on stdin, creates a
//! workspace-capable regular AppContainer, and starts the requested program in a creation-time Job.
//! The request never appears in a command line, environment variable, or temporary file. The
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
use std::process::{Child, Command, ExitStatus, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, FILETIME,
    GENERIC_ALL, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LocalFree, TRUE, WAIT_ABANDONED_0,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::NetworkManagement::WindowsFirewall::NetworkIsolationGetAppContainerConfig;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetExplicitEntriesFromAclW,
    GetSecurityInfo, REVOKE_ACCESS, SDDL_REVISION_1, SE_FILE_OBJECT, SE_OBJECT_TYPE,
    SE_WINDOW_OBJECT, SET_ACCESS, SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
    DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    AccessCheck, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, DuplicateToken,
    DuplicateTokenEx, EqualSid, FreeSid, GENERIC_MAPPING, GetTokenInformation,
    ImpersonateLoggedOnUser, InitializeSecurityDescriptor, OBJECT_INHERIT_ACE,
    OWNER_SECURITY_INFORMATION, PRIVILEGE_SET, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    RevertToSelf, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SECURITY_DESCRIPTOR,
    SID_AND_ATTRIBUTES, SecurityImpersonation, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    TOKEN_QUERY, TOKEN_USER, TokenCapabilities, TokenImpersonation, TokenIsAppContainer, TokenUser,
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
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapFree};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, GetProcessWindowStation, GetThreadDesktop,
    GetUserObjectInformationW, HDESK, HWINSTA, UOI_NAME,
};
use windows_sys::Win32::System::SystemServices::{
    JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE, JOB_OBJECT_UILIMIT_ALL, MAXIMUM_ALLOWED,
    PROCESS_MITIGATION_CHILD_PROCESS_POLICY, SECURITY_DESCRIPTOR_REVISION,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateEventW,
    CreateMutexW, CreateProcessAsUserW, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetExitCodeProcess,
    GetProcessMitigationPolicy, GetProcessTimes, InitializeProcThreadAttributeList, OpenProcess,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, ProcessChildProcessPolicy, ReleaseMutex,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForMultipleObjects, WaitForSingleObject,
};

use crate::process_creation::StdCommandCreationExt;
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

const HELPER_ARG: &str = "--mini-agent-windows-sandbox-helper-v1";
const PROBE_ARG: &str = "--mini-agent-windows-sandbox-runtime-check";
const PARENT_PROBE_ARG: &str = "--mini-agent-windows-sandbox-parent-probe";
const TARGET_PROBE_ARG: &str = "--mini-agent-windows-sandbox-target-probe";
const TARGET_BOUNDARY_ARG: &str = "boundary";
const TARGET_CONFIGURED_ARG: &str = "configured";
const TARGET_NOOP_ARG: &str = "noop";
const TARGET_CONFIGURED_SPAWN_ERROR_BASE: i32 = 0x1_0000;
const TARGET_CONFIGURED_WAIT_ERROR_BASE: i32 = 0x2_0000;
const TARGET_CONFIGURED_STDIN_DUPLICATE_ERROR_BASE: i32 = 0x3_0000;
const TARGET_CONFIGURED_STDOUT_DUPLICATE_ERROR_BASE: i32 = 0x4_0000;
const TARGET_CONFIGURED_STDERR_DUPLICATE_ERROR_BASE: i32 = 0x5_0000;
const TARGET_CONFIGURED_POLICY_QUERY_ERROR_BASE: i32 = 0x6_0000;
const TARGET_CONFIGURED_RAW_SPAWN_ERROR_BASE: i32 = 0x7_0000;
const TARGET_CONFIGURED_EXECUTABLE_OPEN_ERROR_BASE: i32 = 0x8_0000;
const TARGET_SELF_RAW_SPAWN_ERROR_BASE: i32 = 0x9_0000;
const TARGET_JOB_LIMIT_QUERY_ERROR_BASE: i32 = 0xA_0000;
const TARGET_JOB_ACCOUNTING_QUERY_ERROR_BASE: i32 = 0xB_0000;
const TARGET_SELF_TOKEN_OPEN_ERROR_BASE: i32 = 0xC_0000;
const TARGET_DESCENDANT_SPAWN_ERROR_BASE: i32 = 0xD_0000;
const TARGET_DESCENDANT_WAIT_ERROR_BASE: i32 = 0xE_0000;
const AUTHORITY_DESCENDANT_SPAWN_FAILED: i32 = 103;
const AUTHORITY_DESCENDANT_WAIT_FAILED: i32 = 104;
const AUTHORITY_DESCENDANT_NO_EXIT_CODE: i32 = 105;
const AUTHORITY_ARGUMENT_ERROR: i32 = 106;
const AUTHORITY_DESKTOP_QUERY_ERROR: i32 = 107;
const AUTHORITY_TOKEN_QUERY_ERROR: i32 = 108;
const AUTHORITY_CURRENT_EXE_ERROR: i32 = 109;
const AUTHORITY_BREAKAWAY_EXE_ERROR: i32 = 110;
const AUTHORITY_BREAKAWAY_RESULT_ERROR: i32 = 111;
const AUTHORITY_CAPABILITY_QUERY_ERROR: i32 = 112;
const DESCENDANT_ARGUMENT_ERROR: i32 = 119;
const DESCENDANT_CAPABILITY_QUERY_ERROR: i32 = 120;
const DESCENDANT_APPCONTAINER_QUERY_ERROR: i32 = 121;
// The regular AppContainer supplies the Windows system-resource access required for descendant
// creation, so retain the Job's complete UI lockdown in addition to the private desktop.
const GENERAL_JOB_UI_RESTRICTIONS: u32 = JOB_OBJECT_UILIMIT_ALL;
const TARGET_SLEEP_ARG: &str = "sleep";
const TARGET_WRITE_ARG: &str = "write";
const TARGET_PARENT_ARG: &str = "parent";
const TARGET_DESCENDANT_ARG: &str = "descendant";
const AUTHORITY_PROBE_ARG: &str = "--mini-agent-windows-sandbox-authority-probe";
const DESCENDANT_PROBE_ARG: &str = "--mini-agent-windows-appcontainer-descendant-probe";
const HELPER_PID_PLACEHOLDER: &str = "helper-pid";
const DESKTOP_NAME_PLACEHOLDER: &str = "desktop-name";
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
const PREFLIGHT_ROOT_PREFIX: &str = "mini-agent-windows-sandbox-preflight-";
const PREFLIGHT_OWNER_FILE: &str = ".owner-v1";
const PRIVATE_CONTROL_ROOT_NAME: &str = ".mini-agent-appcontainer-control-v1";
const MAX_STALE_PREFLIGHT_ROOTS: usize = 64;
const MAX_PREFLIGHT_RECOVERY_ENTRIES: usize = 4_096;
const MAX_PREFLIGHT_RECOVERY_BYTES: u64 = 1024 * 1024;
const PROFILE_JOURNAL_VERSION: u32 = 2;
const PROFILE_INTENT_VERSION: u32 = 1;
const PROFILE_INTENT_EXTENSION: &str = "intent";
const JOB_NAME_PREFIX: &str = "Global\\mini-agent-general-job-";
const STALE_JOB_CLEANUP_EXIT_CODE: u32 = 126;
const MAX_JOB_PROCESSES: u32 = 64;
const PROCESS_MEMORY_BYTES: usize = 512 * 1024 * 1024;
const JOB_MEMORY_BYTES: usize = 1024 * 1024 * 1024;
const PROCESS_CPU_100NS: i64 = 60 * 10_000_000;
const ACL_MUTEX_WAIT_MS: u32 = 5_000;
const GENERAL_PREFLIGHT_RUN_TIMEOUT: Duration = Duration::from_secs(5);
const GENERAL_PREFLIGHT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const GENERAL_PREFLIGHT_POLL_INTERVAL: Duration = Duration::from_millis(10);
// ACL snapshots must be serialized across terminal/RDP/service sessions. A Local\\ mutex would
// allow two same-user helpers in different sessions to overwrite each other's read/modify/write
// transaction. The object manager applies the creator token's default DACL; a pre-created object
// that we cannot open therefore fails the launch closed.
const ACL_MUTEX_NAME: &str = "Global\\mini-agent-general-sandbox-acl-v1";
const PROFILE_CONTROL_MUTEX_NAME: &str = "Global\\mini-agent-general-sandbox-profile-control-v1";
const HELPER_FAILURE_STATUS_BASE: i32 = 160;
const HELPER_STAGE_REQUEST: u8 = 1;
const HELPER_STAGE_SETUP: u8 = 2;
const HELPER_STAGE_LAUNCH: u8 = 3;
const HELPER_STAGE_VERIFY_JOB: u8 = 4;
const HELPER_STAGE_READY: u8 = 15;
const HELPER_STAGE_WAIT: u8 = 16;
const HELPER_STAGE_EXIT_CODE: u8 = 17;
const HELPER_STAGE_DRAIN: u8 = 18;
const HELPER_STAGE_CLEANUP: u8 = 19;
const HELPER_STAGE_SUSPENDED_JOB: u8 = 20;
const HELPER_STAGE_REGULAR_TOKEN_OPEN: u8 = 21;
const HELPER_STAGE_REGULAR_TOKEN_SID: u8 = 22;
const HELPER_STAGE_REGULAR_TOKEN_DUPLICATE: u8 = 23;
const HELPER_STAGE_REGULAR_TOKEN_DESCRIPTOR: u8 = 24;
const HELPER_STAGE_REGULAR_TOKEN_ACCESS: u8 = 25;
const HELPER_STAGE_RESUME: u8 = 26;

static ACTIVE_REQUEST_FEEDERS: AtomicUsize = AtomicUsize::new(0);
static HELPER_STAGE: AtomicU8 = AtomicU8::new(HELPER_STAGE_REQUEST);
static GENERAL_SANDBOX_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn mark_helper_stage(stage: u8) {
    HELPER_STAGE.store(stage, Ordering::Release);
}

fn helper_failure_status() -> i32 {
    HELPER_FAILURE_STATUS_BASE + i32::from(HELPER_STAGE.load(Ordering::Acquire))
}

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

pub(crate) struct AclMutationGuard(Handle);

impl AclMutationGuard {
    pub(crate) fn acquire() -> Result<Self, String> {
        Self::acquire_until(Instant::now() + Duration::from_millis(ACL_MUTEX_WAIT_MS.into()))
    }

    pub(crate) fn acquire_until(deadline: Instant) -> Result<Self, String> {
        let name = wide_string(ACL_MUTEX_NAME);
        let mutex = Handle::created(
            unsafe { CreateMutexW(null(), 0, name.as_ptr()) },
            "open cross-process ACL mutation mutex",
        )?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("sandbox: timed out serializing ACL mutation".into());
        }
        let wait_ms = remaining.as_millis().clamp(1, u32::MAX as u128) as u32;
        match unsafe { WaitForSingleObject(mutex.raw(), wait_ms) } {
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

struct ProfileControlGuard(Handle);

impl ProfileControlGuard {
    fn acquire_until(deadline: Instant) -> Result<Self, String> {
        ensure_preflight_cleanup_deadline(deadline)?;
        let name = wide_string(PROFILE_CONTROL_MUTEX_NAME);
        let mutex = Handle::created(
            unsafe { CreateMutexW(null(), 0, name.as_ptr()) },
            "open cross-process AppContainer profile-control mutex",
        )?;
        ensure_preflight_cleanup_deadline(deadline)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait_ms = remaining.as_millis().clamp(1, u32::MAX as u128) as u32;
        match unsafe { WaitForSingleObject(mutex.raw(), wait_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED_0 => {
                let guard = Self(mutex);
                ensure_preflight_cleanup_deadline(deadline)?;
                Ok(guard)
            }
            WAIT_TIMEOUT => {
                Err("sandbox: timed out serializing AppContainer profile control".into())
            }
            _ => Err(last_error(
                "wait for cross-process AppContainer profile-control mutex",
            )),
        }
    }
}

impl Drop for ProfileControlGuard {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.0.raw()) };
    }
}

const CONTROL_ROOT_AUTHORITY_ERROR: &str =
    "sandbox: private AppContainer control-root authority validation failed";

#[derive(Clone, Debug)]
struct ProfileJournalRootAuthority(Arc<ProfileJournalRootAuthorityInner>);

#[derive(Debug)]
struct ProfileJournalRootAuthorityInner {
    path: PathBuf,
    identity: crate::fs::WindowsFileIdentity,
    _directory: File,
    // Retaining no-delete-share handles for every ancestor closes the gap between identity
    // revalidation and the unavoidable path-based Win32 journal operations below.
    _ancestors: Vec<File>,
}

impl ProfileJournalRootAuthority {
    fn new(path: PathBuf, directory: File) -> Result<Self, String> {
        let identity = crate::fs::windows_file_identity(&directory)
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        reject_reparse_components(&path).map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;

        let mut ancestor_paths = path
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        ancestor_paths.reverse();
        let mut ancestors = Vec::with_capacity(ancestor_paths.len());
        for ancestor in ancestor_paths {
            ancestors.push(
                open_stable_path(
                    &ancestor,
                    true,
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )
                .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?,
            );
        }

        let authority = Self(Arc::new(ProfileJournalRootAuthorityInner {
            path,
            identity,
            _directory: directory,
            _ancestors: ancestors,
        }));
        authority.revalidate()?;
        Ok(authority)
    }

    fn path(&self) -> &Path {
        &self.0.path
    }

    fn revalidate(&self) -> Result<(), String> {
        reject_reparse_components(self.path())
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        let canonical = std::fs::canonicalize(self.path())
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        reject_reparse_components(&canonical)
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        let observed = open_stable_path(
            &canonical,
            true,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )
        .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        let observed_identity = crate::fs::windows_file_identity(&observed)
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        let retained_identity = crate::fs::windows_file_identity(&self.0._directory)
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        if observed_identity != self.0.identity || retained_identity != self.0.identity {
            return Err(CONTROL_ROOT_AUTHORITY_ERROR.into());
        }
        let user =
            current_user_sid_buffer().map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        attest_control_root_dacl(&self.0._directory, token_user_sid(&user))
            .map_err(|_| CONTROL_ROOT_AUTHORITY_ERROR.to_string())?;
        Ok(())
    }

    fn validate_child(&self, path: &Path) -> Result<(), String> {
        self.revalidate()?;
        if path.parent() != Some(self.path()) {
            return Err(CONTROL_ROOT_AUTHORITY_ERROR.into());
        }
        Ok(())
    }

    fn remove_file(&self, path: &Path, action: &str) -> Result<(), String> {
        self.validate_child(path)?;
        std::fs::remove_file(path).map_err(|error| format!("sandbox: {action}: {error}"))
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
    journal_root: ProfileJournalRootAuthority,
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
        self.journal_root
            .remove_file(&self.journal_path, "remove completed AppContainer journal")?;
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

#[derive(Debug, Serialize, Deserialize)]
struct ProfileIntent {
    version: u32,
    profile_name: String,
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
    station: HWINSTA,
    sid: PSID,
    handle: HDESK,
    name: String,
    startup_name: Vec<u16>,
}

impl Drop for PrivateDesktop {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseDesktop(self.handle) };
        }
        if !self.station.is_null() && !self.sid.is_null() {
            let _ = update_handle_ace(
                self.station,
                SE_WINDOW_OBJECT,
                self.sid,
                REVOKE_ACCESS,
                0,
                0,
            );
        }
    }
}

pub(crate) fn is_available() -> bool {
    cached_general_sandbox_availability(&GENERAL_SANDBOX_AVAILABLE, || {
        let started = Instant::now();
        let result = run_production_preflight();
        tracing::debug!(
            phase = "windows_general_appcontainer_preflight",
            elapsed_ms = started.elapsed().as_millis() as u64,
            available = result.is_ok(),
            "completed closed Windows general-sandbox startup phase"
        );
        result
    })
}

fn cached_general_sandbox_availability(
    cache: &OnceLock<bool>,
    probe: impl FnOnce() -> Result<(), String>,
) -> bool {
    *cache.get_or_init(|| probe().is_ok())
}

struct TemporaryPreflightRoot {
    path: Option<PathBuf>,
    owner: Option<File>,
    authority: Option<ProfileJournalRootAuthority>,
}

impl TemporaryPreflightRoot {
    fn create(temp_root: &Path) -> Result<Self, String> {
        let path = temp_root.join(format!("{PREFLIGHT_ROOT_PREFIX}{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)
            .map_err(|error| format!("sandbox: create production-preflight root: {error}"))?;
        let directory = match protect_and_attest_control_root(&path) {
            Ok(directory) => directory,
            Err(error) => {
                let _ = std::fs::remove_dir(&path);
                return Err(error);
            }
        };
        let authority = match ProfileJournalRootAuthority::new(path.clone(), directory) {
            Ok(authority) => authority,
            Err(error) => {
                let _ = std::fs::remove_dir(&path);
                return Err(error);
            }
        };
        let owner = match create_preflight_owner(&authority) {
            Ok(owner) => owner,
            Err(error) => {
                drop(authority);
                let _ = std::fs::remove_dir(&path);
                return Err(error);
            }
        };
        Ok(Self {
            path: Some(path),
            owner: Some(owner),
            authority: Some(authority),
        })
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("preflight root remains owned")
    }

    fn remove(&mut self) -> Result<(), String> {
        self.path.take();
        let owner = self.owner.take().ok_or_else(|| {
            "sandbox: production-preflight owner lease was already released".to_string()
        })?;
        let authority = self.authority.take().ok_or_else(|| {
            "sandbox: production-preflight root authority was already released".to_string()
        })?;
        remove_preflight_tree(
            authority,
            owner,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
    }

    fn retain_recovery_state(&mut self) {
        self.path.take();
        self.owner.take();
        self.authority.take();
    }
}

impl Drop for TemporaryPreflightRoot {
    fn drop(&mut self) {
        self.path.take();
        if let (Some(owner), Some(authority)) = (self.owner.take(), self.authority.take()) {
            let _ = remove_preflight_tree(
                authority,
                owner,
                Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
            );
        }
    }
}

fn run_production_preflight() -> Result<(), String> {
    let temp_root = canonical_preflight_temp_root()?;
    recover_preserved_preflight_roots(
        &temp_root,
        next_general_preflight_recovery_deadline(Instant::now()),
    )?;
    let started = Instant::now();
    let run_deadline = started + GENERAL_PREFLIGHT_RUN_TIMEOUT;
    let reap_deadline = run_deadline + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT;
    run_production_preflight_owned_in(&temp_root, run_deadline, reap_deadline)
}

fn run_production_preflight_owned(
    run_deadline: Instant,
    reap_deadline: Instant,
) -> Result<(), String> {
    let temp_root = canonical_preflight_temp_root()?;
    run_production_preflight_owned_in(&temp_root, run_deadline, reap_deadline)
}

fn run_production_preflight_owned_in(
    temp_root: &Path,
    run_deadline: Instant,
    reap_deadline: Instant,
) -> Result<(), String> {
    let mut root = TemporaryPreflightRoot::create(temp_root)?;
    let workspace = root.path().join("workspace");
    let cache = root.path().join("cache");
    let outside = root.path().join("outside");
    let operation = (|| -> Result<(), String> {
        std::fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&outside).map_err(|error| error.to_string())?;

        let inside_file = workspace.join("inside.txt");
        let cache_fixture = cache.join("read-only-cache.txt");
        let outside_secret = outside.join("secret.txt");
        let outside_file = outside.join("outside.txt");
        std::fs::write(&cache_fixture, b"cache-readable").map_err(|error| error.to_string())?;
        std::fs::write(&outside_secret, b"outside-secret").map_err(|error| error.to_string())?;
        let executable = canonical_file(
            &std::env::current_exe().map_err(|error| error.to_string())?,
            "production preflight executable",
        )?;
        let cleanup_ready = workspace.join("cleanup-ready.txt");
        let mut command = build_helper_with_ready(
            executable.clone(),
            vec![
                TARGET_PROBE_ARG.into(),
                TARGET_BOUNDARY_ARG.into(),
                inside_file.to_string_lossy().into_owned(),
                cache_fixture.to_string_lossy().into_owned(),
                outside_secret.to_string_lossy().into_owned(),
                outside_file.to_string_lossy().into_owned(),
            ],
            &workspace,
            &cache,
            Some(cleanup_ready.clone()),
        )?;
        command
            .as_std_mut()
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = run_bounded_preflight_helper(&mut command, run_deadline, reap_deadline)?;
        if !status.success() || !inside_file.exists() || outside_file.exists() {
            return Err("production AppContainer preflight denied its declared boundary".into());
        }
        attest_completed_cleanup(&cleanup_ready, [&workspace, &cache, executable.as_path()])
    })();

    // Recovery gets its own bounded window after the helper has been reaped. Reusing the reap
    // deadline here can leave no time to revoke ACLs or delete the ephemeral profile.
    let recovery_deadline = next_general_preflight_recovery_deadline(Instant::now());
    let recovery = recover_preflight_profiles(&cache, recovery_deadline);
    let removal = if recovery.is_ok() {
        root.remove()
    } else {
        // Never erase the only durable profile/ACL recovery record after a failed sweep.
        root.retain_recovery_state();
        Ok(())
    };
    match (operation, recovery, removal) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(()), Ok(())) => Err(error),
        (_, Err(error), _) | (_, _, Err(error)) => Err(error),
    }
}

fn next_general_preflight_recovery_deadline(now: Instant) -> Instant {
    now + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT
}

fn canonical_preflight_temp_root() -> Result<PathBuf, String> {
    let root = canonical_root(&std::env::temp_dir(), "Windows temporary directory")?;
    reject_remote_access_path(&root)?;
    Ok(root)
}

fn preflight_root_name_is_valid(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name.strip_prefix(PREFLIGHT_ROOT_PREFIX) else {
        return false;
    };
    uuid::Uuid::parse_str(suffix).is_ok_and(|parsed| parsed.hyphenated().to_string() == suffix)
}

fn create_preflight_owner(root: &ProfileJournalRootAuthority) -> Result<File, String> {
    root.revalidate()?;
    let path = root.path().join(PREFLIGHT_OWNER_FILE);
    root.validate_child(&path)?;
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
    let handle = Handle::created(raw, "create exclusive production-preflight owner lease")?;
    let file = unsafe { File::from_raw_handle(handle.0.into_raw_handle()) };
    file.sync_all()
        .map_err(|error| format!("sandbox: persist production-preflight owner lease: {error}"))?;
    Ok(file)
}

fn open_preflight_owner(root: &ProfileJournalRootAuthority) -> Result<File, String> {
    let path = root.path().join(PREFLIGHT_OWNER_FILE);
    root.validate_child(&path)?;
    let file = open_stable_path(&path, false, GENERIC_READ | FILE_READ_ATTRIBUTES, 0)
        .map_err(|_| "sandbox: production-preflight root is active or unverifiable".to_string())?;
    let metadata = file
        .metadata()
        .map_err(|_| "sandbox: production-preflight owner lease is unverifiable".to_string())?;
    if metadata.len() != 0
        || crate::fs::windows_file_link_count(&file)
            .map_err(|_| "sandbox: production-preflight owner lease is unverifiable".to_string())?
            != 1
    {
        return Err("sandbox: production-preflight owner lease is unverifiable".into());
    }
    Ok(file)
}

fn attest_existing_private_root(path: &Path) -> Result<ProfileJournalRootAuthority, String> {
    let directory = open_stable_path(
        path,
        true,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
    .map_err(|_| "sandbox: preserved production-preflight root is unverifiable".to_string())?;
    let user = current_user_sid_buffer()
        .map_err(|_| "sandbox: preserved production-preflight root is unverifiable".to_string())?;
    attest_control_root_dacl(&directory, token_user_sid(&user))
        .map_err(|_| "sandbox: preserved production-preflight root is unverifiable".to_string())?;
    ProfileJournalRootAuthority::new(path.to_path_buf(), directory)
        .map_err(|_| "sandbox: preserved production-preflight root is unverifiable".to_string())
}

fn recover_preserved_preflight_roots(temp_root: &Path, deadline: Instant) -> Result<(), String> {
    ensure_preflight_cleanup_deadline(deadline)?;
    let canonical_temp = canonical_root(temp_root, "Windows temporary directory")?;
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&canonical_temp)
        .map_err(|_| "sandbox: enumerate preserved production-preflight roots failed".to_string())?
    {
        ensure_preflight_cleanup_deadline(deadline)?;
        let entry = entry.map_err(|_| {
            "sandbox: enumerate preserved production-preflight root failed".to_string()
        })?;
        let name = entry.file_name();
        if !preflight_root_name_is_valid(&name) {
            if name.to_string_lossy().starts_with(PREFLIGHT_ROOT_PREFIX) {
                return Err("sandbox: preserved production-preflight root name is invalid".into());
            }
            continue;
        }
        if candidates.len() >= MAX_STALE_PREFLIGHT_ROOTS {
            return Err("sandbox: preserved production-preflight root count exceeds 64".into());
        }
        candidates.push(entry.path());
    }
    candidates.sort();
    for candidate in candidates {
        ensure_preflight_cleanup_deadline(deadline)?;
        recover_preserved_preflight_root(&canonical_temp, &candidate, deadline)?;
    }
    Ok(())
}

fn recover_preserved_preflight_root(
    temp_root: &Path,
    candidate: &Path,
    deadline: Instant,
) -> Result<(), String> {
    ensure_preflight_cleanup_deadline(deadline)?;
    if candidate.parent() != Some(temp_root)
        || !candidate
            .file_name()
            .is_some_and(preflight_root_name_is_valid)
    {
        return Err("sandbox: preserved production-preflight root name is invalid".into());
    }
    let metadata = std::fs::symlink_metadata(candidate)
        .map_err(|_| "sandbox: preserved production-preflight root is unverifiable".to_string())?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("sandbox: preserved production-preflight root is unverifiable".into());
    }
    let canonical = canonical_root(candidate, "preserved production-preflight root")?;
    if canonical.parent() != Some(temp_root) || canonical.file_name() != candidate.file_name() {
        return Err("sandbox: preserved production-preflight root is unverifiable".into());
    }
    let authority = attest_existing_private_root(&canonical)?;
    let owner = open_preflight_owner(&authority)?;
    validate_preflight_recovery_schema(&authority, deadline)?;

    let cache = authority.path().join("cache");
    if cache.exists() {
        recover_verified_preflight_profiles(&cache, deadline)?;
    }
    ensure_preflight_cleanup_deadline(deadline)?;
    authority.revalidate()?;
    remove_preflight_tree(authority, owner, deadline)
}

enum PreflightRemovalEntry {
    Visit(PathBuf),
    RemoveDirectory(PathBuf, File),
}

fn remove_preflight_tree(
    root: ProfileJournalRootAuthority,
    owner: File,
    deadline: Instant,
) -> Result<(), String> {
    validate_preflight_recovery_schema(&root, deadline)?;
    let owner_path = root.path().join(PREFLIGHT_OWNER_FILE);
    let mut pending = Vec::new();
    for entry in std::fs::read_dir(root.path())
        .map_err(|_| "sandbox: enumerate production-preflight cleanup root failed".to_string())?
    {
        ensure_preflight_cleanup_deadline(deadline)?;
        let path = entry
            .map_err(|_| {
                "sandbox: enumerate production-preflight cleanup entry failed".to_string()
            })?
            .path();
        if path != owner_path {
            pending.push(PreflightRemovalEntry::Visit(path));
        }
    }

    while let Some(entry) = pending.pop() {
        ensure_preflight_cleanup_deadline(deadline)?;
        match entry {
            PreflightRemovalEntry::Visit(path) => {
                let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
                    "sandbox: inspect production-preflight cleanup entry failed".to_string()
                })?;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(
                        "sandbox: production-preflight cleanup encountered a reparse point".into(),
                    );
                }
                if metadata.is_dir() {
                    let directory = open_stable_path(
                        &path,
                        true,
                        FILE_READ_ATTRIBUTES,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                    )
                    .map_err(|_| {
                        "sandbox: production-preflight cleanup directory is unverifiable"
                            .to_string()
                    })?;
                    pending.push(PreflightRemovalEntry::RemoveDirectory(
                        path.clone(),
                        directory,
                    ));
                    for child in std::fs::read_dir(&path).map_err(|_| {
                        "sandbox: enumerate production-preflight cleanup directory failed"
                            .to_string()
                    })? {
                        ensure_preflight_cleanup_deadline(deadline)?;
                        pending.push(PreflightRemovalEntry::Visit(
                            child
                                .map_err(|_| {
                                    "sandbox: enumerate production-preflight cleanup child failed"
                                        .to_string()
                                })?
                                .path(),
                        ));
                    }
                } else if metadata.is_file() {
                    let file = open_stable_path(
                        &path,
                        false,
                        GENERIC_READ | FILE_READ_ATTRIBUTES,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                    )
                    .map_err(|_| {
                        "sandbox: production-preflight cleanup file is unverifiable".to_string()
                    })?;
                    if crate::fs::windows_file_link_count(&file).map_err(|_| {
                        "sandbox: production-preflight cleanup file is unverifiable".to_string()
                    })? != 1
                    {
                        return Err(
                            "sandbox: production-preflight cleanup file has multiple links".into(),
                        );
                    }
                    drop(file);
                    std::fs::remove_file(&path).map_err(|_| {
                        "sandbox: remove production-preflight cleanup file failed".to_string()
                    })?;
                } else {
                    return Err(
                        "sandbox: production-preflight cleanup entry has unsupported type".into(),
                    );
                }
            }
            PreflightRemovalEntry::RemoveDirectory(path, directory) => {
                let observed = open_stable_path(
                    &path,
                    true,
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                )
                .map_err(|_| {
                    "sandbox: production-preflight cleanup directory is unverifiable".to_string()
                })?;
                let retained_identity =
                    crate::fs::windows_file_identity(&directory).map_err(|_| {
                        "sandbox: production-preflight cleanup directory is unverifiable"
                            .to_string()
                    })?;
                let observed_identity =
                    crate::fs::windows_file_identity(&observed).map_err(|_| {
                        "sandbox: production-preflight cleanup directory is unverifiable"
                            .to_string()
                    })?;
                if retained_identity != observed_identity {
                    return Err(
                        "sandbox: production-preflight cleanup directory identity changed".into(),
                    );
                }
                drop(observed);
                drop(directory);
                std::fs::remove_dir(&path).map_err(|_| {
                    "sandbox: remove empty production-preflight cleanup directory failed"
                        .to_string()
                })?;
            }
        }
    }

    ensure_preflight_cleanup_deadline(deadline)?;
    root.revalidate()?;
    root.validate_child(&owner_path)?;
    drop(owner);
    std::fs::remove_file(&owner_path)
        .map_err(|_| "sandbox: remove production-preflight owner lease failed".to_string())?;
    let path = root.path().to_path_buf();
    drop(root);
    match std::fs::remove_dir(&path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // If the final empty-directory removal loses a transient race, restore the lease so a
            // later startup can retry recovery. Re-attestation prevents publishing into a swapped
            // or permission-weakened root.
            if let Ok(authority) = attest_existing_private_root(&path) {
                let _ = create_preflight_owner(&authority);
            }
            Err("sandbox: remove empty production-preflight root failed".into())
        }
    }
}

fn validate_preflight_recovery_schema(
    root: &ProfileJournalRootAuthority,
    deadline: Instant,
) -> Result<(), String> {
    root.revalidate()?;
    let mut entries = 0usize;
    let mut bytes = 0u64;
    let mut pending = vec![root.path().to_path_buf()];
    while let Some(directory) = pending.pop() {
        ensure_preflight_cleanup_deadline(deadline)?;
        for entry in std::fs::read_dir(&directory).map_err(|_| {
            "sandbox: inspect production-preflight recovery schema failed".to_string()
        })? {
            ensure_preflight_cleanup_deadline(deadline)?;
            entries += 1;
            if entries > MAX_PREFLIGHT_RECOVERY_ENTRIES {
                return Err(
                    "sandbox: production-preflight recovery schema exceeds entry bound".into(),
                );
            }
            let entry = entry.map_err(|_| {
                "sandbox: inspect production-preflight recovery entry failed".to_string()
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
                "sandbox: inspect production-preflight recovery entry failed".to_string()
            })?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(
                    "sandbox: production-preflight recovery schema contains a reparse point".into(),
                );
            }
            if directory == root.path() {
                match entry.file_name().to_str() {
                    Some(PREFLIGHT_OWNER_FILE) if metadata.is_file() => {}
                    Some("workspace") | Some("cache") | Some("outside") if metadata.is_dir() => {}
                    Some(PRIVATE_CONTROL_ROOT_NAME) if metadata.is_dir() => {}
                    _ => {
                        return Err("sandbox: production-preflight recovery schema has an unexpected root entry".into());
                    }
                }
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                if path == root.path().join(PREFLIGHT_OWNER_FILE) {
                    continue;
                }
                let file = open_stable_path(
                    &path,
                    false,
                    GENERIC_READ | FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )
                .map_err(|_| {
                    "sandbox: production-preflight recovery file is unverifiable".to_string()
                })?;
                if crate::fs::windows_file_link_count(&file).map_err(|_| {
                    "sandbox: production-preflight recovery file is unverifiable".to_string()
                })? != 1
                {
                    return Err(
                        "sandbox: production-preflight recovery file has multiple links".into(),
                    );
                }
                bytes = bytes.checked_add(metadata.len()).ok_or_else(|| {
                    "sandbox: production-preflight recovery bytes overflowed".to_string()
                })?;
                if bytes > MAX_PREFLIGHT_RECOVERY_BYTES {
                    return Err(
                        "sandbox: production-preflight recovery schema exceeds byte bound".into(),
                    );
                }
            } else {
                return Err(
                    "sandbox: production-preflight recovery schema has unsupported entry type"
                        .into(),
                );
            }
        }
    }
    Ok(())
}

fn run_bounded_preflight_helper(
    command: &mut tokio::process::Command,
    run_deadline: Instant,
    cleanup_deadline: Instant,
) -> Result<ExitStatus, String> {
    let mut child = command
        .as_std_mut()
        .spawn_guarded_until(run_deadline)
        .map_err(|error| format!("sandbox: start production AppContainer preflight: {error}"))?;
    loop {
        if Instant::now() >= run_deadline {
            terminate_and_reap_owned_helper(&mut child, cleanup_deadline)?;
            return Err("sandbox: Windows general AppContainer preflight timed out".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return if Instant::now() < run_deadline {
                    Ok(status)
                } else {
                    Err("sandbox: Windows general AppContainer preflight timed out".into())
                };
            }
            Ok(None) => {}
            Err(error) => {
                let poll_error =
                    format!("sandbox: poll production AppContainer preflight: {error}");
                return Err(
                    match terminate_and_reap_owned_helper(&mut child, cleanup_deadline) {
                        Ok(()) => poll_error,
                        Err(cleanup) => format!("{poll_error}; {cleanup}"),
                    },
                );
            }
        }
        std::thread::sleep(
            GENERAL_PREFLIGHT_POLL_INTERVAL
                .min(run_deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn terminate_and_reap_owned_helper(
    child: &mut Child,
    cleanup_deadline: Instant,
) -> Result<(), String> {
    let mut first_error = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let wait = child.wait().map_err(|error| {
                    format!("sandbox: finish reaping timed-out AppContainer helper: {error}")
                });
                return match (first_error, wait) {
                    (None, Ok(_)) => Ok(()),
                    (Some(error), Ok(_)) => Err(error),
                    (None, Err(error)) => Err(error),
                    (Some(error), Err(wait)) => Err(format!("{error}; {wait}")),
                };
            }
            Ok(None) => {}
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    format!("sandbox: poll timed-out AppContainer helper: {error}")
                });
            }
        }
        if let Err(error) = child.kill() {
            first_error.get_or_insert_with(|| {
                format!("sandbox: terminate timed-out AppContainer helper: {error}")
            });
        }
        if Instant::now() >= cleanup_deadline {
            return Err(first_error.unwrap_or_else(|| {
                "sandbox: timed-out AppContainer helper exceeded its bounded reap deadline".into()
            }));
        }
        std::thread::sleep(
            GENERAL_PREFLIGHT_POLL_INTERVAL
                .min(cleanup_deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn recover_preflight_profiles(cache: &Path, deadline: Instant) -> Result<(), String> {
    let candidate = private_control_root_candidate(cache)?;
    if candidate.exists() {
        let directory = protect_and_attest_control_root(&candidate)?;
        let journal_root = ProfileJournalRootAuthority::new(candidate, directory)?;
        sweep_stale_profiles_until(&journal_root, deadline)?;
    }
    Ok(())
}

fn recover_verified_preflight_profiles(cache: &Path, deadline: Instant) -> Result<(), String> {
    let candidate = private_control_root_candidate(cache)?;
    if candidate.exists() {
        let journal_root = attest_existing_private_root(&candidate)?;
        sweep_stale_profiles_until(&journal_root, deadline)?;
    }
    Ok(())
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootRole {
    Workspace,
    ReadOnlyApplicationCache,
    ConfiguredReadOnlyRoot,
    ConfiguredWritableRoot,
    SandboxExecutable,
    PrivateControlRoot,
    AuthorizedReadRoot,
    AuthorizedWritableRoot,
}

impl RootRole {
    fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::ReadOnlyApplicationCache => "read-only application cache",
            Self::ConfiguredReadOnlyRoot => "configured read-only root",
            Self::ConfiguredWritableRoot => "configured writable root",
            Self::SandboxExecutable => "sandbox executable",
            Self::PrivateControlRoot => "private AppContainer control root",
            Self::AuthorizedReadRoot => "authorized read root",
            Self::AuthorizedWritableRoot => "authorized writable root",
        }
    }
}

fn root_role_conflict(
    left_role: RootRole,
    left: &Path,
    right_role: RootRole,
    right: &Path,
) -> Option<String> {
    let relation = if left == right {
        format!(
            "{} is the same root as {}",
            left_role.label(),
            right_role.label()
        )
    } else if right.starts_with(left) {
        format!("{} contains {}", left_role.label(), right_role.label())
    } else if left.starts_with(right) {
        format!("{} contains {}", right_role.label(), left_role.label())
    } else {
        return None;
    };
    let remedy = if [left_role, right_role].contains(&RootRole::Workspace)
        && [left_role, right_role].contains(&RootRole::ReadOnlyApplicationCache)
    {
        if left_role == RootRole::Workspace && right.starts_with(left)
            || right_role == RootRole::Workspace && left.starts_with(right)
        {
            "use a project subdirectory that excludes the application cache or set ZS_CACHE_DIR outside the workspace"
        } else {
            "move the project outside the application cache or set ZS_CACHE_DIR to a directory that does not contain the workspace"
        }
    } else if [left_role, right_role].contains(&RootRole::PrivateControlRoot) {
        "set ZS_CACHE_DIR outside every AppContainer access root or narrow the conflicting configured root"
    } else {
        "remove or narrow one of the conflicting Windows AppContainer roots"
    };
    Some(format!(
        "sandbox: AppContainer root-role conflict: {relation}; {remedy}"
    ))
}

fn private_control_root_candidate(cache: &Path) -> Result<PathBuf, String> {
    let parent = cache
        .parent()
        .ok_or("sandbox: application cache has no private control parent")?;
    let candidate = parent.join(PRIVATE_CONTROL_ROOT_NAME);
    reject_reparse_components(&candidate)?;
    match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) => {
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err("sandbox: private AppContainer control root is a reparse point".into());
            }
            if !metadata.is_dir() {
                return Err(
                    "sandbox: private AppContainer control root has unsupported type".into(),
                );
            }
            let canonical = std::fs::canonicalize(candidate).map_err(|_| {
                "sandbox: canonicalize private AppContainer control root failed".to_string()
            })?;
            reject_reparse_components(&canonical)?;
            Ok(canonical)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(_) => Err("sandbox: inspect private AppContainer control root failed".into()),
    }
}

fn validate_private_control_root_policy(
    cache: &Path,
    program: &Path,
    cwd: &Path,
    configured_read_roots: &[PathBuf],
    configured_write_roots: &[PathBuf],
) -> Result<(), String> {
    let control = private_control_root_candidate(cache)?;
    for (role, root) in [
        (RootRole::Workspace, cwd),
        (RootRole::ReadOnlyApplicationCache, cache),
        (RootRole::SandboxExecutable, program),
    ] {
        if let Some(error) = root_role_conflict(RootRole::PrivateControlRoot, &control, role, root)
        {
            return Err(error);
        }
    }
    for root in configured_read_roots {
        if let Some(error) = root_role_conflict(
            RootRole::PrivateControlRoot,
            &control,
            RootRole::ConfiguredReadOnlyRoot,
            root,
        ) {
            return Err(error);
        }
    }
    for root in configured_write_roots {
        if let Some(error) = root_role_conflict(
            RootRole::PrivateControlRoot,
            &control,
            RootRole::ConfiguredWritableRoot,
            root,
        ) {
            return Err(error);
        }
    }
    Ok(())
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
    if let Some(error) = root_role_conflict(
        RootRole::Workspace,
        cwd,
        RootRole::ReadOnlyApplicationCache,
        cache,
    ) {
        return Err(error);
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
    for write in configured_write_roots {
        if let Some(error) = root_role_conflict(
            RootRole::ReadOnlyApplicationCache,
            cache,
            RootRole::ConfiguredWritableRoot,
            write,
        ) {
            return Err(error);
        }
    }
    for read in configured_read_roots {
        if let Some(error) = root_role_conflict(
            RootRole::ConfiguredReadOnlyRoot,
            read,
            RootRole::Workspace,
            cwd,
        ) {
            return Err(error);
        }
        for write in configured_write_roots {
            if let Some(error) = root_role_conflict(
                RootRole::ConfiguredReadOnlyRoot,
                read,
                RootRole::ConfiguredWritableRoot,
                write,
            ) {
                return Err(error);
            }
        }
    }
    for write in configured_write_roots {
        if let Some(error) = root_role_conflict(
            RootRole::Workspace,
            cwd,
            RootRole::ConfiguredWritableRoot,
            write,
        ) {
            return Err(error);
        }
    }
    for (index, write) in configured_write_roots.iter().enumerate() {
        for other in &configured_write_roots[index + 1..] {
            if let Some(error) = root_role_conflict(
                RootRole::ConfiguredWritableRoot,
                write,
                RootRole::ConfiguredWritableRoot,
                other,
            ) {
                return Err(error);
            }
        }
    }
    validate_private_control_root_policy(
        cache,
        program,
        cwd,
        configured_read_roots,
        configured_write_roots,
    )?;
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
        Some(value) if value == OsStr::new(HELPER_ARG) => Some(run_helper().unwrap_or_else(|_| {
            let helper_code = helper_failure_status();
            eprintln!(
                "Windows AppContainer sandbox helper failed at closed stage {}",
                helper_code - HELPER_FAILURE_STATUS_BASE
            );
            helper_code
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
        Some(value) if value == OsStr::new(TARGET_PROBE_ARG) => {
            Some(run_target_probe(args).unwrap_or(96))
        }
        Some(value) if value == OsStr::new(AUTHORITY_PROBE_ARG) => Some(run_authority_probe(args)),
        Some(value) if value == OsStr::new(DESCENDANT_PROBE_ARG) => {
            Some(run_descendant_probe(args))
        }
        _ => None,
    }
}

fn target_probe_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    label: &str,
) -> Result<PathBuf, String> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing Windows sandbox target-probe {label}"))
}

fn target_probe_os_error_code(base: i32, error: &std::io::Error) -> i32 {
    let raw = error
        .raw_os_error()
        .and_then(|code| u16::try_from(code).ok())
        .unwrap_or(u16::MAX);
    base | i32::from(raw)
}

fn target_probe_duplicate_handle(source: *mut c_void, error_base: i32) -> Result<(), i32> {
    let mut duplicate = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return Err(target_probe_os_error_code(error_base, &error));
    }
    unsafe { CloseHandle(duplicate) };
    Ok(())
}

fn target_probe_executable_access(tool: &Path) -> Result<i32, String> {
    let path = wide_null(tool.as_os_str())?;
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        return Ok(target_probe_os_error_code(
            TARGET_CONFIGURED_EXECUTABLE_OPEN_ERROR_BASE,
            &error,
        ));
    }
    unsafe { CloseHandle(raw) };
    Ok(0)
}

fn target_probe_job_status() -> i32 {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
            null_mut(),
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return target_probe_os_error_code(TARGET_JOB_LIMIT_QUERY_ERROR_BASE, &error);
    }
    if limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS == 0 {
        return 62;
    }
    if limits.BasicLimitInformation.ActiveProcessLimit < 2 {
        return 63;
    }

    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of_val(&accounting) as u32,
            null_mut(),
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return target_probe_os_error_code(TARGET_JOB_ACCOUNTING_QUERY_ERROR_BASE, &error);
    }
    if accounting.ActiveProcesses >= limits.BasicLimitInformation.ActiveProcessLimit {
        return 64;
    }
    0
}

fn target_probe_self_token_access() -> i32 {
    let mut token = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut token,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return target_probe_os_error_code(TARGET_SELF_TOKEN_OPEN_ERROR_BASE, &error);
    }
    unsafe { CloseHandle(token) };
    0
}

fn target_probe_raw_spawn(tool: &Path, error_base: i32) -> Result<i32, String> {
    let application = wide_null(tool.as_os_str())?;
    let arguments = [TARGET_PROBE_ARG.to_string(), TARGET_NOOP_ARG.to_string()];
    let mut command_line = wide_string(&windows_command_line(tool, &arguments));
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..STARTUPINFOW::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            FALSE,
            CREATE_UNICODE_ENVIRONMENT,
            null(),
            null(),
            &startup,
            &mut information,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        return Ok(target_probe_os_error_code(error_base, &error));
    }
    unsafe { CloseHandle(information.hThread) };
    let process = Handle::created(information.hProcess, "own raw target-probe child")?;
    if unsafe { WaitForSingleObject(process.raw(), u32::MAX) } != WAIT_OBJECT_0 {
        return Ok(59);
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.raw(), &mut code) } == 0 {
        return Ok(60);
    }
    Ok(if code == 0 { 0 } else { 61 })
}

fn run_target_probe(mut args: std::env::ArgsOs) -> Result<i32, String> {
    let operation = args
        .next()
        .ok_or("missing Windows sandbox target-probe operation")?;
    match operation.as_os_str() {
        value if value == OsStr::new(TARGET_BOUNDARY_ARG) => {
            let inside = target_probe_path(&mut args, "inside path")?;
            let readable = target_probe_path(&mut args, "readable path")?;
            let denied_read = target_probe_path(&mut args, "denied-read path")?;
            let denied_write = target_probe_path(&mut args, "denied-write path")?;
            std::fs::write(inside, b"inside").map_err(|error| error.to_string())?;
            if std::fs::read(readable).map_err(|error| error.to_string())? != b"cache-readable" {
                return Ok(40);
            }
            if std::fs::read(denied_read).is_ok() {
                return Ok(41);
            }
            if std::fs::write(denied_write, b"outside").is_ok() {
                return Ok(42);
            }
            Ok(0)
        }
        value if value == OsStr::new(TARGET_CONFIGURED_ARG) => {
            let readable = target_probe_path(&mut args, "configured read path")?;
            let tool = target_probe_path(&mut args, "configured tool path")?;
            let writable = target_probe_path(&mut args, "configured write path")?;
            let Ok(contents) = std::fs::read(readable) else {
                return Ok(53);
            };
            if contents != b"configured-read" {
                return Ok(51);
            }
            for (handle, error_base) in [
                (
                    std::io::stdin().as_raw_handle(),
                    TARGET_CONFIGURED_STDIN_DUPLICATE_ERROR_BASE,
                ),
                (
                    std::io::stdout().as_raw_handle(),
                    TARGET_CONFIGURED_STDOUT_DUPLICATE_ERROR_BASE,
                ),
                (
                    std::io::stderr().as_raw_handle(),
                    TARGET_CONFIGURED_STDERR_DUPLICATE_ERROR_BASE,
                ),
            ] {
                if let Err(code) = target_probe_duplicate_handle(handle, error_base) {
                    return Ok(code);
                }
            }
            let mut policy = PROCESS_MITIGATION_CHILD_PROCESS_POLICY {
                Anonymous: windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_CHILD_PROCESS_POLICY_0 {
                    Flags: 0,
                },
            };
            if unsafe {
                GetProcessMitigationPolicy(
                    GetCurrentProcess(),
                    ProcessChildProcessPolicy,
                    (&mut policy as *mut PROCESS_MITIGATION_CHILD_PROCESS_POLICY).cast(),
                    size_of::<PROCESS_MITIGATION_CHILD_PROCESS_POLICY>(),
                )
            } == 0
            {
                let error = std::io::Error::last_os_error();
                return Ok(target_probe_os_error_code(
                    TARGET_CONFIGURED_POLICY_QUERY_ERROR_BASE,
                    &error,
                ));
            }
            if unsafe { policy.Anonymous.Flags } & 0b1 != 0 {
                return Ok(58);
            }
            if unsafe { policy.Anonymous.Flags } & 0b100 != 0 {
                return Ok(65);
            }
            if unsafe { policy.Anonymous.Flags } & !0b111 != 0 {
                return Ok(66);
            }
            let token_status = target_probe_self_token_access();
            if token_status != 0 {
                return Ok(token_status);
            }
            let job_status = target_probe_job_status();
            if job_status != 0 {
                return Ok(job_status);
            }
            let executable_status = target_probe_executable_access(&tool)?;
            if executable_status != 0 {
                return Ok(executable_status);
            }
            let current_executable = std::env::current_exe()
                .map_err(|error| format!("resolve current target-probe executable: {error}"))?;
            let self_status =
                target_probe_raw_spawn(&current_executable, TARGET_SELF_RAW_SPAWN_ERROR_BASE)?;
            if self_status != 0 {
                return Ok(self_status);
            }
            let raw_status = target_probe_raw_spawn(&tool, TARGET_CONFIGURED_RAW_SPAWN_ERROR_BASE)?;
            if raw_status != 0 {
                return Ok(raw_status);
            }
            let mut child = match Command::new(tool)
                .args([TARGET_PROBE_ARG, TARGET_NOOP_ARG])
                .spawn_guarded()
            {
                Ok(child) => child,
                Err(error) => {
                    return Ok(target_probe_os_error_code(
                        TARGET_CONFIGURED_SPAWN_ERROR_BASE,
                        &error,
                    ));
                }
            };
            let status = match child.wait() {
                Ok(status) => status,
                Err(error) => {
                    return Ok(target_probe_os_error_code(
                        TARGET_CONFIGURED_WAIT_ERROR_BASE,
                        &error,
                    ));
                }
            };
            if !status.success() {
                return Ok(52);
            }
            if std::fs::write(writable, b"configured-write").is_err() {
                return Ok(55);
            }
            Ok(0)
        }
        value if value == OsStr::new(TARGET_NOOP_ARG) => Ok(0),
        value if value == OsStr::new(TARGET_SLEEP_ARG) => {
            let seconds = args
                .next()
                .and_then(|value| value.to_str().and_then(|value| value.parse::<u64>().ok()))
                .ok_or("invalid Windows sandbox target-probe sleep duration")?;
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            Ok(0)
        }
        value if value == OsStr::new(TARGET_WRITE_ARG) => {
            let path = target_probe_path(&mut args, "write path")?;
            std::fs::write(path, b"concurrent").map_err(|error| error.to_string())?;
            Ok(0)
        }
        value if value == OsStr::new(TARGET_PARENT_ARG) => {
            let descendant_executable = target_probe_path(&mut args, "descendant executable")?;
            let tree_ready = target_probe_path(&mut args, "tree-ready path")?;
            let marker = target_probe_path(&mut args, "parent-death marker")?;
            let mut child = Command::new(descendant_executable);
            child
                .arg(TARGET_PROBE_ARG)
                .arg(TARGET_DESCENDANT_ARG)
                .arg(tree_ready)
                .arg(marker);
            // Keep the already-contained standard handles. Constructing `Stdio::null()` here
            // asks this AppContainer process to open the denied Windows NUL device before
            // CreateProcessW, so the child would never reach its stricter token proof.
            child
                .spawn_guarded()
                .map_err(|error| format!("spawn target-probe descendant: {error}"))?;
            std::thread::sleep(std::time::Duration::from_secs(10));
            Ok(0)
        }
        value if value == OsStr::new(TARGET_DESCENDANT_ARG) => {
            let tree_ready = target_probe_path(&mut args, "tree-ready path")?;
            let marker = target_probe_path(&mut args, "parent-death marker")?;
            let mut ready = File::create(tree_ready).map_err(|error| error.to_string())?;
            ready
                .write_all(b"TARGET_READY\n")
                .and_then(|()| ready.sync_all())
                .map_err(|error| error.to_string())?;
            std::thread::sleep(std::time::Duration::from_secs(2));
            std::fs::write(marker, b"leaked").map_err(|error| error.to_string())?;
            Ok(0)
        }
        _ => Err("unknown Windows sandbox target-probe operation".into()),
    }
}

fn run_helper() -> Result<i32, String> {
    mark_helper_stage(HELPER_STAGE_REQUEST);
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
    mark_helper_stage(HELPER_STAGE_SETUP);
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
    // This desktop-only firewall API reads host configuration and is intentionally unavailable
    // inside AppContainer. Attest the freshly-created profile SID from the trusted helper before
    // launch; the contained authority probe separately verifies that TCP connection attempts fail.
    // Zero token capabilities are the protocol-independent proof that UDP is not authorized: a
    // successful connectionless `send` only proves that Winsock queued a datagram locally.
    if !appcontainer_has_no_loopback_exemption(grants.sid())? {
        return Err("sandbox: AppContainer profile has a loopback exemption".into());
    }
    ensure_parent_alive(&parent)?;
    let token = primary_token()?;
    let desktop = private_desktop(grants.sid())?;
    let omitted_handle = inheritable_omitted_canary()?;
    let mut arguments = request.arguments;
    let authority_probe_requested =
        arguments.first().map(String::as_str) == Some(AUTHORITY_PROBE_ARG);
    if authority_probe_requested {
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
        let control_root = arguments
            .get_mut(5)
            .filter(|value| value.as_str() == CONTROL_ROOT_PLACEHOLDER)
            .ok_or("invalid control-root placeholder")?;
        *control_root = grants
            .profile
            .journal_path
            .parent()
            .ok_or("AppContainer journal control root missing")?
            .to_string_lossy()
            .into_owned();
    }
    grants.disarm_for_launch();
    mark_helper_stage(HELPER_STAGE_LAUNCH);
    let child = match launch_appcontainer(
        &token,
        &job,
        &desktop,
        grants.sid(),
        &program,
        &arguments,
        &cwd,
        &cache,
        &grants.profile.storage,
    ) {
        Ok(child) => child,
        Err(error) => {
            terminate_and_drain_job(&job, 126)?;
            grants.mark_job_quiescent();
            grants.cleanup()?;
            return Err(error);
        }
    };
    mark_helper_stage(HELPER_STAGE_VERIFY_JOB);
    let omitted_canary_inherited = if authority_probe_requested {
        match target_inherited_omitted_canary(&child, &omitted_handle) {
            Ok(inherited) => inherited,
            Err(error) => {
                terminate_and_drain_job(&job, 126)?;
                grants.mark_job_quiescent();
                grants.cleanup()?;
                return Err(error);
            }
        }
    } else {
        false
    };
    mark_helper_stage(HELPER_STAGE_READY);
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
    mark_helper_stage(HELPER_STAGE_WAIT);
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
    mark_helper_stage(HELPER_STAGE_EXIT_CODE);
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(child.raw(), &mut code) } == 0 {
        terminate_and_drain_job(&job, 126)?;
        grants.mark_job_quiescent();
        grants.cleanup()?;
        return Err(last_error("read restricted child exit code"));
    }
    if omitted_canary_inherited {
        code = 97;
    }
    mark_helper_stage(HELPER_STAGE_DRAIN);
    terminate_and_drain_job(&job, code)?;
    grants.mark_job_quiescent();
    mark_helper_stage(HELPER_STAGE_CLEANUP);
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
        // Windows system executables are commonly owned by TrustedInstaller,
        // so an unelevated helper must not try to rewrite their DACL. Keep a
        // non-share-write/delete handle live to bind identity. A regular
        // AppContainer receives only the system image access Windows grants to
        // ALL APPLICATION PACKAGES; an inaccessible image still fails closed.
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
        FILE_SHARE_READ,
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
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
}

fn grant_access_root(
    root: &Path,
    grants: &mut AccessGrants,
    parent: &Handle,
    permissions: u32,
    share: u32,
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
            share,
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
    revoke_tree_with_deadline(root, sid, None)
}

fn revoke_tree_until(root: &Path, sid: PSID, deadline: Instant) -> Result<(), String> {
    revoke_tree_with_deadline(root, sid, Some(deadline))
}

fn revoke_tree_with_deadline(
    root: &Path,
    sid: PSID,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(path) = pending.pop() {
        if let Some(deadline) = deadline {
            ensure_preflight_cleanup_deadline(deadline)?;
        }
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
                if let Some(deadline) = deadline {
                    ensure_preflight_cleanup_deadline(deadline)?;
                }
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
    let mut desktop = PrivateDesktop {
        station,
        sid,
        handle: null_mut(),
        name: String::new(),
        startup_name: Vec::new(),
    };
    update_handle_ace(
        desktop.station,
        SE_WINDOW_OBJECT,
        sid,
        GRANT_ACCESS,
        GENERIC_ALL,
        0,
    )?;
    let station_name = user_object_name(desktop.station, "sandbox window-station")?;
    let desktop_name = format!("mini-agent-{}", uuid::Uuid::new_v4());
    let desktop_name_wide = wide_string(&desktop_name);
    let handle = unsafe {
        CreateDesktopW(
            desktop_name_wide.as_ptr(),
            null(),
            null(),
            0,
            GENERIC_ALL,
            null(),
        )
    };
    if handle.is_null() {
        return Err(last_error("create private sandbox desktop"));
    }
    desktop.handle = handle;
    desktop.name = desktop_name.clone();
    desktop.startup_name = wide_string(&format!("{station_name}\\{desktop_name}"));
    update_handle_ace(
        desktop.handle,
        SE_WINDOW_OBJECT,
        sid,
        GRANT_ACCESS,
        GENERIC_ALL,
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
    let profile_control_deadline = Instant::now() + Duration::from_secs(5);
    let profile_control = ProfileControlGuard::acquire_until(profile_control_deadline)?;
    sweep_stale_profiles_until_locked(&journal_root, profile_control_deadline, &profile_control)?;
    let name_text = format!(
        "{APPCONTAINER_PROFILE_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    );
    let name = wide_string(&name_text);
    // Publish the name before profile creation. If this trusted helper is terminated between
    // CreateAppContainerProfile and the full SID/root journal below, the parent can still delete
    // the exact profile without guessing or deleting the control tree that owns recovery.
    let (intent_path, intent_lease) =
        create_profile_intent(&journal_root, &name_text, &profile_control)?;
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
        if !sid.is_null() {
            unsafe { FreeSid(sid) };
        }
        let create_error =
            format!("sandbox: create unique AppContainer profile: HRESULT {result:#x}");
        return Err(
            match remove_profile_intent(&journal_root, &intent_path, intent_lease, &profile_control)
            {
                Ok(()) => create_error,
                Err(cleanup) => format!("{create_error}; {cleanup}"),
            },
        );
    }
    let mut profile = AppContainerProfile {
        sid,
        name,
        name_text: name_text.clone(),
        text: String::new(),
        storage: PathBuf::new(),
        journal_path: PathBuf::new(),
        journal_lease: None,
        journal_root: journal_root.clone(),
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
        std::fs::create_dir_all(profile.storage.join("Temp")).map_err(|error| {
            format!("sandbox: create private AppContainer temporary storage: {error}")
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
        let (journal_path, mut lease) = create_profile_journal(&journal_root, "json")?;
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
            let _ = journal_root.remove_file(
                &journal_path,
                "remove incomplete AppContainer cleanup journal",
            );
            return Err(error);
        }
        Ok((journal_path, lease))
    })();
    match setup {
        Ok((journal_path, lease)) => {
            profile.journal_path = journal_path;
            profile.journal_lease = Some(lease);
            if let Err(intent_error) =
                remove_profile_intent(&journal_root, &intent_path, intent_lease, &profile_control)
            {
                let cleanup = profile.finalize_cleanup();
                return Err(match cleanup {
                    Ok(()) => intent_error,
                    Err(cleanup) => format!("{intent_error}; {cleanup}"),
                });
            }
            Ok(profile)
        }
        Err(error) => {
            let rollback = profile.rollback_unjournaled();
            let intent =
                remove_profile_intent(&journal_root, &intent_path, intent_lease, &profile_control);
            Err(match (rollback, intent) {
                (Ok(()), Ok(())) => error,
                (Err(rollback), Ok(())) => {
                    format!("{error}; unjournaled rollback failed: {rollback}")
                }
                (Ok(()), Err(intent)) => format!("{error}; {intent}"),
                (Err(rollback), Err(intent)) => {
                    format!("{error}; unjournaled rollback failed: {rollback}; {intent}")
                }
            })
        }
    }
}

fn create_profile_intent(
    journal_root: &ProfileJournalRootAuthority,
    profile_name: &str,
    _profile_control: &ProfileControlGuard,
) -> Result<(PathBuf, File), String> {
    let payload = serde_json::to_vec(&ProfileIntent {
        version: PROFILE_INTENT_VERSION,
        profile_name: profile_name.to_string(),
    })
    .map_err(|error| format!("sandbox: encode AppContainer profile intent: {error}"))?;
    let (path, mut lease) = create_profile_journal(journal_root, PROFILE_INTENT_EXTENSION)?;
    if let Err(error) = lease.write_all(&payload).and_then(|()| lease.sync_all()) {
        drop(lease);
        let _ = journal_root.remove_file(&path, "remove incomplete AppContainer profile intent");
        return Err(format!(
            "sandbox: persist AppContainer profile intent: {error}"
        ));
    }
    Ok((path, lease))
}

fn remove_profile_intent(
    journal_root: &ProfileJournalRootAuthority,
    path: &Path,
    lease: File,
    _profile_control: &ProfileControlGuard,
) -> Result<(), String> {
    drop(lease);
    journal_root.remove_file(path, "remove AppContainer profile intent")
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
) -> Result<ProfileJournalRootAuthority, String> {
    let candidate = private_control_root_candidate(cache)?;
    validate_control_root_against_access_roots(&candidate, read_roots, write_roots)?;
    let root = candidate;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("sandbox: create private AppContainer control root: {error}"))?;
    let root = private_control_root_candidate(cache)?;
    validate_control_root_against_access_roots(&root, read_roots, write_roots)?;
    let directory = protect_and_attest_control_root(&root)?;
    let authority = ProfileJournalRootAuthority::new(root, directory)?;
    validate_control_root_against_access_roots(authority.path(), read_roots, write_roots)?;
    Ok(authority)
}

fn validate_control_root_against_access_roots(
    control_root: &Path,
    read_roots: &[PathBuf],
    write_roots: &[PathBuf],
) -> Result<(), String> {
    for (role, roots) in [
        (RootRole::AuthorizedReadRoot, read_roots),
        (RootRole::AuthorizedWritableRoot, write_roots),
    ] {
        for root in roots {
            if let Some(error) =
                root_role_conflict(RootRole::PrivateControlRoot, control_root, role, root)
            {
                return Err(error);
            }
        }
    }
    Ok(())
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

fn protect_and_attest_control_root(root: &Path) -> Result<File, String> {
    let directory = open_stable_path(
        root,
        true,
        READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
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

    attest_control_root_dacl(&directory, user_sid)?;
    Ok(directory)
}

fn attest_control_root_dacl(directory: &File, user_sid: PSID) -> Result<(), String> {
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

fn create_profile_journal(
    journal_root: &ProfileJournalRootAuthority,
    extension: &str,
) -> Result<(PathBuf, File), String> {
    journal_root.revalidate()?;
    let path = journal_root
        .path()
        .join(format!("{}.{}", uuid::Uuid::new_v4().simple(), extension));
    journal_root.validate_child(&path)?;
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
    Ok((path, unsafe {
        File::from_raw_handle(handle.0.into_raw_handle())
    }))
}

fn open_stale_profile_journal(
    journal_root: &ProfileJournalRootAuthority,
    path: &Path,
) -> Result<Option<File>, String> {
    journal_root.validate_child(path)?;
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

fn sweep_stale_profiles_until(
    journal_root: &ProfileJournalRootAuthority,
    deadline: Instant,
) -> Result<(), String> {
    let profile_control = ProfileControlGuard::acquire_until(deadline)?;
    sweep_stale_profiles_until_locked(journal_root, deadline, &profile_control)
}

fn sweep_stale_profiles_until_locked(
    journal_root: &ProfileJournalRootAuthority,
    deadline: Instant,
    _profile_control: &ProfileControlGuard,
) -> Result<(), String> {
    journal_root.revalidate()?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(journal_root.path())
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
    let mut journals = Vec::new();
    let mut intents = Vec::new();
    for path in entries {
        match path.extension().and_then(OsStr::to_str) {
            Some("json") => journals.push(path),
            Some(PROFILE_INTENT_EXTENSION) => intents.push(path),
            _ => {
                return Err(
                    "sandbox: unexpected entry in AppContainer cleanup journal root".into(),
                );
            }
        }
    }
    // Full journals must run first: they retain the SID and root set required to revoke ACLs.
    // An overlapping intent can then safely delete the already-absent profile idempotently.
    for path in journals {
        ensure_preflight_cleanup_deadline(deadline)?;
        sweep_stale_profile_journal(&path, journal_root, deadline)?;
    }
    for path in intents {
        ensure_preflight_cleanup_deadline(deadline)?;
        sweep_stale_profile_intent(&path, journal_root, deadline)?;
    }
    Ok(())
}

fn read_stale_control_file(
    journal_root: &ProfileJournalRootAuthority,
    path: &Path,
) -> Result<Option<(File, Vec<u8>)>, String> {
    let Some(mut lease) = open_stale_profile_journal(journal_root, path)? else {
        return Ok(None);
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
    Ok(Some((lease, payload)))
}

fn sweep_stale_profile_journal(
    path: &Path,
    journal_root: &ProfileJournalRootAuthority,
    deadline: Instant,
) -> Result<(), String> {
    ensure_preflight_cleanup_deadline(deadline)?;
    let Some((lease, payload)) = read_stale_control_file(journal_root, path)? else {
        return Ok(());
    };
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
    wait_for_stale_job_quiescence_until(&journal.job_name, deadline)?;
    let roots = canonicalize_access_roots(
        journal.roots.iter().map(PathBuf::as_path),
        journal_root.path(),
    )?;
    for root in roots {
        ensure_preflight_cleanup_deadline(deadline)?;
        revoke_tree_until(&root, sid.0, deadline)?;
    }
    ensure_preflight_cleanup_deadline(deadline)?;
    delete_appcontainer_profile(&name)?;
    drop(lease);
    ensure_preflight_cleanup_deadline(deadline)?;
    journal_root.remove_file(path, "remove stale AppContainer journal")
}

fn sweep_stale_profile_intent(
    path: &Path,
    journal_root: &ProfileJournalRootAuthority,
    deadline: Instant,
) -> Result<(), String> {
    ensure_preflight_cleanup_deadline(deadline)?;
    let Some((lease, payload)) = read_stale_control_file(journal_root, path)? else {
        return Ok(());
    };
    let intent: ProfileIntent = serde_json::from_slice(&payload)
        .map_err(|error| format!("sandbox: decode AppContainer profile intent: {error}"))?;
    if intent.version != PROFILE_INTENT_VERSION
        || !intent.profile_name.starts_with(APPCONTAINER_PROFILE_PREFIX)
        || intent.profile_name.len() != APPCONTAINER_PROFILE_PREFIX.len() + 32
    {
        return Err("sandbox: invalid AppContainer profile intent policy".into());
    }
    ensure_preflight_cleanup_deadline(deadline)?;
    delete_appcontainer_profile(&wide_string(&intent.profile_name))?;
    drop(lease);
    ensure_preflight_cleanup_deadline(deadline)?;
    journal_root.remove_file(path, "remove stale AppContainer profile intent")
}

fn ensure_preflight_cleanup_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("sandbox: Windows AppContainer recovery exceeded its cleanup deadline".into())
    } else {
        Ok(())
    }
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
    Ok((job, name))
}

fn configure_job_ui_restrictions(job: &Handle) -> Result<(), String> {
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: GENERAL_JOB_UI_RESTRICTIONS,
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
    Ok(())
}

fn verify_job_membership_and_limits(job: &Handle, child: &Handle) -> Result<(), String> {
    let mut in_job = 0;
    if unsafe { IsProcessInJob(child.raw(), job.raw(), &mut in_job) } == 0 {
        return Err(last_error("query exact restricted process Job membership"));
    }
    if in_job == 0 {
        return Err("sandbox: restricted process escaped its exact creation-time Job".into());
    }

    verify_job_limits(job)?;
    if active_job_processes(job)? != 1 {
        return Err(
            "sandbox: creation-time Job did not contain exactly its suspended target".into(),
        );
    }
    Ok(())
}

fn active_job_processes(job: &Handle) -> Result<u32, String> {
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
        return Err(last_error("query exact Job active process accounting"));
    }
    Ok(accounting.ActiveProcesses)
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
    if ui.UIRestrictionsClass != GENERAL_JOB_UI_RESTRICTIONS {
        return Err("sandbox: restricted process Job UI limits differ from policy".into());
    }
    Ok(())
}

fn wait_for_stale_job_quiescence(name: &str) -> Result<(), String> {
    wait_for_stale_job_quiescence_until(name, Instant::now() + Duration::from_secs(5))
}

fn wait_for_stale_job_quiescence_until(name: &str, deadline: Instant) -> Result<(), String> {
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
    wait_for_job_zero_until(&job, "stale AppContainer Job", deadline)
}

fn wait_for_job_zero(job: &Handle, label: &str) -> Result<(), String> {
    wait_for_job_zero_until(job, label, Instant::now() + Duration::from_secs(5))
}

fn wait_for_job_zero_until(job: &Handle, label: &str, deadline: Instant) -> Result<(), String> {
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
        std::thread::sleep(GENERAL_PREFLIGHT_POLL_INTERVAL);
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
    let mut bytes = 0usize;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes) };
    if bytes == 0 {
        return Err(last_error("size restricted process attribute list"));
    }
    let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let list = storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut bytes) } == 0 {
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
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut information,
        )
    } == 0
    {
        return Err(last_error("launch creation-time-Job AppContainer process"));
    }
    let process = Handle::created(information.hProcess, "own AppContainer process")?;
    let thread = Handle::created(information.hThread, "own suspended AppContainer thread")?;
    mark_helper_stage(HELPER_STAGE_SUSPENDED_JOB);
    // Explicitly associate the suspended target after Windows has placed it in any inherited
    // runner/service Job. This makes our non-breakaway Job the immediate child in that hierarchy;
    // no target code can run between creation and the verified association.
    if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
        let error = last_error("assign suspended AppContainer process to bounded Job");
        unsafe { TerminateProcess(process.raw(), 126) };
        let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
        return Err(error);
    }
    // A Job hierarchy can only be formed while neither Job has UI limits. Apply the complete UI
    // lockdown only after the hierarchy exists and before resuming the target.
    if let Err(error) = configure_job_ui_restrictions(job) {
        unsafe { TerminateProcess(process.raw(), 126) };
        let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
        return Err(error);
    }
    if let Err(error) = verify_job_membership_and_limits(job, &process) {
        unsafe { TerminateProcess(process.raw(), 126) };
        let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
        return Err(error);
    }
    match process_token_is_regular_appcontainer(process.raw()) {
        Ok(true) => {}
        Ok(false) => {
            unsafe { TerminateProcess(process.raw(), 126) };
            let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
            return Err("sandbox: restricted process token was not a regular AppContainer".into());
        }
        Err(error) => {
            unsafe { TerminateProcess(process.raw(), 126) };
            let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
            return Err(error);
        }
    }
    mark_helper_stage(HELPER_STAGE_RESUME);
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        let error = last_error("resume attested AppContainer process");
        unsafe { TerminateProcess(process.raw(), 126) };
        let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
        return Err(error);
    }
    Ok(process)
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

fn target_inherited_omitted_canary(target: &Handle, canary: &Handle) -> Result<bool, String> {
    let mut candidate = null_mut();
    if unsafe {
        DuplicateHandle(
            target.raw(),
            canary.raw(),
            GetCurrentProcess(),
            &mut candidate,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(6) {
            return Ok(false);
        }
        return Err(format!(
            "sandbox: inspect omitted target handle identity: {error}"
        ));
    }
    let candidate = Handle::created(candidate, "own target handle-identity candidate")?;
    Ok(unsafe { CompareObjectHandles(canary.raw(), candidate.raw()) } != 0)
}

fn appcontainer_environment(cache: &Path, private_storage: &Path) -> Vec<u16> {
    let mut entries = essential_windows_environment();
    let private_storage = private_storage.as_os_str().to_string_lossy().into_owned();
    let private_temp = Path::new(&private_storage)
        .join("Temp")
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    let cache = cache.as_os_str().to_string_lossy().into_owned();
    entries.push(("LOCALAPPDATA".into(), private_storage));
    entries.push(("TEMP".into(), private_temp.clone()));
    entries.push(("TMP".into(), private_temp));
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
    let probe_executable = canonical_file(
        &std::env::current_exe().map_err(|error| error.to_string())?,
        "sandbox target-probe executable",
    )?;
    let cleanup_ready = workspace.join("cleanup-ready.txt");
    let mut command = build_helper_with_ready(
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_BOUNDARY_ARG.into(),
            inside_file.to_string_lossy().into_owned(),
            cache_fixture.to_string_lossy().into_owned(),
            outside_secret.to_string_lossy().into_owned(),
            outside_file.to_string_lossy().into_owned(),
        ],
        &workspace,
        &cache,
        Some(cleanup_ready.clone()),
    )?;
    let mut output = command
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run write-boundary probe: {e}"))?;
    if !output.status.success() || !inside_file.exists() || outside_file.exists() {
        if output.stderr.is_empty() {
            let mut diagnostic = String::from("status=");
            diagnostic.push_str(
                &output
                    .status
                    .code()
                    .map_or_else(|| String::from("none"), |code| code.to_string()),
            );
            diagnostic.push_str(" inside=");
            diagnostic.push_str(if inside_file.exists() {
                "true"
            } else {
                "false"
            });
            diagnostic.push_str(" outside=");
            diagnostic.push_str(if outside_file.exists() {
                "true"
            } else {
                "false"
            });
            output.stderr = diagnostic.into_bytes();
        }
        return Err(format!(
            "explicit read/write boundary probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    attest_completed_cleanup(
        &cleanup_ready,
        [&workspace, &cache, probe_executable.as_path()],
    )?;

    let configured_read = base.join("configured-read");
    let configured_write = base.join("configured-write");
    let configured_tool = configured_read.join("configured-tool.exe");
    std::fs::create_dir_all(&configured_read).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&configured_write).map_err(|e| e.to_string())?;
    std::fs::copy(&probe_executable, &configured_tool).map_err(|e| e.to_string())?;
    let configured_fixture = configured_read.join("fixture.txt");
    let configured_output = configured_write.join("output.txt");
    let configured_cleanup_ready = workspace.join("configured-cleanup-ready.txt");
    std::fs::write(&configured_fixture, b"configured-read").map_err(|e| e.to_string())?;
    let mut configured_launch = build_helper_with_ready_and_roots(
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_CONFIGURED_ARG.into(),
            configured_fixture.to_string_lossy().into_owned(),
            configured_tool.to_string_lossy().into_owned(),
            configured_output.to_string_lossy().into_owned(),
        ],
        &workspace,
        &cache,
        Some(configured_cleanup_ready.clone()),
        std::slice::from_ref(&configured_read),
        std::slice::from_ref(&configured_write),
    )?;
    let configured_result = configured_launch
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run configured AppContainer tool/root probe: {e}"))?;
    if !configured_result.status.success() || !configured_output.exists() {
        return Err(format!(
            "configured AppContainer tool/root probe failed: status={} output={}",
            configured_result
                .status
                .code()
                .map_or_else(|| String::from("none"), |code| code.to_string()),
            configured_output.exists()
        ));
    }
    attest_completed_cleanup(
        &configured_cleanup_ready,
        [
            workspace.as_path(),
            cache.as_path(),
            probe_executable.as_path(),
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
        probe_executable.clone(),
        vec![TARGET_PROBE_ARG.into(), TARGET_NOOP_ARG.into()],
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

    let mut max_request = build_helper(
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_NOOP_ARG.into(),
            "x".repeat(18_000),
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
    let mut concurrent_a_command = build_helper(
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_WRITE_ARG.into(),
            concurrent_a.to_string_lossy().into_owned(),
        ],
        &workspace,
        &cache,
    )?;
    let mut concurrent_b_command = build_helper(
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_WRITE_ARG.into(),
            concurrent_b.to_string_lossy().into_owned(),
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
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_SLEEP_ARG.into(),
            "10".into(),
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
    let mut second_launch = build_helper(
        probe_executable.clone(),
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_BOUNDARY_ARG.into(),
            workspace_b_file.to_string_lossy().into_owned(),
            cache_fixture.to_string_lossy().into_owned(),
            outside_secret.to_string_lossy().into_owned(),
            escaped_a.to_string_lossy().into_owned(),
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
    let crash_cleanup_roots = [&workspace, &cache, probe_executable.as_path()];
    attest_cleanup_proof(&crash_proof, crash_cleanup_roots)?;

    let authority_escape = outside.join("authority-escape.txt");
    let authority_read = base.join("authority-descendant-read");
    let authority_descendant = authority_read.join("authority-descendant.exe");
    std::fs::create_dir_all(&authority_read).map_err(|e| e.to_string())?;
    std::fs::copy(&probe_executable, &authority_descendant).map_err(|e| e.to_string())?;
    let mut authority_probe = build_helper_with_ready_and_roots(
        probe_executable.clone(),
        vec![
            AUTHORITY_PROBE_ARG.into(),
            HELPER_PID_PLACEHOLDER.into(),
            unsafe { GetCurrentProcessId() }.to_string(),
            authority_escape.to_string_lossy().into_owned(),
            DESKTOP_NAME_PLACEHOLDER.into(),
            CONTROL_ROOT_PLACEHOLDER.into(),
            authority_descendant.to_string_lossy().into_owned(),
        ],
        &workspace_b,
        &cache,
        None,
        std::slice::from_ref(&authority_read),
        &[],
    )?;
    let authority_result = authority_probe
        .as_std_mut()
        .output_guarded()
        .map_err(|e| format!("run restricted authority probe: {e}"))?;
    if !authority_result.status.success() || authority_escape.exists() || escaped_a.exists() {
        return Err(format!(
            "authority probe failed: status={} outside={} prior_workspace={}",
            authority_result
                .status
                .code()
                .map_or_else(|| String::from("none"), |code| code.to_string()),
            authority_escape.exists(),
            escaped_a.exists()
        ));
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
    if !is_available() || !is_available() {
        return Err("cached production AppContainer preflight failed".into());
    }
    println!(
        "WINDOWS_GENERAL_SANDBOX_PASS appcontainer=regular explicit_reads=pass configured_tool=pass workspace_write=pass outside_read=denied outside_write=denied hardlink=denied unique_profile_crash=pass authority_escape=denied omitted_handle=denied descendant=contained breakaway=denied control_journal=denied bounded_pipe=pass acl_serialization=pass parent_death_job=pass private_desktop=pass ui_job=restricted network=denied registry=not_isolated"
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
    let probe_executable = canonical_file(
        &std::env::current_exe().map_err(|error| error.to_string())?,
        "parent target-probe executable",
    )?;
    let descendant_read = base.join("parent-descendant-read");
    let descendant_executable = descendant_read.join("parent-descendant.exe");
    std::fs::create_dir_all(&descendant_read).map_err(|error| error.to_string())?;
    std::fs::copy(&probe_executable, &descendant_executable).map_err(|error| error.to_string())?;
    let mut helper = build_helper_with_ready_and_roots(
        probe_executable,
        vec![
            TARGET_PROBE_ARG.into(),
            TARGET_PARENT_ARG.into(),
            descendant_executable.to_string_lossy().into_owned(),
            tree_ready.to_string_lossy().into_owned(),
            marker.to_string_lossy().into_owned(),
        ],
        &workspace,
        &cache,
        Some(ready_path.clone()),
        std::slice::from_ref(&descendant_read),
        &[],
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

fn run_authority_probe(mut args: std::env::ArgsOs) -> i32 {
    let Some(helper_pid) = parse_probe_pid(args.next(), "helper").ok() else {
        return AUTHORITY_ARGUMENT_ERROR;
    };
    let Some(parent_pid) = parse_probe_pid(args.next(), "parent").ok() else {
        return AUTHORITY_ARGUMENT_ERROR;
    };
    let Some(outside) = args.next().map(PathBuf::from) else {
        return AUTHORITY_ARGUMENT_ERROR;
    };
    let Some(expected_desktop) = args.next().and_then(|value| value.into_string().ok()) else {
        return AUTHORITY_ARGUMENT_ERROR;
    };
    let Some(control_root) = args.next().map(PathBuf::from) else {
        return AUTHORITY_ARGUMENT_ERROR;
    };
    let Some(descendant_executable) = args.next().map(PathBuf::from) else {
        return AUTHORITY_ARGUMENT_ERROR;
    };
    if args.next().is_some() || outside.exists() {
        return AUTHORITY_ARGUMENT_ERROR;
    }
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    if desktop.is_null() {
        return AUTHORITY_DESKTOP_QUERY_ERROR;
    }
    let Ok(desktop_name) = user_object_name(desktop, "authority-probe desktop") else {
        return AUTHORITY_DESKTOP_QUERY_ERROR;
    };
    if desktop_name != expected_desktop || !expected_desktop.starts_with("mini-agent-") {
        return 93;
    }
    if try_probe_write(&outside) {
        return 91;
    }
    let Ok(is_appcontainer) = current_token_is_appcontainer() else {
        return AUTHORITY_TOKEN_QUERY_ERROR;
    };
    if !is_appcontainer {
        return 102;
    }
    if std::fs::read_dir(&control_root).is_ok()
        || std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(control_root.join("child-access-canary"))
            .is_ok()
    {
        return 100;
    }
    if process_token_is_acquirable(helper_pid, &outside)
        || process_token_is_acquirable(parent_pid, &outside)
    {
        return 90;
    }
    let descendant_status = run_descendant_token_probe(&descendant_executable);
    if descendant_status != 0 {
        return descendant_status;
    }
    let Ok(breakaway_executable) = std::env::current_exe() else {
        return AUTHORITY_BREAKAWAY_EXE_ERROR;
    };
    let mut breakaway = Command::new(breakaway_executable);
    breakaway
        .arg("--help")
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    match breakaway.status_guarded() {
        Err(error) if error.raw_os_error() == Some(5) => {}
        Ok(_) => return 101,
        Err(_) => return AUTHORITY_BREAKAWAY_RESULT_ERROR,
    }
    let Ok(has_zero_capabilities) = current_token_has_zero_capabilities() else {
        return AUTHORITY_CAPABILITY_QUERY_ERROR;
    };
    if !has_zero_capabilities {
        return 92;
    }
    // Zero network capabilities plus the helper's no-loopback-exemption attestation are the
    // protocol-independent enforcement proof. TCP attempts are behavioral negative controls:
    // every connection must fail, while the exact error may instead describe host routing
    // (notably absent IPv6). Do not use UDP `send` as a negative control: Winsock may successfully
    // queue a connectionless datagram even when AppContainer policy prevents network delivery.
    if !tcp_attempt_failed("127.0.0.1:9")
        || !tcp_attempt_failed("1.1.1.1:9")
        || !tcp_attempt_failed("[::1]:9")
        || !tcp_attempt_failed("[2606:4700:4700::1111]:9")
    {
        return 94;
    }
    0
}

fn run_descendant_token_probe(executable: &Path) -> i32 {
    // The child independently proves that ordinary tool descendants retain the zero-capability
    // AppContainer token. Lifetime containment is attested separately by the parent-death probe:
    // Windows can place an AppContainer process tree in a system-managed Job whose descendants do
    // not appear as additional processes in this launcher's private Job accounting.
    let mut descendant_command = Command::new(executable);
    descendant_command.arg(DESCENDANT_PROBE_ARG);
    // Inherit the target's already-contained handles: opening the denied Windows NUL device via
    // `Stdio::null()` would fail before CreateProcessW and would not test descendant containment.
    let mut descendant = match descendant_command.spawn_guarded() {
        Ok(descendant) => descendant,
        Err(error) => {
            let code = target_probe_os_error_code(TARGET_DESCENDANT_SPAWN_ERROR_BASE, &error);
            return if code == TARGET_DESCENDANT_SPAWN_ERROR_BASE {
                AUTHORITY_DESCENDANT_SPAWN_FAILED
            } else {
                code
            };
        }
    };
    let descendant_status = match descendant.wait() {
        Ok(status) => status,
        Err(error) => {
            let code = target_probe_os_error_code(TARGET_DESCENDANT_WAIT_ERROR_BASE, &error);
            return if code == TARGET_DESCENDANT_WAIT_ERROR_BASE {
                AUTHORITY_DESCENDANT_WAIT_FAILED
            } else {
                code
            };
        }
    };
    if descendant_status.success() {
        0
    } else {
        descendant_status
            .code()
            .unwrap_or(AUTHORITY_DESCENDANT_NO_EXIT_CODE)
    }
}

fn run_descendant_probe(mut args: std::env::ArgsOs) -> i32 {
    if args.next().is_some() {
        return DESCENDANT_ARGUMENT_ERROR;
    }
    let Ok(has_zero_capabilities) = current_token_has_zero_capabilities() else {
        return DESCENDANT_CAPABILITY_QUERY_ERROR;
    };
    if !has_zero_capabilities {
        return 2;
    }
    let Ok(is_appcontainer) = current_token_is_appcontainer() else {
        return DESCENDANT_APPCONTAINER_QUERY_ERROR;
    };
    if !is_appcontainer {
        return 2;
    }
    0
}

fn token_is_appcontainer(token: &Handle, context: &str) -> Result<bool, String> {
    let mut value = 0u32;
    let mut returned = 0u32;
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenIsAppContainer,
            (&mut value as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut returned,
        )
    } == 0
    {
        return Err(last_error(context));
    }
    if returned != size_of::<u32>() as u32 {
        return Err(format!("{context}: invalid token value size"));
    }
    Ok(value != 0)
}

fn current_token_is_appcontainer() -> Result<bool, String> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(last_error("open descendant AppContainer token"));
    }
    let token = Handle::created(raw, "open descendant AppContainer token")?;
    token_is_appcontainer(&token, "read descendant TokenIsAppContainer")
}

fn appcontainer_has_no_loopback_exemption(appcontainer_sid: PSID) -> Result<bool, String> {
    if appcontainer_sid.is_null() {
        return Err("sandbox: missing AppContainer SID for loopback proof".into());
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
        if !candidate.is_null() && unsafe { EqualSid(appcontainer_sid, candidate) } != 0 {
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
    let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    let capacity = (storage.len() * size_of::<usize>()) as u32;
    let mut returned = capacity;
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenCapabilities,
            storage.as_mut_ptr().cast(),
            capacity,
            &mut returned,
        )
    } == 0
    {
        return Err(last_error("read AppContainer TokenCapabilities"));
    }
    if returned < size_of::<u32>() as u32 || returned > capacity {
        return Err("sandbox: invalid returned TokenCapabilities size".into());
    }
    let header = unsafe { *storage.as_ptr().cast::<u32>() };
    Ok(header == 0)
}

fn tcp_attempt_failed(address: &str) -> bool {
    let Ok(address) = address.parse() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(750)).is_err()
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

fn process_token_is_regular_appcontainer(process: HANDLE) -> Result<bool, String> {
    mark_helper_stage(HELPER_STAGE_REGULAR_TOKEN_OPEN);
    let mut raw_token = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut raw_token) } == 0 {
        return Err(last_error("open restricted process token"));
    }
    let token = Handle::created(raw_token, "restricted process token")?;
    mark_helper_stage(HELPER_STAGE_REGULAR_TOKEN_SID);
    if !token_is_appcontainer(&token, "read restricted process TokenIsAppContainer")? {
        return Ok(false);
    }
    mark_helper_stage(HELPER_STAGE_REGULAR_TOKEN_DUPLICATE);
    let mut raw_impersonation = null_mut();
    if unsafe { DuplicateToken(token.raw(), SecurityImpersonation, &mut raw_impersonation) } == 0 {
        return Err(last_error(
            "duplicate restricted process token for access semantics",
        ));
    }
    let impersonation =
        Handle::created(raw_impersonation, "restricted process impersonation token")?;

    // A regular AppContainer participates in both ALL APPLICATION PACKAGES (AC) and ALL
    // RESTRICTED APPLICATION PACKAGES; LPAC deliberately ignores AC. AccessCheck therefore
    // distinguishes the effective token semantics without relying on the optional LPAC token
    // information class.
    mark_helper_stage(HELPER_STAGE_REGULAR_TOKEN_DESCRIPTOR);
    let descriptor_sddl = wide_string("O:SYG:SYD:(A;;0x3;;;WD)(A;;0x1;;;AC)(A;;0x2;;;S-1-15-2-2)");
    let mut raw_descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut raw_descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(last_error(
            "construct regular AppContainer ALL_APPLICATION_PACKAGES access descriptor",
        ));
    }
    let descriptor = Local(raw_descriptor);
    let mapping = GENERIC_MAPPING::default();
    let mut privilege_set = PRIVILEGE_SET::default();
    let mut privilege_set_bytes = size_of::<PRIVILEGE_SET>() as u32;
    let mut granted_access = 0u32;
    let mut access_status = 0i32;
    mark_helper_stage(HELPER_STAGE_REGULAR_TOKEN_ACCESS);
    if unsafe {
        AccessCheck(
            descriptor.0,
            impersonation.raw(),
            MAXIMUM_ALLOWED,
            &mapping,
            &mut privilege_set,
            &mut privilege_set_bytes,
            &mut granted_access,
            &mut access_status,
        )
    } == 0
    {
        return Err(last_error(
            "evaluate regular AppContainer ALL_APPLICATION_PACKAGES access",
        ));
    }
    Ok(access_status != 0 && granted_access == 0x3)
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    const PREFLIGHT_RECOVERY_CHILD_ROOT: &str = "ZS_TEST_PREFLIGHT_RECOVERY_ROOT";

    struct RootPolicyFixture(PathBuf);

    impl RootPolicyFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mini-agent-root-policy-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).expect("create root-policy fixture");
            Self(root)
        }

        fn directory(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(&path).expect("create root-policy directory");
            std::fs::canonicalize(path).expect("canonicalize root-policy directory")
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create root-policy file parent");
            }
            std::fs::write(&path, b"fixture").expect("create root-policy file");
            std::fs::canonicalize(path).expect("canonicalize root-policy file")
        }
    }

    impl Drop for RootPolicyFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn validate_policy_fixture(
        program: &Path,
        workspace: &Path,
        cache: &Path,
        configured_reads: &[PathBuf],
        configured_writes: &[PathBuf],
    ) -> Result<(), String> {
        let read_roots = collect_read_roots(program, workspace, cache, configured_reads)?;
        let write_roots = collect_write_roots(workspace, configured_writes)?;
        validate_explicit_root_policy(
            program,
            workspace,
            cache,
            configured_reads,
            configured_writes,
            &read_roots,
            &write_roots,
        )
    }

    #[test]
    fn general_preflight_cache_runs_an_unavailable_probe_once() {
        let cache = Arc::new(OnceLock::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let calls = calls.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                cached_general_sandbox_availability(&cache, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err("closed injected failure".into())
                })
            }));
        }
        for thread in threads {
            assert!(!thread.join().expect("cache caller must not panic"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!cached_general_sandbox_availability(&cache, || {
            panic!("cached unavailable result must not rerun its probe")
        }));
    }

    #[test]
    fn project_workspace_and_separate_default_cache_topology_is_allowed() {
        let fixture = RootPolicyFixture::new();
        let workspace = fixture.directory("projects/example");
        let cache = fixture.directory("home/AppData/Local/zerostack/cache");
        let program = fixture.file("bin/tool.exe");

        validate_policy_fixture(&program, &workspace, &cache, &[], &[])
            .expect("separate project and default-cache topology must remain valid");
    }

    #[test]
    fn workspace_containing_cache_reports_roles_direction_and_remedies_without_paths() {
        let fixture = RootPolicyFixture::new();
        let workspace = fixture.directory("home");
        let cache = fixture.directory("home/AppData/Local/zerostack/cache");
        let program = fixture.file("bin/tool.exe");

        let error = validate_policy_fixture(&program, &workspace, &cache, &[], &[])
            .expect_err("workspace containing cache must fail closed");
        assert!(error.contains("workspace contains read-only application cache"));
        assert!(error.contains("project subdirectory"));
        assert!(error.contains("ZS_CACHE_DIR outside the workspace"));
        assert!(!error.contains(fixture.0.to_string_lossy().as_ref()));
    }

    #[test]
    fn cache_containing_workspace_reports_the_converse_without_paths() {
        let fixture = RootPolicyFixture::new();
        let cache = fixture.directory("cache");
        let workspace = fixture.directory("cache/projects/example");
        let program = fixture.file("bin/tool.exe");

        let error = validate_policy_fixture(&program, &workspace, &cache, &[], &[])
            .expect_err("cache containing workspace must fail closed");
        assert!(error.contains("read-only application cache contains workspace"));
        assert!(error.contains("move the project outside the application cache"));
        assert!(error.contains("does not contain the workspace"));
        assert!(!error.contains(fixture.0.to_string_lossy().as_ref()));
    }

    #[test]
    fn configured_root_conflict_reports_both_roles_and_direction() {
        let fixture = RootPolicyFixture::new();
        let workspace = fixture.directory("project");
        let cache = fixture.directory("state/cache");
        let program = fixture.file("bin/tool.exe");
        let configured_read = fixture.directory("shared");
        let configured_write = fixture.directory("shared/output");

        let error = validate_policy_fixture(
            &program,
            &workspace,
            &cache,
            std::slice::from_ref(&configured_read),
            std::slice::from_ref(&configured_write),
        )
        .expect_err("configured read/write overlap must fail closed");
        assert!(error.contains("configured read-only root contains configured writable root"));
        assert!(!error.contains(fixture.0.to_string_lossy().as_ref()));
    }

    #[test]
    fn configured_read_containing_control_root_is_rejected_before_creation() {
        let fixture = RootPolicyFixture::new();
        let workspace = fixture.directory("project");
        let cache = fixture.directory("state/cache");
        let program = fixture.file("bin/tool.exe");
        let configured_read = std::fs::canonicalize(cache.parent().expect("cache parent"))
            .expect("canonicalize configured read root");
        let control = configured_read.join(PRIVATE_CONTROL_ROOT_NAME);
        assert!(!control.exists());

        let error = validate_policy_fixture(
            &program,
            &workspace,
            &cache,
            std::slice::from_ref(&configured_read),
            &[],
        )
        .expect_err("configured read containing control root must fail closed");
        assert!(
            error.contains("configured read-only root contains private AppContainer control root")
        );
        assert!(error.contains("ZS_CACHE_DIR"));
        assert!(!error.contains(fixture.0.to_string_lossy().as_ref()));
        assert!(
            !control.exists(),
            "validation created the private control root"
        );
    }

    #[test]
    fn ordinary_and_verbatim_aliases_canonicalize_to_one_policy_identity() {
        let fixture = RootPolicyFixture::new();
        let canonical = fixture.directory("aliases/root");
        let canonical_text = canonical.to_string_lossy();
        let (ordinary, verbatim) = if let Some(ordinary) = canonical_text.strip_prefix(r"\\?\") {
            (PathBuf::from(ordinary), canonical.clone())
        } else {
            (
                canonical.clone(),
                PathBuf::from(format!(r"\\?\{canonical_text}")),
            )
        };

        assert_eq!(
            canonical_root(&ordinary, "ordinary alias").expect("canonicalize ordinary alias"),
            canonical_root(&verbatim, "verbatim alias").expect("canonicalize verbatim alias")
        );
        let directory = protect_and_attest_control_root(&canonical)
            .expect("attest control root through canonical alias");
        let authority = ProfileJournalRootAuthority::new(ordinary, directory)
            .expect("bind ordinary alias to the canonical file identity");
        authority
            .revalidate()
            .expect("ordinary/verbatim alias must preserve bound identity");
        assert!(reject_remote_access_path(Path::new(r"\\server\share\root")).is_err());
        assert!(reject_remote_access_path(Path::new(r"\\?\UNC\server\share\root")).is_err());
        drop(authority);
    }

    fn attempt_control_directory_swap(target: PathBuf, replacement: PathBuf) -> std::io::Error {
        let barrier = Arc::new(Barrier::new(2));
        let mutator_barrier = barrier.clone();
        let mutator = std::thread::spawn(move || {
            mutator_barrier.wait();
            std::fs::rename(target, replacement)
                .expect_err("bound control authority must deny directory replacement")
        });
        barrier.wait();
        mutator.join().expect("control-root mutator must not panic")
    }

    #[test]
    fn control_root_authority_denies_root_swap_after_attestation() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-control-root-swap-test-{}",
            uuid::Uuid::new_v4()
        ));
        let cache = base.join("state/cache");
        std::fs::create_dir_all(&cache).expect("create root-swap cache");
        let authority = profile_journal_root(&cache, &[], &[]).expect("bind private control root");
        let replacement = authority.path().with_extension("replacement");

        let error =
            attempt_control_directory_swap(authority.path().to_path_buf(), replacement.clone());
        assert!(matches!(error.raw_os_error(), Some(5 | 32 | 33)));
        authority
            .revalidate()
            .expect("failed swap must leave the original authority valid");
        assert!(!replacement.exists());
        assert_eq!(
            std::fs::read_dir(authority.path())
                .expect("enumerate protected control root")
                .count(),
            0,
            "failed swap must not create an external journal or residue"
        );

        drop(authority);
        std::fs::remove_dir_all(base).expect("remove root-swap tree");
    }

    #[test]
    fn control_root_authority_denies_ancestor_swap_after_attestation() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-control-ancestor-swap-test-{}",
            uuid::Uuid::new_v4()
        ));
        let state = base.join("state");
        let cache = state.join("cache");
        std::fs::create_dir_all(&cache).expect("create ancestor-swap cache");
        let authority = profile_journal_root(&cache, &[], &[]).expect("bind private control root");
        let replacement = base.join("state-replacement");

        let error = attempt_control_directory_swap(state.clone(), replacement.clone());
        assert!(matches!(error.raw_os_error(), Some(5 | 32 | 33)));
        authority
            .revalidate()
            .expect("failed ancestor swap must leave the original authority valid");
        assert!(!replacement.exists());
        assert_eq!(
            std::fs::read_dir(authority.path())
                .expect("enumerate protected control root")
                .count(),
            0,
            "failed ancestor swap must not create an external journal or residue"
        );

        drop(authority);
        std::fs::remove_dir_all(base).expect("remove ancestor-swap tree");
    }

    #[test]
    fn control_root_authority_rejects_rebound_identity_without_leaking_paths() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-control-identity-test-{}",
            uuid::Uuid::new_v4()
        ));
        let original = base.join("original");
        let rebound = base.join("rebound");
        std::fs::create_dir_all(&original).expect("create original control directory");
        std::fs::create_dir_all(&rebound).expect("create rebound control directory");
        let directory =
            protect_and_attest_control_root(&original).expect("attest original control root");

        let error = ProfileJournalRootAuthority::new(rebound.clone(), directory)
            .expect_err("mismatched path and handle identities must fail closed");
        assert_eq!(error, CONTROL_ROOT_AUTHORITY_ERROR);
        assert!(!error.contains(original.to_string_lossy().as_ref()));
        assert!(!error.contains(rebound.to_string_lossy().as_ref()));

        std::fs::remove_dir_all(base).expect("remove identity-test tree");
    }

    #[test]
    fn control_root_authority_rejects_post_attestation_dacl_change() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-control-dacl-test-{}",
            uuid::Uuid::new_v4()
        ));
        let cache = base.join("state/cache");
        std::fs::create_dir_all(&cache).expect("create DACL-test cache");
        let authority = profile_journal_root(&cache, &[], &[]).expect("bind private control root");

        let result = unsafe {
            SetSecurityInfo(
                authority.0._directory.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert_eq!(
            result, 0,
            "replace owner-only DACL with an injected null DACL"
        );
        assert_eq!(
            authority
                .revalidate()
                .expect_err("changed control-root DACL must fail closed"),
            CONTROL_ROOT_AUTHORITY_ERROR
        );

        let restored = protect_and_attest_control_root(authority.path())
            .expect("restore owner-only DACL for cleanup");
        authority
            .revalidate()
            .expect("restored owner-only DACL must revalidate");
        drop(restored);
        drop(authority);
        std::fs::remove_dir_all(base).expect("remove DACL-test tree");
    }

    #[test]
    fn stale_pre_profile_intent_is_removed_without_a_profile() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-general-intent-test-{}",
            uuid::Uuid::new_v4()
        ));
        let cache = base.join("cache");
        std::fs::create_dir_all(&cache).expect("create intent-test cache");
        let journal_root =
            profile_journal_root(&cache, &[], &[]).expect("create private journal root");
        let name = format!(
            "{APPCONTAINER_PROFILE_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        );
        let profile_control =
            ProfileControlGuard::acquire_until(Instant::now() + Duration::from_secs(5))
                .expect("acquire profile control");
        let (intent, lease) = create_profile_intent(&journal_root, &name, &profile_control)
            .expect("persist profile intent");
        drop(lease);
        drop(profile_control);

        sweep_stale_profiles_until(&journal_root, Instant::now() + Duration::from_secs(5))
            .expect("sweep profile intent");
        assert!(!intent.exists());
        drop(journal_root);
        std::fs::remove_dir_all(base).expect("remove intent-test tree");
    }

    #[test]
    fn expired_recovery_preserves_profile_intent_for_later_startup() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-expired-intent-test-{}",
            uuid::Uuid::new_v4()
        ));
        let cache = base.join("cache");
        std::fs::create_dir_all(&cache).expect("create expired-intent cache");
        let journal_root =
            profile_journal_root(&cache, &[], &[]).expect("create private journal root");
        let name = format!(
            "{APPCONTAINER_PROFILE_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        );
        let profile_control =
            ProfileControlGuard::acquire_until(Instant::now() + Duration::from_secs(5))
                .expect("acquire profile control");
        let (intent, lease) = create_profile_intent(&journal_root, &name, &profile_control)
            .expect("persist profile intent");
        drop(lease);

        let error = sweep_stale_profile_intent(&intent, &journal_root, Instant::now())
            .expect_err("expired recovery must not consume its durable intent");
        assert!(error.contains("cleanup deadline"));
        assert!(intent.exists());

        std::fs::remove_file(&intent).expect("remove preserved intent");
        drop(profile_control);
        drop(journal_root);
        std::fs::remove_dir_all(base).expect("remove expired-intent tree");
    }

    #[test]
    fn profile_intent_transition_excludes_a_concurrent_sweeper() {
        let base = std::env::temp_dir().join(format!(
            "mini-agent-general-intent-race-test-{}",
            uuid::Uuid::new_v4()
        ));
        let cache = base.join("cache");
        std::fs::create_dir_all(&cache).expect("create intent-race cache");
        let journal_root =
            profile_journal_root(&cache, &[], &[]).expect("create private journal root");
        let profile_control =
            ProfileControlGuard::acquire_until(Instant::now() + Duration::from_secs(5))
                .expect("acquire profile control");
        let name = format!(
            "{APPCONTAINER_PROFILE_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        );
        let (intent, lease) = create_profile_intent(&journal_root, &name, &profile_control)
            .expect("persist profile intent");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let sweep_root = journal_root.clone();
        let sweeper = std::thread::spawn(move || {
            started_tx.send(()).expect("publish sweeper start");
            let result =
                sweep_stale_profiles_until(&sweep_root, Instant::now() + Duration::from_secs(5));
            finished_tx.send(result).expect("publish sweep result");
        });
        started_rx.recv().expect("sweeper must start");
        assert!(
            finished_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "sweeper entered an active intent transition"
        );

        remove_profile_intent(&journal_root, &intent, lease, &profile_control)
            .expect("remove intent while transition remains owned");
        drop(profile_control);
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("sweeper must finish after transition")
            .expect("sweep after transition");
        sweeper.join().expect("sweeper must not panic");
        assert!(!intent.exists());
        drop(journal_root);
        std::fs::remove_dir_all(base).expect("remove intent-race tree");
    }

    fn new_preflight_recovery_fixture() -> (PathBuf, TemporaryPreflightRoot) {
        let parent = std::env::temp_dir().join(format!(
            "mini-agent-preflight-recovery-test-parent-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&parent).expect("create preflight-recovery parent");
        let parent = canonical_root(&parent, "preflight-recovery parent")
            .expect("canonicalize preflight-recovery parent");
        let root = TemporaryPreflightRoot::create(&parent)
            .expect("create private preflight-recovery root");
        (parent, root)
    }

    #[test]
    fn preflight_recovery_names_are_exact_lower_hex_uuids() {
        assert!(preflight_root_name_is_valid(OsStr::new(
            "mini-agent-windows-sandbox-preflight-01234567-89ab-cdef-0123-456789abcdef"
        )));
        for invalid in [
            "mini-agent-windows-sandbox-preflight-0123456789abcdef",
            "mini-agent-windows-sandbox-preflight-01234567-89AB-CDEF-0123-456789ABCDEF",
            "mini-agent-windows-sandbox-preflight-01234567-89ab-cdef-0123-456789abcdeg",
            "prefix-mini-agent-windows-sandbox-preflight-01234567-89ab-cdef-0123-456789abcdef",
        ] {
            assert!(!preflight_root_name_is_valid(OsStr::new(invalid)));
        }
    }

    #[test]
    fn later_preflight_sweep_removes_a_verified_abandoned_root_only() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let abandoned = root.path().to_path_buf();
        std::fs::create_dir(abandoned.join("workspace")).expect("create abandoned workspace");
        std::fs::create_dir(abandoned.join("cache")).expect("create abandoned cache");
        std::fs::create_dir(abandoned.join("outside")).expect("create abandoned outside root");
        std::fs::write(abandoned.join("workspace/output.txt"), b"bounded")
            .expect("write bounded abandoned output");
        let unrelated = parent.join("unrelated-entry");
        std::fs::write(&unrelated, b"preserve").expect("write unrelated temp entry");
        root.retain_recovery_state();

        recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect("recover verified abandoned preflight root");

        assert!(!abandoned.exists());
        assert_eq!(
            std::fs::read(&unrelated).expect("read unrelated entry"),
            b"preserve"
        );
        std::fs::remove_dir_all(parent).expect("remove preflight-recovery parent");
    }

    #[test]
    fn preserved_preflight_recovery_child() {
        let Some(parent) = std::env::var_os(PREFLIGHT_RECOVERY_CHILD_ROOT) else {
            return;
        };
        recover_preserved_preflight_roots(
            Path::new(&parent),
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect("child process recovers preserved preflight root");
    }

    #[test]
    fn next_process_recovers_a_preserved_profile_job_acls_and_root() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let abandoned = root.path().to_path_buf();
        let workspace = abandoned.join("workspace");
        let cache = abandoned.join("cache");
        std::fs::create_dir(&workspace).expect("create restart-recovery workspace");
        std::fs::create_dir(&cache).expect("create restart-recovery cache");
        let executable = canonical_file(
            &std::env::current_exe().expect("resolve restart-recovery executable"),
            "restart-recovery executable",
        )
        .expect("canonicalize restart-recovery executable");
        let ready = workspace.join("restart-ready.txt");
        let mut command = build_helper_with_ready(
            executable.clone(),
            vec![
                TARGET_PROBE_ARG.into(),
                TARGET_SLEEP_ARG.into(),
                "10".into(),
            ],
            &workspace,
            &cache,
            Some(ready.clone()),
        )
        .expect("build restart-recovery helper");
        command
            .as_std_mut()
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut helper = command
            .as_std_mut()
            .spawn_guarded_until(Instant::now() + Duration::from_secs(5))
            .expect("start restart-recovery helper");
        wait_for_probe_file(&ready).expect("restart-recovery helper must become ready");
        helper.kill().expect("crash restart-recovery helper");
        let _ = helper.wait();
        let proof = parse_crash_cleanup_proof(&ready)
            .expect("crashed helper must leave complete recovery proof");
        assert!(proof.storage.exists());
        assert!(proof.journal.exists());
        root.retain_recovery_state();

        let output = Command::new(std::env::current_exe().expect("resolve test executable"))
            .arg("--exact")
            .arg("sandbox::windows::tests::preserved_preflight_recovery_child")
            .arg("--nocapture")
            .env(PREFLIGHT_RECOVERY_CHILD_ROOT, &parent)
            .output_guarded()
            .expect("run restart-recovery child");
        assert!(output.status.success());
        assert!(!abandoned.exists());
        attest_cleanup_proof(&proof, [executable.as_path()])
            .expect("next process removes the exact profile, Job, ACLs, and journal");

        std::fs::remove_dir(parent).expect("remove restart-recovery parent");
    }

    #[test]
    fn junction_in_preserved_preflight_root_is_rejected_without_mutation() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let abandoned = root.path().to_path_buf();
        let outside = parent.join("outside");
        let junction = abandoned.join("workspace");
        std::fs::create_dir(&outside).expect("create junction target");
        let status = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                junction.to_str().expect("junction path is UTF-8"),
                outside.to_str().expect("junction target is UTF-8"),
            ])
            .status_guarded()
            .expect("create preflight recovery junction");
        assert!(status.success(), "fixture must create a real junction");
        root.retain_recovery_state();

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("junction must fail closed");
        assert!(error.contains("reparse point"));
        assert!(junction.exists());
        assert!(outside.exists());

        std::fs::remove_dir(&junction).expect("remove junction only");
        std::fs::remove_dir_all(parent).expect("remove junction preflight fixture");
    }

    #[test]
    fn active_preflight_owner_is_rejected_without_mutation() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let active = root.path().to_path_buf();
        std::fs::create_dir(active.join("workspace")).expect("create active workspace");

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("live owner lease must block recovery");
        assert!(error.contains("active or unverifiable"));
        assert!(active.join(PREFLIGHT_OWNER_FILE).exists());
        assert!(active.join("workspace").exists());

        root.remove().expect("remove active preflight fixture");
        std::fs::remove_dir(parent).expect("remove active preflight parent");
    }

    #[test]
    fn changed_preflight_root_dacl_is_rejected_without_cleanup() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let changed = root.path().to_path_buf();
        let handle = root
            .authority
            .as_ref()
            .expect("preflight authority remains held")
            .0
            ._directory
            .as_raw_handle();
        let result = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert_eq!(result, 0, "replace preflight owner-only DACL");
        root.retain_recovery_state();

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("changed preflight DACL must fail closed");
        assert!(error.contains("unverifiable"));
        assert!(changed.join(PREFLIGHT_OWNER_FILE).exists());

        let restored = protect_and_attest_control_root(&changed)
            .expect("restore private DACL for fixture cleanup");
        drop(restored);
        std::fs::remove_dir_all(parent).expect("remove changed-DACL preflight fixture");
    }

    #[test]
    fn malformed_preflight_schema_is_preserved_without_mutation() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let malformed = root.path().to_path_buf();
        std::fs::write(malformed.join("unexpected"), b"evidence")
            .expect("write malformed recovery evidence");
        root.retain_recovery_state();

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("unexpected root entry must fail closed");
        assert!(error.contains("unexpected root entry"));
        assert_eq!(
            std::fs::read(malformed.join("unexpected")).expect("read preserved evidence"),
            b"evidence"
        );

        std::fs::remove_dir_all(parent).expect("remove malformed preflight fixture");
    }

    #[test]
    fn malformed_preflight_namespace_name_fails_closed_without_mutation() {
        let parent = std::env::temp_dir().join(format!(
            "mini-agent-preflight-name-test-parent-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&parent).expect("create malformed-name parent");
        let parent = canonical_root(&parent, "malformed-name parent")
            .expect("canonicalize malformed-name parent");
        let malformed = parent.join(format!("{PREFLIGHT_ROOT_PREFIX}not-a-uuid"));
        std::fs::create_dir(&malformed).expect("create malformed namespace entry");

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("malformed namespace entry must fail closed");
        assert!(error.contains("root name is invalid"));
        assert!(malformed.exists());

        std::fs::remove_dir_all(parent).expect("remove malformed-name fixture");
    }

    #[test]
    fn excess_preflight_root_count_is_bounded_without_mutation() {
        let parent = std::env::temp_dir().join(format!(
            "mini-agent-preflight-count-test-parent-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&parent).expect("create excess-count parent");
        let parent = canonical_root(&parent, "excess-count parent")
            .expect("canonicalize excess-count parent");
        let mut candidates = Vec::new();
        for _ in 0..=MAX_STALE_PREFLIGHT_ROOTS {
            let candidate = parent.join(format!("{PREFLIGHT_ROOT_PREFIX}{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&candidate).expect("create excess-count candidate");
            candidates.push(candidate);
        }

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("excess candidate count must fail closed");
        assert!(error.contains("root count exceeds 64"));
        assert!(candidates.iter().all(|candidate| candidate.exists()));

        std::fs::remove_dir_all(parent).expect("remove excess-count fixture");
    }

    #[test]
    fn hardlinked_preflight_file_is_preserved_without_cleanup() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let malformed = root.path().to_path_buf();
        let workspace = malformed.join("workspace");
        std::fs::create_dir(&workspace).expect("create hardlink workspace");
        let source = workspace.join("source");
        let alias = workspace.join("alias");
        std::fs::write(&source, b"evidence").expect("write hardlink source");
        std::fs::hard_link(&source, &alias).expect("create hardlink alias");
        root.retain_recovery_state();

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("hardlinked recovery evidence must fail closed");
        assert!(error.contains("multiple links"));
        assert!(source.exists());
        assert!(alias.exists());

        std::fs::remove_dir_all(parent).expect("remove hardlink preflight fixture");
    }

    #[test]
    fn oversized_preflight_tree_is_preserved_without_cleanup() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let oversized = root.path().to_path_buf();
        let workspace = oversized.join("workspace");
        std::fs::create_dir(&workspace).expect("create oversized workspace");
        std::fs::write(
            workspace.join("oversized"),
            vec![0u8; MAX_PREFLIGHT_RECOVERY_BYTES as usize + 1],
        )
        .expect("write oversized recovery fixture");
        root.retain_recovery_state();

        let error = recover_preserved_preflight_roots(
            &parent,
            Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT,
        )
        .expect_err("oversized recovery tree must fail closed");
        assert!(error.contains("exceeds byte bound"));
        assert!(oversized.join("workspace/oversized").exists());

        std::fs::remove_dir_all(parent).expect("remove oversized preflight fixture");
    }

    #[test]
    fn expired_preflight_root_sweep_is_bounded_and_preserves_evidence() {
        let (parent, mut root) = new_preflight_recovery_fixture();
        let abandoned = root.path().to_path_buf();
        root.retain_recovery_state();

        let error = recover_preserved_preflight_roots(&parent, Instant::now())
            .expect_err("expired sweep must fail before mutation");
        assert!(error.contains("cleanup deadline"));
        assert!(abandoned.join(PREFLIGHT_OWNER_FILE).exists());

        std::fs::remove_dir_all(parent).expect("remove expired preflight fixture");
    }

    #[test]
    fn recovery_deadline_starts_after_reap_and_bounds_acl_traversal() {
        let after_reap = Instant::now();
        assert_eq!(
            next_general_preflight_recovery_deadline(after_reap),
            after_reap + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT
        );

        let error = revoke_tree_until(
            Path::new(r"C:\this-path-must-not-be-inspected"),
            null_mut(),
            Instant::now(),
        )
        .expect_err("expired recovery must stop before ACL traversal");
        assert!(error.contains("cleanup deadline"));
    }

    #[test]
    fn timed_out_general_preflight_reaps_tree_and_removes_recovery_state() {
        let temp_root = std::env::temp_dir().join(format!(
            "mini-agent-general-timeout-test-parent-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&temp_root).expect("create timeout-test parent");
        let temp_root = canonical_root(&temp_root, "timeout-test parent")
            .expect("canonicalize timeout-test parent");
        let mut root =
            TemporaryPreflightRoot::create(&temp_root).expect("create private timeout-test root");
        let base = root.path().to_path_buf();
        let workspace = base.join("workspace");
        let cache = base.join("cache");
        std::fs::create_dir_all(&workspace).expect("create timeout-test workspace");
        std::fs::create_dir_all(&cache).expect("create timeout-test cache");
        let executable = canonical_file(
            &std::env::current_exe().expect("resolve test executable"),
            "timeout-test executable",
        )
        .expect("canonicalize test executable");
        let cleanup_ready = workspace.join("cleanup-ready.txt");
        let tree_ready = workspace.join("tree-ready.txt");
        let leaked_marker = workspace.join("leaked.txt");
        let mut command = build_helper_with_ready(
            executable.clone(),
            vec![
                TARGET_PROBE_ARG.into(),
                TARGET_PARENT_ARG.into(),
                executable.to_string_lossy().into_owned(),
                tree_ready.to_string_lossy().into_owned(),
                leaked_marker.to_string_lossy().into_owned(),
            ],
            &workspace,
            &cache,
            Some(cleanup_ready.clone()),
        )
        .expect("build timeout-test helper");
        command
            .as_std_mut()
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .as_std_mut()
            .spawn_guarded_until(Instant::now() + Duration::from_secs(5))
            .expect("start timeout-test helper");
        wait_for_probe_file(&tree_ready).expect("descendant must become ready");

        let cleanup_deadline = Instant::now() + GENERAL_PREFLIGHT_CLEANUP_TIMEOUT;
        terminate_and_reap_owned_helper(&mut child, cleanup_deadline)
            .expect("reap exact timed-out helper");
        let proof = parse_crash_cleanup_proof(&cleanup_ready)
            .expect("timed-out helper must leave bounded recovery proof");
        recover_preflight_profiles(&cache, cleanup_deadline)
            .expect("recover timed-out AppContainer state");
        attest_cleanup_proof(&proof, [&workspace, &cache, executable.as_path()])
            .expect("profile, Job and ACL state must be absent");
        std::thread::sleep(Duration::from_millis(2_100));
        assert!(
            !leaked_marker.exists(),
            "contained descendant survived reap"
        );
        root.remove().expect("remove exact timeout-test tree");
        assert!(!base.exists(), "timeout-test temporary tree survived");
        std::fs::remove_dir(temp_root).expect("remove timeout-test parent");
    }

    #[test]
    fn windows_argument_quoting_handles_empty_quotes_and_trailing_slashes() {
        assert_eq!(quote_windows_argument("plain"), "plain");
        assert_eq!(quote_windows_argument(""), "\"\"");
        assert_eq!(quote_windows_argument("a b"), "\"a b\"");
        assert_eq!(quote_windows_argument("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_windows_argument("a b\\"), "\"a b\\\\\"");
    }

    #[test]
    fn appcontainer_environment_uses_container_local_profile_and_temp() {
        let storage = Path::new(r"C:\Users\runner\AppData\Local\Packages\mini-agent\AC");
        let block = appcontainer_environment(Path::new(r"C:\cache"), storage);
        let entries = String::from_utf16(&block)
            .expect("environment UTF-16")
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();

        assert!(entries.iter().any(|entry| {
            entry == r"LOCALAPPDATA=C:\Users\runner\AppData\Local\Packages\mini-agent\AC"
        }));
        assert!(entries.iter().any(|entry| {
            entry == r"TEMP=C:\Users\runner\AppData\Local\Packages\mini-agent\AC\Temp"
        }));
        assert!(entries.iter().any(|entry| {
            entry == r"TMP=C:\Users\runner\AppData\Local\Packages\mini-agent\AC\Temp"
        }));
        assert!(
            entries
                .iter()
                .any(|entry| entry == r"ZS_CACHE_DIR=C:\cache")
        );
    }

    #[test]
    fn windows_system_executables_are_not_mutated_by_the_unelevated_helper() {
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
