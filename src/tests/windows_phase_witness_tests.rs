//! Finite .108.2 native primitive experiment. No production observer is enabled.
//! The existing helper request, actor, permission and absolute 5+5 remain intact.
use super::general_process_prototype_tests::{Fixture, unique_initial_thread};
use super::*;
use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{GetProcessId, GetProcessIdOfThread};

#[allow(unsafe_code)]
fn ticks() -> u64 {
    // SAFETY: monotonic scalar query has no pointer/resource requirements.
    unsafe { GetTickCount64() }
}
fn remaining(deadline: u64) -> u32 {
    deadline.saturating_sub(ticks()).min(u32::MAX as u64) as u32
}

struct Outer {
    child: std::process::Child,
    job: Handle,
    cleanup: u64,
    settled: bool,
}
impl Outer {
    #[allow(unsafe_code)]
    fn spawn(fixture: &Fixture, case: &str, run: u64, cleanup: u64) -> Result<Self, String> {
        // SAFETY: unnamed Job, no pointer-bearing input; retain immediately.
        let job = Handle::created(
            unsafe { CreateJobObjectW(null(), null()) },
            "phase outer Job",
        )?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: retained Job and correctly-sized live native structure.
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(last_error("phase Job limits"));
        }
        let output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(fixture.path().join("phase-observation.json"))
            .map_err(|_| "phase output create")?;
        let _creation = crate::process_creation::creation_guard_until(
            Instant::now() + Duration::from_millis(remaining(run) as u64),
        )
        .map_err(|_| "phase creation deadline")?;
        if remaining(run) == 0 {
            return Err("phase run deadline".into());
        }
        let mut command =
            std::process::Command::new(fixture.path().join("fixture bin 雪").join("gh.exe"));
        command
            .args([
                "--phase-probe",
                case,
                &run.to_string(),
                &cleanup.to_string(),
            ])
            .current_dir(child_process_path(fixture.path()))
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .creation_flags(CREATE_SUSPENDED);
        for name in ["SystemRoot", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let child = command.spawn().map_err(|_| "phase suspended spawn")?;
        // Establish exact ownership before any fallible thread/Job operation.
        let owner = Self {
            child,
            job,
            cleanup,
            settled: false,
        };
        drop(command);
        let thread = unique_initial_thread(owner.child.id())?;
        // SAFETY: exact retained suspended process/Job; gate held until resume.
        if unsafe { AssignProcessToJobObject(owner.job.raw(), owner.child.as_raw_handle()) } == 0 {
            return Err(last_error("phase Job assignment"));
        }
        let mut member = FALSE;
        // SAFETY: same exact handles, valid scalar output.
        if unsafe { IsProcessInJob(owner.child.as_raw_handle(), owner.job.raw(), &mut member) } == 0
            || member != TRUE
            || active_job_processes(&owner.job)? != 1
        {
            return Err("phase initial membership differs".into());
        }
        // SAFETY: retained unique initial thread rechecked against exact child.
        if remaining(run) == 0
            || unsafe { GetProcessIdOfThread(thread.raw()) } != owner.child.id()
            || unsafe { ResumeThread(thread.raw()) } != 1
        {
            return Err("phase initial resume refused".into());
        }
        Ok(owner)
    }

    fn completion(&mut self, run: u64) -> Result<(), String> {
        let status = loop {
            if let Some(status) = self.child.try_wait().map_err(|_| "phase leader poll")? {
                break status;
            }
            if remaining(run) == 0 {
                return Err("phase run deadline".into());
            }
            std::thread::sleep(Duration::from_millis(10.min(remaining(run) as u64)));
        };
        self.retire()?;
        if !status.success() {
            return Err("phase inner fixture failed".into());
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn retire(&mut self) -> Result<(), String> {
        if self.settled {
            return Ok(());
        }
        // Retain exact currently-present member handles before termination. A
        // later error still invokes kill; no fallible query skips termination.
        #[repr(C)]
        struct Members {
            assigned: u32,
            returned: u32,
            ids: [usize; 16],
        }
        let mut list = Members {
            assigned: 0,
            returned: 0,
            ids: [0; 16],
        };
        let mut retained = Vec::new();
        let mut snapshot_error = None;
        // SAFETY: fixed aligned initialized output covers the entire native list.
        if unsafe {
            QueryInformationJobObject(
                self.job.raw(),
                windows_sys::Win32::System::JobObjects::JobObjectBasicProcessIdList,
                (&mut list as *mut Members).cast(),
                size_of_val(&list) as u32,
                null_mut(),
            )
        } == 0
        {
            snapshot_error = Some("phase member snapshot failed");
        } else if list.returned > 16 || list.returned != list.assigned {
            snapshot_error = Some("phase member snapshot incomplete");
        } else {
            for &pid in &list.ids[..list.returned as usize] {
                let Ok(pid) = u32::try_from(pid) else {
                    snapshot_error = Some("phase member identity overflow");
                    break;
                };
                // SAFETY: fixed Job member PID, immediately retain/query same
                // object; no path/authority is inferred from PID alone.
                let member = Handle::created(
                    unsafe {
                        OpenProcess(
                            PROCESS_QUERY_LIMITED_INFORMATION
                                | windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE,
                            FALSE,
                            pid,
                        )
                    },
                    "phase retained member",
                );
                match member {
                    Ok(handle) => {
                        let mut belongs = FALSE;
                        // SAFETY: retained process/Job and valid scalar output.
                        if unsafe { GetProcessId(handle.raw()) } != pid
                            || unsafe { IsProcessInJob(handle.raw(), self.job.raw(), &mut belongs) }
                                == 0
                            || belongs != TRUE
                        {
                            snapshot_error = Some("phase retained member differs");
                        }
                        retained.push(handle);
                    }
                    Err(_) => {
                        snapshot_error = Some("phase retain member failed");
                    }
                }
            }
        }
        // SAFETY: exact enclosing fixture Job and leader, original cleanup
        // deadline. No outside process or new grace window is involved.
        unsafe {
            TerminateJobObject(self.job.raw(), 125);
            TerminateProcess(self.child.as_raw_handle(), 125);
        }
        // SAFETY: retained exact process capability remains live during wait.
        if unsafe { WaitForSingleObject(self.child.as_raw_handle(), remaining(self.cleanup)) }
            != WAIT_OBJECT_0
        {
            return Err("phase uncertain cleanup".into());
        }
        self.child.wait().map_err(|_| "phase leader reap")?;
        for member in retained {
            // SAFETY: retained membership-checked process, same cleanup budget.
            if unsafe { WaitForSingleObject(member.raw(), remaining(self.cleanup)) }
                != WAIT_OBJECT_0
            {
                return Err("phase uncertain member cleanup".into());
            }
        }
        loop {
            if active_job_processes(&self.job)? == 0 {
                break;
            }
            if remaining(self.cleanup) == 0 {
                return Err("phase uncertain Job cleanup".into());
            }
            std::thread::sleep(Duration::from_millis(
                10.min(remaining(self.cleanup) as u64),
            ));
        }
        self.settled = true;
        if let Some(error) = snapshot_error {
            return Err(error.into());
        }
        Ok(())
    }
}
impl Drop for Outer {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.retire();
            if !self.settled {
                // Fatal test-only uncertain owner: do not delete fixture roots
                // or detach children. Native CI reports experiment failure.
                eprintln!("PHASE108 uncertain_outer_cleanup");
                std::process::abort();
            }
        }
    }
}

fn run_case(case: &str) -> serde_json::Value {
    let fixture = Fixture::new();
    let start = ticks();
    let run = start.checked_add(5000).unwrap();
    let cleanup = start.checked_add(10000).unwrap();
    let mut owner =
        Outer::spawn(&fixture, case, run, cleanup).expect("phase exact outer ownership");
    let result = owner.completion(run);
    // No panic/root deletion until the exact family has terminal proof.
    let retirement = owner.retire();
    assert!(
        owner.settled,
        "phase family must retire before fixture deletion"
    );
    retirement.expect("phase exact family retirement");
    if result.is_err() {
        // Only closed fixed-role errors from this inert owned fixture are
        // observed, after the whole family is terminal; no product data.
        for role in ["probe", "helper", "target"] {
            if let Ok(file) =
                std::fs::File::open(fixture.path().join(format!("phase-{role}-error")))
            {
                assert!(file.metadata().unwrap().len() <= 64);
                let mut bytes = Vec::new();
                file.take(65).read_to_end(&mut bytes).unwrap();
                assert!(
                    bytes.len() <= 64
                        && bytes
                            .iter()
                            .all(|byte| byte.is_ascii_lowercase() || *byte == b'_')
                );
                eprintln!(
                    "PHASE108 role={role} closed_failure={}",
                    std::str::from_utf8(&bytes).unwrap()
                );
            }
        }
    }
    result.expect("phase fixture under original 5+5");
    assert_eq!(active_job_processes(&owner.job).unwrap(), 0);
    let file = std::fs::File::open(fixture.path().join("phase-observation.json")).unwrap();
    assert!(file.metadata().unwrap().len() <= 1024);
    let mut bytes = Vec::new();
    file.take(1025).read_to_end(&mut bytes).unwrap();
    assert!(bytes.len() <= 1024);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["actor_terminal"], true);
    assert_eq!(value["feeder_zero"], true);
    println!("PHASE108 case={case} exact_family_terminal=true feeder_zero=true");
    value
}

#[test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
fn windows_phase_witness_known_store_survives_forced_kill_after_exact_retirement() {
    let value = run_case("known");
    assert_eq!(value["observation"], "resume");
    assert_eq!(value["effect"], false);
    assert_eq!(value["helper_exit"], 125);
}
#[test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
fn windows_phase_witness_target_cannot_inherit_noninheritable_observer() {
    let value = run_case("noinherit");
    assert_eq!(value["observation"], "resume");
    assert_eq!(value["effect"], true);
    assert_eq!(value["helper_exit"], 0);
}
#[test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
fn windows_phase_witness_whole_request_and_feeder_errors_retire_without_effect() {
    let value = run_case("errors");
    assert_eq!(value["error_cases"], 6);
    assert_eq!(
        value["completed"],
        serde_json::json!([
            "duplicate",
            "encode",
            "launch",
            "kill",
            "partial",
            "pending_writer"
        ])
    );
    assert_eq!(value["effect"], false);
}
#[test]
#[ignore = "requires guarded native Windows GH fixture discovery"]
fn windows_phase_witness_invalid_stage_and_none_preserve_fixed_target_outcome() {
    let invalid = run_case("invalid");
    let ordinary = run_case("none");
    assert_eq!(invalid["observation"], "invalid_stage");
    assert_eq!(ordinary["observation"], "not_attached");
    assert_eq!(invalid["effect"], true);
    assert_eq!(ordinary["effect"], true);
    assert_eq!(invalid["helper_exit"], 0);
    assert_eq!(ordinary["helper_exit"], 0);
}
