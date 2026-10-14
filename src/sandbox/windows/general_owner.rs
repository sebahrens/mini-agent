//! Exact family ownership for ordinary general processes. The separate
//! AppContainer/worker launchers keep their existing creation-time contracts.
use super::*;
use std::sync::{Mutex, atomic::AtomicBool};
use tokio::process::{Child as TokioChild, Command as TokioCommand};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::JobObjectBasicProcessIdList;
use windows_sys::Win32::System::Threading::{
    GetProcessId, GetProcessIdOfThread, GetThreadId, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    THREAD_SUSPEND_RESUME,
};

const MAX_CAPTURED_MEMBERS: usize = 4096;

pub(crate) struct GeneralOwner {
    job: Handle,
    leader: Handle,
    pid: u32,
    stopped: AtomicBool,
    changed: tokio::sync::Notify,
    retirement: Mutex<Retirement>,
}

struct Retirement {
    deadline: Option<Instant>,
    members: Vec<Handle>,
    prepared: bool,
    settled: bool,
    failure: Option<String>,
}

impl std::fmt::Debug for GeneralOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GeneralOwner { retained process family }")
    }
}

struct SuspendedChild {
    child: Option<TokioChild>,
    owner: Option<Arc<GeneralOwner>>,
}
impl Drop for SuspendedChild {
    fn drop(&mut self) {
        // After registration, even a construction failure fences/captures the
        // Job before terminating the leader. Do not let Child's kill-on-drop
        // skip the retained family retirement boundary on unwind.
        if let Some(owner) = &self.owner {
            owner.request_stop();
            let _ = owner.retire();
        }
        if let Some(child) = &mut self.child {
            if let Some(process) = child.raw_handle() {
                // SAFETY: the child pins this process handle throughout cleanup.
                unsafe {
                    TerminateProcess(process, 125);
                    WaitForSingleObject(process, drain_millis());
                }
            }
            let _ = child.start_kill();
        }
    }
}

fn drain_millis() -> u32 {
    crate::sandbox::PROCESS_GROUP_DRAIN_BUDGET.as_millis() as u32
}

fn check_setup(deadline: Instant, cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        return Err("sandbox: general process cancelled before execution".into());
    }
    if Instant::now() >= deadline {
        return Err("sandbox: general process construction deadline elapsed".into());
    }
    if let Some(run) = crate::spend::run::Run::current()
        && run.admit_effect().is_err()
    {
        return Err("sandbox: captured spend admission refused general execution".into());
    }
    Ok(())
}

impl GeneralOwner {
    pub(crate) fn launch(
        command: &mut TokioCommand,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
        register: impl FnOnce(Arc<Self>) -> Result<(), String>,
    ) -> Result<(TokioChild, Arc<Self>), String> {
        Self::spawn_inner(
            command,
            deadline,
            cancelled,
            register,
            #[cfg(test)]
            None,
        )
    }

    fn spawn_inner(
        command: &mut TokioCommand,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
        register: impl FnOnce(Arc<Self>) -> Result<(), String>,
        #[cfg(test)] probe: Option<&ConstructionProbe>,
    ) -> Result<(TokioChild, Arc<Self>), String> {
        check_setup(deadline, &cancelled)?;
        let job = Handle::created(
            unsafe { CreateJobObjectW(null(), null()) },
            "general Job creation",
        )?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(last_error("general Job limit setup"));
        }
        check_setup(deadline, &cancelled)?;
        // General constructors currently author no additional Windows flags.
        // AppContainer helper commands never enter this method, preserving
        // their separate BREAKAWAY flag and creation-time target Job.
        command.creation_flags(CREATE_SUSPENDED).kill_on_drop(true);
        crate::process_creation::launch_tokio_with_creation_guard_until(
            command,
            deadline,
            |child| {
                let mut child = SuspendedChild {
                    child: Some(child),
                    owner: None,
                };
                // The child is still suspended. Retain/register its exact
                // owner before checking spend changed during the gate wait.
                let raw = child
                    .child
                    .as_ref()
                    .unwrap()
                    .raw_handle()
                    .ok_or("sandbox: general process handle unavailable")?;
                // SAFETY: the suspended child pins the borrowed process handle.
                let leader = Handle(
                    unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(raw) }
                        .try_clone_to_owned()
                        .map_err(|_| "sandbox: general process handle retention failed")?,
                );
                let pid = unsafe { GetProcessId(leader.raw()) };
                if pid == 0 || child.child.as_ref().unwrap().id() != Some(pid) {
                    return Err("sandbox: general process identity unavailable".into());
                }
                let owner = Arc::new(Self {
                    job,
                    leader,
                    pid,
                    stopped: AtomicBool::new(false),
                    changed: tokio::sync::Notify::new(),
                    retirement: Mutex::new(Retirement {
                        deadline: None,
                        members: Vec::new(),
                        prepared: false,
                        settled: false,
                        failure: None,
                    }),
                });
                child.owner = Some(Arc::clone(&owner));
                #[cfg(test)]
                if let Some(probe) = probe {
                    // Retain the actual child before any injected failure. The test
                    // checks its termination signal after normal error-path cleanup.
                    *probe.leader.lock().unwrap() = Some(Handle(
                        unsafe {
                            std::os::windows::io::BorrowedHandle::borrow_raw(owner.leader.raw())
                        }
                        .try_clone_to_owned()
                        .unwrap(),
                    ));
                }
                // Capture ownership before any fallible thread/membership query.
                // A concurrent stop uses this exact leader even before assignment.
                register(Arc::clone(&owner))?;
                check_setup(deadline, &|| cancelled() || owner.is_stopped())?;
                #[cfg(test)]
                if let Some(probe) = probe {
                    if matches!(probe.fault, ConstructionFault::AbsentThread) {
                        initial_thread(u32::MAX, deadline)?;
                        return Err("test impossible thread owner unexpectedly enumerated".into());
                    }
                }
                let thread = initial_thread(pid, deadline)?;
                #[cfg(test)]
                if let Some(probe) = probe {
                    match probe.fault {
                        ConstructionFault::Assignment => {
                            if unsafe { AssignProcessToJobObject(null_mut(), owner.leader.raw()) }
                                != 0
                            {
                                return Err("invalid assignment unexpectedly succeeded".into());
                            }
                            return Err("sandbox: actual invalid Job assignment refused".into());
                        }
                        ConstructionFault::WrongThreadOwner => {
                            let wrong = Handle::created(
                                unsafe {
                                    OpenThread(
                                        THREAD_QUERY_LIMITED_INFORMATION,
                                        FALSE,
                                        windows_sys::Win32::System::Threading::GetCurrentThreadId(),
                                    )
                                },
                                "test parent thread",
                            )?;
                            validate_thread_owner(wrong.raw(), pid)?;
                            unreachable!("parent thread cannot belong to newly suspended child");
                        }
                        ConstructionFault::MultipleThreads => {
                            let mut found = Some(thread);
                            let duplicate = initial_thread(pid, deadline)?;
                            accept_unique_thread(&mut found, duplicate)?;
                            unreachable!("two candidates must fail uniqueness");
                        }
                        _ => {}
                    }
                }
                check_setup(deadline, &cancelled)?;
                if unsafe { AssignProcessToJobObject(owner.job.raw(), owner.leader.raw()) } == 0 {
                    return Err(last_error("general suspended Job assignment"));
                }
                owner.verify_membership(owner.leader.raw())?;
                let mut checked_limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                if unsafe {
                    QueryInformationJobObject(
                        owner.job.raw(),
                        JobObjectExtendedLimitInformation,
                        (&mut checked_limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        size_of_val(&checked_limits) as u32,
                        null_mut(),
                    )
                } == 0
                    || checked_limits.BasicLimitInformation.LimitFlags
                        != JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                {
                    return Err("sandbox: general creation Job limits differ".into());
                }
                if active_job_processes(&owner.job)? != 1 {
                    return Err("sandbox: general suspended Job membership count differs".into());
                }
                #[cfg(test)]
                if probe.is_some_and(|probe| matches!(probe.fault, ConstructionFault::ResumeCount))
                    && unsafe { windows_sys::Win32::System::Threading::SuspendThread(thread.raw()) }
                        != 1
                {
                    return Err("test initial suspend count injection failed".into());
                }
                check_setup(deadline, &|| cancelled() || owner.is_stopped())?;
                validate_thread_owner(thread.raw(), pid)?;
                if unsafe { ResumeThread(thread.raw()) } != 1 {
                    return Err("sandbox: general initial thread resume proof failed".into());
                }
                // Transfer the live child and retained owner together. Only
                // unsuccessful construction keeps the local cleanup guard armed.
                child.owner = None;
                Ok((child.child.take().unwrap(), owner))
            },
        )
        .map_err(|error| error.to_string())
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub(crate) async fn cancelled(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            changed.await;
        }
    }
    pub(crate) fn failed_retirement(&self) -> bool {
        let state = self.retirement.lock().unwrap_or_else(|e| e.into_inner());
        state.failure.is_some()
            || (state.prepared
                && !state.settled
                && state
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline))
    }
    pub(crate) fn record_failure(&self, reason: &'static str) {
        let mut state = self.retirement.lock().unwrap_or_else(|e| e.into_inner());
        state.failure.get_or_insert_with(|| reason.to_string());
        state.settled = false;
    }
    pub(crate) fn drain_deadline(&self) -> Option<Instant> {
        self.retirement
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deadline
    }

    fn verify_membership(&self, process: HANDLE) -> Result<(), String> {
        let mut member = FALSE;
        if unsafe { IsProcessInJob(process, self.job.raw(), &mut member) } == 0 || member != TRUE {
            return Err("sandbox: retained general process Job membership unavailable".into());
        }
        Ok(())
    }

    fn snapshot_members(&self, state: &mut Retirement) -> Result<(), String> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 0;
        if unsafe {
            SetInformationJobObject(
                self.job.raw(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(last_error("general retirement membership fence"));
        }
        let mut checked = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                self.job.raw(),
                JobObjectExtendedLimitInformation,
                (&mut checked as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&checked) as u32,
                null_mut(),
            )
        } == 0
            || checked.BasicLimitInformation.LimitFlags != limits.BasicLimitInformation.LimitFlags
            || checked.BasicLimitInformation.ActiveProcessLimit != 0
        {
            return Err("sandbox: general retirement membership fence differs".into());
        }
        // Aligned, initialized storage for two u32 header fields followed by
        // MAX_CAPTURED_MEMBERS ULONG_PTR entries; no unbounded OS allocation.
        let header_words = 8usize.div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; MAX_CAPTURED_MEMBERS + header_words];
        if unsafe {
            QueryInformationJobObject(
                self.job.raw(),
                JobObjectBasicProcessIdList,
                storage.as_mut_ptr().cast(),
                (storage.len() * size_of::<usize>()) as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error("general retirement member list"));
        }
        // SAFETY: the initialized aligned allocation covers the checked header
        // and fixed-capacity payload supplied to QueryInformationJobObject.
        let list = unsafe {
            &*(storage
                .as_ptr()
                .cast::<windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_PROCESS_ID_LIST>())
        };
        let count = list.NumberOfProcessIdsInList as usize;
        if count > MAX_CAPTURED_MEMBERS || list.NumberOfAssignedProcesses as usize != count {
            return Err("sandbox: general retirement member list incomplete".into());
        }
        for &pid in &storage[header_words..header_words + count] {
            if Instant::now() >= state.deadline.unwrap() {
                return Err("sandbox: general retirement snapshot deadline".into());
            }
            let pid =
                u32::try_from(pid).map_err(|_| "sandbox: general member identity overflow")?;
            let member = Handle::created(
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid) },
                "retain general member",
            )?;
            if unsafe { GetProcessId(member.raw()) } != pid {
                return Err("sandbox: retained general member identity differs".into());
            }
            self.verify_membership(member.raw())?;
            // Retain incrementally: a later query/open failure cannot discard
            // capabilities already captured for this exact retirement.
            state.members.push(member);
        }
        Ok(())
    }

    pub(crate) fn request_stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.changed.notify_waiters();
        let mut state = self.retirement.lock().unwrap_or_else(|e| e.into_inner());
        self.prepare_retirement(&mut state);
    }

    fn prepare_retirement(&self, state: &mut Retirement) {
        if state.prepared {
            return;
        }
        state.prepared = true;
        state.deadline = Some(Instant::now() + crate::sandbox::PROCESS_GROUP_DRAIN_BUDGET);
        if let Err(error) = self.snapshot_members(state) {
            state.failure = Some(error);
        }
        if unsafe { TerminateJobObject(self.job.raw(), 125) } == 0 {
            state
                .failure
                .get_or_insert_with(|| last_error("general Job termination"));
        }
        // Also covers setup failure before assignment, with the exact retained
        // leader rather than reopening a possibly recycled PID.
        if unsafe { WaitForSingleObject(self.leader.raw(), 0) } == WAIT_TIMEOUT {
            unsafe {
                TerminateProcess(self.leader.raw(), 125);
            }
        }
    }

    pub(crate) fn retire(&self) -> Result<(), String> {
        let mut state = self.retirement.lock().unwrap_or_else(|e| e.into_inner());
        if state.settled {
            return Ok(());
        }
        self.prepare_retirement(&mut state);
        loop {
            let handles = std::iter::once(&self.leader).chain(state.members.iter());
            let mut signaled = true;
            let mut signal_error = None;
            for handle in handles {
                match unsafe { WaitForSingleObject(handle.raw(), 0) } {
                    WAIT_OBJECT_0 => {}
                    WAIT_TIMEOUT => signaled = false,
                    _ => {
                        signal_error = Some(last_error("general member termination signal"));
                        signaled = false;
                        break;
                    }
                }
            }
            if let Some(error) = signal_error {
                state.failure.get_or_insert(error);
            }
            let zero = match active_job_processes(&self.job) {
                Ok(count) => count == 0,
                Err(error) => {
                    state.failure.get_or_insert(error);
                    false
                }
            };
            if signaled && zero {
                if let Some(error) = &state.failure {
                    return Err(error.clone());
                }
                state.settled = true;
                return Ok(());
            }
            if Instant::now() >= state.deadline.unwrap() {
                return Err(state
                    .failure
                    .clone()
                    .unwrap_or_else(|| "sandbox: general family drain deadline".into()));
            }
            std::thread::sleep(
                Duration::from_millis(5).min(
                    state
                        .deadline
                        .unwrap()
                        .saturating_duration_since(Instant::now()),
                ),
            );
        }
    }
}

impl Drop for GeneralOwner {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

fn initial_thread(pid: u32, deadline: Instant) -> Result<Handle, String> {
    let snapshot = Handle::created(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) },
        "general thread snapshot",
    )?;
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Thread32First(snapshot.raw(), &mut entry) };
    let mut found = None;
    let mut visited = 0usize;
    while next != 0 {
        visited += 1;
        if visited > 65_536 || Instant::now() >= deadline {
            return Err("sandbox: general initial thread snapshot deadline".into());
        }
        if entry.th32OwnerProcessID == pid {
            let thread = Handle::created(
                unsafe {
                    OpenThread(
                        THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                        FALSE,
                        entry.th32ThreadID,
                    )
                },
                "general initial thread retention",
            )?;
            if unsafe { GetProcessIdOfThread(thread.raw()) } != pid
                || unsafe { GetThreadId(thread.raw()) } != entry.th32ThreadID
            {
                return Err("sandbox: general initial thread identity differs".into());
            }
            accept_unique_thread(&mut found, thread)?;
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        next = unsafe { Thread32Next(snapshot.raw(), &mut entry) };
    }
    if unsafe { windows_sys::Win32::Foundation::GetLastError() }
        != windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES
    {
        return Err(last_error("general initial thread enumeration"));
    }
    found.ok_or_else(|| "sandbox: general initial thread absent".into())
}

fn validate_thread_owner(thread: HANDLE, pid: u32) -> Result<(), String> {
    if unsafe { GetProcessIdOfThread(thread) } != pid {
        return Err("sandbox: general initial thread owner differs".into());
    }
    Ok(())
}

fn accept_unique_thread(found: &mut Option<Handle>, thread: Handle) -> Result<(), String> {
    if found.is_some() {
        return Err("sandbox: general initial thread is not unique".into());
    }
    *found = Some(thread);
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum ConstructionFault {
    AbsentThread,
    MultipleThreads,
    WrongThreadOwner,
    Assignment,
    ResumeCount,
    CancelledRegistration,
    FailedRegistration,
}
#[cfg(test)]
struct ConstructionProbe {
    fault: ConstructionFault,
    leader: Mutex<Option<Handle>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HeldCreationLock {
        release: Option<std::sync::mpsc::SyncSender<()>>,
        task: Option<std::thread::JoinHandle<()>>,
    }
    impl HeldCreationLock {
        fn new() -> Self {
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let task = std::thread::Builder::new()
                .name("owned-108-creation-lock".into())
                .spawn(move || {
                    let held = crate::process_creation::creation_guard_until(
                        Instant::now() + Duration::from_secs(5),
                    )
                    .unwrap();
                    ready_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(15));
                    drop(held);
                })
                .unwrap();
            let owner = Self {
                release: Some(release_tx),
                task: Some(task),
            };
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("exact creation guard held");
            owner
        }
        fn release(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            if let Some(task) = self.task.take() {
                if task.join().is_err() && !std::thread::panicking() {
                    panic!("owned creation-lock thread failed");
                }
            }
        }
    }
    impl Drop for HeldCreationLock {
        fn drop(&mut self) {
            self.release();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires guarded native Windows process ownership discovery"]
    async fn windows_general_owner_detached_workers_recheck_captured_spend_after_actual_lock_wait()
    {
        use crate::process_creation::TokioCommandCreationExt;
        use crate::sandbox::{
            BackgroundJobStatus, CommandStatus, Sandbox, SupportCommandAudit, SupportCommandLimits,
        };
        use crate::spend::{Account, Ledger, PriceSnapshot, run::Run};
        use futures::FutureExt;
        // Third case is the actual helper guard primitive, not an assertion
        // that this inert PowerShell command creates an AppContainer.
        for boundary in 0..3 {
            let status_consumer = boundary == 0;
            let helper_primitive = boundary == 2;
            let root = crate::tests::OwnedTestDirectory::new("windows-general-owner-spend");
            let state = root.path().join("state");
            crate::fs::ensure_private_directory(&state).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let account = Account::new(&id, Some(1.0), 0.0).unwrap();
            let run = Run::fixture(
                account.clone(),
                Arc::new(Ledger::new(state)),
                &id,
                PriceSnapshot::capture(Some((1.0, 1.0)), Default::default(), false),
            );
            let work = crate::agent::runner::AgentWorkScope::new();
            let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&work));
            let binding = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
            let sandbox = Sandbox::new(false, "appcontainer")
                .with_shell("powershell.exe")
                .with_workspace_binding(binding);
            let marker = root.path().join("must-not-execute");
            let script = format!(
                "[System.IO.File]::WriteAllText('{}','executed')",
                marker.to_string_lossy().replace('\'', "''")
            );
            let mut job = None;
            let assertions = std::panic::AssertUnwindSafe(work.run(async {
                run.prepare().await.unwrap();
                let mut gate = HeldCreationLock::new();
                let before = crate::process_creation::creation_wait_attempts();
                let change_spend = async {
                    // This counter observes a real WouldBlock, not a delay or
                    // inferred task readiness; the owning ignored row is serial.
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while crate::process_creation::creation_wait_attempts() == before {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .expect("actual native launch reached held creation lock");
                    // Controlled accounting primitive, not claimed HTTP usage.
                    run.observe(crate::event::UsageDelta {
                        input_tokens: 2_000_000,
                        ..Default::default()
                    });
                    assert_eq!(account.snapshot().recorded_spend(), 2.0);
                };
                let controller = async {
                    // Catch BEFORE join! can discard the status receiver. Its
                    // real worker must return a terminal result even when this
                    // diagnostic branch panics or times out.
                    let result = std::panic::AssertUnwindSafe(change_spend)
                        .catch_unwind()
                        .await;
                    if result.is_err() {
                        sandbox.kill_active();
                    }
                    gate.release();
                    result
                };
                crate::spend::run::scope(Some(run.clone()), async {
                    if helper_primitive {
                        let mut command = TokioCommand::new("powershell.exe");
                        command
                            .args([
                                "-NoLogo",
                                "-NoProfile",
                                "-NonInteractive",
                                "-Command",
                                &script,
                            ])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .kill_on_drop(true);
                        let task = crate::agent::runner::spawn_async_scoped(async move {
                            command.spawn_guarded_until(Instant::now() + Duration::from_secs(15))
                        });
                        let (result, controller) = tokio::join!(task, controller);
                        let result = result.unwrap();
                        // Even a controlled-red unexpected child is retired
                        // before an assertion or controller panic can unwind.
                        let refusal = match result {
                            Ok(mut child) => {
                                let _ = child.start_kill();
                                tokio::time::timeout(Duration::from_secs(2), child.wait())
                                    .await
                                    .unwrap()
                                    .unwrap();
                                None
                            }
                            Err(error) => Some(error),
                        };
                        if let Err(panic) = controller {
                            std::panic::resume_unwind(panic);
                        }
                        let refusal = refusal.expect("over-cap helper primitive cannot spawn");
                        assert_eq!(refusal.kind(), std::io::ErrorKind::PermissionDenied);
                        assert_eq!(
                            refusal.to_string(),
                            "sandbox: captured spend admission refused process launch"
                        );
                    } else if status_consumer {
                        let mut command = TokioCommand::new("powershell.exe");
                        command
                            .args([
                                "-NoLogo",
                                "-NoProfile",
                                "-NonInteractive",
                                "-Command",
                                &script,
                            ])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());
                        let (result, controller) = tokio::join!(
                            sandbox.status_support_command(
                                command,
                                SupportCommandLimits {
                                    timeout: Duration::from_secs(15)
                                },
                                SupportCommandAudit::new(
                                    "inert spend fixture",
                                    "explicit disabled policy"
                                )
                            ),
                            controller
                        );
                        // The receiver has reached its owning retirement
                        // boundary before propagating any controller failure.
                        if let Err(panic) = controller {
                            std::panic::resume_unwind(panic);
                        }
                        let result = result.unwrap();
                        let observed = result.status;
                        assert_eq!(observed, CommandStatus::Failed);
                    } else {
                        job = Some(
                            sandbox
                                .start_background_command(script.clone(), Duration::from_secs(15))
                                .await
                                .unwrap(),
                        );
                        if let Err(panic) = controller.await {
                            std::panic::resume_unwind(panic);
                        }
                        let result = sandbox
                            .wait_background_job(job.as_ref().unwrap(), Duration::from_secs(5))
                            .await
                            .unwrap();
                        let observed = result.status;
                        assert_eq!(observed, BackgroundJobStatus::Failed);
                    }
                })
                .await;
                assert!(
                    !marker.exists(),
                    "spend refusal must precede actual child execution"
                );
                assert_eq!(sandbox.active_group_count(), 0);
                assert!(
                    sandbox.general_owners.lock().unwrap().is_empty(),
                    "exact suspended owner retired before terminal result"
                );
                run.flush().await;
            }))
            .catch_unwind()
            .await;
            sandbox.kill_active();
            let background_settled = if let Some(job) = &job {
                sandbox
                    .wait_background_job(job, Duration::from_secs(5))
                    .await
                    .is_ok_and(|snapshot| snapshot.status.is_terminal())
            } else {
                true
            };
            cleanup.settle().await;
            let family_settled = sandbox.active_group_count() == 0
                && sandbox.general_owners.lock().unwrap().is_empty();
            if !background_settled || !family_settled {
                // No terminal proof: preserve only this owned inert fixture
                // rather than deleting paths that an outstanding worker uses.
                std::mem::forget(root);
                panic!(
                    "native spend fixture retained because owned worker retirement was not proven"
                );
            }
            if let Err(panic) = assertions {
                std::panic::resume_unwind(panic);
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires guarded native Windows process ownership discovery"]
    async fn windows_general_owner_construction_failures_never_execute_and_reap_exact_child() {
        // A busy creation lock consumes the original deadline; it must not
        // spawn first and start a fresh timeout after the wait.
        {
            let _held = crate::process_creation::creation_guard().unwrap();
            let mut command = TokioCommand::new("powershell.exe");
            command.args(["-NoProfile", "-Command", "exit 9"]);
            let registered = std::sync::atomic::AtomicBool::new(false);
            let start = Instant::now();
            let result = GeneralOwner::launch(
                &mut command,
                start + Duration::from_millis(25),
                || false,
                |_| {
                    registered.store(true, Ordering::Release);
                    Ok(())
                },
            );
            assert!(
                matches!(result, Err(error) if error == "Windows process-creation lock deadline elapsed")
            );
            assert!(!registered.load(Ordering::Acquire));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        {
            let mut command = TokioCommand::new("powershell.exe");
            let result = GeneralOwner::launch(
                &mut command,
                Instant::now() + Duration::from_secs(15),
                || true,
                |_| panic!("cancelled construction cannot register or execute"),
            );
            assert!(
                matches!(result, Err(error) if error == "sandbox: general process cancelled before execution")
            );
        }
        for (fault, expected) in [
            (
                ConstructionFault::AbsentThread,
                "sandbox: general initial thread absent",
            ),
            (
                ConstructionFault::MultipleThreads,
                "sandbox: general initial thread is not unique",
            ),
            (
                ConstructionFault::WrongThreadOwner,
                "sandbox: general initial thread owner differs",
            ),
            (
                ConstructionFault::Assignment,
                "sandbox: actual invalid Job assignment refused",
            ),
            (
                ConstructionFault::ResumeCount,
                "sandbox: general initial thread resume proof failed",
            ),
            (
                ConstructionFault::CancelledRegistration,
                "sandbox: general process cancelled before execution",
            ),
            (
                ConstructionFault::FailedRegistration,
                "test exact registration refused",
            ),
        ] {
            let root = crate::tests::OwnedTestDirectory::new("windows-general-owner-construction");
            let marker = root.path().join("must-not-execute");
            let script = format!(
                "[System.IO.File]::WriteAllText('{}','executed')",
                marker.to_string_lossy().replace('\'', "''")
            );
            let mut command = TokioCommand::new("powershell.exe");
            command
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &script,
                ])
                .current_dir(root.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let probe = ConstructionProbe {
                fault,
                leader: Mutex::new(None),
            };
            let result = GeneralOwner::spawn_inner(
                &mut command,
                Instant::now() + Duration::from_secs(15),
                || false,
                |owner| {
                    if matches!(fault, ConstructionFault::CancelledRegistration) {
                        owner.request_stop();
                    }
                    if matches!(fault, ConstructionFault::FailedRegistration) {
                        return Err("test exact registration refused".into());
                    }
                    Ok(())
                },
                Some(&probe),
            );
            // The production error path settles the suspended child before
            // this fixture can remove its directory, even on assertion panic.
            let held = probe
                .leader
                .lock()
                .unwrap()
                .take()
                .expect("actual suspended child observed");
            assert_eq!(unsafe { WaitForSingleObject(held.raw(), 0) }, WAIT_OBJECT_0);
            assert!(matches!(result, Err(error) if error == expected));
            assert!(!marker.exists());
        }
    }
    #[tokio::test]
    #[ignore = "requires guarded native Windows GH fixture discovery"]
    async fn windows_general_owner_cancel_and_leader_exit_signal_exact_family() {
        use futures::FutureExt;
        for wait in [true, false] {
            let fixture = super::super::general_process_prototype_tests::Fixture::new();
            let mut command = fixture.command(wait);
            let (mut child, owner) = GeneralOwner::launch(
                &mut command,
                Instant::now() + Duration::from_secs(15),
                || false,
                |_| Ok(()),
            )
            .unwrap();
            let assertions = std::panic::AssertUnwindSafe(async {
                let descendant = fixture.ready().await;
                owner.verify_membership(descendant.raw()).unwrap();
                assert_eq!(
                    unsafe { WaitForSingleObject(descendant.raw(), 0) },
                    WAIT_TIMEOUT
                );
                if wait {
                    assert_eq!(active_job_processes(&owner.job).unwrap(), 2);
                } else {
                    tokio::time::timeout(Duration::from_secs(5), child.wait())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(active_job_processes(&owner.job).unwrap(), 1);
                }
                owner.request_stop();
                owner.retire().unwrap();
                assert_eq!(
                    unsafe { WaitForSingleObject(owner.leader.raw(), 0) },
                    WAIT_OBJECT_0
                );
                assert_eq!(
                    unsafe { WaitForSingleObject(descendant.raw(), 0) },
                    WAIT_OBJECT_0
                );
                assert_eq!(active_job_processes(&owner.job).unwrap(), 0);
                assert!(!owner.failed_retirement());
                std::fs::write(
                    fixture.path().join("gh-release"),
                    b"released only after proven retirement",
                )
                .unwrap();
                assert!(!fixture.path().join("gh-late").exists());
            })
            .catch_unwind()
            .await;
            // Exact production owner retirement and child reaping precede
            // assertion unwinding and fixture directory removal.
            owner.request_stop();
            owner.retire().unwrap();
            tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .unwrap()
                .unwrap();
            drop(owner);
            if let Err(panic) = assertions {
                std::panic::resume_unwind(panic);
            }
        }
    }
    #[tokio::test]
    #[ignore = "requires guarded native Windows GH fixture discovery"]
    async fn windows_general_owner_status_and_background_publish_only_after_family_drain() {
        use crate::sandbox::{
            BackgroundJobStatus, CommandStatus, Sandbox, SupportCommandAudit, SupportCommandLimits,
        };
        use futures::FutureExt;
        for status_consumer in [true, false] {
            let fixture = super::super::general_process_prototype_tests::Fixture::new();
            let binding =
                Arc::new(crate::paths::WorkspaceBinding::capture(fixture.path()).unwrap());
            let sandbox = Sandbox::new(false, "appcontainer")
                .with_shell("powershell.exe")
                .with_workspace_binding(binding);
            let mut job = None;
            let assertions = std::panic::AssertUnwindSafe(async {
                let held = if status_consumer {
                    let command = fixture.command(true);
                    let (result, held) = tokio::join!(
                        sandbox.status_support_command(
                            command,
                            SupportCommandLimits {
                                timeout: Duration::from_secs(15)
                            },
                            SupportCommandAudit::new(
                                "inert native fixture",
                                "explicit disabled policy"
                            )
                        ),
                        async {
                            let held = fixture.ready().await;
                            sandbox.kill_active();
                            held
                        },
                    );
                    let result = result.unwrap();
                    let observed = result.status;
                    assert_eq!(observed, CommandStatus::Cancelled);
                    assert!(!result.descendants_escaped);
                    held
                } else {
                    job = Some(
                        sandbox
                            .start_background_command(fixture.script(true), Duration::from_secs(15))
                            .await
                            .unwrap(),
                    );
                    let held = fixture.ready().await;
                    let terminal = sandbox
                        .stop_background_job(job.as_ref().unwrap())
                        .await
                        .unwrap();
                    let observed = terminal.status;
                    assert_eq!(observed, BackgroundJobStatus::Cancelled);
                    held
                };
                assert_eq!(unsafe { WaitForSingleObject(held.raw(), 0) }, WAIT_OBJECT_0);
                assert_eq!(sandbox.active_group_count(), 0);
                assert!(sandbox.general_owners.lock().unwrap().is_empty());
                std::fs::write(
                    fixture.path().join("gh-release"),
                    b"release only after terminal settlement",
                )
                .unwrap();
                assert!(!fixture.path().join("gh-late").exists());
            })
            .catch_unwind()
            .await;
            sandbox.kill_active();
            if let Some(job) = &job {
                let final_state = sandbox
                    .wait_background_job(job, Duration::from_secs(5))
                    .await
                    .unwrap();
                let observed = final_state.status;
                assert!(observed.is_terminal());
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                while sandbox.active_group_count() != 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("owned native fixture workers settled before directory teardown");
            drop(sandbox);
            if let Err(panic) = assertions {
                std::panic::resume_unwind(panic);
            }
        }
    }
}
