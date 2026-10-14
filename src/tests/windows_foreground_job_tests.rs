//! Inert native experiment for .108. Production Shell does not use this owner.
//! This proves only suspended assignment/resume and exact family retirement;
//! the crash window before assignment is deliberately not claimed atomic.

use super::*;
use std::process::Stdio;
use tokio::process::{Child, Command};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetProcessId, GetProcessIdOfThread, GetThreadId, OpenThread, SuspendThread,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

#[derive(Clone, Copy)]
enum Fault {
    None,
    Assignment,
    CancelBeforeResume,
    UnexpectedResumeCount,
}

struct SuspendedFamily {
    child: Child,
    job: Handle,
    descendant: Option<Handle>,
}

impl SuspendedFamily {
    fn spawn(
        mut command: Command,
        fault: Fault,
        cancellation: &crate::agent::runner::AgentWorkCancellation,
        before_resume: impl FnOnce(&Child),
    ) -> Result<Self, String> {
        let job = Handle::created(unsafe { CreateJobObjectW(null(), null()) }, "prototype Job")?;
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
            return Err(last_error("prototype Job limits"));
        }
        // The existing creation gate remains held until assignment and resume.
        // Do not call spawn_guarded here: that would acquire the same mutex twice.
        let _creation =
            crate::process_creation::creation_guard().map_err(|_| "prototype creation gate")?;
        command.creation_flags(CREATE_SUSPENDED).kill_on_drop(true);
        let child = command.spawn().map_err(|_| "prototype suspended spawn")?;
        // Establish the cleanup owner before any fallible native query.
        let family = Self {
            child,
            job,
            descendant: None,
        };
        let process = family
            .child
            .raw_handle()
            .ok_or("prototype process handle unavailable")?;
        let pid = family
            .child
            .id()
            .ok_or("prototype process identity unavailable")?;
        let thread = unique_initial_thread(pid)?;
        before_resume(&family.child);
        if matches!(fault, Fault::Assignment) {
            // An actual invalid Job assignment must fail while code is suspended.
            if unsafe { AssignProcessToJobObject(null_mut(), process) } != 0 {
                return Err("prototype invalid assignment unexpectedly succeeded".into());
            }
            return Err("prototype injected assignment failure".into());
        }
        if unsafe { AssignProcessToJobObject(family.job.raw(), process) } == 0 {
            return Err(last_error("prototype suspended assignment"));
        }
        let mut member = FALSE;
        if unsafe { IsProcessInJob(process, family.job.raw(), &mut member) } == 0 || member != TRUE
        {
            return Err("prototype Job membership unavailable".into());
        }
        if active_job_processes(&family.job)? != 1 {
            return Err("prototype initial Job count was not one".into());
        }
        if cancellation.is_cancelled() {
            return Err("prototype cancelled before resume".into());
        }
        // A retained thread handle is rechecked against the retained process.
        if unsafe { GetProcessIdOfThread(thread.raw()) } != pid {
            return Err("prototype initial thread owner changed".into());
        }
        if matches!(fault, Fault::UnexpectedResumeCount)
            && unsafe { SuspendThread(thread.raw()) } != 1
        {
            return Err("prototype suspension injection failed".into());
        }
        if unsafe { ResumeThread(thread.raw()) } != 1 {
            return Err("prototype unexpected initial resume count".into());
        }
        Ok(family)
    }

    fn retain_descendant(&mut self, descendant: &Handle) -> Result<(), String> {
        if self.descendant.is_some() {
            return Err("prototype descendant already retained".into());
        }
        let mut member = FALSE;
        if unsafe { IsProcessInJob(descendant.raw(), self.job.raw(), &mut member) } == 0
            || member != TRUE
        {
            return Err("prototype descendant is not in the retained Job".into());
        }
        // SAFETY: the fixture retains the actual process handle throughout
        // cloning. No PID lookup or post-exit re-resolution grants ownership.
        let borrowed =
            unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(descendant.raw()) };
        self.descendant = Some(Handle(
            borrowed
                .try_clone_to_owned()
                .map_err(|_| "prototype descendant handle clone")?,
        ));
        Ok(())
    }

    async fn retire(&mut self) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        if unsafe { TerminateJobObject(self.job.raw(), 125) } == 0 {
            return Err(last_error("prototype terminate Job"));
        }
        // The leader may have exited already. Reap it and then prove family
        // quiescence, rather than treating its exit as descendant settlement.
        let _ = self.child.start_kill();
        tokio::time::timeout_at(deadline, self.child.wait())
            .await
            .map_err(|_| "prototype leader wait deadline")?
            .map_err(|_| "prototype leader wait")?;
        loop {
            // TerminateJobObject initiates asynchronous process termination.
            // Zero active accounting is not an actual process-handle signal;
            // pending I/O can still be retiring. Require both witnesses under
            // the same unchanged boundary before publishing settlement.
            let descendant_settled = match &self.descendant {
                None => true,
                Some(descendant) => match unsafe { WaitForSingleObject(descendant.raw(), 0) } {
                    WAIT_OBJECT_0 => true,
                    windows_sys::Win32::Foundation::WAIT_TIMEOUT => false,
                    _ => return Err(last_error("prototype descendant signal wait")),
                },
            };
            if active_job_processes(&self.job)? == 0 && descendant_settled {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("prototype family drain deadline".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for SuspendedFamily {
    fn drop(&mut self) {
        // Exact retained capabilities only. This also covers setup failure
        // before assignment: the still-suspended leader is terminated directly.
        let deadline = Instant::now() + Duration::from_secs(5);
        unsafe {
            TerminateJobObject(self.job.raw(), 125);
        }
        if let Some(process) = self.child.raw_handle() {
            unsafe {
                TerminateProcess(process, 125);
                WaitForSingleObject(process, 2_000);
            }
        }
        if let Some(descendant) = &self.descendant {
            // Even on assertion unwind, retain the exact handle until its
            // asynchronous termination has signalled or the same bound ends.
            let remaining = deadline.saturating_duration_since(Instant::now());
            unsafe {
                WaitForSingleObject(
                    descendant.raw(),
                    u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX),
                );
            }
        }
        while active_job_processes(&self.job).is_ok_and(|active| active != 0)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn unique_initial_thread(pid: u32) -> Result<Handle, String> {
    let snapshot = Handle::created(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) },
        "prototype thread snapshot",
    )?;
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    let mut found = None;
    let mut next = unsafe { Thread32First(snapshot.raw(), &mut entry) };
    let mut visited = 0usize;
    while next != 0 {
        visited += 1;
        if visited > 65_536 {
            return Err("prototype thread snapshot limit".into());
        }
        if entry.th32OwnerProcessID == pid {
            if found.is_some() {
                return Err("prototype multiple initial threads".into());
            }
            let thread = Handle::created(
                unsafe {
                    OpenThread(
                        THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                        FALSE,
                        entry.th32ThreadID,
                    )
                },
                "prototype initial thread",
            )?;
            if unsafe { GetProcessIdOfThread(thread.raw()) } != pid
                || unsafe { GetThreadId(thread.raw()) } != entry.th32ThreadID
            {
                return Err("prototype initial thread identity mismatch".into());
            }
            found = Some(thread);
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        next = unsafe { Thread32Next(snapshot.raw(), &mut entry) };
    }
    if unsafe { windows_sys::Win32::Foundation::GetLastError() }
        != windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES
    {
        return Err(last_error("prototype thread enumeration"));
    }
    found.ok_or_else(|| "prototype initial thread absent".into())
}

pub(super) struct Fixture(crate::tests::OwnedTestDirectory);
impl Fixture {
    pub(super) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(super) fn new() -> Self {
        let root = crate::tests::OwnedTestDirectory::new("windows-foreground-job108");
        let image = std::env::var_os("MINI_AGENT_WINDOWS_GH_FIXTURE_EXE")
            .expect("guarded helper discovery required");
        let bin = root.path().join("fixture bin 雪");
        std::fs::create_dir(&bin).unwrap();
        std::fs::copy(image, bin.join("gh.exe")).unwrap();
        std::fs::write(root.path().join("gh-role"), b"native32").unwrap();
        std::fs::write(root.path().join("gh-mode"), b"wait").unwrap();
        Self(root)
    }
    pub(super) fn command(&self, wait_for_child: bool) -> Command {
        self.command_with_script(&self.script(wait_for_child))
    }
    pub(super) fn script(&self, wait_for_child: bool) -> String {
        format!(
            "$i=[System.Diagnostics.ProcessStartInfo]::new();$i.FileName=($ExecutionContext.SessionState.Path.CurrentFileSystemLocation.ProviderPath+'\\fixture bin 雪\\gh.exe');$i.Arguments='pr create --title inert --body inert';$i.UseShellExecute=$false;$p=[System.Diagnostics.Process]::new();$p.StartInfo=$i;try{{if(-not $p.Start()){{exit 9}};{}}}finally{{$p.Dispose()}}",
            if wait_for_child {
                "$p.WaitForExit();exit $p.ExitCode"
            } else {
                "exit 0"
            }
        )
    }
    fn command_with_script(&self, script: &str) -> Command {
        let shell = super::super::ShellCapability::resolve(
            "powershell.exe",
            self.0.path(),
            std::env::var_os("PATH").as_deref(),
        )
        .expect("native PowerShell required");
        let mut command = Command::new(shell.executable());
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]);
        command
            .current_dir(child_process_path(self.0.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear();
        for name in ["SystemRoot", "PATH", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }
    pub(super) async fn ready(&self) -> Handle {
        let path = self.0.path().join("gh-ready.json");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prototype helper readiness");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let pid = u32::try_from(value["pid"].as_u64().unwrap()).unwrap();
        Handle::created(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION
                        | windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE,
                    FALSE,
                    pid,
                )
            },
            "prototype retain descendant",
        )
        .unwrap()
    }
}

/// Experimental retirement fence only: no production command uses a zero
/// active-process limit until the native membership and signal proof passes.
#[tokio::test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
async fn windows_general_job_zero_limit_preserves_snapshot_and_refuses_new_descendants() {
    use futures::FutureExt;

    let fixture = Fixture::new();
    // Typed .NET constructors avoid PowerShell cmdlet autoload. All paths and
    // operands are fixed owned-fixture data; no GitHub or Git operation runs.
    let script = "$root=$ExecutionContext.SessionState.Path.CurrentFileSystemLocation.ProviderPath;\
        $i=[System.Diagnostics.ProcessStartInfo]::new();\
        $i.FileName=($root+'\\fixture bin 雪\\gh.exe');\
        $i.Arguments='pr create --title inert --body inert';$i.UseShellExecute=$false;\
        $p=[System.Diagnostics.Process]::new();$p.StartInfo=$i;\
        if(-not $p.Start()){exit 9};\
        $clock=[System.Diagnostics.Stopwatch]::StartNew();\
        while(-not [System.IO.File]::Exists($root+'\\fence-go')){\
            if($clock.ElapsedMilliseconds -ge 15000){exit 8};\
            [System.Threading.Thread]::Sleep(10)};\
        $next=[System.Diagnostics.Process]::new();$next.StartInfo=$i;\
        $result='started';try{[void]$next.Start()}\
        catch [System.ComponentModel.Win32Exception]{\
            $result=$_.Exception.NativeErrorCode.ToString([System.Globalization.CultureInfo]::InvariantCulture)}\
        finally{$next.Dispose()};\
        [System.IO.File]::WriteAllText($root+'\\fence-result.part',$result);\
        [System.IO.File]::Move($root+'\\fence-result.part',$root+'\\fence-result');\
        while($clock.ElapsedMilliseconds -lt 15000){[System.Threading.Thread]::Sleep(10)};\
        $p.Dispose();exit 7";
    let scope = crate::agent::runner::AgentWorkScope::new();
    let mut family = SuspendedFamily::spawn(
        fixture.command_with_script(script),
        Fault::None,
        &scope.cancellation_handle(),
        |_| {},
    )
    .unwrap();
    let assertions = std::panic::AssertUnwindSafe(async {
        let descendant = fixture.ready().await;
        family.retain_descendant(&descendant).unwrap();
        let argv_before = std::fs::read(fixture.0.path().join("gh-argv.json")).unwrap();
        // Retain independently of Tokio's leader wait/reap representation.
        let leader = Handle(
            unsafe {
                std::os::windows::io::BorrowedHandle::borrow_raw(family.child.raw_handle().unwrap())
            }
            .try_clone_to_owned()
            .unwrap(),
        );
        assert_eq!(active_job_processes(&family.job).unwrap(), 2);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 0;
        assert_ne!(
            unsafe {
                SetInformationJobObject(
                    family.job.raw(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of_val(&limits) as u32,
                )
            },
            0,
            "candidate zero-limit retirement fence must be supported"
        );
        let mut checked = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    family.job.raw(),
                    JobObjectExtendedLimitInformation,
                    (&mut checked as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of_val(&checked) as u32,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!(
            checked.BasicLimitInformation.LimitFlags,
            limits.BasicLimitInformation.LimitFlags
        );
        assert_eq!(checked.BasicLimitInformation.ActiveProcessLimit, 0);
        assert_eq!(active_job_processes(&family.job).unwrap(), 2);
        for process in [leader.raw(), descendant.raw()] {
            assert_eq!(
                unsafe { WaitForSingleObject(process, 0) },
                windows_sys::Win32::Foundation::WAIT_TIMEOUT
            );
        }
        std::fs::write(fixture.0.path().join("fence-go"), b"owned fork attempt").unwrap();
        let result_path = fixture.0.path().join("fence-result");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !result_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native zero-limit fork result must settle within its own bound");
        assert_eq!(
            std::fs::read_to_string(result_path).unwrap(),
            "1816",
            "candidate fence must reject the actual new process with ERROR_NOT_ENOUGH_QUOTA"
        );
        // Fixed capacity covers exactly the two held fixture processes. A
        // truncated or changed membership list rejects this candidate proof.
        #[repr(C)]
        struct Pair {
            assigned: u32,
            returned: u32,
            ids: [usize; 2],
        }
        let mut list = Pair {
            assigned: 0,
            returned: 0,
            ids: [0; 2],
        };
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    family.job.raw(),
                    windows_sys::Win32::System::JobObjects::JobObjectBasicProcessIdList,
                    (&mut list as *mut Pair).cast(),
                    size_of_val(&list) as u32,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!((list.assigned, list.returned), (2, 2));
        list.ids.sort_unstable();
        let mut expected =
            [
                family.child.id().unwrap() as usize,
                unsafe { GetProcessId(descendant.raw()) } as usize,
            ];
        expected.sort_unstable();
        assert_eq!(list.ids, expected);
        assert_eq!(
            std::fs::read(fixture.0.path().join("gh-argv.json")).unwrap(),
            argv_before
        );
        assert!(!fixture.0.path().join("gh-late").exists());
        family.retire().await.unwrap();
        assert_eq!(active_job_processes(&family.job).unwrap(), 0);
        for process in [leader.raw(), descendant.raw()] {
            assert_eq!(unsafe { WaitForSingleObject(process, 0) }, WAIT_OBJECT_0);
        }
        std::fs::write(
            fixture.0.path().join("gh-release"),
            b"after exact settlement",
        )
        .unwrap();
        assert!(!fixture.0.path().join("gh-late").exists());
    })
    .catch_unwind()
    .await;
    // Assertion failure cannot abandon the exact family or its fixture root.
    family.retire().await.unwrap();
    drop(family);
    if let Err(panic) = assertions {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
async fn windows_general_suspended_job_setup_failures_never_execute_helper() {
    for (fault, expected) in [
        (Fault::Assignment, "prototype injected assignment failure"),
        (
            Fault::CancelBeforeResume,
            "prototype cancelled before resume",
        ),
        (
            Fault::UnexpectedResumeCount,
            "prototype unexpected initial resume count",
        ),
    ] {
        let fixture = Fixture::new();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cancellation = scope.cancellation_handle();
        let mut exact_process = None;
        let result = SuspendedFamily::spawn(fixture.command(true), fault, &cancellation, |child| {
            // SAFETY: child owns this valid process handle throughout cloning.
            let borrowed = unsafe {
                std::os::windows::io::BorrowedHandle::borrow_raw(child.raw_handle().unwrap())
            };
            exact_process = Some(Handle(borrowed.try_clone_to_owned().unwrap()));
            assert!(!fixture.0.path().join("gh-argv.json").exists());
            if matches!(fault, Fault::CancelBeforeResume) {
                cancellation.cancel();
            }
        });
        assert!(matches!(result, Err(error) if error == expected));
        let exact_process = exact_process.expect("actual suspended setup phase observed");
        assert_eq!(
            unsafe { WaitForSingleObject(exact_process.raw(), 0) },
            WAIT_OBJECT_0,
            "setup failure must reap the actual suspended child"
        );
        assert!(!fixture.0.path().join("gh-argv.json").exists());
        assert!(!fixture.0.path().join("gh-late").exists());
    }
}

#[tokio::test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
async fn windows_general_suspended_job_cancel_and_leader_exit_retire_exact_descendant() {
    use futures::FutureExt;
    for wait in [true, false] {
        let fixture = Fixture::new();
        let scope = crate::agent::runner::AgentWorkScope::new();
        let mut family = SuspendedFamily::spawn(
            fixture.command(wait),
            Fault::None,
            &scope.cancellation_handle(),
            |_| {
                assert!(!fixture.0.path().join("gh-argv.json").exists());
            },
        )
        .unwrap();
        let assertions = std::panic::AssertUnwindSafe(async {
            let descendant = fixture.ready().await;
            family.retain_descendant(&descendant).unwrap();
            assert_eq!(
                unsafe { WaitForSingleObject(descendant.raw(), 0) },
                windows_sys::Win32::Foundation::WAIT_TIMEOUT
            );
            if !wait {
                tokio::time::timeout(Duration::from_secs(5), family.child.wait())
                    .await
                    .unwrap()
                    .unwrap();
            }
            family.retire().await.unwrap();
            assert_eq!(active_job_processes(&family.job).unwrap(), 0);
            assert_eq!(
                unsafe { WaitForSingleObject(descendant.raw(), 0) },
                WAIT_OBJECT_0
            );
            std::fs::write(
                fixture.0.path().join("gh-release"),
                b"release after exact family retirement",
            )
            .unwrap();
            assert!(!fixture.0.path().join("gh-late").exists());
        })
        .catch_unwind()
        .await;
        // Unconditional owned settlement precedes fixture directory removal.
        family.retire().await.unwrap();
        drop(family);
        if let Err(panic) = assertions {
            std::panic::resume_unwind(panic);
        }
    }
}
