//! Native test-only scalar witness and request-feeder experiment. No production
//! helper protocol, permission, profile, or availability decision uses this code.
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::*;

const MAX_REQUEST: usize = 24 * 1024;
const CELL_BYTES: usize = 32;
const STAGE_OFFSET: usize = 24;
const STAGE_RESUME: u32 = 26;
static FEEDERS: AtomicUsize = AtomicUsize::new(0);

#[allow(unsafe_code)]
pub fn ticks() -> u64 {
    // SAFETY: this query has no pointer or resource lifetime requirements.
    unsafe { GetTickCount64() }
}
fn remaining(deadline: u64) -> u32 {
    deadline.saturating_sub(ticks()).min(u32::MAX as u64) as u32
}
fn delay(deadline: u64) {
    std::thread::sleep(std::time::Duration::from_millis(
        10.min(remaining(deadline) as u64),
    ));
}

struct Handle(OwnedHandle);
impl Handle {
    #[allow(unsafe_code)]
    fn created(raw: HANDLE) -> Result<Self, &'static str> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err("native_handle_creation_failed");
        }
        // SAFETY: each caller transfers one newly-created native handle.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }
    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }
}
struct View(MEMORY_MAPPED_VIEW_ADDRESS);
impl Drop for View {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: this owner retains exactly one successful mapped view.
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
    #[allow(unsafe_code)]
    fn new() -> Result<Self, &'static str> {
        // SAFETY: unnamed, noninheritable pagefile mapping, fixed nonzero size;
        // no executable protection or filesystem object is involved.
        let handle = Handle::created(unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null(),
                PAGE_READWRITE,
                0,
                CELL_BYTES as u32,
                null(),
            )
        })?;
        // SAFETY: retained mapping grants read/write, and exactly CELL_BYTES
        // are mapped at aligned offset zero.
        let mapped = unsafe { MapViewOfFile(handle.raw(), FILE_MAP_WRITE, 0, 0, CELL_BYTES) };
        if mapped.Value.is_null() {
            return Err("native_mapping_failed");
        }
        let nonce = *uuid::Uuid::new_v4().as_bytes();
        let view = View(mapped);
        // SAFETY: initialization occurs before transfer; there are no Rust
        // references into the foreign mapping and no other writer exists.
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
        assert_eq!((view.0.Value as usize + STAGE_OFFSET) % 4, 0);
        Ok(Self {
            handle,
            view,
            nonce,
        })
    }
    #[allow(unsafe_code)]
    fn duplicate_into(&self, child: &Child) -> Result<usize, &'static str> {
        let mut remote = null_mut();
        // SAFETY: both retained handles are live; the output is a target-local
        // handle, not adopted or closed in this process. It is noninheritable.
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
            return Err("native_mapping_duplicate_failed");
        }
        Ok(remote as usize)
    }
    #[allow(unsafe_code)]
    fn observe_after_retirement(&self, owner: &Actor) -> Result<&'static str, &'static str> {
        if !owner.settled {
            return Err("observation_before_retirement_refused");
        }
        // SAFETY: exact helper is terminal; no concurrent reader/writer or Rust
        // reference exists. Immutable header preceded transfer. DWORD aligned.
        let bytes =
            unsafe { std::slice::from_raw_parts(self.view.0.Value.cast::<u8>(), CELL_BYTES) };
        if bytes[..4] != 1u32.to_le_bytes()
            || bytes[4..8] != (CELL_BYTES as u32).to_le_bytes()
            || bytes[8..24] != self.nonce
        {
            return Ok("invalid_header");
        }
        // SAFETY: same post-retirement view/alignment argument; one DWORD read.
        let phase = unsafe {
            std::ptr::read_volatile(
                self.view
                    .0
                    .Value
                    .cast::<u8>()
                    .add(STAGE_OFFSET)
                    .cast::<u32>(),
            )
        };
        Ok(match phase {
            0 => "not_attached",
            STAGE_RESUME => "resume",
            _ => "invalid_stage",
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    version: u32,
    mode: String,
    mapping: usize,
    nonce: [u8; 16],
    run: u64,
    cleanup: u64,
    padding: Vec<u8>,
}
fn read_packet() -> Result<Packet, &'static str> {
    let mut size = [0; 4];
    std::io::stdin()
        .read_exact(&mut size)
        .map_err(|_| "packet_length_read_failed")?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > MAX_REQUEST {
        return Err("packet_size_refused");
    }
    let mut bytes = vec![0; size];
    std::fs::write("phase-length-read", b"read").map_err(|_| "length_ready_failed")?;
    std::io::stdin()
        .read_exact(&mut bytes)
        .map_err(|_| "whole_packet_read_failed")?;
    let packet: Packet = serde_json::from_slice(&bytes).map_err(|_| "packet_parse_failed")?;
    if packet.version != 1
        || packet.cleanup != packet.run.checked_add(5000).ok_or("deadline_overflow")?
        || remaining(packet.run) == 0
        || !matches!(
            packet.mode.as_str(),
            "known" | "noinherit" | "invalid" | "none"
        )
    {
        return Err("packet_refused");
    }
    Ok(packet)
}

struct Feeder {
    thread: Option<JoinHandle<std::io::Result<()>>>,
    cancelled: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
}
impl Feeder {
    fn start(writer: std::fs::File, bytes: Vec<u8>) -> Result<Self, &'static str> {
        let announced = bytes.len();
        Self::start_packet(writer, bytes, announced, false)
    }
    fn start_packet(
        mut writer: std::fs::File,
        bytes: Vec<u8>,
        announced: usize,
        hold: bool,
    ) -> Result<Self, &'static str> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let entered = Arc::new(AtomicBool::new(false));
        let active = entered.clone();
        FEEDERS.fetch_add(1, Ordering::AcqRel);
        let thread = std::thread::Builder::new()
            .name("phase108-owned-feeder".into())
            .spawn(move || {
                struct Permit;
                impl Drop for Permit {
                    fn drop(&mut self) {
                        FEEDERS.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _permit = Permit;
                if flag.load(Ordering::Acquire) {
                    return Err(std::io::Error::from_raw_os_error(
                        ERROR_OPERATION_ABORTED as i32,
                    ));
                }
                active.store(true, Ordering::Release);
                writer.write_all(&(announced as u32).to_le_bytes())?;
                if flag.load(Ordering::Acquire) {
                    return Err(std::io::Error::from_raw_os_error(
                        ERROR_OPERATION_ABORTED as i32,
                    ));
                }
                writer.write_all(&bytes)?;
                while hold && !flag.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(())
            })
            .map_err(|_| {
                FEEDERS.fetch_sub(1, Ordering::AcqRel);
                "feeder_spawn_failed"
            })?;
        Ok(Self {
            thread: Some(thread),
            cancelled,
            entered,
        })
    }
    #[allow(unsafe_code)]
    fn finish_delivery(&mut self, run: u64) -> Result<(), &'static str> {
        let thread = self.thread.as_ref().ok_or("feeder_missing")?;
        // SAFETY: exact retained thread; the original run deadline includes
        // this delivery completion, not a fresh five-second wait.
        if unsafe { WaitForSingleObject(thread.as_raw_handle(), remaining(run)) } != WAIT_OBJECT_0 {
            return Err("delivery_deadline");
        }
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| "feeder_panicked")?
            .map_err(|_| "delivery_failed")
    }
    #[allow(unsafe_code)]
    fn cancel_actual_pending_write(&mut self, run: u64) -> Result<(), &'static str> {
        let thread = self.thread.as_ref().ok_or("feeder_missing")?;
        while remaining(run) > 0 {
            if thread.is_finished() {
                return Err("pending_write_not_observed");
            }
            if self.entered.load(Ordering::Acquire) {
                // SAFETY: exact JoinHandle remains owned. Success witnesses an
                // actual pending synchronous I/O cancellation, not a sleep.
                if unsafe { CancelSynchronousIo(thread.as_raw_handle()) } != 0 {
                    self.cancelled.store(true, Ordering::Release);
                    return Ok(());
                }
                if unsafe { GetLastError() } != ERROR_NOT_FOUND {
                    return Err("pending_cancel_failed");
                }
            }
            delay(run);
        }
        Err("run_deadline")
    }
    #[allow(unsafe_code)]
    fn settle(&mut self, cleanup: u64) -> Result<Option<i32>, &'static str> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(None);
        };
        self.cancelled.store(true, Ordering::Release);
        loop {
            // SAFETY: JoinHandle retains the exact thread handle while borrowed.
            let signal = unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) };
            if signal == WAIT_OBJECT_0 {
                break;
            }
            if signal != WAIT_TIMEOUT || remaining(cleanup) == 0 {
                return Err("uncertain_cleanup");
            }
            // SAFETY: exact owned thread only; cancellation is not settlement.
            if unsafe { CancelSynchronousIo(thread.as_raw_handle()) } == 0
                && unsafe { GetLastError() } != ERROR_NOT_FOUND
            {
                return Err("feeder_cancel_failed");
            }
            delay(cleanup);
        }
        let joined = self
            .thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| "feeder_panicked")?;
        match joined {
            Ok(()) => Ok(None),
            Err(error) => Ok(Some(
                error.raw_os_error().ok_or("unclassified_feeder_error")?,
            )),
        }
    }
}
impl Drop for Feeder {
    fn drop(&mut self) {
        if self.thread.is_some() {
            // Never turn an unsettled owner into a detached thread. This code
            // runs only in an already Job-owned inert fixture child.
            eprintln!("PHASE108 uncertain_cleanup");
            std::process::abort();
        }
    }
}
struct Actor {
    child: Child,
    feeder: Option<Feeder>,
    cleanup: u64,
    settled: bool,
    exit: Option<i32>,
}
impl Actor {
    #[allow(unsafe_code)]
    fn spawn(mode: &str, run: u64, cleanup: u64) -> Result<(Self, std::fs::File), &'static str> {
        if remaining(run) == 0 {
            return Err("run_deadline");
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: FALSE,
        };
        let (mut read, mut write) = (null_mut(), null_mut());
        // SAFETY: output pointers valid, noninheritable pipe, existing512 bound.
        if unsafe { CreatePipe(&mut read, &mut write, &attributes, 512) } == 0 {
            return Err("pipe_creation_failed");
        }
        // SAFETY: transfer each newly-created pipe end exactly once.
        let reader = unsafe { std::fs::File::from_raw_handle(read) };
        let writer = unsafe { std::fs::File::from_raw_handle(write) };
        for marker in ["phase-ready", "phase-length-read"] {
            if Path::new(marker).exists() {
                std::fs::remove_file(marker).map_err(|_| "stale_ready_remove_failed")?;
            }
        }
        let image = std::env::current_exe().map_err(|_| "image_missing")?;
        let mut command = Command::new(image);
        command
            .args(["--phase-helper", mode])
            .stdin(Stdio::from(reader))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().map_err(|_| "helper_spawn_failed")?;
        let actor = Self {
            child,
            feeder: None,
            cleanup,
            settled: false,
            exit: None,
        };
        // Close the final parent-held read-end inside Command immediately after
        // spawn, before duplicate/encode failures or feeder publication.
        command.stdin(Stdio::null());
        drop(command);
        Ok((actor, writer))
    }
    fn feed(&mut self, writer: std::fs::File, bytes: Vec<u8>) -> Result<(), &'static str> {
        if bytes.len() > MAX_REQUEST {
            return Err("packet_size_refused");
        }
        self.feeder = Some(Feeder::start(writer, bytes)?);
        Ok(())
    }
    fn wait_ready(&mut self, run: u64) -> Result<(), &'static str> {
        self.wait_file("phase-ready", run)
    }
    fn wait_file(&mut self, marker: &str, run: u64) -> Result<(), &'static str> {
        while !Path::new(marker).is_file() {
            if remaining(run) == 0 {
                return Err("run_deadline");
            }
            if self
                .child
                .try_wait()
                .map_err(|_| "helper_poll_failed")?
                .is_some()
            {
                return Err("helper_exited_before_ready");
            }
            delay(run);
        }
        Ok(())
    }
    fn wait_normal(&mut self, run: u64) -> Result<(), &'static str> {
        loop {
            match self.child.try_wait().map_err(|_| "helper_poll_failed")? {
                Some(status) if status.code() == Some(0) => return Ok(()),
                Some(_) => return Err("helper_normal_status_failed"),
                None if remaining(run) == 0 => return Err("run_deadline"),
                None => delay(run),
            }
        }
    }
    #[allow(unsafe_code)]
    fn retire(&mut self) -> Result<Option<i32>, &'static str> {
        if self.settled {
            return Ok(None);
        }
        // SAFETY: exact retained child capability; kill is followed by signal.
        if self
            .child
            .try_wait()
            .map_err(|_| "helper_poll_failed")?
            .is_none()
        {
            if unsafe { TerminateProcess(self.child.as_raw_handle(), 125) } == 0 {
                return Err("helper_kill_failed");
            }
        }
        let wait = remaining(self.cleanup);
        // SAFETY: retained process handle remains live throughout bounded wait.
        if unsafe { WaitForSingleObject(self.child.as_raw_handle(), wait) } != WAIT_OBJECT_0 {
            return Err("uncertain_cleanup");
        }
        self.exit = self.child.wait().map_err(|_| "helper_reap_failed")?.code();
        let result = match &mut self.feeder {
            Some(feeder) => feeder.settle(self.cleanup)?,
            None => None,
        };
        if FEEDERS.load(Ordering::Acquire) != 0 {
            return Err("feeder_owner_not_retired");
        }
        self.settled = true;
        Ok(result)
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        if !self.settled && self.retire().is_err() {
            // Keep exact owners live until fatal process termination. Outer Job
            // owner must prove the entire fixture family terminal before delete.
            eprintln!("PHASE108 uncertain_cleanup");
            std::process::abort();
        }
    }
}

#[allow(unsafe_code)]
fn map_packet(packet: &Packet) -> Result<Option<(Handle, View)>, &'static str> {
    if packet.mapping == 0 {
        return Ok(None);
    }
    let raw = packet.mapping as HANDLE;
    let mut flags = 0;
    // SAFETY: caller is the receiving fixture process; validate before adoption.
    if unsafe { GetHandleInformation(raw, &mut flags) } == 0 || flags & HANDLE_FLAG_INHERIT != 0 {
        return Err("observer_handle_refused");
    }
    // SAFETY: mapping API checks handle type/access and fixed requested range.
    let mapped = unsafe { MapViewOfFile(raw, FILE_MAP_WRITE, 0, 0, CELL_BYTES) };
    if mapped.Value.is_null() {
        return Err("observer_mapping_refused");
    }
    let view = View(mapped);
    // SAFETY: successful fixed32 view; immutable bytes initialized pretransfer.
    let bytes = unsafe { std::slice::from_raw_parts(view.0.Value.cast::<u8>(), CELL_BYTES) };
    if bytes[..4] != 1u32.to_le_bytes()
        || bytes[4..8] != (CELL_BYTES as u32).to_le_bytes()
        || bytes[8..24] != packet.nonce
    {
        return Err("observer_header_refused");
    }
    Ok(Some((Handle::created(raw)?, view)))
}
#[allow(unsafe_code)]
fn publish(view: &View, stage: u32) {
    assert_eq!((view.0.Value as usize + STAGE_OFFSET) % 4, 0);
    // SAFETY: sole helper writer; parent never reads before helper retirement;
    // one aligned DWORD store, no Rust reference or multiword publication.
    unsafe {
        std::ptr::write_volatile(
            view.0.Value.cast::<u8>().add(STAGE_OFFSET).cast::<u32>(),
            stage,
        );
    }
}

pub fn helper(mode: &str) -> Result<i32, &'static str> {
    if mode == "hold_reader" {
        std::fs::write("phase-ready", b"ready").map_err(|_| "ready_failed")?;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    if mode != "packet" {
        return Err("helper_mode_refused");
    }
    let packet = read_packet()?;
    let observer = map_packet(&packet)?;
    // The only target launch follows successful entire bounded typed packet.
    if matches!(packet.mode.as_str(), "noinherit" | "invalid" | "none") {
        launch_target(&packet)?;
    }
    if let Some((_, view)) = &observer {
        publish(
            view,
            if packet.mode == "invalid" {
                u32::MAX
            } else {
                STAGE_RESUME
            },
        );
    }
    std::fs::write("phase-ready", b"ready").map_err(|_| "ready_failed")?;
    if packet.mode == "known" {
        while remaining(packet.run) > 0 {
            delay(packet.run);
        }
        return Err("run_deadline");
    }
    Ok(0)
}

#[allow(unsafe_code)]
fn duplicate_inheritable(raw: HANDLE) -> Result<Handle, &'static str> {
    let mut duplicate = null_mut();
    // SAFETY: retain stdio owners across duplicate/launch, exact local process.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            raw,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            TRUE,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err("stdio_duplicate_failed");
    }
    Handle::created(duplicate)
}
#[allow(unsafe_code)]
fn launch_target(packet: &Packet) -> Result<(), &'static str> {
    let image = std::env::current_exe().map_err(|_| "image_missing")?;
    let application: Vec<u16> = image.as_os_str().encode_wide().chain(Some(0)).collect();
    // Fixed image and fixed literal roles only; no user-supplied executable or
    // general argv builder. Decimal observer handle is diagnostic metadata.
    let mut command: Vec<u16> =
        format!("\"{}\" --phase-target {}", image.display(), packet.mapping)
            .encode_utf16()
            .chain(Some(0))
            .collect();
    let input = std::fs::File::open("NUL").map_err(|_| "null_input_failed")?;
    let output = std::fs::OpenOptions::new()
        .write(true)
        .open("NUL")
        .map_err(|_| "null_output_failed")?;
    let stdin = duplicate_inheritable(input.as_raw_handle())?;
    let stdout = duplicate_inheritable(output.as_raw_handle())?;
    let stderr = duplicate_inheritable(output.as_raw_handle())?;
    let handles = [stdin.raw(), stdout.raw(), stderr.raw()];
    let mut size = 0;
    // SAFETY: standard sizing call, no attribute list exists yet.
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
    }
    if size == 0 {
        return Err("attribute_size_failed");
    }
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let list = storage.as_mut_ptr().cast();
    // SAFETY: aligned buffer has the exact requested byte capacity.
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err("attribute_init_failed");
    }
    struct Attributes(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for Attributes {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: exactly one initialized list retained by this guard.
            unsafe {
                DeleteProcThreadAttributeList(self.0);
            }
        }
    }
    let _attributes = Attributes(list);
    // SAFETY: only retained three stdio handles, mapping excluded explicitly;
    // array/storage/owners outlive CreateProcessW.
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast_mut().cast(),
            size_of::<[HANDLE; 3]>(),
            null_mut(),
            null_mut(),
        )
    } == 0
    {
        return Err("attribute_handles_failed");
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.raw();
    startup.StartupInfo.hStdOutput = stdout.raw();
    startup.StartupInfo.hStdError = stderr.raw();
    startup.lpAttributeList = list;
    let mut information = PROCESS_INFORMATION::default();
    if remaining(packet.run) == 0 {
        return Err("run_deadline");
    }
    // SAFETY: fixed copied image/role, mutable NUL-terminated command, exact
    // retained stdio list. Target inherits enclosing fixture Job, no breakaway.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &startup.StartupInfo,
            &mut information,
        )
    } == 0
    {
        return Err("target_spawn_failed");
    }
    let process = Handle::created(information.hProcess)?;
    let _thread = Handle::created(information.hThread)?;
    // SAFETY: exact process retained; same original run/cleanup, no fresh wait.
    if unsafe { WaitForSingleObject(process.raw(), remaining(packet.run)) } != WAIT_OBJECT_0 {
        unsafe {
            TerminateProcess(process.raw(), 125);
        }
        if unsafe { WaitForSingleObject(process.raw(), remaining(packet.cleanup)) } != WAIT_OBJECT_0
        {
            eprintln!("PHASE108 uncertain_cleanup");
            std::process::abort();
        }
        return Err("target_deadline");
    }
    let mut code = 0;
    // SAFETY: exact terminal process handle, output scalar valid.
    if unsafe { GetExitCodeProcess(process.raw(), &mut code) } == 0 || code != 0 {
        return Err("target_failed");
    }
    Ok(())
}
#[allow(unsafe_code)]
pub fn target(raw: usize) -> Result<i32, &'static str> {
    if raw != 0 {
        // SAFETY: bounded fixed-range mapping attempt only; no bytes/logging
        // from arbitrary memory. Same exact target must lack the observer.
        let view = unsafe { MapViewOfFile(raw as HANDLE, FILE_MAP_WRITE, 0, 0, CELL_BYTES) };
        if !view.Value.is_null() {
            unsafe {
                UnmapViewOfFile(view);
            }
            return Err("observer_inherited");
        }
        if unsafe { GetLastError() } != ERROR_INVALID_HANDLE {
            return Err("unexpected_target_handle_error");
        }
    }
    std::fs::write("phase-effect", b"one").map_err(|_| "effect_failed")?;
    Ok(0)
}

pub fn probe(case: &str, run: u64, cleanup: u64) -> Result<i32, &'static str> {
    if cleanup != run.checked_add(5000).ok_or("deadline_overflow")?
        || remaining(run) == 0
        || remaining(cleanup) > 10000
    {
        return Err("deadline_refused");
    }
    match case {
        "known" | "noinherit" | "invalid" | "none" => {
            let cell = Cell::new()?;
            let (mut actor, writer) = Actor::spawn("packet", run, cleanup)?;
            let remote = if case == "none" {
                0
            } else {
                cell.duplicate_into(&actor.child)?
            };
            let packet = Packet {
                version: 1,
                mode: case.into(),
                mapping: remote,
                nonce: cell.nonce,
                run,
                cleanup,
                padding: Vec::new(),
            };
            actor.feed(
                writer,
                serde_json::to_vec(&packet).map_err(|_| "packet_encode_failed")?,
            )?;
            actor.wait_ready(run)?;
            actor
                .feeder
                .as_mut()
                .ok_or("feeder_missing")?
                .finish_delivery(run)?;
            if case != "known" {
                actor.wait_normal(run)?;
            }
            // No cell read until both exact helper and feeder are terminal.
            let feeder_error = actor.retire()?;
            if feeder_error.is_some() {
                return Err("valid_packet_feeder_failed");
            }
            let observation = cell.observe_after_retirement(&actor)?;
            let effect = Path::new("phase-effect").is_file();
            let expected_exit = if case == "known" { 125 } else { 0 };
            if actor.exit != Some(expected_exit) {
                return Err("helper_terminal_exit_differs");
            }
            println!(
                "{}",
                serde_json::json!({"observation":observation,"effect":effect,"feeder_zero":FEEDERS.load(Ordering::Acquire)==0,"actor_terminal":actor.settled,"helper_exit":actor.exit})
            );
        }
        "errors" => {
            let mut completed = Vec::new();
            for fault in [
                "duplicate",
                "encode",
                "launch",
                "kill",
                "partial",
                "pending_writer",
            ] {
                if fault == "launch" {
                    invalid_launch(cleanup)?;
                    completed.push(fault);
                    continue;
                }
                let (mut actor, writer) = Actor::spawn(
                    if fault == "partial" {
                        "packet"
                    } else {
                        "hold_reader"
                    },
                    run,
                    cleanup,
                )?;
                if fault != "partial" {
                    actor.wait_ready(run)?;
                }
                match fault {
                    "duplicate" => {
                        let cell = Cell::new()?;
                        // Actual invalid source handle duplication fails; actor
                        // still owns the child and no feeder/effect can escape.
                        invalid_duplicate(&actor.child, &cell)?;
                        drop(writer);
                    }
                    "encode" => {
                        let packet = Packet {
                            version: 1,
                            mode: "noinherit".into(),
                            mapping: 0,
                            nonce: [0; 16],
                            run,
                            cleanup,
                            padding: vec![0; MAX_REQUEST],
                        };
                        let bytes =
                            serde_json::to_vec(&packet).map_err(|_| "packet_encode_failed")?;
                        if bytes.len() <= MAX_REQUEST {
                            return Err("oversize_not_observed");
                        }
                        if actor.feed(writer, bytes) != Err("packet_size_refused") {
                            return Err("oversize_publication_not_refused");
                        }
                    }
                    "kill" => {
                        invalid_kill()?;
                        drop(writer);
                    }
                    "partial" => {
                        let packet = Packet {
                            version: 1,
                            mode: "none".into(),
                            mapping: 0,
                            nonce: [0; 16],
                            run,
                            cleanup,
                            padding: Vec::new(),
                        };
                        let mut bytes =
                            serde_json::to_vec(&packet).map_err(|_| "packet_encode_failed")?;
                        let declared = bytes.len();
                        bytes.pop();
                        actor.feeder = Some(Feeder::start_packet(writer, bytes, declared, true)?);
                        actor.wait_file("phase-length-read", run)?;
                        if actor
                            .child
                            .try_wait()
                            .map_err(|_| "helper_poll_failed")?
                            .is_some()
                        {
                            return Err("partial_reader_not_pending");
                        }
                    }
                    "pending_writer" => {
                        actor.feed(writer, vec![b'x'; MAX_REQUEST])?;
                        // This case intentionally holds the only reader in the
                        // helper. Cancellation/kill must settle exact writer.
                        actor
                            .feeder
                            .as_mut()
                            .ok_or("feeder_missing")?
                            .cancel_actual_pending_write(run)?;
                    }
                    _ => unreachable!(),
                }
                let error = actor.retire()?;
                if error.is_some_and(|code| !matches!(code, 995 | 109)) {
                    return Err("unexpected_feeder_status");
                }
                if Path::new("phase-effect").exists() {
                    return Err("partial_request_effect");
                }
                completed.push(fault);
            }
            println!(
                "{}",
                serde_json::json!({"error_cases":completed.len(),"completed":completed,"effect":false,"feeder_zero":FEEDERS.load(Ordering::Acquire)==0,"actor_terminal":true})
            );
        }
        _ => return Err("probe_case_refused"),
    }
    Ok(0)
}
#[allow(unsafe_code)]
fn invalid_duplicate(child: &Child, _cell: &Cell) -> Result<(), &'static str> {
    let mut remote = null_mut();
    // SAFETY: deliberate null source handle (not the -1 process pseudo-handle), exact retained target process;
    // failure output is never adopted/closed in either process.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            null_mut(),
            child.as_raw_handle(),
            &mut remote,
            FILE_MAP_READ,
            FALSE,
            0,
        )
    } != 0
    {
        return Err("invalid_duplicate_succeeded");
    }
    if unsafe { GetLastError() } != ERROR_INVALID_HANDLE {
        return Err("unexpected_duplicate_error");
    }
    Ok(())
}

#[allow(unsafe_code)]
fn invalid_kill() -> Result<(), &'static str> {
    // SAFETY: deliberate null process handle cannot signal any process. The
    // actual helper remains retained and is retired by its owner afterward.
    if unsafe { TerminateProcess(null_mut(), 125) } != 0 {
        return Err("invalid_kill_succeeded");
    }
    if unsafe { GetLastError() } != ERROR_INVALID_HANDLE {
        return Err("unexpected_kill_error");
    }
    Ok(())
}

#[allow(unsafe_code)]
fn invalid_launch(cleanup: u64) -> Result<(), &'static str> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let image = std::env::current_exe().map_err(|_| "image_missing")?;
    let parent_path = image.parent().ok_or("image_parent_missing")?;
    let parent_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent_path)
        .map_err(|_| "image_parent_open_failed")?;
    let metadata = parent_file
        .metadata()
        .map_err(|_| "image_parent_query_failed")?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("image_parent_refused");
    }
    let parent = cap_std::fs::Dir::from_std_file(parent_file);
    const MISSING: &str = "phase108-missing.exe";
    // Fixed sibling only, resolved relative to the retained directory. This is
    // an inert fixture absence check, not an exact-object executable authority.
    match parent.open(MISSING) {
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {}
        Err(_) => return Err("missing_image_absence_error"),
        Ok(_) => return Err("missing_image_present"),
    }
    let image = parent_path.join(MISSING);
    let application: Vec<u16> = image.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command: Vec<u16> = format!("\"{}\"", image.display())
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // The previous cb=0 injector was falsified by native Windows accepting it.
    // Correct cb and a fully-qualified non-null application prevent PATH search.
    // CREATE_SUSPENDED prevents execution if the absent namespace changes.
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..STARTUPINFOW::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: fixed owned-fixture sibling path, valid buffers/output/structure;
    // retained no-share-delete parent remains live throughout the call.
    let launched = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            FALSE,
            CREATE_SUSPENDED,
            null(),
            null(),
            &startup,
            &mut info,
        )
    };
    if launched != 0 {
        let process = Handle::created(info.hProcess)?;
        let _thread = Handle::created(info.hThread)?;
        // SAFETY: unexpected suspended fixed-image child remains exactly owned.
        unsafe {
            TerminateProcess(process.raw(), 125);
        }
        if unsafe { WaitForSingleObject(process.raw(), remaining(cleanup)) } != WAIT_OBJECT_0 {
            eprintln!("PHASE108 uncertain_cleanup");
            std::process::abort();
        }
        drop(parent);
        return Err("invalid_launch_succeeded");
    }
    if unsafe { GetLastError() } != ERROR_FILE_NOT_FOUND {
        return Err("unexpected_launch_error");
    }
    drop(parent);
    Ok(())
}
