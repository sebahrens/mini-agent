#![allow(unsafe_code)]

use std::io;
use std::process::{Child, ExitStatus};

use super::{WorkerBackend, WorkerContainmentStatus, WorkerLaunchError, WorkerProcess};

const BACKEND: WorkerBackend = WorkerBackend::WindowsLpac;
const UNAVAILABLE_REASON: &str =
    "the zero-capability LPAC/AppContainer creation-time Job backend has not been delivered";

pub(super) fn containment_status() -> WorkerContainmentStatus {
    WorkerContainmentStatus::Unavailable {
        backend: BACKEND,
        reason: UNAVAILABLE_REASON.to_string(),
    }
}

pub(super) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    Err(WorkerLaunchError::Unavailable {
        backend: BACKEND,
        reason: UNAVAILABLE_REASON.to_string(),
    })
}

// This temporary std::process-backed type is reachable only from the test
// launcher. The A03 LPAC code below is a target-gated research helper, not a
// production launcher. A26 will replace this type with directly owned process
// and Job handles after the real Windows gate has passed.
#[derive(Debug)]
pub(super) struct WorkerChild {
    child: Child,
}

impl WorkerChild {
    #[cfg(test)]
    pub(super) fn from_unconfined_test_child(child: Child) -> Self {
        Self { child }
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn terminate_tree(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

#[cfg(test)]
mod feasibility {
    use std::ffi::{OsStr, c_void};
    use std::fmt;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE,
        GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, GENERIC_WRITE, GetHandleInformation,
        GetLastError, HANDLE, HANDLE_FLAG_INHERIT, LocalFree, SetHandleInformation, TRUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
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
        CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_APPEND_DATA,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, GetDriveTypeW, OPEN_EXISTING, WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleWindow, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Memory::{
        GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc, HeapFree,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
        GetExitCodeProcess, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        OpenProcessToken, PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_FIXED, PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
    };

    const PROFILE_NAME: &str = "mini-agent.worker-image-loading-gate.v1";
    const CHILD_TEST_NAME: &str = "sandbox::worker::platform::tests::windows_lpac_gate_child";
    const INSTALLED_EXE_ENV: &str = "MINI_AGENT_LPAC_CARGO_INSTALL_EXE";
    const SENTINEL_ENV: &str = "MINI_AGENT_LPAC_SENTINEL";
    const CANARY_HANDLE_ENV: &str = "MINI_AGENT_LPAC_OMITTED_HANDLE";
    const READY_DENIED: &[u8] =
        b"MINI_AGENT_LPAC_READY_V2:LPAC:ZERO_CAPS:NO_CONSOLE:WORKSPACE_DENIED:HANDLE_LIST_EXACT\n";
    const READY_OPENED: &[u8] = b"MINI_AGENT_LPAC_READY_V1:AUTHORITY_LEAKED\n";
    const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    #[derive(Debug)]
    pub(super) struct GateError(String);

    impl fmt::Display for GateError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl std::error::Error for GateError {}

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
    struct WinHandle(OwnedHandle);

    impl WinHandle {
        fn from_created(raw: HANDLE, context: &str) -> Result<Self, GateError> {
            if raw.is_null() || raw == (-1isize as HANDLE) {
                return Err(last_error(context));
            }
            // SAFETY: `raw` is a newly returned, non-null owned Win32 handle.
            // This conversion transfers its single CloseHandle obligation to
            // OwnedHandle; no other owner is retained.
            Ok(Self(unsafe { OwnedHandle::from_raw_handle(raw) }))
        }

        fn raw(&self) -> HANDLE {
            self.0.as_raw_handle()
        }

        fn clear_inherit(&self) -> Result<(), GateError> {
            // SAFETY: the handle remains owned by `self` for the call, and
            // SetHandleInformation neither stores nor closes it.
            if unsafe { SetHandleInformation(self.raw(), HANDLE_FLAG_INHERIT, 0) } == 0 {
                return Err(last_error("clear pipe inheritance"));
            }
            Ok(())
        }

        fn into_file(self) -> File {
            File::from(self.0)
        }
    }

    fn close_unowned_handle(raw: HANDLE) {
        if !raw.is_null() && raw != (-1isize as HANDLE) {
            // SAFETY: this helper is called only for a raw handle returned by a
            // failed/malformed FFI operation before ownership was transferred.
            // It discharges that operation's sole CloseHandle obligation.
            unsafe {
                CloseHandle(raw);
            }
        }
    }

    // WinHandle normally delegates cleanup to OwnedHandle. Keep this direct
    // CloseHandle assertion near the FFI definitions so source inspection also
    // verifies which API owns all raw HANDLE values used by the gate.
    const _: unsafe extern "system" fn(HANDLE) -> i32 = CloseHandle;

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
    enum InstallLocation {
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
    }

    impl AppContainerProfile {
        fn stable_zero_capability() -> Result<Self, GateError> {
            let name = wide_string(PROFILE_NAME);
            let display = wide_string("mini-agent worker image-loading gate");
            let description = wide_string("zero-capability LPAC feasibility profile");
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
            })
        }

        fn finish(mut self) -> Result<(), GateError> {
            if self.created {
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
            if self.created {
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
        let mut copy = OwnedSid(vec![
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

    pub(super) fn dangerous_write_mask(mask: u32) -> bool {
        mask & (FILE_WRITE_DATA
            | FILE_APPEND_DATA
            | FILE_ADD_FILE
            | FILE_ADD_SUBDIRECTORY
            | FILE_DELETE_CHILD
            | FILE_WRITE_ATTRIBUTES
            | FILE_WRITE_EA
            | DELETE
            | WRITE_DAC
            | WRITE_OWNER
            | GENERIC_WRITE
            | GENERIC_ALL)
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
        let required = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
        let mut exact_rx_aces = 0u32;
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
            if policy.broad(sid) && (reject_broad_access || dangerous_write_mask(ace.Mask)) {
                return Err(GateError(
                    "broad-principal allow ACE is forbidden".to_string(),
                ));
            }
            if reject_broad_access
                && ace.Mask & required != 0
                && !policy.trusted_writer(sid)
                && !sid_equal(sid, appcontainer_sid)
            {
                return Err(GateError(
                    "an unexpected principal can read or execute the image".to_string(),
                ));
            }
            if dangerous_write_mask(ace.Mask) && !policy.trusted_writer(sid) {
                return Err(GateError(
                    "another principal can write, modify, or delete the path".to_string(),
                ));
            }
            if sid_equal(sid, appcontainer_sid) {
                if u32::from(header.AceFlags) != NO_INHERITANCE
                    || ace.Mask & required != required
                    || dangerous_write_mask(ace.Mask)
                {
                    return Err(GateError(
                        "AppContainer ACE is not exact non-inheriting RX".to_string(),
                    ));
                }
                exact_rx_aces += 1;
            }
        }
        if require_exact_appcontainer_rx && exact_rx_aces != 1 {
            return Err(GateError(format!(
                "expected one exact AppContainer RX ACE, found {exact_rx_aces}"
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
        let required = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
        if rights & required != required {
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
        if result != 0 || effective & required != required || dangerous_write_mask(effective) {
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

    struct ProtocolPipes {
        parent_input: WinHandle,
        parent_output: WinHandle,
        parent_error: WinHandle,
        child_input: WinHandle,
        child_output: WinHandle,
        child_error: WinHandle,
    }

    fn inheritable_pipe() -> Result<(WinHandle, WinHandle), GateError> {
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
        let read = WinHandle::from_created(read, "create protocol read handle")?;
        let write = WinHandle::from_created(write, "create protocol write handle")?;
        Ok((read, write))
    }

    impl ProtocolPipes {
        fn exact_anonymous_set() -> Result<Self, GateError> {
            let (child_input, parent_input) = inheritable_pipe()?;
            parent_input.clear_inherit()?;
            let (parent_output, child_output) = inheritable_pipe()?;
            parent_output.clear_inherit()?;
            let (parent_error, child_error) = inheritable_pipe()?;
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

    #[derive(Debug, Clone, Copy)]
    enum ProbeKind {
        Harness,
        VersionBinary,
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
            ProbeKind::VersionBinary => "--version".to_string(),
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
    }

    impl Drop for Sentinel {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    struct DisposableArtifact {
        executable: PathBuf,
        directory: PathBuf,
        expected: InstallLocation,
        probe: ProbeKind,
        cleaned: bool,
    }

    struct ArtifactSpec {
        source: PathBuf,
        directory: PathBuf,
        expected: InstallLocation,
        probe: ProbeKind,
    }

    impl DisposableArtifact {
        fn copy_into(
            source: &Path,
            source_lock: WinHandle,
            directory: PathBuf,
            expected: InstallLocation,
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
            if classify_install_location(&executable) != expected {
                let file_cleanup = std::fs::remove_file(&executable);
                let directory_cleanup = std::fs::remove_dir(&directory);
                if let Err(error) = file_cleanup.and(directory_cleanup) {
                    return Err(GateError(format!(
                        "disposable artifact did not retain expected {expected:?} location; cleanup also failed: {error}"
                    )));
                }
                return Err(GateError(format!(
                    "disposable artifact did not retain expected {expected:?} location"
                )));
            }
            Ok(Self {
                executable,
                directory,
                expected,
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
                    "{INSTALLED_EXE_ENV} must name a real `cargo install --path . --debug` artifact"
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
        let build_dir = build
            .parent()
            .ok_or_else(|| GateError("build harness has no parent".to_string()))?
            .join(format!(".mini-agent-lpac-build-{suffix}"));
        let install_dir = installed
            .parent()
            .ok_or_else(|| GateError("installed artifact has no parent".to_string()))?
            .join(format!(".mini-agent-lpac-install-{suffix}"));
        let archive_root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| {
                GateError("LOCALAPPDATA is required for the archive case".to_string())
            })?;
        let archive_dir = archive_root.join(format!("mini-agent-lpac-archive-{suffix}"));

        Ok(vec![
            ArtifactSpec {
                source: build,
                directory: build_dir,
                expected: InstallLocation::CargoBuild,
                probe: ProbeKind::Harness,
            },
            ArtifactSpec {
                source: installed.clone(),
                directory: install_dir,
                expected: InstallLocation::CargoInstall,
                probe: ProbeKind::VersionBinary,
            },
            ArtifactSpec {
                source: installed,
                directory: archive_dir,
                expected: InstallLocation::UserArchive,
                probe: ProbeKind::VersionBinary,
            },
        ])
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

    pub(super) fn run_artifact_matrix() -> Result<(), GateError> {
        let profile = AppContainerProfile::stable_zero_capability()?;
        let result = (|| {
            let policy = SidPolicy::current()?;
            for specification in artifact_matrix()? {
                let source_lock = validate_source_artifact(
                    &specification.source,
                    specification.expected,
                    &policy,
                )?;
                let artifact = DisposableArtifact::copy_into(
                    &specification.source,
                    source_lock,
                    specification.directory,
                    specification.expected,
                    specification.probe,
                )?;
                eprintln!("LPAC real-artifact candidate: {:?}", artifact.expected);
                let probe = (|| {
                    let (executable, location, image_lock) =
                        prepare_executable_acl(&artifact.executable, profile.sid, &policy)?;
                    if location != artifact.expected {
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

    fn launch_and_probe(
        executable: &Path,
        appcontainer_sid: PSID,
        probe: ProbeKind,
    ) -> Result<(), GateError> {
        let sentinel = Sentinel::workspace_file()?;
        let pipes = ProtocolPipes::exact_anonymous_set()?;
        // Both canary endpoints are deliberately inheritable but absent from
        // HANDLE_LIST. The child receives only the numeric value and must prove
        // it is invalid, demonstrating that the allow-list excluded ambient
        // inheritable handles rather than merely listing the intended three.
        let (canary_read, canary_write) = inheritable_pipe()?;
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
                return Err(error);
            }
        };
        let thread =
            WinHandle::from_created(process_information.hThread, "own LPAC thread handle")?;
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
            ProbeKind::VersionBinary => {
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
        })
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

#[cfg(test)]
fn run_lpac_image_loading_gate() -> Result<(), feasibility::GateError> {
    feasibility::run_artifact_matrix()
}

#[cfg(test)]
mod tests {
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
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_WRITE_DATA,
        };

        assert!(!super::feasibility::dangerous_write_mask(
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
        ));
        assert!(super::feasibility::dangerous_write_mask(FILE_WRITE_DATA));
        assert!(super::feasibility::dangerous_write_mask(DELETE));
    }

    #[test]
    fn windows_lpac_gate_child() {
        super::feasibility::run_child();
    }
}
