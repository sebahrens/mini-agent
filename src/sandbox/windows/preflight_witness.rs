//! Closed post-retirement observation for the explicit installed self-check.
//! This mapping is never a capability, readiness proof, or availability result.
use super::*;
use std::cell::RefCell;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{
    ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, GetHandleInformation, GetLastError,
    HANDLE_FLAG_INHERIT,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};

const CELL_BYTES: usize = 32;
const STAGE_OFFSET: usize = 24;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Attachment {
    handle: usize,
    nonce: [u8; 16],
}
struct View(MEMORY_MAPPED_VIEW_ADDRESS);
impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: this owner retains one successful mapped view until drop.
        unsafe {
            UnmapViewOfFile(self.0);
        }
    }
}
struct Cell {
    handle: Handle,
    view: View,
    nonce: [u8; 16],
}
impl Cell {
    fn new() -> Result<Self, String> {
        // SAFETY: fixed-size unnamed pagefile mapping with no inheritance or
        // executable pages; valid native out parameters owned immediately.
        let handle = Handle::created(
            unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    null(),
                    PAGE_READWRITE,
                    0,
                    CELL_BYTES as u32,
                    null(),
                )
            },
            "create preflight observation",
        )?;
        // SAFETY: mapping retained; fixed nonzero view within its exact size.
        let mapped = unsafe { MapViewOfFile(handle.raw(), FILE_MAP_WRITE, 0, 0, CELL_BYTES) };
        if mapped.Value.is_null() {
            return Err("sandbox: preflight observation unavailable".into());
        }
        let view = View(mapped);
        let nonce = *uuid::Uuid::new_v4().as_bytes();
        // SAFETY: initialization precedes transfer. No references into shared
        // storage, concurrent access, overlapping widths or unaligned DWORD.
        unsafe {
            std::ptr::write_bytes(view.0.Value.cast::<u8>(), 0, CELL_BYTES);
            std::ptr::copy_nonoverlapping(
                1u32.to_le_bytes().as_ptr(),
                view.0.Value.cast::<u8>(),
                4,
            );
            std::ptr::copy_nonoverlapping(
                (CELL_BYTES as u32).to_le_bytes().as_ptr(),
                view.0.Value.cast::<u8>().add(4),
                4,
            );
            std::ptr::copy_nonoverlapping(nonce.as_ptr(), view.0.Value.cast::<u8>().add(8), 16);
        }
        if (view.0.Value as usize + STAGE_OFFSET) % 4 != 0 {
            return Err("sandbox: preflight observation unavailable".into());
        }
        Ok(Self {
            handle,
            view,
            nonce,
        })
    }
    fn duplicate_into(&self, child: &Child) -> Result<Attachment, String> {
        let mut remote = null_mut();
        // SAFETY: exact live parent and spawned helper handles, minimum section
        // access, noninheritable target-local handle. Parent never closes remote.
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle.raw(),
                child.as_raw_handle(),
                &mut remote,
                FILE_MAP_READ | FILE_MAP_WRITE,
                FALSE,
                0,
            )
        } == 0
        {
            return Err("sandbox: preflight observation attachment unavailable".into());
        }
        Ok(Attachment {
            handle: remote as usize,
            nonce: self.nonce,
        })
    }
    fn observe_retired(&self, child: &Child) -> &'static str {
        // SAFETY: only exact signaled helper may authorize sampling this purely
        // observational cell. No concurrent sample is ever performed.
        if unsafe { WaitForSingleObject(child.as_raw_handle(), 0) } != WAIT_OBJECT_0 {
            return "unretired";
        }
        let mut bytes = [0u8; CELL_BYTES];
        // SAFETY: retained view; exact helper has retired. Immutable header was
        // initialized before transfer. One aligned volatile DWORD completes sample.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.view.0.Value.cast::<u8>(),
                bytes.as_mut_ptr(),
                STAGE_OFFSET,
            );
            let phase = std::ptr::read_volatile(
                self.view
                    .0
                    .Value
                    .cast::<u8>()
                    .add(STAGE_OFFSET)
                    .cast::<u32>(),
            );
            bytes[STAGE_OFFSET..STAGE_OFFSET + 4].copy_from_slice(&phase.to_le_bytes());
        }
        decode(&bytes, &self.nonce)
    }
}
fn decode(bytes: &[u8; CELL_BYTES], nonce: &[u8; 16]) -> &'static str {
    if bytes[..4] != 1u32.to_le_bytes()
        || bytes[4..8] != (CELL_BYTES as u32).to_le_bytes()
        || bytes[8..24] != *nonce
    {
        return "invalid_header";
    }
    match u32::from_le_bytes(
        bytes[STAGE_OFFSET..STAGE_OFFSET + 4]
            .try_into()
            .expect("fixed DWORD"),
    ) {
        0 => "not_attached",
        1 => "request",
        2 => "setup",
        3 => "launch",
        4 => "verify_job",
        15 => "ready",
        16 => "wait",
        17 => "exit_code",
        18 => "drain",
        19 => "cleanup",
        20 => "suspended_job",
        21 => "regular_token_open",
        22 => "regular_token_sid",
        23 => "regular_token_duplicate",
        24 => "regular_token_descriptor",
        25 => "regular_token_access",
        26 => "resume",
        _ => "invalid_stage",
    }
}
struct HelperCell {
    _handle: Handle,
    view: View,
}
thread_local! { static HELPER_CELL: RefCell<Option<HelperCell>> = const { RefCell::new(None) }; }
pub(super) fn attach(attachment: &Attachment) -> Result<(), String> {
    let raw = attachment.handle as HANDLE;
    let mut flags = 0;
    // SAFETY: inspect only the process-local integer received in the complete
    // private request. Do not adopt/close it until mapping/header are validated.
    if raw.is_null()
        || raw == INVALID_HANDLE_VALUE
        || unsafe { GetHandleInformation(raw, &mut flags) } == 0
        || flags & HANDLE_FLAG_INHERIT != 0
    {
        return Err("sandbox: preflight observation attachment invalid".into());
    }
    // SAFETY: MapView validates section access/type; fixed-size view is retained.
    let mapped = unsafe { MapViewOfFile(raw, FILE_MAP_WRITE, 0, 0, CELL_BYTES) };
    if mapped.Value.is_null() {
        return Err("sandbox: preflight observation attachment invalid".into());
    }
    let view = View(mapped);
    let mut bytes = [0u8; CELL_BYTES];
    // SAFETY: parent initialized immutable header before duplicate/whole packet.
    // Helper has not stored a stage and parent never samples concurrently.
    unsafe {
        std::ptr::copy_nonoverlapping(view.0.Value.cast::<u8>(), bytes.as_mut_ptr(), CELL_BYTES);
    }
    if decode(&bytes, &attachment.nonce) != "not_attached"
        || (view.0.Value as usize + STAGE_OFFSET) % 4 != 0
    {
        return Err("sandbox: preflight observation attachment invalid".into());
    }
    let handle = Handle::created(raw, "adopt preflight observation")?;
    HELPER_CELL.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_some() {
            return Err("sandbox: duplicate preflight observation attachment".into());
        }
        *cell = Some(HelperCell {
            _handle: handle,
            view,
        });
        Ok(())
    })
}
pub(super) fn mark(stage: u8) {
    HELPER_CELL.with(|cell| {
        if let Some(cell) = cell.borrow().as_ref() {
            // SAFETY: helper's live owned view; parent does not read until helper
            // retirement. One aligned DWORD; no Rust shared-storage reference.
            unsafe {
                std::ptr::write_volatile(
                    cell.view
                        .0
                        .Value
                        .cast::<u8>()
                        .add(STAGE_OFFSET)
                        .cast::<u32>(),
                    u32::from(stage),
                );
            }
        }
    });
}

struct Feeder {
    thread: Option<JoinHandle<std::io::Result<()>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}
impl Feeder {
    fn start(mut writer: File, payload: Vec<u8>) -> Result<Self, String> {
        ACTIVE_REQUEST_FEEDERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_REQUEST_FEEDERS).then_some(active + 1)
            })
            .map_err(|_| "sandbox: too many pending Windows request feeders".to_string())?;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancelled.clone();
        let thread = std::thread::Builder::new()
            .name("windows-preflight-request".into())
            .spawn(move || {
                struct Permit;
                impl Drop for Permit {
                    fn drop(&mut self) {
                        ACTIVE_REQUEST_FEEDERS.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _permit = Permit;
                if flag.load(Ordering::Acquire) {
                    return Err(std::io::Error::from_raw_os_error(
                        ERROR_OPERATION_ABORTED as i32,
                    ));
                }
                writer.write_all(&(payload.len() as u32).to_le_bytes())?;
                if flag.load(Ordering::Acquire) {
                    return Err(std::io::Error::from_raw_os_error(
                        ERROR_OPERATION_ABORTED as i32,
                    ));
                }
                writer.write_all(&payload)
            })
            .map_err(|_| {
                ACTIVE_REQUEST_FEEDERS.fetch_sub(1, Ordering::AcqRel);
                "sandbox: start preflight request feeder failed".to_string()
            })?;
        Ok(Self {
            thread: Some(thread),
            cancelled,
        })
    }
    fn settle(&mut self, deadline: Instant) -> Result<(), String> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(());
        };
        self.cancelled.store(true, Ordering::Release);
        loop {
            // SAFETY: exact JoinHandle retains the native thread throughout.
            let signal = unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) };
            if signal == WAIT_OBJECT_0 {
                break;
            }
            if signal != WAIT_TIMEOUT || Instant::now() >= deadline {
                return Err("sandbox: preflight request feeder cleanup uncertain".into());
            }
            // SAFETY: cancel only this exact synchronous writer; cancellation
            // alone is not settlement. ERROR_NOT_FOUND is only an I/O race.
            if unsafe { CancelSynchronousIo(thread.as_raw_handle()) } == 0
                && unsafe { GetLastError() } != ERROR_NOT_FOUND
            {
                return Err("sandbox: preflight request feeder cancellation failed".into());
            }
            std::thread::sleep(
                GENERAL_PREFLIGHT_POLL_INTERVAL
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        // Native signal proves the join is complete, not an unbounded wait.
        self.thread
            .take()
            .expect("owned feeder")
            .join()
            .map_err(|_| "sandbox: preflight request feeder panicked".to_string())?
            .map_err(|_| "sandbox: preflight request delivery failed".to_string())
    }
}
impl Drop for Feeder {
    fn drop(&mut self) {
        if self.thread.is_some() {
            uncertain_cleanup();
        }
    }
}
fn uncertain_cleanup() -> ! {
    // Explicit self-check only. Preserve recovery state rather than detach a
    // writer or release an unretired actor. Never claim sandbox availability.
    eprintln!(
        "WINDOWS_GENERAL_PREFLIGHT_HELPER_DIAGNOSTIC v=1 phase=uncertain_cleanup elapsed_ms=0"
    );
    std::process::abort()
}
struct Actor {
    child: Child,
    feeder: Option<Feeder>,
    cleanup: Instant,
    settled: bool,
}
impl Actor {
    fn settle(&mut self) {
        if self.settled {
            return;
        }
        // Keep exact process and feeder owners through every error/unwind. Same
        // original cleanup deadline; no retry actor or renewed budget.
        if terminate_and_reap_owned_helper(&mut self.child, self.cleanup, None).is_err() {
            uncertain_cleanup();
        }
        if let Some(feeder) = &mut self.feeder {
            let result = feeder.settle(self.cleanup);
            if feeder.thread.is_some() {
                uncertain_cleanup();
            }
            // Delivery failure does not authorize availability; the helper's
            // original result remains primary and complete-packet checks remain.
            let _ = result;
        }
        self.settled = true;
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.settle();
    }
}

pub(super) struct Pending {
    command: tokio::process::Command,
    writer: Option<File>,
    request: LaunchRequest,
    cell: Cell,
}
pub(super) fn prepare(
    program: PathBuf,
    arguments: Vec<String>,
    cwd: &Path,
    cache: &Path,
    ready_path: Option<PathBuf>,
) -> Result<Pending, String> {
    let request = prepare_launch_request(program, arguments, cwd, cache, ready_path, &[], &[])?;
    let cell = Cell::new()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: FALSE,
    };
    let (mut read, mut write) = (null_mut(), null_mut());
    // SAFETY: fixed original pipe capacity and valid owned output handles.
    if unsafe { CreatePipe(&mut read, &mut write, &mut attributes, REQUEST_PIPE_BUFFER) } == 0 {
        return Err("sandbox: create preflight request pipe failed".into());
    }
    // SAFETY: transfer each newly-created pipe end to its unique File owner.
    let reader = unsafe { File::from_raw_handle(read) };
    let writer = unsafe { File::from_raw_handle(write) };
    let executable =
        std::env::current_exe().map_err(|_| "sandbox: locate preflight helper failed")?;
    #[cfg(test)]
    let executable = std::env::var_os("MINI_AGENT_TEST_WINDOWS_SANDBOX_EXE")
        .map(PathBuf::from)
        .unwrap_or(executable);
    let mut helper = Command::new(executable);
    helper
        .arg(HELPER_ARG)
        .env_clear()
        .stdin(Stdio::from(reader))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in essential_windows_environment() {
        helper.env(name, value);
    }
    let mut command = tokio::process::Command::from(helper);
    crate::sandbox::configure_child_lifetime(&mut command);
    Ok(Pending {
        command,
        writer: Some(writer),
        request,
        cell,
    })
}
impl Pending {
    pub(super) fn run(
        mut self,
        run: Instant,
        cleanup: Instant,
        diagnostics: &PreflightDiagnostics,
    ) -> Result<ExitStatus, String> {
        let child = spawn_preflight_helper(&mut self.command, run, Some(diagnostics))?;
        let mut actor = Actor {
            child,
            feeder: None,
            cleanup,
            settled: false,
        };
        // Release every parent reader held in Command immediately after spawn,
        // including on duplication, encoding or feeder startup errors.
        self.command.as_std_mut().stdin(Stdio::null());
        let operation = (|| {
            if Instant::now() >= run {
                return Err("sandbox: Windows general AppContainer preflight timed out".into());
            }
            self.request.preflight_witness = Some(self.cell.duplicate_into(&actor.child)?);
            let payload = serde_json::to_vec(&self.request)
                .map_err(|_| "sandbox: encode preflight request failed".to_string())?;
            if payload.len() > REQUEST_MAX_BYTES {
                return Err("sandbox: preflight request exceeds transport bound".into());
            }
            if Instant::now() >= run {
                return Err("sandbox: Windows general AppContainer preflight timed out".into());
            }
            actor.feeder = Some(Feeder::start(
                self.writer.take().expect("owned preflight writer"),
                payload,
            )?);
            wait_bounded_preflight_child(&mut actor.child, run, cleanup, Some(diagnostics))
        })();
        drop(self.writer.take());
        actor.settle();
        diagnostics
            .helper_phase
            .set(Some(self.cell.observe_retired(&actor.child)));
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preflight_helper_observation_decodes_only_closed_stages_and_exact_header() {
        let nonce = [9u8; 16];
        let mut bytes = [0u8; CELL_BYTES];
        bytes[..4].copy_from_slice(&1u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&(CELL_BYTES as u32).to_le_bytes());
        bytes[8..24].copy_from_slice(&nonce);
        for (stage, label) in [
            (0u32, "not_attached"),
            (1, "request"),
            (2, "setup"),
            (3, "launch"),
            (4, "verify_job"),
            (15, "ready"),
            (16, "wait"),
            (17, "exit_code"),
            (18, "drain"),
            (19, "cleanup"),
            (20, "suspended_job"),
            (21, "regular_token_open"),
            (22, "regular_token_sid"),
            (23, "regular_token_duplicate"),
            (24, "regular_token_descriptor"),
            (25, "regular_token_access"),
            (26, "resume"),
        ] {
            bytes[24..28].copy_from_slice(&stage.to_le_bytes());
            assert_eq!(decode(&bytes, &nonce), label);
        }
        for stage in [5u32, 14, 27, u32::MAX] {
            bytes[24..28].copy_from_slice(&stage.to_le_bytes());
            assert_eq!(decode(&bytes, &nonce), "invalid_stage");
        }
        bytes[0] = 2;
        assert_eq!(decode(&bytes, &nonce), "invalid_header");
        bytes[0] = 1;
        assert_eq!(decode(&bytes, &[8u8; 16]), "invalid_header");
    }
    #[test]
    fn ordinary_launch_request_omits_observer_and_diagnostics_are_value_free() {
        let request = LaunchRequest {
            version: REQUEST_VERSION,
            program: PathBuf::from("PRIVATE_PROGRAM"),
            program_proof: FileProof {
                volume_serial_number: 1,
                file_id: [0; 16],
                sha256: [0; 32],
            },
            arguments: vec!["PRIVATE_ARGUMENT".into()],
            cwd: "PRIVATE_WORKSPACE".into(),
            cache: "PRIVATE_CACHE".into(),
            read_roots: vec![],
            write_roots: vec![],
            configured_read_roots: vec![],
            configured_write_roots: vec![],
            parent_pid: 1,
            parent_created: 1,
            ready_path: None,
            preflight_witness: None,
        };
        let mut original = serde_json::to_value(&request).unwrap();
        assert!(original.get("preflight_witness").is_none());
        assert!(
            serde_json::from_value::<LaunchRequest>(original.clone())
                .unwrap()
                .preflight_witness
                .is_none()
        );
        original["preflight_witness"] = serde_json::json!({"handle":12,"nonce":vec![0u8;16]});
        assert!(serde_json::from_value::<LaunchRequest>(original).is_ok());
        let diagnostics = PreflightDiagnostics::new();
        diagnostics.helper_phase.set(Some("regular_token_access"));
        let rendered = diagnostics.render();
        for secret in [
            "PRIVATE_PROGRAM",
            "PRIVATE_ARGUMENT",
            "PRIVATE_WORKSPACE",
            "PRIVATE_CACHE",
            "nonce",
            "handle",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert!(rendered.contains("phase=regular_token_access"));
    }
}
