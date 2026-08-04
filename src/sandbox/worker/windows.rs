#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::OnceLock;

#[cfg(test)]
use std::process::Child;
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};

#[cfg(test)]
use super::WindowsWorkerProcessObservation;
use super::{
    WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus, WorkerLaunchError,
    WorkerProcess,
};

const BACKEND: WorkerBackend = WorkerBackend::WindowsLpac;
const PREFLIGHT_FAILURE_REASON: &str = "Windows LPAC production runtime preflight failed";

static STATUS: OnceLock<WorkerContainmentStatus> = OnceLock::new();

#[derive(Debug)]
struct WinHandle {
    handle: Option<OwnedHandle>,
    inheritable: bool,
}

impl WinHandle {
    fn from_created(raw: HANDLE, context: &str) -> io::Result<Self> {
        if raw.is_null() || raw == (-1isize as HANDLE) {
            return Err(contextual_last_error(context));
        }
        // SAFETY: `raw` is a newly returned, non-null owned Win32 handle. This conversion
        // transfers its single CloseHandle obligation to OwnedHandle; no other owner remains.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        #[cfg(test)]
        LIVE_WIN_HANDLES.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(Self {
            handle: Some(handle),
            inheritable: false,
        })
    }

    fn from_inheritable_created(raw: HANDLE, context: &str) -> io::Result<Self> {
        let mut handle = Self::from_created(raw, context)?;
        handle.inheritable = true;
        Ok(handle)
    }

    fn raw(&self) -> HANDLE {
        self.handle
            .as_ref()
            .expect("owned Windows handle was already transferred")
            .as_raw_handle()
    }

    fn clear_inherit(&mut self) -> io::Result<()> {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

        if !self.inheritable {
            return Ok(());
        }

        // SAFETY: the handle remains owned by `self` for the call, and
        // SetHandleInformation neither stores nor closes it.
        if unsafe { SetHandleInformation(self.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(contextual_last_error("clear protocol-pipe inheritance"));
        }
        self.inheritable = false;
        Ok(())
    }

    fn into_file(mut self) -> File {
        File::from(
            self.handle
                .take()
                .expect("owned Windows handle was already transferred"),
        )
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        if self.inheritable {
            // Best-effort fail-safe for every early return and panic while the shared creation
            // lock is still held. Closing the handle follows immediately even if clearing fails.
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
            if let Some(handle) = &self.handle {
                // SAFETY: this owned handle remains live until the end of Drop. The call only
                // clears a flag and neither retains nor closes the handle.
                unsafe {
                    SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, 0);
                }
            }
            self.inheritable = false;
        }
        #[cfg(test)]
        LIVE_WIN_HANDLES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(test)]
static LIVE_WIN_HANDLES: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn contextual_last_error(context: &str) -> io::Error {
    io::Error::other(format!("{context}: {}", io::Error::last_os_error()))
}

fn close_unowned_handle(raw: HANDLE) {
    if !raw.is_null() && raw != (-1isize as HANDLE) {
        // SAFETY: callers pass only a raw handle returned by a failed/malformed FFI operation
        // before ownership was transferred. This discharges that operation's sole obligation.
        unsafe {
            CloseHandle(raw);
        }
    }
}

pub(super) fn standard_streams_are_protocol_pipes() -> bool {
    fn is_pipe(handle: RawHandle) -> bool {
        // SAFETY: GetFileType only inspects the borrowed standard-stream handle. The handle is
        // owned by the process for this synchronous call and is neither closed nor retained.
        // Windows implements anonymous pipes using its named-pipe mechanism, so FILE_TYPE_PIPE
        // is the narrowest handle classification exposed by the OS.
        unsafe { GetFileType(handle as HANDLE) == FILE_TYPE_PIPE }
    }

    is_pipe(std::io::stdin().as_raw_handle())
        && is_pipe(std::io::stdout().as_raw_handle())
        && is_pipe(std::io::stderr().as_raw_handle())
}

pub(super) fn containment_status() -> WorkerContainmentStatus {
    STATUS.get_or_init(probe_containment).clone()
}

fn probe_containment() -> WorkerContainmentStatus {
    match feasibility::production_runtime_preflight() {
        Ok(()) => WorkerContainmentStatus::Available {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::Enforced,
        },
        Err(_) => WorkerContainmentStatus::Unavailable {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::Enforced,
            reason: PREFLIGHT_FAILURE_REASON.to_string(),
        },
    }
}

pub(super) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    match containment_status() {
        WorkerContainmentStatus::Unavailable {
            backend, reason, ..
        } => Err(WorkerLaunchError::Unavailable { backend, reason }),
        WorkerContainmentStatus::Available {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::Enforced,
        } => feasibility::launch_production(feasibility::ProductionLaunchHooks::production())
            .map_err(|error| WorkerLaunchError::Io {
                backend: BACKEND,
                source: io::Error::other(error.0),
            }),
        WorkerContainmentStatus::Available { backend, .. } => Err(WorkerLaunchError::Unavailable {
            backend,
            reason: "Windows worker containment preflight selected the wrong backend".into(),
        }),
    }
}

#[cfg(test)]
pub(super) fn containment_status_for_benchmark(
    executable: &std::path::Path,
) -> WorkerContainmentStatus {
    match feasibility::installed_worker_runtime_preflight(executable) {
        Ok(()) => WorkerContainmentStatus::Available {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::Enforced,
        },
        Err(_) => WorkerContainmentStatus::Unavailable {
            backend: BACKEND,
            assurance: WorkerContainmentAssurance::Enforced,
            reason: PREFLIGHT_FAILURE_REASON.to_string(),
        },
    }
}

#[cfg(test)]
pub(super) fn launch_executable_for_benchmark(
    executable: &std::path::Path,
) -> Result<WorkerProcess, WorkerLaunchError> {
    feasibility::launch_production(feasibility::ProductionLaunchHooks::installed_worker(
        executable.to_path_buf(),
    ))
    .map_err(|error| WorkerLaunchError::Io {
        backend: BACKEND,
        source: io::Error::other(error.0),
    })
}

#[derive(Debug)]
pub(super) struct WorkerChild {
    inner: WorkerChildInner,
}

#[derive(Debug)]
enum WorkerChildInner {
    Contained {
        process: WinHandle,
        job: Option<WinHandle>,
        process_id: u32,
        status: Option<ExitStatus>,
    },
    #[cfg(test)]
    Unconfined(Child),
}

impl WorkerChild {
    fn contained(process: WinHandle, job: WinHandle, process_id: u32) -> Self {
        Self {
            inner: WorkerChildInner::Contained {
                process,
                job: Some(job),
                process_id,
                status: None,
            },
        }
    }

    #[cfg(test)]
    pub(super) fn from_unconfined_test_child(child: Child) -> Self {
        Self {
            inner: WorkerChildInner::Unconfined(child),
        }
    }

    pub(super) fn id(&self) -> u32 {
        match &self.inner {
            WorkerChildInner::Contained { process_id, .. } => *process_id,
            #[cfg(test)]
            WorkerChildInner::Unconfined(child) => child.id(),
        }
    }

    pub(super) fn finalize_authenticated_ready(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn retire_after_reap(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn terminate_tree(&mut self) -> io::Result<()> {
        match &mut self.inner {
            WorkerChildInner::Contained { job, .. } => {
                let job = job
                    .as_ref()
                    .ok_or_else(|| io::Error::other("JavaScript worker Job was already closed"))?;
                // SAFETY: `job` is the directly owned creation-time Job for this worker. The
                // call terminates every member synchronously and does not retain the handle.
                if unsafe { TerminateJobObject(job.raw(), 1) } == 0 {
                    return Err(contextual_last_error("terminate JavaScript worker Job"));
                }
                Ok(())
            }
            #[cfg(test)]
            WorkerChildInner::Unconfined(child) => child.kill(),
        }
    }

    fn runtime_controls_match(&self) -> Result<(), feasibility::GateError> {
        match &self.inner {
            WorkerChildInner::Contained { process, job, .. } => {
                let job = job.as_ref().ok_or_else(|| {
                    feasibility::GateError("Windows containment Job was already closed".to_string())
                })?;
                feasibility::verify_runtime_controls(process, job)
            }
            #[cfg(test)]
            WorkerChildInner::Unconfined(_) => Err(feasibility::GateError(
                "Windows containment probe received an uncontained child".to_string(),
            )),
        }
    }

    #[cfg(test)]
    pub(super) fn process_observation_for_test(
        &self,
    ) -> io::Result<WindowsWorkerProcessObservation> {
        let WorkerChildInner::Contained {
            job, process_id, ..
        } = &self.inner
        else {
            return Err(io::Error::other(
                "benchmark requires a production-contained Windows worker",
            ));
        };
        let job = job
            .as_ref()
            .ok_or_else(|| io::Error::other("benchmark Windows worker Job was already closed"))?;
        let active_job_processes =
            feasibility::active_job_processes(job).map_err(|error| io::Error::other(error.0))?;
        Ok(WindowsWorkerProcessObservation {
            exact_worker_pid: *process_id,
            active_job_processes,
        })
    }

    #[cfg(test)]
    fn close_job_for_probe(&mut self) -> Result<(), feasibility::GateError> {
        let WorkerChildInner::Contained { job, .. } = &mut self.inner else {
            return Err(feasibility::GateError(
                "Windows containment probe received an uncontained child".to_string(),
            ));
        };
        let job = job.take().ok_or_else(|| {
            feasibility::GateError("Windows containment Job was already closed".to_string())
        })?;
        drop(job);
        Ok(())
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.inner {
            WorkerChildInner::Contained {
                process, status, ..
            } => {
                if let Some(status) = status {
                    return Ok(Some(*status));
                }
                // SAFETY: the process handle remains directly owned for this nonblocking wait.
                match unsafe { WaitForSingleObject(process.raw(), 0) } {
                    WAIT_TIMEOUT => Ok(None),
                    WAIT_OBJECT_0 => {
                        let exited = process_exit_status(process)?;
                        *status = Some(exited);
                        Ok(Some(exited))
                    }
                    WAIT_FAILED => Err(contextual_last_error("poll JavaScript worker process")),
                    other => Err(io::Error::other(format!(
                        "unexpected JavaScript worker wait result {other}"
                    ))),
                }
            }
            #[cfg(test)]
            WorkerChildInner::Unconfined(child) => child.try_wait(),
        }
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.inner {
            WorkerChildInner::Contained {
                process, status, ..
            } => {
                if let Some(status) = status {
                    return Ok(*status);
                }
                // SAFETY: the process handle remains directly owned for this wait. Common caller
                // paths bound it by terminating the kill-on-close Job before waiting.
                match unsafe { WaitForSingleObject(process.raw(), INFINITE) } {
                    WAIT_OBJECT_0 => {
                        let exited = process_exit_status(process)?;
                        *status = Some(exited);
                        Ok(exited)
                    }
                    WAIT_FAILED => Err(contextual_last_error("wait for JavaScript worker process")),
                    other => Err(io::Error::other(format!(
                        "unexpected JavaScript worker wait result {other}"
                    ))),
                }
            }
            #[cfg(test)]
            WorkerChildInner::Unconfined(child) => child.wait(),
        }
    }
}

fn process_exit_status(process: &WinHandle) -> io::Result<ExitStatus> {
    let mut code = 0u32;
    // SAFETY: `process` is a live directly owned process handle and `code` is one initialized,
    // writable DWORD. GetExitCodeProcess does not retain either pointer or handle.
    if unsafe { GetExitCodeProcess(process.raw(), &mut code) } == 0 {
        return Err(contextual_last_error(
            "read JavaScript worker process exit code",
        ));
    }
    Ok(ExitStatus::from_raw(code))
}

#[allow(dead_code)]
mod feasibility {
    use super::{WinHandle, WorkerChild, close_unowned_handle};
    use crate::process_creation::{CreationGuard, StdCommandCreationExt};
    use crate::sandbox::worker::{
        INTERNAL_WORKER_MARKER, INTERNAL_WORKER_MARKER_VALUE, WorkerBackend, WorkerProcess,
    };
    use std::ffi::{OsStr, c_void};
    use std::fmt;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    #[cfg(test)]
    use std::io::{BufRead, BufReader};
    use std::mem::{size_of, size_of_val};
    #[cfg(test)]
    use std::net::TcpListener;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::AsRawHandle;
    #[cfg(test)]
    use std::os::windows::io::AsRawSocket;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus};
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE, GENERIC_ALL,
        GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, GetHandleInformation, GetLastError, HANDLE,
        LocalFree, TRUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetEffectiveRightsFromAclW,
        GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
        TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeleteAppContainerProfile,
        DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION,
        AclSizeInformation, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, FreeSid,
        GetAce, GetAclInformation, GetLengthSid, GetTokenInformation, INHERITED_ACE, IsValidSid,
        NO_INHERITANCE, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
        SECURITY_CAPABILITIES, TOKEN_QUERY, TOKEN_USER, TokenCapabilities, TokenIsAppContainer,
        TokenIsLessPrivilegedAppContainer, TokenUser, WinAuthenticatedUserSid,
        WinBuiltinAdministratorsSid, WinBuiltinAnyPackageSid, WinBuiltinUsersSid,
        WinLocalSystemSid, WinWorldSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS,
        FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, GetDriveTypeW, OPEN_EXISTING,
        WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleWindow, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    #[cfg(test)]
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAndIoAccountingInformation, JobObjectBasicUIRestrictions,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Memory::{
        GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc, HeapFree,
    };
    use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_UILIMIT_ALL;
    use windows_sys::Win32::System::SystemServices::{
        PROCESS_MITIGATION_ASLR_POLICY, PROCESS_MITIGATION_CHILD_PROCESS_POLICY,
        PROCESS_MITIGATION_DYNAMIC_CODE_POLICY, PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY,
        PROCESS_MITIGATION_IMAGE_LOAD_POLICY, PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
        GetExitCodeProcess, GetProcessMitigationPolicy, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
        PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ProcessASLRPolicy,
        ProcessChildProcessPolicy, ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy,
        ProcessImageLoadPolicy, ProcessSystemCallDisablePolicy, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_FIXED, PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
        PROCESS_CREATION_CHILD_PROCESS_RESTRICTED,
    };

    const PROFILE_NAME: &str = "mini-agent.worker-image-loading-gate.v1";
    const PRODUCTION_PROFILE_NAME: &str = "mini-agent.worker.production.v1";
    pub(super) const PROCESS_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
    pub(super) const PROCESS_CPU_LIMIT_100NS: i64 = 35 * 10_000_000;
    pub(super) const MITIGATION_POLICY: u64 = (1u64 << 8) // force image relocation (mandatory ASLR)
        | (1u64 << 12) // terminate on heap corruption
        | (1u64 << 16) // bottom-up ASLR
        | (1u64 << 20) // high-entropy ASLR
        | (1u64 << 32) // disable legacy extension points
        | (1u64 << 52) // deny remote image loads
        | (1u64 << 56) // deny low-integrity image loads
        | (1u64 << 60); // prefer System32 image resolution
    const CHILD_TEST_NAME: &str = "sandbox::worker::platform::tests::windows_lpac_gate_child";
    const INSTALLED_EXE_ENV: &str = "MINI_AGENT_LPAC_CARGO_INSTALL_EXE";
    const PROTECTED_EXE_ENV: &str = "MINI_AGENT_LPAC_PROTECTED_EXE";
    const SENTINEL_ENV: &str = "MINI_AGENT_LPAC_SENTINEL";
    const CANARY_HANDLE_ENV: &str = "MINI_AGENT_LPAC_OMITTED_HANDLE";
    const READY_DENIED: &[u8] =
        b"MINI_AGENT_LPAC_READY_V2:LPAC:ZERO_CAPS:NO_CONSOLE:WORKSPACE_DENIED:HANDLE_LIST_EXACT\n";
    const READY_OPENED: &[u8] = b"MINI_AGENT_LPAC_READY_V1:AUTHORITY_LEAKED\n";
    const CONTAINMENT_CHILD_TEST_NAME: &str =
        "extras::js::tests::worker_containment::windows_containment_probe_child";
    const PROTOCOL_CHILD_TEST_NAME: &str =
        "extras::js::tests::worker_runtime::worker_bootstrap_test_child";
    const CONTAINMENT_MARKER_VALUE: &str = "windows-containment-probe-v1";
    const CONTAINMENT_READY: &[u8] = b"MINI_AGENT_WINDOWS_CONTAINMENT_PASS_V1\n";
    const PROBE_WORKSPACE_ENV: &str = "MINI_AGENT_WINDOWS_PROBE_WORKSPACE";
    const PROBE_SKILL_DATABASE_ENV: &str = "MINI_AGENT_WINDOWS_PROBE_SKILL_DATABASE";
    const PROBE_FILE_HANDLE_ENV: &str = "MINI_AGENT_WINDOWS_PROBE_FILE_HANDLE";
    const PROBE_SOCKET_HANDLE_ENV: &str = "MINI_AGENT_WINDOWS_PROBE_SOCKET_HANDLE";
    const PROBE_TCP_PORT_ENV: &str = "MINI_AGENT_WINDOWS_PROBE_TCP_PORT";
    const PROBE_UDP_PORT_ENV: &str = "MINI_AGENT_WINDOWS_PROBE_UDP_PORT";
    const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
    const PRODUCTION_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
    const PRODUCTION_PREFLIGHT_REAP_TIMEOUT: Duration = Duration::from_secs(1);
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    #[derive(Debug)]
    pub(super) struct GateError(pub(super) String);

    impl fmt::Display for GateError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl std::error::Error for GateError {}

    impl From<io::Error> for GateError {
        fn from(error: io::Error) -> Self {
            Self(error.to_string())
        }
    }

    fn last_error(context: &str) -> GateError {
        GateError(format!("{context}: {}", io::Error::last_os_error()))
    }

    fn win32_error(context: &str, code: u32) -> GateError {
        GateError(format!(
            "{context}: {}",
            io::Error::from_raw_os_error(code as i32)
        ))
    }

    fn hresult_error(context: &str, result: i32) -> GateError {
        GateError(format!("{context}: HRESULT 0x{:08x}", result as u32))
    }

    fn hresult_from_win32(code: u32) -> i32 {
        if code == 0 {
            0
        } else {
            ((code & 0xffff) | 0x8007_0000) as i32
        }
    }

    fn wide_null(value: &OsStr) -> Result<Vec<u16>, GateError> {
        let mut wide = value.encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(GateError(
                "Windows path contains an interior NUL".to_string(),
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    fn wide_string(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    #[derive(Debug)]
    struct LocalMemory(*mut c_void);

    impl Drop for LocalMemory {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetNamedSecurityInfoW/SetEntriesInAclW returned this
                // LocalAlloc-owned pointer, it has not been freed, and LocalFree
                // is the required matching deallocator.
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum InstallLocation {
        CargoBuild,
        CargoInstall,
        UserArchive,
        ProtectedMachineWide,
        Unsupported,
    }

    fn components_lower(path: &Path) -> Vec<String> {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect()
    }

    fn normalized_windows_path(path: &Path) -> String {
        let mut normalized = path
            .as_os_str()
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        if let Some(rest) = normalized.strip_prefix("\\\\?\\unc\\") {
            normalized = format!("\\\\{rest}");
        } else if let Some(rest) = normalized.strip_prefix("\\\\?\\") {
            normalized = rest.to_string();
        }
        while normalized.ends_with('\\') && normalized.len() > 3 {
            normalized.pop();
        }
        normalized
    }

    pub(super) fn reject_unc_or_remote_syntax(path: &Path) -> Result<(), GateError> {
        let normalized = normalized_windows_path(path);
        if normalized.starts_with("\\\\") {
            return Err(GateError(
                "UNC and remote executable roots are unsupported".to_string(),
            ));
        }
        let bytes = normalized.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'\\'
        {
            return Err(GateError(
                "executable path is not rooted on a local drive".to_string(),
            ));
        }
        let root = wide_string(&normalized[..3]);
        // SAFETY: root is an absolute, NUL-terminated drive root and is read
        // only for the duration of the call.
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            return Err(GateError(
                "executable root is not a fixed local drive".to_string(),
            ));
        }
        Ok(())
    }

    fn reject_reparse_components(path: &Path) -> Result<(), GateError> {
        let mut cursor = Some(path);
        while let Some(component) = cursor {
            let metadata = std::fs::symlink_metadata(component)
                .map_err(|error| GateError(format!("inspect path component: {error}")))?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(GateError(
                    "reparse points are forbidden in the image path".to_string(),
                ));
            }
            cursor = component.parent();
        }
        Ok(())
    }

    fn starts_with_case_insensitive(path: &Path, root: &Path) -> bool {
        let path = normalized_windows_path(path);
        let root = normalized_windows_path(root);
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|suffix| suffix.starts_with('\\'))
    }

    fn under_environment_root(path: &Path, name: &str) -> bool {
        std::env::var_os(name)
            .map(PathBuf::from)
            .is_some_and(|root| starts_with_case_insensitive(path, &root))
    }

    fn classify_install_location(path: &Path) -> InstallLocation {
        if [
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
            "SystemRoot",
        ]
        .iter()
        .any(|name| under_environment_root(path, name))
        {
            return InstallLocation::ProtectedMachineWide;
        }

        if std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|root| PathBuf::from(root).join(".cargo"))
            })
            .is_some_and(|root| starts_with_case_insensitive(path, &root))
        {
            return InstallLocation::CargoInstall;
        }

        let components = components_lower(path);
        if components
            .windows(2)
            .any(|pair| pair[0] == "target" && (pair[1] == "debug" || pair[1] == "release"))
        {
            return InstallLocation::CargoBuild;
        }

        if ["USERPROFILE", "LOCALAPPDATA", "TEMP", "TMP"]
            .iter()
            .any(|name| under_environment_root(path, name))
        {
            return InstallLocation::UserArchive;
        }

        InstallLocation::Unsupported
    }

    #[derive(Debug)]
    struct AppContainerProfile {
        name: Vec<u16>,
        sid: PSID,
        created: bool,
        cleanup_created: bool,
    }

    impl AppContainerProfile {
        fn stable_zero_capability() -> Result<Self, GateError> {
            Self::stable_zero_capability_named(
                PROFILE_NAME,
                "mini-agent worker image-loading gate",
                "zero-capability LPAC feasibility profile",
                true,
            )
        }

        fn production_zero_capability() -> Result<Self, GateError> {
            // Production keeps this stable zero-capability profile installed. Deleting and
            // recreating a shared profile on every launch would race concurrent mini-agent
            // parents. The name deterministically identifies the same package SID.
            Self::stable_zero_capability_named(
                PRODUCTION_PROFILE_NAME,
                "mini-agent JavaScript worker",
                "zero-capability LPAC production worker",
                false,
            )
        }

        fn stable_zero_capability_named(
            profile_name: &str,
            display_name: &str,
            profile_description: &str,
            cleanup_created: bool,
        ) -> Result<Self, GateError> {
            let name = wide_string(profile_name);
            let display = wide_string(display_name);
            let description = wide_string(profile_description);
            let mut sid = null_mut();

            // SAFETY: all three UTF-16 strings are NUL-terminated and live
            // through the call. Capability count is zero, so the null
            // capability pointer has length zero. `sid` receives one SID that
            // must later be released with FreeSid.
            let create_result = unsafe {
                CreateAppContainerProfile(
                    name.as_ptr(),
                    display.as_ptr(),
                    description.as_ptr(),
                    null(),
                    0,
                    &mut sid,
                )
            };
            if create_result >= 0 {
                if sid.is_null() {
                    // SAFETY: profile creation succeeded under this exact
                    // stable name, so remove the malformed profile before
                    // returning the missing-SID error.
                    let cleanup = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
                    if cleanup < 0 {
                        return Err(GateError(format!(
                            "CreateAppContainerProfile returned a null SID; cleanup also failed with HRESULT 0x{:08x}",
                            cleanup as u32
                        )));
                    }
                    return Err(GateError(
                        "CreateAppContainerProfile returned a null SID".to_string(),
                    ));
                }
                return Ok(Self {
                    name,
                    sid,
                    created: true,
                    cleanup_created,
                });
            }

            if !sid.is_null() {
                // SAFETY: a failing profile call nevertheless returned an
                // allocated SID; FreeSid is the documented owner cleanup.
                unsafe {
                    FreeSid(sid);
                }
                sid = null_mut();
            }
            if create_result != hresult_from_win32(ERROR_ALREADY_EXISTS) {
                return Err(hresult_error(
                    "create zero-capability AppContainer profile",
                    create_result,
                ));
            }

            // SAFETY: the stable profile name remains NUL-terminated and live;
            // `sid` receives one allocation owned by this object and freed in
            // Drop.
            let derive_result =
                unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
            if derive_result < 0 || sid.is_null() {
                if !sid.is_null() {
                    // SAFETY: Derive returned an allocation on its failure
                    // path; this is its sole matching FreeSid cleanup.
                    unsafe {
                        FreeSid(sid);
                    }
                }
                return Err(hresult_error(
                    "derive existing AppContainer SID",
                    derive_result,
                ));
            }
            Ok(Self {
                name,
                sid,
                created: false,
                cleanup_created,
            })
        }

        fn finish(mut self) -> Result<(), GateError> {
            if self.created && self.cleanup_created {
                // SAFETY: this exact stable profile was created by this gate,
                // and its NUL-terminated name remains alive for the call.
                let result = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
                if result < 0 {
                    return Err(hresult_error(
                        "delete gate-created AppContainer profile",
                        result,
                    ));
                }
                self.created = false;
            }
            Ok(())
        }
    }

    impl Drop for AppContainerProfile {
        fn drop(&mut self) {
            if self.created && self.cleanup_created {
                // SAFETY: the stable NUL-terminated name is still alive. The
                // test removes only the profile it created; pre-existing
                // profiles are retained.
                let result = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
                debug_assert!(result >= 0, "gate-created AppContainer cleanup failed");
            }
            if !self.sid.is_null() {
                // SAFETY: Create/DeriveAppContainerSid returned this SID and it
                // has exactly one FreeSid obligation.
                unsafe {
                    FreeSid(self.sid);
                }
            }
        }
    }

    #[derive(Debug)]
    struct OwnedSid(Vec<usize>);

    impl OwnedSid {
        fn as_psid(&self) -> PSID {
            self.0.as_ptr().cast_mut().cast()
        }

        fn well_known(kind: i32) -> Result<Self, GateError> {
            let mut storage = vec![0usize; 16];
            let mut bytes = (storage.len() * size_of::<usize>()) as u32;
            // SAFETY: storage is aligned and writable for `bytes`; a null
            // domain SID requests the process-local well-known SID.
            if unsafe {
                CreateWellKnownSid(kind, null_mut(), storage.as_mut_ptr().cast(), &mut bytes)
            } == 0
            {
                return Err(last_error("create well-known SID"));
            }
            Ok(Self(storage))
        }
    }

    fn current_user_sid() -> Result<OwnedSid, GateError> {
        let mut raw_token = null_mut();
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle. The output
        // slot receives a new token handle that is immediately transferred to
        // OwnedHandle and closed exactly once.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
            return Err(last_error("open current process token"));
        }
        let token = WinHandle::from_created(raw_token, "open current process token")?;

        let mut required = 0u32;
        // SAFETY: the null buffer has declared length zero; `required` is a
        // valid initialized output slot for the exact byte count.
        let first =
            unsafe { GetTokenInformation(token.raw(), TokenUser, null_mut(), 0, &mut required) };
        if first != 0 || required == 0 {
            return Err(GateError(
                "GetTokenInformation size probe returned an invalid length".to_string(),
            ));
        }
        // SAFETY: GetLastError has no pointer arguments or ownership effects.
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(last_error("size current token user"));
        }

        let slot_size = size_of::<usize>();
        let slot_count = (required as usize).div_ceil(slot_size);
        let mut storage = vec![0usize; slot_count];
        // SAFETY: `storage` is pointer-aligned and contains at least `required`
        // writable bytes. The API initializes those bytes before TOKEN_USER is
        // read, and the returned SID pointer remains inside `storage` for the
        // EqualSid call below.
        if unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                storage.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(last_error("read current token user"));
        }
        // SAFETY: the successful call initialized a TOKEN_USER at the aligned
        // start of `storage`, whose lifetime covers this read.
        let token_user = unsafe { &*(storage.as_ptr().cast::<TOKEN_USER>()) };
        // SAFETY: the successful token query returned a valid SID for the
        // duration of storage. GetLengthSid reads it synchronously.
        let sid_bytes = unsafe { GetLengthSid(token_user.User.Sid) };
        if sid_bytes == 0 {
            return Err(last_error("size current token SID"));
        }
        let copy = OwnedSid(vec![
            0usize;
            (sid_bytes as usize).div_ceil(size_of::<usize>())
        ]);
        // SAFETY: `copy` has at least sid_bytes aligned writable bytes and the
        // token SID remains live until this synchronous copy completes.
        if unsafe {
            windows_sys::Win32::Security::CopySid(sid_bytes, copy.as_psid(), token_user.User.Sid)
        } == 0
        {
            return Err(last_error("copy current token SID"));
        }
        Ok(copy)
    }

    fn sid_equal(left: PSID, right: PSID) -> bool {
        !left.is_null() && !right.is_null() && unsafe { EqualSid(left, right) } != 0
    }

    struct SidPolicy {
        user: OwnedSid,
        system: OwnedSid,
        administrators: OwnedSid,
        broad: Vec<OwnedSid>,
        restricted_packages: LocalMemory,
    }

    impl SidPolicy {
        fn current() -> Result<Self, GateError> {
            let mut restricted_packages = null_mut();
            let restricted_text = wide_string("S-1-15-2-2");
            // SAFETY: the SID string is NUL terminated; LocalFree owns the
            // returned allocation through `restricted_packages`.
            if unsafe { ConvertStringSidToSidW(restricted_text.as_ptr(), &mut restricted_packages) }
                == 0
            {
                return Err(last_error("create restricted application packages SID"));
            }
            Ok(Self {
                user: current_user_sid()?,
                system: OwnedSid::well_known(WinLocalSystemSid)?,
                administrators: OwnedSid::well_known(WinBuiltinAdministratorsSid)?,
                broad: vec![
                    OwnedSid::well_known(WinWorldSid)?,
                    OwnedSid::well_known(WinAuthenticatedUserSid)?,
                    OwnedSid::well_known(WinBuiltinUsersSid)?,
                    OwnedSid::well_known(WinBuiltinAnyPackageSid)?,
                ],
                restricted_packages: LocalMemory(restricted_packages),
            })
        }

        fn trusted_writer(&self, sid: PSID) -> bool {
            sid_equal(sid, self.user.as_psid())
                || sid_equal(sid, self.system.as_psid())
                || sid_equal(sid, self.administrators.as_psid())
        }

        fn broad(&self, sid: PSID) -> bool {
            self.broad
                .iter()
                .any(|known| sid_equal(sid, known.as_psid()))
                || sid_equal(sid, self.restricted_packages.0)
        }
    }

    struct FileSecurity {
        _descriptor: LocalMemory,
        owner: PSID,
        dacl: *mut ACL,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct FileSecuritySnapshot {
        owner: Vec<u8>,
        dacl: Vec<u8>,
    }

    fn read_file_security(path: &Path) -> Result<FileSecurity, GateError> {
        let path = wide_null(path.as_os_str())?;
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(win32_error("read path owner and DACL", result));
        }
        if descriptor.is_null() || owner.is_null() {
            if !descriptor.is_null() {
                unsafe { LocalFree(descriptor) };
            }
            return Err(GateError(
                "path security descriptor omitted its owner".to_string(),
            ));
        }
        if dacl.is_null() {
            unsafe { LocalFree(descriptor) };
            return Err(GateError(
                "NULL DACL is forbidden by the LPAC gate".to_string(),
            ));
        }
        Ok(FileSecurity {
            _descriptor: LocalMemory(descriptor),
            owner,
            dacl,
        })
    }

    fn snapshot_file_security(security: &FileSecurity) -> Result<FileSecuritySnapshot, GateError> {
        // SAFETY: `owner` is a validated SID inside the live owned security descriptor.
        let owner_bytes = unsafe { GetLengthSid(security.owner) } as usize;
        if owner_bytes == 0 {
            return Err(last_error("measure path-owner SID"));
        }
        let mut information = ACL_SIZE_INFORMATION::default();
        // SAFETY: `dacl` is non-null and remains inside the live owned descriptor, while the
        // initialized output buffer has the exact size required for this synchronous query.
        if unsafe {
            GetAclInformation(
                security.dacl,
                (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(last_error("measure path DACL"));
        }
        let dacl_bytes = information.AclBytesInUse as usize;
        if dacl_bytes < size_of::<ACL>() {
            return Err(GateError(
                "path DACL snapshot was structurally truncated".to_string(),
            ));
        }
        // SAFETY: both pointers and measured lengths refer to regions inside the live security
        // descriptor; the bytes are copied before that descriptor can be dropped.
        let owner = unsafe { std::slice::from_raw_parts(security.owner.cast::<u8>(), owner_bytes) }
            .to_vec();
        // SAFETY: same owned-descriptor lifetime and exact measured-byte argument as above.
        let dacl =
            unsafe { std::slice::from_raw_parts(security.dacl.cast::<u8>(), dacl_bytes) }.to_vec();
        Ok(FileSecuritySnapshot { owner, dacl })
    }

    fn effective_file_rights(dacl: *mut ACL, sid: PSID) -> Result<u32, GateError> {
        let trustee = trustee_for_sid(sid);
        let mut rights = 0u32;
        // SAFETY: the caller keeps the DACL and exact SID live through this synchronous query;
        // GetEffectiveRightsFromAclW retains neither pointer.
        let result = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) };
        if result != 0 {
            return Err(win32_error("inspect effective file rights", result));
        }
        Ok(mapped_file_mask(rights))
    }

    fn verify_current_user_cannot_modify(
        path: &Path,
        policy: &SidPolicy,
        require_non_owner: bool,
    ) -> Result<FileSecuritySnapshot, GateError> {
        let security = read_file_security(path)?;
        if require_non_owner && sid_equal(security.owner, policy.user.as_psid()) {
            return Err(GateError(
                "protected negative control is owned by the current user".to_string(),
            ));
        }
        let rights = effective_file_rights(security.dacl, policy.user.as_psid())?;
        if dangerous_write_mask(rights) {
            return Err(GateError(
                "protected negative control is writable or deletable by the current user"
                    .to_string(),
            ));
        }
        snapshot_file_security(&security)
    }

    pub(super) fn mapped_file_mask(mask: u32) -> u32 {
        let generic = mask & (GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
        let mut mapped = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
        if generic & GENERIC_READ != 0 {
            mapped |= FILE_GENERIC_READ;
        }
        if generic & GENERIC_WRITE != 0 {
            mapped |= FILE_GENERIC_WRITE;
        }
        if generic & GENERIC_EXECUTE != 0 {
            mapped |= FILE_GENERIC_EXECUTE;
        }
        if generic & GENERIC_ALL != 0 {
            mapped |= FILE_ALL_ACCESS;
        }
        mapped
    }

    fn required_image_mask() -> u32 {
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
    }

    pub(super) fn package_allow_set_is_exact(aces: &[(u32, u8)]) -> bool {
        aces.len() == 1
            && u32::from(aces[0].1) == NO_INHERITANCE
            && mapped_file_mask(aces[0].0) == required_image_mask()
    }

    pub(super) fn dangerous_write_mask(mask: u32) -> bool {
        mapped_file_mask(mask)
            & (FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | FILE_ADD_FILE
                | FILE_ADD_SUBDIRECTORY
                | FILE_DELETE_CHILD
                | FILE_WRITE_ATTRIBUTES
                | FILE_WRITE_EA
                | DELETE
                | WRITE_DAC
                | WRITE_OWNER)
            != 0
    }

    fn inspect_acl(
        dacl: *mut ACL,
        policy: &SidPolicy,
        appcontainer_sid: PSID,
        require_exact_appcontainer_rx: bool,
        reject_broad_access: bool,
    ) -> Result<(), GateError> {
        let mut information = ACL_SIZE_INFORMATION::default();
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(last_error("enumerate DACL"));
        }
        let required = required_image_mask();
        let mut appcontainer_allows = Vec::new();
        for index in 0..information.AceCount {
            let mut raw_ace = null_mut();
            if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 || raw_ace.is_null() {
                return Err(last_error("read DACL ACE"));
            }
            let header = unsafe { &*(raw_ace.cast::<ACE_HEADER>()) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE && header.AceType != ACCESS_DENIED_ACE_TYPE
            {
                return Err(GateError(
                    "unsupported object/callback ACE is ambiguous".to_string(),
                ));
            }
            if usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
                return Err(GateError("DACL contains a truncated ACE".to_string()));
            }
            let ace = unsafe { &*(raw_ace.cast::<ACCESS_ALLOWED_ACE>()) };
            let sid = (&ace.SidStart as *const u32).cast_mut().cast();
            let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
            if unsafe { IsValidSid(sid) } == 0
                || unsafe { GetLengthSid(sid) } as usize
                    > usize::from(header.AceSize).saturating_sub(sid_offset)
            {
                return Err(GateError("DACL contains an invalid ACE SID".to_string()));
            }
            if header.AceType == ACCESS_DENIED_ACE_TYPE {
                let _deny_layout = unsafe { &*(raw_ace.cast::<ACCESS_DENIED_ACE>()) };
                if u32::from(header.AceFlags) & INHERITED_ACE != 0 {
                    return Err(GateError("inherited deny ACE is ambiguous".to_string()));
                }
                continue;
            }
            let mapped_mask = mapped_file_mask(ace.Mask);
            if policy.broad(sid) && (reject_broad_access || dangerous_write_mask(mapped_mask)) {
                return Err(GateError(
                    "broad-principal allow ACE is forbidden".to_string(),
                ));
            }
            if sid_equal(sid, appcontainer_sid) {
                appcontainer_allows.push((ace.Mask, header.AceFlags));
                continue;
            }
            if reject_broad_access && mapped_mask & required != 0 && !policy.trusted_writer(sid) {
                return Err(GateError(
                    "an unexpected principal can read or execute the image".to_string(),
                ));
            }
            if dangerous_write_mask(mapped_mask) && !policy.trusted_writer(sid) {
                return Err(GateError(
                    "another principal can write, modify, or delete the path".to_string(),
                ));
            }
        }
        if (require_exact_appcontainer_rx || !appcontainer_allows.is_empty())
            && !package_allow_set_is_exact(&appcontainer_allows)
        {
            return Err(GateError(format!(
                "expected one exact AppContainer RX ACE, found {} package allow ACEs",
                appcontainer_allows.len()
            )));
        }
        Ok(())
    }

    fn trustee_for_sid(sid: PSID) -> TRUSTEE_W {
        TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.cast(),
        }
    }

    fn prepare_executable_acl(
        executable: &Path,
        appcontainer_sid: PSID,
        policy: &SidPolicy,
    ) -> Result<(PathBuf, InstallLocation, WinHandle), GateError> {
        reject_unc_or_remote_syntax(executable)?;
        reject_reparse_components(executable)?;
        let executable = std::fs::canonicalize(executable)
            .map_err(|error| GateError(format!("canonicalize disposable executable: {error}")))?;
        if !executable
            .metadata()
            .map_err(|error| GateError(format!("stat current executable: {error}")))?
            .is_file()
        {
            return Err(GateError(
                "LPAC feasibility ACL target is not an exact file".to_string(),
            ));
        }

        let location = classify_install_location(&executable);
        if matches!(
            location,
            InstallLocation::ProtectedMachineWide | InstallLocation::Unsupported
        ) {
            return Err(GateError(format!(
                "current executable location is {location:?}; protected or unknown installs remain unsupported"
            )));
        }

        reject_unc_or_remote_syntax(&executable)?;
        validate_path_ancestors(&executable, location, policy)?;

        let security = read_file_security(&executable)?;
        if !sid_equal(security.owner, policy.user.as_psid()) {
            return Err(GateError(
                "current user does not own the executable; ACL mutation refused".to_string(),
            ));
        }
        inspect_acl(security.dacl, policy, appcontainer_sid, false, true)?;

        let trustee = trustee_for_sid(appcontainer_sid);
        let mut rights = 0u32;
        // SAFETY: `dacl` remains within the owned security descriptor and the
        // trustee's exact AppContainer SID lives through the call. No pointer is
        // retained by GetEffectiveRightsFromAclW.
        let result = unsafe { GetEffectiveRightsFromAclW(security.dacl, &trustee, &mut rights) };
        if result != 0 {
            return Err(win32_error(
                "inspect AppContainer executable rights",
                result,
            ));
        }
        let required = required_image_mask();
        if mapped_file_mask(rights) & required != required {
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: required,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: trustee,
            };
            debug_assert_eq!(entry.Trustee.TrusteeType, TRUSTEE_IS_USER);
            debug_assert_ne!(entry.Trustee.TrusteeType, TRUSTEE_IS_UNKNOWN);
            let mut new_acl: *mut ACL = null_mut();
            let result = unsafe { SetEntriesInAclW(1, &entry, security.dacl, &mut new_acl) };
            if result != 0 {
                if !new_acl.is_null() {
                    drop(LocalMemory(new_acl.cast()));
                }
                return Err(win32_error("construct exact AppContainer ACE", result));
            }
            if new_acl.is_null() {
                return Err(GateError(
                    "SetEntriesInAclW returned a null ACL".to_string(),
                ));
            }
            let new_acl = LocalMemory(new_acl.cast());
            let mut path = wide_null(executable.as_os_str())?;
            let result = unsafe {
                SetNamedSecurityInfoW(
                    path.as_mut_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    new_acl.0.cast(),
                    null_mut(),
                )
            };
            if result != 0 {
                return Err(win32_error("set exact AppContainer executable ACE", result));
            }
        }

        // Re-read after mutation: never trust a constructed ACL without
        // checking the state the filesystem actually committed.
        let committed = read_file_security(&executable)?;
        inspect_acl(committed.dacl, policy, appcontainer_sid, true, true)?;
        let mut effective = 0u32;
        let result =
            unsafe { GetEffectiveRightsFromAclW(committed.dacl, &trustee, &mut effective) };
        if result != 0
            || mapped_file_mask(effective) & required != required
            || dangerous_write_mask(effective)
        {
            return Err(GateError(
                "committed AppContainer access is not read/execute-only".to_string(),
            ));
        }

        let path = wide_null(executable.as_os_str())?;
        // FILE_SHARE_READ deliberately excludes share-write and share-delete.
        // Keeping this handle alive through CreateProcess closes the final-file
        // replacement window between ACL inspection and image mapping.
        let image_lock = WinHandle::from_created(
            unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_EXECUTE,
                    FILE_SHARE_READ,
                    null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    null_mut(),
                )
            },
            "lock inspected executable against write/delete",
        )?;
        Ok((executable, location, image_lock))
    }

    fn supported_root(path: &Path, location: InstallLocation) -> Result<PathBuf, GateError> {
        match location {
            InstallLocation::CargoInstall => std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".cargo"))
                })
                .ok_or_else(|| GateError("Cargo home is unavailable".to_string())),
            InstallLocation::CargoBuild => {
                let mut cursor = path.parent();
                while let Some(candidate) = cursor {
                    if candidate
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("target"))
                    {
                        return candidate.parent().map(Path::to_path_buf).ok_or_else(|| {
                            GateError("target directory has no project root".to_string())
                        });
                    }
                    cursor = candidate.parent();
                }
                Err(GateError(
                    "Cargo build path has no target ancestor".to_string(),
                ))
            }
            InstallLocation::UserArchive => std::env::var_os("LOCALAPPDATA")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .ok_or_else(|| GateError("user archive root is unavailable".to_string())),
            _ => Err(GateError(
                "unsupported location has no trusted root".to_string(),
            )),
        }
    }

    fn validate_path_ancestors(
        executable: &Path,
        location: InstallLocation,
        policy: &SidPolicy,
    ) -> Result<(), GateError> {
        let root = std::fs::canonicalize(supported_root(executable, location)?)
            .map_err(|error| GateError(format!("canonicalize supported root: {error}")))?;
        if !starts_with_case_insensitive(executable, &root) {
            return Err(GateError(
                "executable escaped its classified root".to_string(),
            ));
        }
        let mut cursor = Some(executable);
        while let Some(path) = cursor {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| GateError(format!("inspect path component: {error}")))?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(GateError(
                    "reparse points are forbidden in the image path".to_string(),
                ));
            }
            if path != executable {
                let security = read_file_security(path)?;
                if !policy.trusted_writer(security.owner) {
                    return Err(GateError(
                        "an untrusted principal owns an image-path ancestor".to_string(),
                    ));
                }
                inspect_acl(security.dacl, policy, null_mut(), false, false)?;
            }
            if starts_with_case_insensitive(&root, path) || path == root {
                break;
            }
            cursor = path.parent();
        }
        Ok(())
    }

    fn validate_source_artifact(
        source: &Path,
        expected: InstallLocation,
        policy: &SidPolicy,
    ) -> Result<WinHandle, GateError> {
        if classify_install_location(source) != expected {
            return Err(GateError(format!(
                "source artifact is not in its required {expected:?} location"
            )));
        }
        reject_unc_or_remote_syntax(source)?;
        reject_reparse_components(source)?;
        validate_path_ancestors(source, expected, policy)?;
        let source_wide = wide_null(source.as_os_str())?;
        let source_lock = WinHandle::from_created(
            unsafe {
                CreateFileW(
                    source_wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ,
                    null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    null_mut(),
                )
            },
            "lock validated source artifact while copying",
        )?;
        let security = read_file_security(source)?;
        if !policy.trusted_writer(security.owner) {
            return Err(GateError(
                "an untrusted principal owns the source artifact".to_string(),
            ));
        }
        inspect_acl(security.dacl, policy, null_mut(), false, false)?;
        Ok(source_lock)
    }

    #[derive(Debug)]
    struct AttributeList {
        pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
        heap: HANDLE,
    }

    impl AttributeList {
        fn new(count: u32) -> Result<Self, GateError> {
            let mut bytes = 0usize;
            // SAFETY: the null attribute-list pointer is the documented sizing
            // probe. `bytes` is initialized and writable for the returned exact
            // allocation length.
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes);
            }
            if bytes == 0 {
                return Err(last_error("size process attribute list"));
            }
            // SAFETY: GetProcessHeap returns a borrowed process heap handle; the
            // requested `bytes` is exactly the size reported above.
            let heap = unsafe { GetProcessHeap() };
            // SAFETY: HeapAlloc receives the valid process heap and exact byte
            // count. The returned allocation is released with HeapFree in Drop.
            let pointer = unsafe { HeapAlloc(heap, HEAP_ZERO_MEMORY, bytes) };
            if pointer.is_null() {
                return Err(last_error("allocate process attribute list"));
            }
            // SAFETY: `pointer` references `bytes` writable bytes and remains
            // owned through DeleteProcThreadAttributeList/HeapFree.
            if unsafe { InitializeProcThreadAttributeList(pointer, count, 0, &mut bytes) } == 0 {
                // SAFETY: initialization failed before an attribute-list cleanup
                // obligation arose; HeapFree releases the exact allocation.
                unsafe {
                    HeapFree(heap, 0, pointer);
                }
                return Err(last_error("initialize process attribute list"));
            }
            Ok(Self { pointer, heap })
        }

        fn update<T>(&mut self, attribute: u32, value: &T) -> Result<(), GateError> {
            // SAFETY: `value` points to exactly size_of::<T>() initialized bytes
            // and every caller keeps both the value and any referenced buffers
            // alive until CreateProcessW returns. The attribute list owns only a
            // borrowed pointer during process creation.
            if unsafe {
                UpdateProcThreadAttribute(
                    self.pointer,
                    0,
                    attribute as usize,
                    (value as *const T).cast(),
                    size_of::<T>(),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(last_error("update process attribute"));
            }
            Ok(())
        }

        fn update_slice<T>(&mut self, attribute: u32, value: &[T]) -> Result<(), GateError> {
            // SAFETY: the slice pointer references exactly size_of_val(value)
            // initialized bytes and the caller keeps the slice alive through
            // CreateProcessW. No previous-value buffer is requested.
            if unsafe {
                UpdateProcThreadAttribute(
                    self.pointer,
                    0,
                    attribute as usize,
                    value.as_ptr().cast(),
                    size_of_val(value),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(last_error("update process handle-list attribute"));
            }
            Ok(())
        }
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            // SAFETY: successful initialization created exactly one delete
            // obligation for `pointer`; Delete does not free the backing bytes.
            unsafe {
                DeleteProcThreadAttributeList(self.pointer);
            }
            // SAFETY: the same process heap and allocation returned by HeapAlloc
            // are used once, after the attribute list no longer references it.
            unsafe {
                HeapFree(self.heap, 0, self.pointer);
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ProductionFailurePoint {
        CreateProfile,
        PrepareExecutableAcl,
        CreateStdinPipe,
        CreateStdoutPipe,
        CreateStderrPipe,
        CreateJob,
        SetJobLimits,
        SetJobUiRestrictions,
        AllocateAttributeList,
        SecurityCapabilitiesAttribute,
        LpacOptOutAttribute,
        JobListAttribute,
        ChildProcessPolicyAttribute,
        MitigationPolicyAttribute,
        HandleListAttribute,
        CreateProcess,
        OwnProcessHandle,
        OwnThreadHandle,
        ClearStdinInheritance,
        ClearStdoutInheritance,
        ClearStderrInheritance,
        VerifyCreationTimeJob,
    }

    pub(super) struct ProductionLaunchHooks {
        fail_at: Option<ProductionFailurePoint>,
        deadline: Option<Instant>,
        child: ProductionChild,
        #[cfg(test)]
        containment: Option<ContainmentProbeConfiguration>,
        #[cfg(test)]
        executable_override: Option<PathBuf>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProductionChild {
        Worker,
        FailureTest,
        #[cfg(test)]
        ContainmentTest,
        #[cfg(test)]
        ProtocolTest,
    }

    #[cfg(test)]
    #[derive(Debug)]
    pub(super) struct ContainmentProbeConfiguration {
        workspace: PathBuf,
        skill_database: PathBuf,
        file_handle: HANDLE,
        socket_handle: HANDLE,
        file_canary: Option<File>,
        socket_canary: Option<TcpListener>,
        tcp_port: u16,
        udp_port: u16,
    }

    #[cfg(test)]
    struct ProbeCanaryInheritance {
        handles: [HANDLE; 2],
        _file_canary: Option<File>,
        _socket_canary: Option<TcpListener>,
        cleared: bool,
    }

    #[cfg(test)]
    impl ProbeCanaryInheritance {
        fn new(
            configuration: Option<&mut ContainmentProbeConfiguration>,
        ) -> Result<Self, GateError> {
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

            let Some(configuration) = configuration else {
                return Ok(Self {
                    handles: [null_mut(), null_mut()],
                    _file_canary: None,
                    _socket_canary: None,
                    cleared: true,
                });
            };
            let handles = [configuration.file_handle, configuration.socket_handle];
            let mut armed = Self {
                handles,
                _file_canary: configuration.file_canary.take(),
                _socket_canary: configuration.socket_canary.take(),
                cleared: false,
            };
            if armed._file_canary.is_none() || armed._socket_canary.is_none() {
                return Err(GateError(
                    "Windows containment canary ownership was already transferred".to_string(),
                ));
            }
            for (index, handle) in handles.into_iter().enumerate() {
                if handle.is_null() || handle == (-1isize as HANDLE) {
                    armed.clear_best_effort();
                    return Err(GateError(format!(
                        "Windows containment canary {index} is not a valid handle"
                    )));
                }
                // SAFETY: `armed` owns both canary resources through the complete production
                // launch. The shared creation lock is already held, and this call changes only
                // the inheritance flag without retaining or closing the handle.
                if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                    == 0
                {
                    armed.clear_best_effort();
                    return Err(last_error("mark omitted containment canary inheritable"));
                }
            }
            armed.cleared = false;
            Ok(armed)
        }

        fn clear(&mut self) -> Result<(), GateError> {
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

            if self.cleared {
                return Ok(());
            }
            for handle in self.handles {
                // SAFETY: each owned resource remains live, the creation lock remains held, and
                // this call clears only its inheritance bit.
                if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
                    return Err(last_error("clear omitted containment-canary inheritance"));
                }
            }
            self.cleared = true;
            Ok(())
        }

        fn clear_best_effort(&mut self) {
            use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

            for handle in self.handles {
                if !handle.is_null() && handle != (-1isize as HANDLE) {
                    // SAFETY: Drop/error cleanup changes only the owned live handle's inherit bit
                    // while the creation lock is still held. The owned resource closes
                    // immediately afterward even if this best-effort clear fails.
                    unsafe {
                        SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                    }
                }
            }
            self.cleared = true;
        }
    }

    #[cfg(test)]
    impl Drop for ProbeCanaryInheritance {
        fn drop(&mut self) {
            if !self.cleared {
                self.clear_best_effort();
            }
        }
    }

    impl ProductionLaunchHooks {
        pub(super) const fn production() -> Self {
            Self {
                fail_at: None,
                deadline: None,
                child: ProductionChild::Worker,
                #[cfg(test)]
                containment: None,
                #[cfg(test)]
                executable_override: None,
            }
        }

        #[cfg(test)]
        pub(super) const fn fail_at(point: ProductionFailurePoint) -> Self {
            Self {
                fail_at: Some(point),
                deadline: None,
                child: ProductionChild::FailureTest,
                containment: None,
                executable_override: None,
            }
        }

        #[cfg(test)]
        pub(super) fn containment(
            configuration: ContainmentProbeConfiguration,
            executable: PathBuf,
        ) -> Self {
            Self {
                fail_at: None,
                deadline: None,
                child: ProductionChild::ContainmentTest,
                containment: Some(configuration),
                executable_override: Some(executable),
            }
        }

        #[cfg(test)]
        pub(super) fn protocol_test(executable: PathBuf) -> Self {
            Self {
                fail_at: None,
                deadline: None,
                child: ProductionChild::ProtocolTest,
                containment: None,
                executable_override: Some(executable),
            }
        }

        #[cfg(test)]
        pub(super) fn installed_worker(executable: PathBuf) -> Self {
            Self {
                fail_at: None,
                deadline: None,
                child: ProductionChild::Worker,
                containment: None,
                executable_override: Some(executable),
            }
        }

        fn checkpoint(&self, point: ProductionFailurePoint) -> Result<(), GateError> {
            if self.fail_at == Some(point) {
                return Err(GateError(format!(
                    "injected Windows launcher failure at {point:?}"
                )));
            }
            Ok(())
        }

        fn with_deadline(mut self, deadline: Instant) -> Self {
            self.deadline = Some(deadline);
            self
        }

        fn require_before_deadline(&self) -> Result<(), GateError> {
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                Err(GateError(
                    "Windows production launch exceeded its caller deadline".to_string(),
                ))
            } else {
                Ok(())
            }
        }
    }

    pub(super) struct ProtocolPipes {
        parent_input: WinHandle,
        parent_output: WinHandle,
        parent_error: WinHandle,
        child_input: WinHandle,
        child_output: WinHandle,
        child_error: WinHandle,
    }

    fn inheritable_pipe(
        _inheritance_guard: &CreationGuard,
    ) -> Result<(WinHandle, WinHandle), GateError> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: TRUE,
        };
        let mut read = null_mut();
        let mut write = null_mut();
        // SAFETY: both output HANDLE slots and the initialized attributes live
        // through the call. On success each raw handle has one CloseHandle
        // obligation immediately transferred to OwnedHandle.
        if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
            close_unowned_handle(read);
            close_unowned_handle(write);
            return Err(last_error("create anonymous protocol pipe"));
        }
        let read = WinHandle::from_inheritable_created(read, "create protocol read handle")?;
        let write = WinHandle::from_inheritable_created(write, "create protocol write handle")?;
        Ok((read, write))
    }

    impl ProtocolPipes {
        pub(super) fn exact_anonymous_set(
            inheritance_guard: &CreationGuard,
        ) -> Result<Self, GateError> {
            let (child_input, mut parent_input) = inheritable_pipe(inheritance_guard)?;
            parent_input.clear_inherit()?;
            let (mut parent_output, child_output) = inheritable_pipe(inheritance_guard)?;
            parent_output.clear_inherit()?;
            let (mut parent_error, child_error) = inheritable_pipe(inheritance_guard)?;
            parent_error.clear_inherit()?;
            Ok(Self {
                parent_input,
                parent_output,
                parent_error,
                child_input,
                child_output,
                child_error,
            })
        }

        fn child_handles(&self) -> [HANDLE; 3] {
            [
                self.child_input.raw(),
                self.child_output.raw(),
                self.child_error.raw(),
            ]
        }

        fn production_set(
            hooks: &ProductionLaunchHooks,
            inheritance_guard: &CreationGuard,
        ) -> Result<Self, GateError> {
            hooks.checkpoint(ProductionFailurePoint::CreateStdinPipe)?;
            let (child_input, mut parent_input) = inheritable_pipe(inheritance_guard)?;
            parent_input.clear_inherit()?;
            hooks.checkpoint(ProductionFailurePoint::CreateStdoutPipe)?;
            let (mut parent_output, child_output) = inheritable_pipe(inheritance_guard)?;
            parent_output.clear_inherit()?;
            hooks.checkpoint(ProductionFailurePoint::CreateStderrPipe)?;
            let (mut parent_error, child_error) = inheritable_pipe(inheritance_guard)?;
            parent_error.clear_inherit()?;
            Ok(Self {
                parent_input,
                parent_output,
                parent_error,
                child_input,
                child_output,
                child_error,
            })
        }

        pub(super) fn clear_child_inheritance(&mut self) -> Result<(), GateError> {
            self.child_input.clear_inherit()?;
            self.child_output.clear_inherit()?;
            self.child_error.clear_inherit()?;
            Ok(())
        }

        #[cfg(test)]
        pub(super) fn into_test_handles(
            self,
        ) -> (
            WinHandle,
            WinHandle,
            WinHandle,
            WinHandle,
            WinHandle,
            WinHandle,
        ) {
            (
                self.parent_input,
                self.parent_output,
                self.parent_error,
                self.child_input,
                self.child_output,
                self.child_error,
            )
        }
    }

    fn temporary_job() -> Result<WinHandle, GateError> {
        // SAFETY: null attributes/name request an unnamed Job and return one
        // owned handle, transferred immediately to OwnedHandle.
        let job = WinHandle::from_created(
            unsafe { CreateJobObjectW(null(), null()) },
            "create temporary LPAC Job",
        )?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        // SAFETY: `limits` is a fully initialized value with the exact byte
        // length declared. SetInformationJobObject reads it synchronously and
        // does not retain the pointer.
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(last_error("configure temporary LPAC Job"));
        }
        Ok(job)
    }

    fn production_job(hooks: &ProductionLaunchHooks) -> Result<WinHandle, GateError> {
        hooks.checkpoint(ProductionFailurePoint::CreateJob)?;
        // SAFETY: null attributes/name request one private unnamed Job. Ownership transfers
        // immediately to WinHandle, whose OwnedHandle closes it on every return path.
        let job = WinHandle::from_created(
            unsafe { CreateJobObjectW(null(), null()) },
            "create JavaScript worker Job",
        )?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_PROCESS_TIME;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.BasicLimitInformation.PerProcessUserTimeLimit = PROCESS_CPU_LIMIT_100NS;
        limits.ProcessMemoryLimit = PROCESS_MEMORY_LIMIT_BYTES;
        // SAFETY: `limits` is fully initialized and lives for this synchronous call. The Job
        // copies the values and retains no pointer.
        hooks.checkpoint(ProductionFailurePoint::SetJobLimits)?;
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(last_error("configure JavaScript worker Job limits"));
        }

        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_ALL,
        };
        // SAFETY: `ui` is initialized with the complete documented restriction mask and lives
        // through this synchronous call. No pointer is retained by the Job.
        hooks.checkpoint(ProductionFailurePoint::SetJobUiRestrictions)?;
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        } == 0
        {
            return Err(last_error(
                "configure JavaScript worker Job UI restrictions",
            ));
        }
        Ok(job)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ProbeKind {
        Harness,
        ImageLoadingOnly,
    }

    fn command_line(executable: &Path, probe: ProbeKind) -> Result<Vec<u16>, GateError> {
        let display = executable.as_os_str().to_string_lossy();
        if display.contains('"') {
            return Err(GateError(
                "current executable path contains a quote".to_string(),
            ));
        }
        let arguments = match probe {
            ProbeKind::Harness => {
                format!("--exact {CHILD_TEST_NAME} --nocapture --test-threads=1")
            }
            ProbeKind::ImageLoadingOnly => "--version".to_string(),
        };
        Ok(wide_string(&format!("\"{display}\" {arguments}")))
    }

    fn environment_block(sentinel: &Path, omitted_handle: HANDLE) -> Result<Vec<u16>, GateError> {
        let sentinel = sentinel.as_os_str().to_string_lossy();
        if sentinel.contains('\0') || sentinel.contains('=') {
            return Err(GateError(
                "sentinel path cannot enter environment".to_string(),
            ));
        }
        let mut entries = vec![
            format!("{SENTINEL_ENV}={sentinel}"),
            format!("{CANARY_HANDLE_ENV}={}", omitted_handle as usize),
        ];
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            entries.push(format!("SystemRoot={}", system_root.to_string_lossy()));
        }
        entries.sort_by_key(|entry| entry.to_ascii_lowercase());
        let mut block = Vec::new();
        for entry in entries {
            block.extend(OsStr::new(&entry).encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    struct Sentinel(PathBuf);

    impl Sentinel {
        fn workspace_file() -> Result<Self, GateError> {
            let path = std::env::current_dir()
                .map_err(|error| GateError(format!("resolve workspace: {error}")))?
                .join(format!(
                    ".mini-agent-lpac-workspace-sentinel-{}",
                    std::process::id()
                ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| GateError(format!("create workspace sentinel: {error}")))?;
            file.write_all(b"LPAC must not read this workspace sentinel")
                .map_err(|error| GateError(format!("write workspace sentinel: {error}")))?;
            file.sync_all()
                .map_err(|error| GateError(format!("sync workspace sentinel: {error}")))?;
            Ok(Self(path))
        }

        fn skill_database_file() -> Result<Self, GateError> {
            let path = std::env::temp_dir().join(format!(
                "mini-agent-lpac-skill-database-sentinel-{}.sqlite3",
                std::process::id()
            ));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| GateError(format!("create skill database sentinel: {error}")))?;
            file.write_all(b"LPAC must not read or write this skill database sentinel")
                .map_err(|error| GateError(format!("write skill database sentinel: {error}")))?;
            file.sync_all()
                .map_err(|error| GateError(format!("sync skill database sentinel: {error}")))?;
            Ok(Self(path))
        }
    }

    impl Drop for Sentinel {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    struct DisposableArtifact {
        executable: PathBuf,
        directory: PathBuf,
        destination_expected: InstallLocation,
        probe: ProbeKind,
        cleaned: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ArtifactSource {
        Harness,
        Installed,
    }

    #[derive(Debug, Clone, Copy)]
    pub(super) struct ArtifactContract {
        pub(super) source: ArtifactSource,
        pub(super) source_location: InstallLocation,
        pub(super) destination: InstallLocation,
        pub(super) probe: ProbeKind,
    }

    pub(super) fn artifact_contracts() -> [ArtifactContract; 5] {
        [
            ArtifactContract {
                source: ArtifactSource::Harness,
                source_location: InstallLocation::CargoBuild,
                destination: InstallLocation::CargoBuild,
                probe: ProbeKind::Harness,
            },
            ArtifactContract {
                source: ArtifactSource::Harness,
                source_location: InstallLocation::CargoBuild,
                destination: InstallLocation::CargoInstall,
                probe: ProbeKind::Harness,
            },
            ArtifactContract {
                source: ArtifactSource::Harness,
                source_location: InstallLocation::CargoBuild,
                destination: InstallLocation::UserArchive,
                probe: ProbeKind::Harness,
            },
            ArtifactContract {
                source: ArtifactSource::Installed,
                source_location: InstallLocation::CargoInstall,
                destination: InstallLocation::CargoInstall,
                probe: ProbeKind::ImageLoadingOnly,
            },
            ArtifactContract {
                source: ArtifactSource::Installed,
                source_location: InstallLocation::CargoInstall,
                destination: InstallLocation::UserArchive,
                probe: ProbeKind::ImageLoadingOnly,
            },
        ]
    }

    struct ArtifactSpec {
        source: PathBuf,
        source_expected: InstallLocation,
        directory: PathBuf,
        destination_expected: InstallLocation,
        probe: ProbeKind,
    }

    impl DisposableArtifact {
        fn copy_into(
            source: &Path,
            source_lock: WinHandle,
            directory: PathBuf,
            destination_expected: InstallLocation,
            probe: ProbeKind,
        ) -> Result<Self, GateError> {
            std::fs::create_dir(&directory).map_err(|error| {
                GateError(format!("create private artifact directory: {error}"))
            })?;
            let executable = directory.join("mini-agent-lpac-gate.exe");
            if let Err(error) = std::fs::copy(source, &executable) {
                return match std::fs::remove_dir(&directory) {
                    Ok(()) => Err(GateError(format!(
                        "copy real artifact for LPAC gate: {error}"
                    ))),
                    Err(cleanup) => Err(GateError(format!(
                        "copy real artifact for LPAC gate: {error}; directory cleanup also failed: {cleanup}"
                    ))),
                };
            }
            drop(source_lock);
            if classify_install_location(&executable) != destination_expected {
                let file_cleanup = std::fs::remove_file(&executable);
                let directory_cleanup = std::fs::remove_dir(&directory);
                if let Err(error) = file_cleanup.and(directory_cleanup) {
                    return Err(GateError(format!(
                        "disposable artifact did not retain expected {destination_expected:?} location; cleanup also failed: {error}"
                    )));
                }
                return Err(GateError(format!(
                    "disposable artifact did not retain expected {destination_expected:?} location"
                )));
            }
            Ok(Self {
                executable,
                directory,
                destination_expected,
                probe,
                cleaned: false,
            })
        }

        fn cleanup_inner(&mut self) -> Result<(), GateError> {
            if self.cleaned {
                return Ok(());
            }
            std::fs::remove_file(&self.executable).map_err(|error| {
                GateError(format!("remove disposable LPAC executable: {error}"))
            })?;
            std::fs::remove_dir(&self.directory)
                .map_err(|error| GateError(format!("remove disposable LPAC directory: {error}")))?;
            self.cleaned = true;
            Ok(())
        }

        fn cleanup(mut self) -> Result<(), GateError> {
            self.cleanup_inner()
        }
    }

    impl Drop for DisposableArtifact {
        fn drop(&mut self) {
            if !self.cleaned {
                let _ = self.cleanup_inner();
            }
        }
    }

    fn artifact_matrix() -> Result<Vec<ArtifactSpec>, GateError> {
        let build = std::env::current_exe()
            .map_err(|error| GateError(format!("resolve Cargo build harness: {error}")))?;
        reject_unc_or_remote_syntax(&build)?;
        reject_reparse_components(&build)?;
        let build = std::fs::canonicalize(build)
            .map_err(|error| GateError(format!("canonicalize Cargo build harness: {error}")))?;
        if classify_install_location(&build) != InstallLocation::CargoBuild {
            return Err(GateError(
                "current test executable is not a Cargo build artifact".to_string(),
            ));
        }

        let installed = std::env::var_os(INSTALLED_EXE_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| {
                GateError(format!(
                    "{INSTALLED_EXE_ENV} must name a real locked debug install with only the js feature"
                ))
            })?;
        reject_unc_or_remote_syntax(&installed)?;
        reject_reparse_components(&installed)?;
        let installed = std::fs::canonicalize(installed)
            .map_err(|error| GateError(format!("canonicalize Cargo install artifact: {error}")))?;
        if !installed
            .metadata()
            .map_err(|error| GateError(format!("stat Cargo install artifact: {error}")))?
            .is_file()
        {
            return Err(GateError(
                "Cargo install artifact is not an exact file".to_string(),
            ));
        }
        if classify_install_location(&installed) != InstallLocation::CargoInstall {
            return Err(GateError(format!(
                "{INSTALLED_EXE_ENV} is not beneath the active Cargo home"
            )));
        }

        let suffix = format!("{}-{}", std::process::id(), PROFILE_NAME.len());
        let build_parent = build
            .parent()
            .ok_or_else(|| GateError("build harness has no parent".to_string()))?;
        let install_parent = installed
            .parent()
            .ok_or_else(|| GateError("installed artifact has no parent".to_string()))?;
        let archive_root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| {
                GateError("LOCALAPPDATA is required for the archive case".to_string())
            })?;
        let directories = [
            build_parent.join(format!(".mini agent λ lpac containment build {suffix}")),
            install_parent.join(format!(".mini agent λ lpac containment install {suffix}")),
            archive_root.join(format!("mini agent λ lpac containment archive {suffix}")),
            install_parent.join(format!(".mini agent λ lpac image install {suffix}")),
            archive_root.join(format!("mini agent λ lpac image archive {suffix}")),
        ];

        Ok(artifact_contracts()
            .into_iter()
            .zip(directories)
            .map(|(contract, directory)| ArtifactSpec {
                source: match contract.source {
                    ArtifactSource::Harness => build.clone(),
                    ArtifactSource::Installed => installed.clone(),
                },
                source_expected: contract.source_location,
                directory,
                destination_expected: contract.destination,
                probe: contract.probe,
            })
            .collect())
    }

    fn wait_for_child(process: &WinHandle) -> Result<u32, GateError> {
        let timeout = u32::try_from(CHILD_TIMEOUT.as_millis())
            .map_err(|_| GateError("child timeout does not fit u32".to_string()))?;
        // SAFETY: the process handle remains owned and open for the bounded
        // wait; WaitForSingleObject neither stores nor closes it.
        match unsafe { WaitForSingleObject(process.raw(), timeout) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                return Err(GateError(
                    "LPAC child did not produce readiness before timeout".to_string(),
                ));
            }
            _ => return Err(last_error("wait for LPAC child")),
        }
        let mut exit_code = 0u32;
        // SAFETY: `exit_code` is an initialized output slot and the process
        // handle stays valid through the call.
        if unsafe { GetExitCodeProcess(process.raw(), &mut exit_code) } == 0 {
            return Err(last_error("read LPAC child exit code"));
        }
        Ok(exit_code)
    }

    fn drain_pipe(
        handle: WinHandle,
        retained_limit: usize,
        label: &'static str,
    ) -> std::thread::JoinHandle<Result<Vec<u8>, GateError>> {
        std::thread::spawn(move || {
            let mut file = handle.into_file();
            let mut retained = Vec::new();
            let mut chunk = [0u8; 4 * 1024];
            loop {
                let read = file
                    .read(&mut chunk)
                    .map_err(|error| GateError(format!("read {label}: {error}")))?;
                if read == 0 {
                    return Ok(retained);
                }
                let keep = retained_limit.saturating_sub(retained.len()).min(read);
                retained.extend_from_slice(&chunk[..keep]);
            }
        })
    }

    fn join_pipe(
        reader: std::thread::JoinHandle<Result<Vec<u8>, GateError>>,
        label: &str,
    ) -> Result<Vec<u8>, GateError> {
        reader
            .join()
            .map_err(|_| GateError(format!("{label} reader panicked")))?
    }

    fn production_command_line(
        executable: &Path,
        child: ProductionChild,
    ) -> Result<Vec<u16>, GateError> {
        let display = executable.as_os_str().to_string_lossy();
        if display.contains('"') {
            return Err(GateError(
                "Windows worker executable path contains a quote".to_string(),
            ));
        }
        let arguments = match child {
            ProductionChild::Worker => "".to_string(),
            ProductionChild::FailureTest => " --exact sandbox::worker::platform::tests::windows_production_failure_child --nocapture --test-threads=1".to_string(),
            #[cfg(test)]
            ProductionChild::ContainmentTest => format!(
                " --exact {CONTAINMENT_CHILD_TEST_NAME} --nocapture --test-threads=1"
            ),
            #[cfg(test)]
            ProductionChild::ProtocolTest => format!(
                " --exact {PROTOCOL_CHILD_TEST_NAME} --nocapture --test-threads=1"
            ),
        };
        Ok(wide_string(&format!("\"{display}\"{arguments}")))
    }

    fn production_environment_block(hooks: &ProductionLaunchHooks) -> Result<Vec<u16>, GateError> {
        let marker = match hooks.child {
            ProductionChild::Worker | ProductionChild::FailureTest => INTERNAL_WORKER_MARKER_VALUE,
            #[cfg(test)]
            ProductionChild::ContainmentTest => CONTAINMENT_MARKER_VALUE,
            #[cfg(test)]
            ProductionChild::ProtocolTest => INTERNAL_WORKER_MARKER_VALUE,
        };
        let mut entries = vec![format!("{INTERNAL_WORKER_MARKER}={marker}")];
        #[cfg(test)]
        if let Some(probe) = &hooks.containment {
            for (name, value) in [
                (
                    PROBE_WORKSPACE_ENV,
                    probe.workspace.to_string_lossy().into_owned(),
                ),
                (
                    PROBE_SKILL_DATABASE_ENV,
                    probe.skill_database.to_string_lossy().into_owned(),
                ),
                (
                    PROBE_FILE_HANDLE_ENV,
                    (probe.file_handle as usize).to_string(),
                ),
                (
                    PROBE_SOCKET_HANDLE_ENV,
                    (probe.socket_handle as usize).to_string(),
                ),
                (PROBE_TCP_PORT_ENV, probe.tcp_port.to_string()),
                (PROBE_UDP_PORT_ENV, probe.udp_port.to_string()),
            ] {
                if value.contains('\0') || value.contains('=') {
                    return Err(GateError(format!(
                        "{name} cannot enter the probe environment"
                    )));
                }
                entries.push(format!("{name}={value}"));
            }
        }
        // SystemRoot is non-secret loader configuration required by Windows system DLL
        // resolution. No PATH, profile, credential, workspace, or application variable crosses.
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let system_root = system_root.to_string_lossy();
            if system_root.contains('\0') || system_root.contains('=') {
                return Err(GateError(
                    "SystemRoot cannot enter the worker environment".to_string(),
                ));
            }
            entries.push(format!("SystemRoot={system_root}"));
        }
        entries.sort_by_key(|entry| entry.to_ascii_lowercase());
        let mut block = Vec::new();
        for entry in entries {
            block.extend(OsStr::new(&entry).encode_wide());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }

    fn production_executable(_hooks: &ProductionLaunchHooks) -> Result<PathBuf, GateError> {
        #[cfg(test)]
        if let Some(executable) = &_hooks.executable_override {
            return Ok(executable.clone());
        }
        std::env::current_exe()
            .map_err(|error| GateError(format!("resolve current executable: {error}")))
    }

    pub(super) fn production_runtime_preflight() -> Result<(), GateError> {
        run_runtime_preflight(RuntimePreflightTarget::CurrentExecutable)
    }

    #[cfg(test)]
    pub(super) fn installed_worker_runtime_preflight(executable: &Path) -> Result<(), GateError> {
        run_runtime_preflight(RuntimePreflightTarget::Installed(executable.to_path_buf()))
    }

    pub(super) fn launch_production(
        hooks: ProductionLaunchHooks,
    ) -> Result<WorkerProcess, GateError> {
        #[cfg(test)]
        let mut hooks = hooks;
        hooks.checkpoint(ProductionFailurePoint::CreateProfile)?;
        let profile = AppContainerProfile::production_zero_capability()?;
        hooks.checkpoint(ProductionFailurePoint::PrepareExecutableAcl)?;
        let executable = production_executable(&hooks)?;
        let policy = SidPolicy::current()?;
        let (executable, _location, image_lock) =
            prepare_executable_acl(&executable, profile.sid, &policy)?;
        let inheritance_guard = crate::process_creation::creation_guard()?;
        hooks.require_before_deadline()?;
        let mut pipes = ProtocolPipes::production_set(&hooks, &inheritance_guard)?;
        #[cfg(test)]
        let mut probe_canary_inheritance = ProbeCanaryInheritance::new(hooks.containment.as_mut())?;
        let job = production_job(&hooks)?;

        let security_capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: profile.sid,
            Capabilities: null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let inherited_handles = pipes.child_handles();
        let job_handles = [job.raw()];
        let all_packages_policy = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
        let child_process_policy = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
        let mitigation_policy = MITIGATION_POLICY;
        hooks.checkpoint(ProductionFailurePoint::AllocateAttributeList)?;
        let mut attributes = AttributeList::new(6)?;
        hooks.checkpoint(ProductionFailurePoint::SecurityCapabilitiesAttribute)?;
        attributes.update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            &security_capabilities,
        )?;
        hooks.checkpoint(ProductionFailurePoint::LpacOptOutAttribute)?;
        attributes.update(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            &all_packages_policy,
        )?;
        hooks.checkpoint(ProductionFailurePoint::JobListAttribute)?;
        attributes.update_slice(PROC_THREAD_ATTRIBUTE_JOB_LIST, &job_handles)?;
        hooks.checkpoint(ProductionFailurePoint::ChildProcessPolicyAttribute)?;
        attributes.update(
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
            &child_process_policy,
        )?;
        hooks.checkpoint(ProductionFailurePoint::MitigationPolicyAttribute)?;
        attributes.update(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigation_policy)?;
        hooks.checkpoint(ProductionFailurePoint::HandleListAttribute)?;
        attributes.update_slice(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited_handles)?;

        let executable_wide = wide_null(executable.as_os_str())?;
        let child_directory = executable
            .parent()
            .ok_or_else(|| GateError("worker executable has no parent directory".to_string()))?;
        let child_directory_wide = wide_null(child_directory.as_os_str())?;
        let mut command_line = production_command_line(&executable, hooks.child)?;
        let environment = production_environment_block(&hooks)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited_handles[0];
        startup.StartupInfo.hStdOutput = inherited_handles[1];
        startup.StartupInfo.hStdError = inherited_handles[2];
        startup.lpAttributeList = attributes.pointer;
        let mut process_information = PROCESS_INFORMATION::default();

        hooks.checkpoint(ProductionFailurePoint::CreateProcess)?;
        hooks.require_before_deadline()?;
        // SAFETY: all UTF-16 buffers are NUL-terminated and remain live; command_line is mutable
        // as required. STARTUPINFOEX and all six attribute values remain initialized until after
        // CreateProcessW. TRUE is required for HANDLE_LIST, whose exact three inheritable pipe
        // handles are the only handles admitted. JOB_LIST assigns the configured Job before the
        // first instruction. Returned process/thread handles transfer immediately to RAII owners.
        if unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                TRUE,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS,
                environment.as_ptr().cast(),
                child_directory_wide.as_ptr(),
                &startup.StartupInfo,
                &mut process_information,
            )
        } == 0
        {
            return Err(last_error("create zero-capability LPAC JavaScript worker"));
        }
        #[cfg(test)]
        if let Err(error) = probe_canary_inheritance.clear() {
            close_unowned_handle(process_information.hProcess);
            close_unowned_handle(process_information.hThread);
            return Err(error);
        }
        #[cfg(test)]
        LAST_PRODUCTION_TEST_PID.store(
            process_information.dwProcessId,
            std::sync::atomic::Ordering::Release,
        );

        if let Err(error) = hooks.checkpoint(ProductionFailurePoint::OwnProcessHandle) {
            close_unowned_handle(process_information.hProcess);
            close_unowned_handle(process_information.hThread);
            return Err(error);
        }
        let process = match WinHandle::from_created(
            process_information.hProcess,
            "own LPAC worker process handle",
        ) {
            Ok(process) => process,
            Err(error) => {
                close_unowned_handle(process_information.hThread);
                return Err(error.into());
            }
        };
        if let Err(error) = hooks.checkpoint(ProductionFailurePoint::OwnThreadHandle) {
            close_unowned_handle(process_information.hThread);
            return Err(error);
        }
        let thread = WinHandle::from_created(
            process_information.hThread,
            "own LPAC worker initial-thread handle",
        )?;

        // Only the three child endpoints were inheritable during CreateProcessW. Clear each bit
        // immediately after creation, before any fallible membership verification or handoff.
        hooks.checkpoint(ProductionFailurePoint::ClearStdinInheritance)?;
        pipes.child_input.clear_inherit()?;
        hooks.checkpoint(ProductionFailurePoint::ClearStdoutInheritance)?;
        pipes.child_output.clear_inherit()?;
        hooks.checkpoint(ProductionFailurePoint::ClearStderrInheritance)?;
        pipes.child_error.clear_inherit()?;
        drop(inheritance_guard);
        drop(thread);

        hooks.checkpoint(ProductionFailurePoint::VerifyCreationTimeJob)?;
        let mut in_creation_job = 0;
        // SAFETY: both directly owned handles are live. This verifies the exact Job supplied in
        // JOB_LIST; a nested-Job or attribute incompatibility must already have failed creation.
        if unsafe { IsProcessInJob(process.raw(), job.raw(), &mut in_creation_job) } == 0 {
            return Err(last_error("verify LPAC worker creation-time Job"));
        }
        if in_creation_job == 0 {
            return Err(GateError(
                "LPAC worker escaped its requested creation-time Job".to_string(),
            ));
        }

        let ProtocolPipes {
            parent_input,
            parent_output,
            parent_error,
            child_input,
            child_output,
            child_error,
        } = pipes;
        drop(child_input);
        drop(child_output);
        drop(child_error);
        drop(attributes);
        drop(image_lock);
        drop(profile);

        Ok(WorkerProcess {
            process: WorkerChild::contained(process, job, process_information.dwProcessId),
            input: parent_input.into_file(),
            output: parent_output.into_file(),
            stderr: parent_error.into_file(),
            backend: WorkerBackend::WindowsLpac,
            #[cfg(test)]
            reap_observer: None,
            #[cfg(test)]
            force_tree_termination_error: false,
            #[cfg(test)]
            authenticated_ready_observer: None,
            #[cfg(test)]
            force_authenticated_ready_finalization_error: false,
            #[cfg(test)]
            parent_write_observer: None,
        })
    }

    enum RuntimePreflightTarget {
        CurrentExecutable,
        #[cfg(test)]
        Installed(PathBuf),
    }

    fn run_runtime_preflight(target: RuntimePreflightTarget) -> Result<(), GateError> {
        let deadline = Instant::now() + PRODUCTION_PREFLIGHT_TIMEOUT;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("mini-agent-windows-preflight".to_string())
            .spawn(move || {
                let hooks = match target {
                    RuntimePreflightTarget::CurrentExecutable => {
                        ProductionLaunchHooks::production()
                    }
                    #[cfg(test)]
                    RuntimePreflightTarget::Installed(executable) => {
                        ProductionLaunchHooks::installed_worker(executable)
                    }
                }
                .with_deadline(deadline);
                // The helper retains sole ownership of any late worker result. Even if the
                // the caller's wait deadline wins during ACL work, creation-lock wait, or a
                // synchronous CreateProcessW call, a worker produced later is terminated and
                // reaped below before this helper exits. CreateProcessW itself is not cancellable.
                let _ = sender.send(run_runtime_preflight_owned(hooks, deadline));
            })
            .map_err(|error| GateError(format!("start Windows preflight helper: {error}")))?;

        let remaining = deadline.saturating_duration_since(Instant::now());
        receiver.recv_timeout(remaining).map_err(|_| {
            GateError(
                "Windows production runtime preflight exceeded its caller wait deadline"
                    .to_string(),
            )
        })?
    }

    fn run_runtime_preflight_owned(
        hooks: ProductionLaunchHooks,
        deadline: Instant,
    ) -> Result<(), GateError> {
        let mut process = launch_production(hooks)?;
        let result = (|| {
            if Instant::now() >= deadline {
                return Err(GateError(
                    "Windows production worker launch completed after its caller deadline"
                        .to_string(),
                ));
            }
            process.process.runtime_controls_match()?;
            run_authenticated_round_trip(&mut process, |process| {
                read_worker_frame_exact_bounded(process, deadline)
            })?;
            let status = wait_for_protocol_worker_exit_until(&mut process, deadline)?;
            if !status.success() {
                return Err(GateError(
                    "Windows production protocol worker exited unsuccessfully".to_string(),
                ));
            }
            Ok(())
        })();

        if result.is_err() {
            let _ = process.terminate_and_reap(PRODUCTION_PREFLIGHT_REAP_TIMEOUT);
        }
        result
    }

    fn run_authenticated_round_trip(
        process: &mut WorkerProcess,
        mut read_worker_frame: impl FnMut(
            &mut WorkerProcess,
        ) -> Result<
            crate::extras::js::protocol::WorkerWireFrame,
            GateError,
        >,
    ) -> Result<(), GateError> {
        use crate::extras::js::protocol::{
            BuildIdentity, ContainmentAttestation, ContainmentProbe, InvocationId, ParentFrame,
            ParentProtocol, RunStep, StepOutcome, WireFrame, WorkerFrame, write_frame,
        };

        let build = BuildIdentity::current();
        let mut protocol = ParentProtocol::new(build.clone());
        let hello = WireFrame::connection(build.clone(), 0, ParentFrame::Hello(protocol.hello()));
        protocol
            .on_send(&hello)
            .map_err(|error| GateError(format!("validate Windows Hello: {error}")))?;
        write_frame(&mut process.input, &hello)
            .map_err(|error| GateError(format!("write Windows Hello: {error}")))?;
        process
            .input
            .flush()
            .map_err(|error| GateError(format!("flush Windows Hello: {error}")))?;
        let ready = read_worker_frame(process)?;
        protocol
            .on_receive(&ready)
            .map_err(|error| GateError(format!("validate Windows Ready: {error}")))?;
        if !matches!(ready.message, WorkerFrame::Ready(_)) {
            return Err(GateError("Windows worker did not emit Ready".to_string()));
        }
        process
            .finalize_authenticated_ready()
            .map_err(|error| GateError(format!("finalize authenticated Windows Ready: {error}")))?;

        let containment = WireFrame::connection(
            build.clone(),
            2,
            ParentFrame::ContainmentProbe(ContainmentProbe {}),
        );
        protocol
            .on_send(&containment)
            .map_err(|error| GateError(format!("validate Windows containment probe: {error}")))?;
        write_frame(&mut process.input, &containment)
            .map_err(|error| GateError(format!("write Windows containment probe: {error}")))?;
        process
            .input
            .flush()
            .map_err(|error| GateError(format!("flush Windows containment probe: {error}")))?;
        let attestation = read_worker_frame(process)?;
        protocol.on_receive(&attestation).map_err(|error| {
            GateError(format!("validate Windows containment attestation: {error}"))
        })?;
        if !matches!(
            attestation.message,
            WorkerFrame::ContainmentAttested(ContainmentAttestation::Passed)
        ) {
            return Err(GateError(
                "Windows worker emitted no closed containment attestation".to_string(),
            ));
        }

        let invocation = InvocationId::new("windows-containment-protocol")
            .map_err(|error| GateError(format!("construct Windows invocation: {error}")))?;
        let step = WireFrame::invocation(
            build.clone(),
            invocation,
            4,
            ParentFrame::RunStep(RunStep::new("6 * 7".to_string())),
        );
        protocol
            .on_send(&step)
            .map_err(|error| GateError(format!("validate Windows RunStep: {error}")))?;
        write_frame(&mut process.input, &step)
            .map_err(|error| GateError(format!("write Windows RunStep: {error}")))?;
        process
            .input
            .flush()
            .map_err(|error| GateError(format!("flush Windows RunStep: {error}")))?;
        let result = read_worker_frame(process)?;
        protocol
            .on_receive(&result)
            .map_err(|error| GateError(format!("validate Windows StepResult: {error}")))?;
        let WorkerFrame::StepResult(result) = result.message else {
            return Err(GateError(
                "Windows worker returned no StepResult".to_string(),
            ));
        };
        if result.outcome != StepOutcome::Value("42".to_string()) {
            return Err(GateError(
                "Windows worker protocol evaluation returned the wrong value".to_string(),
            ));
        }

        let shutdown = WireFrame::connection(build, 6, ParentFrame::Shutdown);
        protocol
            .on_send(&shutdown)
            .map_err(|error| GateError(format!("validate Windows Shutdown: {error}")))?;
        write_frame(&mut process.input, &shutdown)
            .map_err(|error| GateError(format!("write Windows Shutdown: {error}")))?;
        process
            .input
            .flush()
            .map_err(|error| GateError(format!("flush Windows Shutdown: {error}")))?;
        Ok(())
    }

    fn pipe_bytes_available(pipe: &File) -> Result<usize, GateError> {
        let mut available = 0u32;
        // SAFETY: the anonymous-pipe read handle remains owned by `pipe`; null buffer arguments
        // request only the currently buffered byte count and are not retained by PeekNamedPipe.
        if unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle() as HANDLE,
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error("inspect Windows worker protocol pipe"));
        }
        Ok(available as usize)
    }

    fn wait_for_pipe_bytes(
        process: &mut WorkerProcess,
        required: usize,
        deadline: Instant,
    ) -> Result<(), GateError> {
        loop {
            if pipe_bytes_available(&process.output)? >= required {
                return Ok(());
            }
            if process
                .try_wait()
                .map_err(|error| GateError(format!("poll Windows protocol worker: {error}")))?
                .is_some()
            {
                return Err(GateError(
                    "Windows protocol worker exited before emitting a complete frame".to_string(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(GateError(
                    "Windows worker protocol read timed out".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read_worker_frame_exact_bounded(
        process: &mut WorkerProcess,
        deadline: Instant,
    ) -> Result<crate::extras::js::protocol::WorkerWireFrame, GateError> {
        wait_for_pipe_bytes(process, 4, deadline)?;
        let mut header = [0u8; 4];
        process
            .output
            .read_exact(&mut header)
            .map_err(|error| GateError(format!("read Windows worker frame header: {error}")))?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > crate::extras::js::protocol::MAX_FRAME_BYTES {
            return Err(GateError(
                "Windows worker emitted an invalid protocol frame length".to_string(),
            ));
        }
        wait_for_pipe_bytes(process, length, deadline)?;
        let mut encoded = Vec::with_capacity(4 + length);
        encoded.extend_from_slice(&header);
        encoded.resize(4 + length, 0);
        process
            .output
            .read_exact(&mut encoded[4..])
            .map_err(|error| GateError(format!("read Windows worker frame payload: {error}")))?;
        crate::extras::js::protocol::read_frame(&mut encoded.as_slice())
            .map_err(|error| GateError(format!("decode Windows worker frame: {error}")))
    }

    fn wait_for_protocol_worker_exit_until(
        process: &mut WorkerProcess,
        deadline: Instant,
    ) -> Result<ExitStatus, GateError> {
        loop {
            if let Some(status) = process
                .try_wait()
                .map_err(|error| GateError(format!("poll Windows protocol worker: {error}")))?
            {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(GateError(
                    "Windows protocol worker did not exit before the preflight deadline"
                        .to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(test)]
    pub(super) static LAST_PRODUCTION_TEST_PID: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);

    pub(super) fn run_artifact_matrix() -> Result<(), GateError> {
        let profile = AppContainerProfile::stable_zero_capability()?;
        let result = (|| {
            let policy = SidPolicy::current()?;
            for specification in artifact_matrix()? {
                let source_lock = validate_source_artifact(
                    &specification.source,
                    specification.source_expected,
                    &policy,
                )?;
                let artifact = DisposableArtifact::copy_into(
                    &specification.source,
                    source_lock,
                    specification.directory,
                    specification.destination_expected,
                    specification.probe,
                )?;
                eprintln!(
                    "LPAC artifact destination: {:?}; evidence: {:?}",
                    artifact.destination_expected, artifact.probe
                );
                let probe = (|| {
                    let (executable, location, image_lock) =
                        prepare_executable_acl(&artifact.executable, profile.sid, &policy)?;
                    if location != artifact.destination_expected {
                        return Err(GateError(
                            "artifact location changed after canonicalization".to_string(),
                        ));
                    }
                    let result = launch_and_probe(&executable, profile.sid, artifact.probe);
                    drop(image_lock);
                    result
                })();
                let cleanup = artifact.cleanup();
                match (probe, cleanup) {
                    (Ok(()), Ok(())) => {}
                    (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                    (Err(probe), Err(cleanup)) => {
                        return Err(GateError(format!(
                            "{probe}; disposable artifact cleanup also failed: {cleanup}"
                        )));
                    }
                }
            }
            Ok(())
        })();
        let cleanup = profile.finish();
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(gate), Err(cleanup)) => Err(GateError(format!(
                "{gate}; AppContainer profile cleanup also failed: {cleanup}"
            ))),
        }
    }

    pub(super) fn run_protected_install_negative_control() -> Result<(), GateError> {
        let profile = AppContainerProfile::stable_zero_capability()?;
        let result = (|| {
            let policy = SidPolicy::current()?;
            verify_protected_install_fails_closed(profile.sid, &policy)
        })();
        let cleanup = profile.finish();
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(gate), Err(cleanup)) => Err(GateError(format!(
                "{gate}; AppContainer profile cleanup also failed: {cleanup}"
            ))),
        }
    }

    fn verify_protected_install_fails_closed(
        appcontainer_sid: PSID,
        policy: &SidPolicy,
    ) -> Result<(), GateError> {
        let protected = std::env::var_os(PROTECTED_EXE_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| {
                GateError(format!(
                    "{PROTECTED_EXE_ENV} must name the protected machine-wide negative-control image"
                ))
            })?;
        reject_unc_or_remote_syntax(&protected)?;
        reject_reparse_components(&protected)?;
        let protected = std::fs::canonicalize(&protected).map_err(|error| {
            GateError(format!(
                "canonicalize protected machine-wide negative control: {error}"
            ))
        })?;
        if classify_install_location(&protected) != InstallLocation::ProtectedMachineWide {
            return Err(GateError(format!(
                "{PROTECTED_EXE_ENV} is not beneath a protected machine-wide root"
            )));
        }
        let before = verify_current_user_cannot_modify(&protected, policy, true)?;
        let parent = protected.parent().ok_or_else(|| {
            GateError("protected machine-wide control has no parent directory".to_string())
        })?;
        verify_current_user_cannot_modify(parent, policy, false)?;
        match OpenOptions::new().write(true).open(&protected) {
            Ok(_) => {
                return Err(GateError(
                    "protected negative control accepted a current-user write handle".to_string(),
                ));
            }
            Err(error) if access_was_denied(&error) => {}
            Err(error) => {
                return Err(GateError(format!(
                    "protected negative-control write probe failed ambiguously: {error}"
                )));
            }
        }
        let error = prepare_executable_acl(&protected, appcontainer_sid, policy)
            .expect_err("protected machine-wide image must fail closed before ACL mutation");
        if !error.0.contains("ProtectedMachineWide") {
            return Err(GateError(format!(
                "protected machine-wide image failed for an unrelated reason: {error}"
            )));
        }
        let after = snapshot_file_security(&read_file_security(&protected)?)?;
        if before != after {
            return Err(GateError(
                "protected machine-wide negative-control owner or DACL changed".to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn run_production_containment_probe() -> Result<(), GateError> {
        let _nested_parent_job = ensure_compatible_parent_job()?;

        for specification in artifact_matrix()? {
            let policy = SidPolicy::current()?;
            let source_lock = validate_source_artifact(
                &specification.source,
                specification.source_expected,
                &policy,
            )?;
            let artifact = DisposableArtifact::copy_into(
                &specification.source,
                source_lock,
                specification.directory,
                specification.destination_expected,
                specification.probe,
            )?;
            let probe = match artifact.probe {
                ProbeKind::Harness => {
                    run_single_production_containment(artifact.executable.clone()).and_then(|()| {
                        run_production_protocol_round_trip(ProductionLaunchHooks::protocol_test(
                            artifact.executable.clone(),
                        ))
                    })
                }
                ProbeKind::ImageLoadingOnly => run_production_protocol_round_trip(
                    ProductionLaunchHooks::installed_worker(artifact.executable.clone()),
                ),
            };
            let cleanup = artifact.cleanup();
            match (probe, cleanup) {
                (Ok(()), Ok(())) => {}
                (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                (Err(probe), Err(cleanup)) => {
                    return Err(GateError(format!(
                        "{probe}; disposable artifact cleanup also failed: {cleanup}"
                    )));
                }
            }
        }

        eprintln!(
            "WINDOWS_CONTAINMENT_PASS backend=lpac job_close=pass nested_parent_job=pass protocol=pass"
        );
        Ok(())
    }

    #[cfg(test)]
    fn run_single_production_containment(executable: PathBuf) -> Result<(), GateError> {
        let workspace = Sentinel::workspace_file()?;
        let skill_database = Sentinel::skill_database_file()?;
        let file_canary = File::open(&workspace.0)
            .map_err(|error| GateError(format!("open omitted file-handle canary: {error}")))?;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| GateError(format!("bind TCP denial canary: {error}")))?;
        let udp_listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| GateError(format!("bind UDP denial canary: {error}")))?;
        let tcp_port = tcp_listener
            .local_addr()
            .map_err(|error| GateError(format!("read TCP denial port: {error}")))?
            .port();
        let udp_port = udp_listener
            .local_addr()
            .map_err(|error| GateError(format!("read UDP denial port: {error}")))?
            .port();
        let configuration = ContainmentProbeConfiguration {
            workspace: workspace.0.clone(),
            skill_database: skill_database.0.clone(),
            file_handle: file_canary.as_raw_handle(),
            socket_handle: tcp_listener.as_raw_socket() as HANDLE,
            file_canary: Some(file_canary),
            socket_canary: Some(tcp_listener),
            tcp_port,
            udp_port,
        };

        let mut process = launch_production(ProductionLaunchHooks::containment(
            configuration,
            executable,
        ))?;
        let readiness = process
            .output
            .try_clone()
            .map_err(|error| GateError(format!("clone containment readiness pipe: {error}")))?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            sender.send(read_fixed_containment_ready(readiness)).ok();
        });
        let readiness = receiver
            .recv_timeout(CHILD_TIMEOUT)
            .map_err(|_| GateError("Windows containment readiness timed out".to_string()))??;
        if readiness != CONTAINMENT_READY {
            return Err(GateError(
                "Windows containment child emitted no fixed pass frame".to_string(),
            ));
        }
        reader
            .join()
            .map_err(|_| GateError("Windows containment readiness reader panicked".to_string()))?;

        process.process.runtime_controls_match()?;
        process.process.close_job_for_probe()?;
        let status = wait_for_worker_exit_after_job_close(&mut process)?;
        let job_close_kills_worker = !status.success();
        if !job_close_kills_worker {
            return Err(GateError(
                "closing the kill-on-close Job did not terminate the worker".to_string(),
            ));
        }

        Ok(())
    }

    #[cfg(test)]
    fn ensure_compatible_parent_job() -> Result<Option<WinHandle>, GateError> {
        let mut in_job = 0;
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle, null queries any Job, and
        // the initialized BOOL output is live for the synchronous call.
        if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) } == 0 {
            return Err(last_error("query nested parent Job membership"));
        }
        if in_job != 0 {
            return Ok(None);
        }

        let job = WinHandle::from_created(
            // SAFETY: null attributes and name create one private Job whose sole owned handle is
            // transferred immediately to WinHandle.
            unsafe { CreateJobObjectW(null(), null()) },
            "create compatible outer containment-probe Job",
        )?;
        // SAFETY: the Job is directly owned and live, while GetCurrentProcess returns the valid
        // borrowed pseudo-handle for this test process. The call retains the process membership,
        // not either handle value.
        if unsafe { AssignProcessToJobObject(job.raw(), GetCurrentProcess()) } == 0 {
            return Err(last_error(
                "assign containment-probe parent to compatible outer Job",
            ));
        }
        let mut in_exact_job = 0;
        // SAFETY: both handles remain live and the initialized BOOL is writable for the call.
        if unsafe { IsProcessInJob(GetCurrentProcess(), job.raw(), &mut in_exact_job) } == 0 {
            return Err(last_error("verify compatible outer Job membership"));
        }
        if in_exact_job == 0 {
            return Err(GateError(
                "containment-probe parent escaped its compatible outer Job".to_string(),
            ));
        }
        Ok(Some(job))
    }

    #[cfg(test)]
    fn read_fixed_containment_ready(output: File) -> Result<&'static [u8], GateError> {
        let mut reader = BufReader::new(output);
        let mut observed = 0usize;
        loop {
            let mut line = Vec::new();
            let read = reader
                .read_until(b'\n', &mut line)
                .map_err(|error| GateError(format!("read containment readiness: {error}")))?;
            if read == 0 {
                return Err(GateError(
                    "Windows containment child exited before readiness".to_string(),
                ));
            }
            observed = observed.saturating_add(read);
            if observed > 64 * 1024 {
                return Err(GateError(
                    "Windows containment readiness preamble exceeded 64 KiB".to_string(),
                ));
            }
            if line
                .windows(CONTAINMENT_READY.len())
                .any(|window| window == CONTAINMENT_READY)
            {
                return Ok(CONTAINMENT_READY);
            }
        }
    }

    #[cfg(test)]
    fn wait_for_worker_exit_after_job_close(
        process: &mut WorkerProcess,
    ) -> Result<ExitStatus, GateError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = process
                .try_wait()
                .map_err(|error| GateError(format!("poll Job-close worker exit: {error}")))?
            {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                return Err(GateError(
                    "Job close did not reap the Windows worker within five seconds".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(test)]
    fn read_worker_frame_after_preamble(
        input: &mut impl Read,
    ) -> Result<crate::extras::js::protocol::WorkerWireFrame, GateError> {
        let mut preamble = Vec::new();
        let mut window = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            input
                .read_exact(&mut byte)
                .map_err(|error| GateError(format!("read Windows worker frame: {error}")))?;
            window.push(byte[0]);
            if window.len() < 5 {
                continue;
            }
            let length =
                u32::from_be_bytes(window[..4].try_into().expect("four-byte window")) as usize;
            if length > 0
                && length <= crate::extras::js::protocol::MAX_FRAME_BYTES
                && window[4] == b'{'
            {
                let mut encoded = window[..5].to_vec();
                let mut tail = vec![0u8; length - 1];
                input.read_exact(&mut tail).map_err(|error| {
                    GateError(format!("read Windows worker frame payload: {error}"))
                })?;
                encoded.extend_from_slice(&tail);
                if let Ok(frame) = crate::extras::js::protocol::read_frame(&mut encoded.as_slice())
                {
                    return Ok(frame);
                }
            }
            preamble.push(window.remove(0));
            if preamble.len() > 4096 {
                return Err(GateError(
                    "Windows worker emitted an unbounded libtest preamble".to_string(),
                ));
            }
        }
    }

    #[cfg(test)]
    fn read_worker_frame_bounded(
        output: &File,
    ) -> Result<crate::extras::js::protocol::WorkerWireFrame, GateError> {
        let mut output = output
            .try_clone()
            .map_err(|error| GateError(format!("clone Windows worker output pipe: {error}")))?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            sender
                .send(read_worker_frame_after_preamble(&mut output))
                .ok();
        });
        let frame = receiver
            .recv_timeout(CHILD_TIMEOUT)
            .map_err(|_| GateError("Windows worker protocol read timed out".to_string()))??;
        reader
            .join()
            .map_err(|_| GateError("Windows worker protocol reader panicked".to_string()))?;
        Ok(frame)
    }

    #[cfg(test)]
    fn run_production_protocol_round_trip(hooks: ProductionLaunchHooks) -> Result<(), GateError> {
        let mut process = launch_production(hooks)?;
        process.process.runtime_controls_match()?;
        run_authenticated_round_trip(&mut process, |process| {
            read_worker_frame_bounded(&process.output)
        })?;
        let status = wait_for_protocol_worker_exit(&mut process)?;
        if !status.success() {
            return Err(GateError(
                "Windows production protocol worker exited unsuccessfully".to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn wait_for_protocol_worker_exit(process: &mut WorkerProcess) -> Result<ExitStatus, GateError> {
        wait_for_protocol_worker_exit_until(process, Instant::now() + Duration::from_secs(5))
    }

    fn launch_and_probe(
        executable: &Path,
        appcontainer_sid: PSID,
        probe: ProbeKind,
    ) -> Result<(), GateError> {
        let sentinel = Sentinel::workspace_file()?;
        let inheritance_guard = crate::process_creation::creation_guard()?;
        let mut pipes = ProtocolPipes::exact_anonymous_set(&inheritance_guard)?;
        // Both canary endpoints are deliberately inheritable but absent from
        // HANDLE_LIST. The child receives only the numeric value and must prove
        // it is invalid, demonstrating that the allow-list excluded ambient
        // inheritable handles rather than merely listing the intended three.
        let (mut canary_read, mut canary_write) = inheritable_pipe(&inheritance_guard)?;
        let job = temporary_job()?;
        let security_capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: appcontainer_sid,
            Capabilities: null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let inherited_handles = pipes.child_handles();
        let job_handles = [job.raw()];
        let all_packages_policy = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
        let mut attributes = AttributeList::new(4)?;
        attributes.update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            &security_capabilities,
        )?;
        attributes.update_slice(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited_handles)?;
        attributes.update_slice(PROC_THREAD_ATTRIBUTE_JOB_LIST, &job_handles)?;
        attributes.update(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            &all_packages_policy,
        )?;

        let executable_wide = wide_null(executable.as_os_str())?;
        let child_directory = executable
            .parent()
            .ok_or_else(|| GateError("executable has no launch directory".to_string()))?;
        let child_directory_wide = wide_null(child_directory.as_os_str())?;
        let mut command_line = command_line(executable, probe)?;
        let environment = environment_block(&sentinel.0, canary_read.raw())?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited_handles[0];
        startup.StartupInfo.hStdOutput = inherited_handles[1];
        startup.StartupInfo.hStdError = inherited_handles[2];
        startup.lpAttributeList = attributes.pointer;
        let mut process_information = PROCESS_INFORMATION::default();

        // SAFETY: application/current-directory/environment/command buffers are
        // NUL-terminated and live through the call; command_line is mutable as
        // required by CreateProcessW. STARTUPINFOEX and all four attribute
        // values (security capabilities, exact three-handle list, one Job, and
        // LPAC opt-out policy) remain initialized and alive. The returned
        // process/thread handles are transferred immediately to OwnedHandle.
        if unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                TRUE,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS,
                environment.as_ptr().cast(),
                child_directory_wide.as_ptr(),
                &startup.StartupInfo,
                &mut process_information,
            )
        } == 0
        {
            return Err(last_error("create zero-capability LPAC child"));
        }
        let process = match WinHandle::from_created(
            process_information.hProcess,
            "own LPAC process handle",
        ) {
            Ok(process) => process,
            Err(error) => {
                close_unowned_handle(process_information.hThread);
                return Err(error.into());
            }
        };
        let thread =
            WinHandle::from_created(process_information.hThread, "own LPAC thread handle")?;
        pipes.clear_child_inheritance()?;
        canary_read.clear_inherit()?;
        canary_write.clear_inherit()?;
        drop(inheritance_guard);
        drop(thread);
        drop(attributes);

        let mut in_creation_job = 0;
        // SAFETY: both process and Job handles are live and owned. This checks
        // the exact Job supplied in PROC_THREAD_ATTRIBUTE_JOB_LIST, not merely
        // whether an ambient parent Job exists.
        if unsafe { IsProcessInJob(process.raw(), job.raw(), &mut in_creation_job) } == 0 {
            return Err(last_error(
                "verify LPAC process creation-time Job membership",
            ));
        }
        if in_creation_job == 0 {
            return Err(GateError(
                "LPAC process was not created in the requested Job".to_string(),
            ));
        }

        let ProtocolPipes {
            parent_input,
            parent_output,
            parent_error,
            child_input,
            child_output,
            child_error,
        } = pipes;
        drop(child_input);
        drop(child_output);
        drop(child_error);
        drop(parent_input);
        drop(canary_read);
        drop(canary_write);

        // Drain both child streams while waiting so libtest diagnostics cannot
        // fill an anonymous-pipe buffer and prevent process exit. Diagnostics
        // are retained only to a fixed bound, while excess bytes are discarded.
        let output_reader = drain_pipe(parent_output, 64 * 1024, "LPAC readiness pipe");
        let diagnostics_reader = drain_pipe(parent_error, 64 * 1024, "LPAC diagnostics pipe");
        let exit_code = match wait_for_child(&process) {
            Ok(exit_code) => exit_code,
            Err(error) => {
                // Closing a kill-on-close Job is the bounded failure cleanup;
                // it also closes the child's pipe ends so both readers finish.
                drop(job);
                let _ = join_pipe(output_reader, "LPAC readiness");
                let _ = join_pipe(diagnostics_reader, "LPAC diagnostics");
                return Err(error);
            }
        };
        let output = join_pipe(output_reader, "LPAC readiness")?;
        let diagnostics = join_pipe(diagnostics_reader, "LPAC diagnostics")?;

        match probe {
            ProbeKind::Harness => {
                if output
                    .windows(READY_OPENED.len())
                    .any(|bytes| bytes == READY_OPENED)
                {
                    return Err(GateError(
                        "LPAC child opened the workspace sentinel".to_string(),
                    ));
                }
                if !output
                    .windows(READY_DENIED.len())
                    .any(|bytes| bytes == READY_DENIED)
                {
                    return Err(GateError(format!(
                        "LPAC child emitted no fixed readiness frame (exit {exit_code}, stderr {} bytes)",
                        diagnostics.len()
                    )));
                }
            }
            ProbeKind::ImageLoadingOnly => {
                if !output
                    .windows(b"mini-agent".len())
                    .any(|bytes| bytes == b"mini-agent")
                {
                    return Err(GateError(format!(
                        "installed/archive image emitted no version identity (exit {exit_code}, stderr {} bytes)",
                        diagnostics.len()
                    )));
                }
            }
        }
        if exit_code != 0 {
            return Err(GateError(format!(
                "LPAC child exited with {exit_code} after readiness"
            )));
        }

        drop(process);
        drop(job);
        Ok(())
    }

    fn token_u32(token: HANDLE, class: i32, label: &str) -> Result<u32, GateError> {
        let mut value = 0u32;
        let mut returned = 0u32;
        if unsafe {
            GetTokenInformation(
                token,
                class,
                (&mut value as *mut u32).cast(),
                size_of::<u32>() as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(last_error(label));
        }
        if returned != size_of::<u32>() as u32 {
            return Err(GateError(format!("{label}: unexpected token value size")));
        }
        Ok(value)
    }

    fn child_token_is_zero_capability_lpac() -> Result<bool, GateError> {
        let mut raw_token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
            return Err(last_error("open LPAC child token"));
        }
        let token = WinHandle::from_created(raw_token, "own LPAC child token")?;
        if token_u32(token.raw(), TokenIsAppContainer, "read TokenIsAppContainer")? != 1
            || token_u32(
                token.raw(),
                TokenIsLessPrivilegedAppContainer,
                "read TokenIsLessPrivilegedAppContainer",
            )? != 1
        {
            return Ok(false);
        }

        let mut required = 0u32;
        let first = unsafe {
            GetTokenInformation(token.raw(), TokenCapabilities, null_mut(), 0, &mut required)
        };
        if first != 0 || required < size_of::<u32>() as u32 {
            return Err(GateError(
                "invalid TokenCapabilities size probe".to_string(),
            ));
        }
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(last_error("size TokenCapabilities"));
        }
        let mut storage = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.raw(),
                TokenCapabilities,
                storage.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(last_error("read TokenCapabilities"));
        }
        let group_count = unsafe { *storage.as_ptr().cast::<u32>() };
        Ok(group_count == 0)
    }

    fn no_console_devices() -> bool {
        if !unsafe { GetConsoleWindow() }.is_null() || unsafe { GetConsoleCP() } != 0 {
            return false;
        }
        for device in ["CONIN$", "CONOUT$"] {
            let device = wide_string(device);
            let handle = unsafe {
                CreateFileW(
                    device.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ,
                    null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    null_mut(),
                )
            };
            if !handle.is_null() && handle != (-1isize as HANDLE) {
                close_unowned_handle(handle);
                return false;
            }
        }
        true
    }

    fn exact_protocol_std_handles() -> bool {
        let handles = unsafe {
            [
                GetStdHandle(STD_INPUT_HANDLE),
                GetStdHandle(STD_OUTPUT_HANDLE),
                GetStdHandle(STD_ERROR_HANDLE),
            ]
        };
        if handles
            .iter()
            .any(|handle| handle.is_null() || *handle == (-1isize as HANDLE))
            || handles[0] == handles[1]
            || handles[0] == handles[2]
            || handles[1] == handles[2]
        {
            return false;
        }
        handles.iter().all(|handle| {
            let mut flags = 0u32;
            (unsafe { GetHandleInformation(*handle, &mut flags) }) != 0
        }) && super::standard_streams_are_protocol_pipes()
    }

    fn access_was_denied(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(5 | 10013))
    }

    fn inherited_handle_is_invalid(name: &str) -> bool {
        let Ok(value) = std::env::var(name) else {
            return false;
        };
        let Ok(value) = value.parse::<usize>() else {
            return false;
        };
        let mut flags = 0u32;
        // SAFETY: `flags` is an initialized output slot. The numeric handle was supplied by the
        // test parent and deliberately omitted from HANDLE_LIST; no ownership is assumed.
        (unsafe { GetHandleInformation(value as HANDLE, &mut flags) }) == 0
            // SAFETY: GetLastError is read immediately after the failed query and has no pointer
            // or ownership effects.
            && unsafe { GetLastError() } == ERROR_INVALID_HANDLE
    }

    pub(super) fn attest_containment(
        _probe: &crate::extras::js::protocol::ContainmentProbe,
    ) -> bool {
        child_token_is_zero_capability_lpac().unwrap_or(false)
            && exact_protocol_std_handles()
            && no_console_devices()
    }

    fn mitigation_policy_matches_for_handle(process: HANDLE) -> bool {
        fn query<T: Default>(process: HANDLE, policy: i32) -> Option<T> {
            let mut value = T::default();
            // SAFETY: `process` is a borrowed live process handle and `value` is an initialized
            // output buffer of the exact type/size requested for this synchronous query. The
            // function retains neither the handle nor the pointer.
            if unsafe {
                GetProcessMitigationPolicy(
                    process,
                    policy,
                    (&mut value as *mut T).cast(),
                    size_of::<T>(),
                )
            } == 0
            {
                return None;
            }
            Some(value)
        }

        let Some(aslr) = query::<PROCESS_MITIGATION_ASLR_POLICY>(process, ProcessASLRPolicy) else {
            return false;
        };
        let Some(extension) = query::<PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY>(
            process,
            ProcessExtensionPointDisablePolicy,
        ) else {
            return false;
        };
        let Some(image) =
            query::<PROCESS_MITIGATION_IMAGE_LOAD_POLICY>(process, ProcessImageLoadPolicy)
        else {
            return false;
        };
        let Some(dynamic_code) =
            query::<PROCESS_MITIGATION_DYNAMIC_CODE_POLICY>(process, ProcessDynamicCodePolicy)
        else {
            return false;
        };
        let Some(system_calls) = query::<PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY>(
            process,
            ProcessSystemCallDisablePolicy,
        ) else {
            return false;
        };
        // SAFETY: these are the documented `Flags` members of initialized union values returned
        // by GetProcessMitigationPolicy.
        let aslr = unsafe { aslr.Anonymous.Flags };
        // SAFETY: same initialized-union argument as above.
        let extension = unsafe { extension.Anonymous.Flags };
        // SAFETY: same initialized-union argument as above.
        let image = unsafe { image.Anonymous.Flags };
        // SAFETY: same initialized-union argument as above.
        let dynamic_code = unsafe { dynamic_code.Anonymous.Flags };
        // SAFETY: same initialized-union argument as above.
        let system_calls = unsafe { system_calls.Anonymous.Flags };
        aslr & 0b111 == 0b111
            && extension & 0b1 == 0b1
            && image & 0b111 == 0b111
            && dynamic_code & 0b1 == 0
            && system_calls & 0b1 == 0
    }

    fn mitigation_policy_matches() -> bool {
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle valid in this process.
        mitigation_policy_matches_for_handle(unsafe { GetCurrentProcess() })
    }

    fn child_process_policy_matches(process: HANDLE) -> bool {
        let mut policy = PROCESS_MITIGATION_CHILD_PROCESS_POLICY {
            Anonymous: windows_sys::Win32::System::SystemServices::PROCESS_MITIGATION_CHILD_PROCESS_POLICY_0 {
                Flags: 0,
            },
        };
        // SAFETY: `process` is a borrowed live process handle and `policy` is an initialized
        // exact-size output buffer retained only for this synchronous query.
        if unsafe {
            GetProcessMitigationPolicy(
                process,
                ProcessChildProcessPolicy,
                (&mut policy as *mut PROCESS_MITIGATION_CHILD_PROCESS_POLICY).cast(),
                size_of::<PROCESS_MITIGATION_CHILD_PROCESS_POLICY>(),
            )
        } == 0
        {
            return false;
        }
        // SAFETY: this is the documented Flags member of the initialized union returned above.
        (unsafe { policy.Anonymous.Flags } & 0b1) == 0b1
    }

    pub(super) fn verify_runtime_controls(
        process: &WinHandle,
        job: &WinHandle,
    ) -> Result<(), GateError> {
        let mut in_creation_job = 0;
        // SAFETY: both handles are directly owned and live. This query retains neither.
        if unsafe { IsProcessInJob(process.raw(), job.raw(), &mut in_creation_job) } == 0 {
            return Err(last_error("verify exact creation-time Job membership"));
        }
        if in_creation_job == 0 {
            return Err(GateError(
                "worker escaped the exact creation-time Job".to_string(),
            ));
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: the Job handle is live and `limits` is an initialized exact-size output buffer.
        if unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error("query JavaScript worker Job limits"));
        }
        let required_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_PROCESS_TIME;
        let creation_time_job_limits_match =
            limits.BasicLimitInformation.LimitFlags & required_flags == required_flags
                && limits.BasicLimitInformation.ActiveProcessLimit == 1
                && limits.BasicLimitInformation.PerProcessUserTimeLimit == PROCESS_CPU_LIMIT_100NS
                && limits.ProcessMemoryLimit == PROCESS_MEMORY_LIMIT_BYTES;
        if !creation_time_job_limits_match {
            return Err(GateError(
                "creation-time Job limits differ from the reviewed policy".to_string(),
            ));
        }

        if active_job_processes(job)? != 1 {
            return Err(GateError(
                "creation-time Job did not contain exactly one active process".to_string(),
            ));
        }

        let mut ui = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
        // SAFETY: the Job handle is live and `ui` is an initialized exact-size output buffer.
        if unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectBasicUIRestrictions,
                (&mut ui as *mut JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error("query JavaScript worker Job UI restrictions"));
        }
        if ui.UIRestrictionsClass != JOB_OBJECT_UILIMIT_ALL {
            return Err(GateError(
                "creation-time Job UI restrictions differ from the reviewed policy".to_string(),
            ));
        }
        if !mitigation_policy_matches_for_handle(process.raw()) {
            return Err(GateError(
                "effective process mitigations differ from the reviewed policy".to_string(),
            ));
        }
        if !child_process_policy_matches(process.raw()) {
            return Err(GateError(
                "effective child-process restriction differs from the reviewed policy".to_string(),
            ));
        }
        Ok(())
    }

    pub(super) fn active_job_processes(job: &WinHandle) -> Result<u32, GateError> {
        let mut accounting = JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION::default();
        // SAFETY: the Job handle is live and `accounting` is an initialized exact-size output.
        if unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectBasicAndIoAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error("query active JavaScript worker Job processes"));
        }
        Ok(accounting.BasicInfo.ActiveProcesses)
    }

    pub(super) fn run_containment_child_probe() -> io::Result<()> {
        let workspace = PathBuf::from(
            std::env::var_os(PROBE_WORKSPACE_ENV)
                .ok_or_else(|| io::Error::other("missing workspace sentinel probe metadata"))?,
        );
        let skill_database =
            PathBuf::from(std::env::var_os(PROBE_SKILL_DATABASE_ENV).ok_or_else(|| {
                io::Error::other("missing skill-database sentinel probe metadata")
            })?);
        let tcp_port = std::env::var(PROBE_TCP_PORT_ENV)
            .map_err(|_| io::Error::other("missing TCP probe metadata"))?
            .parse::<u16>()
            .map_err(|_| io::Error::other("invalid TCP probe metadata"))?;
        let udp_port = std::env::var(PROBE_UDP_PORT_ENV)
            .map_err(|_| io::Error::other("missing UDP probe metadata"))?
            .parse::<u16>()
            .map_err(|_| io::Error::other("invalid UDP probe metadata"))?;

        let workspace_read_denied =
            File::open(&workspace).is_err_and(|error| access_was_denied(&error));
        let workspace_write_denied = OpenOptions::new()
            .append(true)
            .open(&workspace)
            .is_err_and(|error| access_was_denied(&error));
        let skill_database_read_denied =
            File::open(&skill_database).is_err_and(|error| access_was_denied(&error));
        let skill_database_write_denied = OpenOptions::new()
            .append(true)
            .open(&skill_database)
            .is_err_and(|error| access_was_denied(&error));
        let credential_environment_absent = [
            "PATH",
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AZURE_CLIENT_SECRET",
            "GITHUB_TOKEN",
            "MINI_AGENT_CONFIG",
            "MINI_AGENT_WORKSPACE",
        ]
        .into_iter()
        .all(|name| std::env::var_os(name).is_none());

        let tcp_address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, tcp_port));
        let tcp_denied = TcpStream::connect_timeout(&tcp_address, Duration::from_secs(2))
            .is_err_and(|error| access_was_denied(&error));
        let udp_denied = match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            Err(error) => access_was_denied(&error),
            Ok(socket) => socket
                .send_to(
                    b"lpac-network-canary",
                    SocketAddrV4::new(Ipv4Addr::LOCALHOST, udp_port),
                )
                .is_err_and(|error| access_was_denied(&error)),
        };
        let network_denied = tcp_denied && udp_denied;

        let child_process_denied = std::env::current_exe()
            .ok()
            .and_then(|executable| {
                Command::new(executable)
                    .arg("--version")
                    .status_guarded()
                    .err()
            })
            .is_some_and(|error| access_was_denied(&error));
        let unlisted_file_handle_denied = inherited_handle_is_invalid(PROBE_FILE_HANDLE_ENV);
        let unlisted_socket_handle_denied = inherited_handle_is_invalid(PROBE_SOCKET_HANDLE_ENV);
        let protocol_handles_exact = exact_protocol_std_handles();
        let token_is_zero_capability_lpac =
            child_token_is_zero_capability_lpac().map_err(|error| io::Error::other(error.0))?;
        let no_console = no_console_devices();
        let mut in_job = 0;
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle, null queries any Job, and
        // the initialized BOOL output lives for the call.
        let creation_time_job_membership =
            unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) } != 0
                && in_job != 0;
        let mitigation_policy_matches = mitigation_policy_matches();

        let passed = workspace_read_denied
            && workspace_write_denied
            && skill_database_read_denied
            && skill_database_write_denied
            && credential_environment_absent
            && network_denied
            && child_process_denied
            && unlisted_file_handle_denied
            && unlisted_socket_handle_denied
            && protocol_handles_exact
            && token_is_zero_capability_lpac
            && no_console
            && creation_time_job_membership
            && mitigation_policy_matches;
        if !passed {
            return Err(io::Error::other("Windows containment child probe failed"));
        }
        std::io::stdout().lock().write_all(CONTAINMENT_READY)?;
        std::io::stdout().lock().flush()?;
        std::thread::park_timeout(Duration::from_secs(30));
        Err(io::Error::other(
            "Windows containment Job did not terminate the probe child",
        ))
    }

    pub(super) fn run_child() {
        let Some(sentinel) = std::env::var_os(SENTINEL_ENV) else {
            return;
        };
        let omitted_handle = std::env::var(CANARY_HANDLE_ENV)
            .expect("omitted handle marker")
            .parse::<usize>()
            .expect("numeric omitted handle marker") as HANDLE;
        let denied = match File::open(PathBuf::from(sentinel)) {
            Ok(_) => false,
            Err(error) => {
                error.kind() == io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(5)
            }
        };
        let mut flags = 0u32;
        // SAFETY: `flags` is an initialized writable output slot. The numeric
        // handle is intentionally omitted from HANDLE_LIST; the test expects
        // GetHandleInformation to reject it without storing the pointer.
        let canary_excluded = unsafe { GetHandleInformation(omitted_handle, &mut flags) } == 0
            // SAFETY: GetLastError has no pointer or ownership effects and is
            // read immediately after the failed handle query.
            && unsafe { GetLastError() } == ERROR_INVALID_HANDLE;
        let token_is_lpac = child_token_is_zero_capability_lpac().unwrap_or(false);
        let detached = no_console_devices();
        let protocol_handles = exact_protocol_std_handles();
        let frame = if denied && canary_excluded && token_is_lpac && detached && protocol_handles {
            READY_DENIED
        } else {
            READY_OPENED
        };
        std::io::stdout()
            .lock()
            .write_all(frame)
            .expect("write fixed LPAC readiness frame");
        std::io::stdout()
            .lock()
            .flush()
            .expect("flush fixed LPAC readiness frame");
        assert!(denied, "LPAC child unexpectedly opened workspace sentinel");
        assert!(
            canary_excluded,
            "LPAC child inherited a handle omitted from HANDLE_LIST"
        );
        assert!(token_is_lpac, "child token is not a zero-capability LPAC");
        assert!(detached, "LPAC child retained a console or console device");
        assert!(
            protocol_handles,
            "LPAC child standard handles are not the exact distinct protocol set"
        );
    }
}

pub(super) fn attest_containment(probe: &crate::extras::js::protocol::ContainmentProbe) -> bool {
    feasibility::attest_containment(probe)
}

#[cfg(test)]
fn run_lpac_image_loading_gate() -> Result<(), feasibility::GateError> {
    feasibility::run_artifact_matrix()
}

#[cfg(test)]
pub(super) fn run_containment_probe() -> io::Result<()> {
    feasibility::run_protected_install_negative_control()
        .map_err(|error| io::Error::other(error.0))?;
    feasibility::run_production_containment_probe().map_err(|error| io::Error::other(error.0))
}

#[cfg(test)]
pub(super) fn run_containment_child_probe() -> io::Result<()> {
    feasibility::run_containment_child_probe()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_PARAMETER, FALSE, GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessHandleCount, OpenProcess, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };

    #[test]
    #[ignore = "requires a real Windows AppContainer backend"]
    fn windows_lpac_can_load_current_exe_with_only_protocol_handles() {
        super::run_lpac_image_loading_gate()
            .expect("all required real artifacts must pass the Windows LPAC feasibility gate");
    }

    #[test]
    fn windows_lpac_path_policy_rejects_unc_roots() {
        assert!(
            super::feasibility::reject_unc_or_remote_syntax(std::path::Path::new(
                r"\\server\share\mini-agent.exe"
            ))
            .is_err()
        );
        assert!(
            super::feasibility::reject_unc_or_remote_syntax(std::path::Path::new(
                r"\\?\UNC\server\share\mini-agent.exe"
            ))
            .is_err()
        );
    }

    #[test]
    fn windows_lpac_acl_policy_distinguishes_rx_from_mutation() {
        use windows_sys::Win32::Foundation::{GENERIC_EXECUTE, GENERIC_READ};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_WRITE_DATA,
        };

        let mapped_generic = super::feasibility::mapped_file_mask(GENERIC_READ | GENERIC_EXECUTE);
        assert_eq!(
            mapped_generic & (FILE_GENERIC_READ | FILE_GENERIC_EXECUTE),
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
        );
        assert!(!super::feasibility::dangerous_write_mask(
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
        ));
        assert!(super::feasibility::dangerous_write_mask(FILE_WRITE_DATA));
        assert!(super::feasibility::dangerous_write_mask(DELETE));
        assert!(super::feasibility::package_allow_set_is_exact(&[(
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            0
        )]));
        assert!(super::feasibility::package_allow_set_is_exact(&[(
            GENERIC_READ | GENERIC_EXECUTE,
            0
        )]));
        assert!(!super::feasibility::package_allow_set_is_exact(&[
            (FILE_GENERIC_READ, 0),
            (FILE_GENERIC_EXECUTE, 0),
        ]));
        assert!(!super::feasibility::package_allow_set_is_exact(&[(
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | FILE_WRITE_DATA,
            0,
        )]));
        assert!(!super::feasibility::package_allow_set_is_exact(&[(
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            1,
        )]));
    }

    #[test]
    fn windows_lpac_matrix_full_probes_every_location_and_splits_copy_expectations() {
        use super::feasibility::{ArtifactSource, InstallLocation, ProbeKind, artifact_contracts};

        let contracts = artifact_contracts();
        assert_eq!(
            contracts
                .iter()
                .filter(|contract| contract.probe == ProbeKind::Harness)
                .count(),
            3
        );
        for destination in [
            InstallLocation::CargoBuild,
            InstallLocation::CargoInstall,
            InstallLocation::UserArchive,
        ] {
            assert!(contracts.iter().any(|contract| {
                contract.destination == destination
                    && contract.source == ArtifactSource::Harness
                    && contract.source_location == InstallLocation::CargoBuild
                    && contract.probe == ProbeKind::Harness
            }));
        }
        assert!(contracts.iter().any(|contract| {
            contract.destination == InstallLocation::UserArchive
                && contract.source == ArtifactSource::Installed
                && contract.source_location == InstallLocation::CargoInstall
                && contract.probe == ProbeKind::ImageLoadingOnly
        }));
        assert_eq!(
            contracts
                .iter()
                .filter(|contract| contract.probe == ProbeKind::ImageLoadingOnly)
                .count(),
            2
        );
        assert!(contracts.iter().any(|contract| {
            contract.destination == InstallLocation::CargoInstall
                && contract.source == ArtifactSource::Installed
                && contract.source_location == InstallLocation::CargoInstall
                && contract.probe == ProbeKind::ImageLoadingOnly
        }));
    }

    #[test]
    fn windows_production_policy_has_required_limits_and_compatible_mitigations() {
        use super::feasibility::{
            MITIGATION_POLICY, PROCESS_CPU_LIMIT_100NS, PROCESS_MEMORY_LIMIT_BYTES,
        };

        assert_eq!(PROCESS_MEMORY_LIMIT_BYTES, 256 * 1024 * 1024);
        assert_eq!(PROCESS_CPU_LIMIT_100NS, 35 * 10_000_000);
        for required in [8, 12, 16, 20, 32, 52, 56, 60] {
            assert_ne!(MITIGATION_POLICY & (1u64 << required), 0);
        }
        assert_eq!(
            MITIGATION_POLICY & (1u64 << 28),
            0,
            "Win32k denial requires an A27 compatibility proof"
        );
        assert_eq!(
            MITIGATION_POLICY & (1u64 << 36),
            0,
            "dynamic-code denial requires an exact release-binary compatibility proof"
        );
    }

    #[test]
    fn windows_production_status_fails_closed_for_nonproduction_test_image() {
        match super::containment_status() {
            crate::sandbox::worker::WorkerContainmentStatus::Unavailable {
                backend,
                assurance,
                reason,
            } => {
                assert_eq!(backend, crate::sandbox::worker::WorkerBackend::WindowsLpac);
                assert_eq!(
                    assurance,
                    crate::sandbox::worker::WorkerContainmentAssurance::Enforced
                );
                assert_eq!(reason, "Windows LPAC production runtime preflight failed");
            }
            status => {
                panic!("libtest image unexpectedly reported production available: {status:?}")
            }
        }
    }

    #[test]
    fn windows_production_launch_uses_cached_unavailable_without_second_child() {
        assert!(matches!(
            super::containment_status(),
            crate::sandbox::worker::WorkerContainmentStatus::Unavailable { .. }
        ));
        let _creation_guard = crate::process_creation::creation_guard()
            .expect("isolate the production-launch observation from raw launcher tests");
        super::feasibility::LAST_PRODUCTION_TEST_PID.store(0, Ordering::Release);
        let error = super::launch().expect_err("cached failed preflight must fail closed");
        assert!(matches!(
            error,
            crate::sandbox::worker::WorkerLaunchError::Unavailable {
                backend: crate::sandbox::worker::WorkerBackend::WindowsLpac,
                ..
            }
        ));
        assert_eq!(
            super::feasibility::LAST_PRODUCTION_TEST_PID.load(Ordering::Acquire),
            0,
            "cached unavailable launch path reached CreateProcessW a second time"
        );
    }

    #[test]
    fn windows_production_inheriting_process_lock_serializes_launch_windows() {
        let first = crate::process_creation::creation_guard()
            .expect("first inheriting-process creation lock acquisition");
        let (attempting_tx, attempting_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            attempting_tx.send(()).expect("announce lock attempt");
            let _guard = crate::process_creation::creation_guard()
                .expect("contending inheriting-process creation lock acquisition");
            acquired_tx.send(()).expect("announce lock acquisition");
        });

        attempting_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("contender did not attempt lock acquisition");
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "two inheritable-handle windows overlapped"
        );
        drop(first);
        acquired_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("contender did not acquire released lock");
        contender.join().expect("lock contender panicked");
    }

    #[test]
    fn windows_production_creation_boundary_excludes_ordinary_piped_child() {
        use crate::process_creation::StdCommandCreationExt;
        use std::process::Command as ProcessBuilder;

        let inheritance_guard = crate::process_creation::creation_guard()
            .expect("acquire LPAC inheritable-handle window");
        let mut pipes = super::feasibility::ProtocolPipes::exact_anonymous_set(&inheritance_guard)
            .expect("create LPAC protocol pipes");
        let (attempting_tx, attempting_rx) = std::sync::mpsc::channel();
        let (spawned_tx, spawned_rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            let mut command =
                ProcessBuilder::new(std::env::current_exe().expect("resolve libtest executable"));
            command
                .args([
                    "--exact",
                    "sandbox::worker::platform::tests::windows_production_failure_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(
                    super::super::INTERNAL_WORKER_MARKER,
                    super::super::INTERNAL_WORKER_MARKER_VALUE,
                )
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            attempting_tx.send(()).expect("announce ordinary spawn");
            spawned_tx
                .send(StdCommandCreationExt::spawn_guarded(&mut command))
                .expect("report ordinary spawn result");
        });

        attempting_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ordinary child did not reach creation boundary");
        assert!(
            spawned_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "ordinary piped child spawned inside the LPAC inheritance window"
        );

        pipes
            .clear_child_inheritance()
            .expect("clear LPAC protocol inheritance before releasing boundary");
        drop(inheritance_guard);
        let mut child = spawned_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ordinary child remained blocked after LPAC window closed")
            .expect("spawn ordinary piped child");
        contender.join().expect("ordinary spawn contender panicked");
        assert!(
            child.try_wait().expect("poll ordinary child").is_none(),
            "ordinary child exited before handle-retention proof"
        );
        drop(
            crate::process_creation::creation_guard()
                .expect("ordinary spawn retained creation lock while child was running"),
        );

        let (parent_input, parent_output, parent_error, child_input, child_output, child_error) =
            pipes.into_test_handles();
        drop(child_input);
        drop(child_output);
        drop(child_error);

        let mut parent_input = parent_input.into_file();
        assert!(
            parent_input.write_all(b"x").is_err(),
            "ordinary child retained the LPAC stdin read endpoint"
        );
        drop(parent_input);

        let (eof_tx, eof_rx) = std::sync::mpsc::channel();
        let readers =
            [("stdout", parent_output), ("stderr", parent_error)].map(|(label, handle)| {
                let eof_tx = eof_tx.clone();
                std::thread::spawn(move || {
                    let mut handle = handle.into_file();
                    let mut byte = [0u8; 1];
                    eof_tx
                        .send((label, handle.read(&mut byte)))
                        .expect("report LPAC pipe EOF");
                })
            });
        drop(eof_tx);
        for expected in ["stdout", "stderr"] {
            let (label, result) = match eof_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(result) => result,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("ordinary child retained LPAC {expected} endpoint: {error}");
                }
            };
            assert_eq!(result.expect("read LPAC parent endpoint"), 0, "{label}");
        }
        for reader in readers {
            reader.join().expect("LPAC EOF reader panicked");
        }
        child
            .kill()
            .expect("terminate ordinary boundary-test child");
        child.wait().expect("reap ordinary boundary-test child");
    }

    #[test]
    fn windows_production_failure_injection_closes_raii_handles_and_reaps_children() {
        use super::feasibility::{
            LAST_PRODUCTION_TEST_PID, ProductionFailurePoint, ProductionLaunchHooks,
            launch_production,
        };

        // The first AppContainer API call lazily initializes process-global
        // Windows state that remains live for the process. Warm that stable
        // production profile before measuring per-launch HANDLE ownership so
        // the test detects launcher leaks rather than OS one-time caches.
        let warmup = launch_production(ProductionLaunchHooks::fail_at(
            ProductionFailurePoint::PrepareExecutableAcl,
        ))
        .expect_err("profile warmup must stop at the injected checkpoint");
        assert!(warmup.to_string().contains("PrepareExecutableAcl"));

        let points = [
            ProductionFailurePoint::CreateProfile,
            ProductionFailurePoint::PrepareExecutableAcl,
            ProductionFailurePoint::CreateStdinPipe,
            ProductionFailurePoint::CreateStdoutPipe,
            ProductionFailurePoint::CreateStderrPipe,
            ProductionFailurePoint::CreateJob,
            ProductionFailurePoint::SetJobLimits,
            ProductionFailurePoint::SetJobUiRestrictions,
            ProductionFailurePoint::AllocateAttributeList,
            ProductionFailurePoint::SecurityCapabilitiesAttribute,
            ProductionFailurePoint::LpacOptOutAttribute,
            ProductionFailurePoint::JobListAttribute,
            ProductionFailurePoint::ChildProcessPolicyAttribute,
            ProductionFailurePoint::MitigationPolicyAttribute,
            ProductionFailurePoint::HandleListAttribute,
            ProductionFailurePoint::CreateProcess,
            ProductionFailurePoint::OwnProcessHandle,
            ProductionFailurePoint::OwnThreadHandle,
            ProductionFailurePoint::ClearStdinInheritance,
            ProductionFailurePoint::ClearStdoutInheritance,
            ProductionFailurePoint::ClearStderrInheritance,
            ProductionFailurePoint::VerifyCreationTimeJob,
        ];

        for point in points {
            let baseline = super::LIVE_WIN_HANDLES.load(Ordering::Acquire);
            let os_handle_baseline = current_process_handle_count();
            LAST_PRODUCTION_TEST_PID.store(0, Ordering::Release);
            let error = launch_production(ProductionLaunchHooks::fail_at(point))
                .expect_err("each injected launcher failure must fail closed");
            assert!(
                error.to_string().contains(&format!("{point:?}")),
                "launcher failed before injected point {point:?}: {error}"
            );
            let pid = LAST_PRODUCTION_TEST_PID.load(Ordering::Acquire);
            if pid != 0 {
                assert_process_exits_after_job_cleanup(pid);
            }
            assert_eq!(
                super::LIVE_WIN_HANDLES.load(Ordering::Acquire),
                baseline,
                "owned Win32 handle leaked at {point:?}"
            );
            assert_eq!(
                current_process_handle_count(),
                os_handle_baseline,
                "kernel HANDLE leaked at {point:?}"
            );
            drop(
                crate::process_creation::creation_guard()
                    .expect("failed launch poisoned the shared creation lock"),
            );
        }
    }

    fn current_process_handle_count() -> u32 {
        let mut count = 0;
        // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle, and `count` is an
        // initialized writable DWORD retained only for this synchronous diagnostic call.
        assert_ne!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
            0,
            "query current process HANDLE count"
        );
        count
    }

    fn assert_process_exits_after_job_cleanup(pid: u32) {
        // SAFETY: OpenProcess receives a numeric PID reported by CreateProcessW. On success the
        // returned SYNCHRONIZE-only handle transfers immediately into the module's RAII owner.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, FALSE, pid) };
        if raw.is_null() {
            // SAFETY: OpenProcess failed immediately above; GetLastError reads that thread-local
            // failure code and has no pointer or ownership effects. Only a vanished PID proves
            // cleanup; access denial would make the reap assertion inconclusive.
            assert_eq!(
                unsafe { GetLastError() },
                ERROR_INVALID_PARAMETER,
                "injected worker {pid} still exists but cannot be opened for synchronization"
            );
            return;
        }
        let process = super::WinHandle::from_created(raw, "open injected worker for reap check")
            .expect("OpenProcess returned a valid owned handle");
        // SAFETY: the SYNCHRONIZE handle is live for the bounded wait and no pointer is retained.
        assert_eq!(
            unsafe {
                WaitForSingleObject(process.raw(), Duration::from_secs(5).as_millis() as u32)
            },
            WAIT_OBJECT_0,
            "kill-on-close Job did not reap injected child {pid}"
        );
    }

    #[test]
    fn windows_production_failure_child() {
        if std::env::var_os(super::super::INTERNAL_WORKER_MARKER).is_some() {
            std::thread::park_timeout(Duration::from_secs(30));
        }
    }

    #[test]
    fn windows_lpac_gate_child() {
        super::feasibility::run_child();
    }
}
