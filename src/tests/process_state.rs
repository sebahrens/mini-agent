//! State and identity observations for owned Linux/macOS test processes.

use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessState {
    Live {
        native: u32,
        parent: u32,
        group: u32,
    },
    Exited,
    Gone,
    Replaced,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    started: (u64, u64),
}

struct Snapshot {
    started: Option<(u64, u64)>,
    state: ProcessState,
}

impl ProcessIdentity {
    pub(crate) fn capture(pid: u32) -> io::Result<Self> {
        let snapshot = snapshot(pid)?.ok_or_else(|| io::Error::other("process already gone"))?;
        if !matches!(snapshot.state, ProcessState::Live { .. }) {
            return Err(io::Error::other("process already exited"));
        }
        Ok(Self {
            pid,
            started: snapshot
                .started
                .ok_or_else(|| io::Error::other("missing process identity"))?,
        })
    }

    pub(crate) fn state(&self) -> io::Result<ProcessState> {
        Ok(match snapshot(self.pid)? {
            None => ProcessState::Gone,
            Some(snapshot) if snapshot.started.is_some_and(|start| start != self.started) => {
                ProcessState::Replaced
            }
            Some(snapshot) => snapshot.state,
        })
    }
}

#[cfg(target_os = "linux")]
fn snapshot(pid: u32) -> io::Result<Option<Snapshot>> {
    snapshot_from_stat(pid, std::fs::read_to_string(format!("/proc/{pid}/stat")))
}

#[cfg(target_os = "linux")]
fn snapshot_from_stat(pid: u32, read: io::Result<String>) -> io::Result<Option<Snapshot>> {
    match read {
        Ok(stat) => parse_stat(pid, &stat).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        // read_to_string opens and then reads. A task that exits in between
        // fails the read with ESRCH rather than ENOENT, so the entry vanishing
        // mid-read is the same observation as it never being there. Only this
        // race is folded in; every other read failure still propagates, so an
        // unreadable /proc never becomes termination evidence.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn parse_stat(pid: u32, stat: &str) -> io::Result<Snapshot> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process stat");
    let (prefix, tail) = stat.rsplit_once(") ").ok_or_else(invalid)?;
    if prefix
        .split_once(" (")
        .and_then(|(pid, _)| pid.parse::<u32>().ok())
        != Some(pid)
    {
        return Err(invalid());
    }
    let fields: Vec<_> = tail.split_whitespace().collect();
    let native = match fields.first().copied() {
        Some("Z" | "X" | "x") => None,
        Some(value @ ("R" | "S" | "D" | "T" | "t" | "W" | "K" | "P" | "I")) => {
            Some(u32::from(value.as_bytes()[0]))
        }
        _ => return Err(invalid()),
    };
    let parent = fields
        .get(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid)?;
    let group = fields
        .get(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid)?;
    let started = fields
        .get(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid)?;
    Ok(Snapshot {
        started: Some((started, 0)),
        state: native.map_or(ProcessState::Exited, |native| ProcessState::Live {
            native,
            parent,
            group,
        }),
    })
}

#[cfg(target_os = "macos")]
fn snapshot(pid: u32) -> io::Result<Option<Snapshot>> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("invalid process ID"))?;
    if pid <= 0 {
        return Err(io::Error::other("invalid process ID"));
    }
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: info is writable for size bytes; only a complete result is read.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if read == size as i32 {
        // SAFETY: proc_pidinfo initialized the complete structure above.
        let info = unsafe { info.assume_init() };
        return Ok(Some(Snapshot {
            started: Some((info.pbi_start_tvsec, info.pbi_start_tvusec)),
            state: if info.pbi_status == libc::SZOMB {
                ProcessState::Exited
            } else {
                ProcessState::Live {
                    native: info.pbi_status,
                    parent: info.pbi_ppid,
                    group: info.pbi_pgid,
                }
            },
        }));
    }
    let error = io::Error::last_os_error();
    if read != 0 || error.raw_os_error() != Some(libc::ESRCH) {
        return Err(error);
    }

    // macOS libproc omits zombies (ESRCH), although kill(pid, 0) still succeeds.
    // ps exposes their state through the kernel process table. Never turn an
    // unavailable observation into successful termination evidence.
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()?;
    if output.status.success()
        && std::str::from_utf8(&output.stdout).is_ok_and(|state| state.trim().starts_with('Z'))
    {
        return Ok(Some(Snapshot {
            started: None,
            state: ProcessState::Exited,
        }));
    }
    // SAFETY: signal zero observes this positive PID and does not signal it.
    if unsafe { libc::kill(pid, 0) } == -1
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return Ok(None);
    }
    Err(io::Error::other(
        "process state unavailable after libproc lookup",
    ))
}

struct OwnedChild(std::process::Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn owned_process_observation_distinguishes_live_zombie_reaped_and_replaced() {
    assert!(ProcessIdentity::capture(0).is_err());
    let mut child = OwnedChild(
        std::process::Command::new("/bin/sh")
            .args(["-c", "IFS= read -r release"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let identity = ProcessIdentity::capture(child.0.id()).unwrap();
    assert!(matches!(
        identity.state().unwrap(),
        ProcessState::Live { .. }
    ));
    let replaced = ProcessIdentity {
        started: (identity.started.0 + 1, identity.started.1),
        ..identity
    };
    assert_eq!(replaced.state().unwrap(), ProcessState::Replaced);
    // Hold the proc inode while its task is still live. Reading this same
    // descriptor after reaping reproduces the open/read race deterministically.
    #[cfg(target_os = "linux")]
    let mut opened_stat = std::fs::File::open(format!("/proc/{}/stat", child.0.id())).unwrap();
    child.0.kill().unwrap();
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
    // SAFETY: this is our unreaped child. WNOWAIT establishes its exit without
    // consuming the wait status; OwnedChild retains responsibility for reaping.
    assert_eq!(
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.0.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        },
        0
    );
    assert_eq!(identity.state().unwrap(), ProcessState::Exited);
    assert!(ProcessIdentity::capture(child.0.id()).is_err());
    // The original kill-zero assertion rejects this dead, unreaped process.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, 0) }, 0);
    child.0.wait().unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut stat = String::new();
        let read = opened_stat.read_to_string(&mut stat).map(|_| stat);
        assert_eq!(read.as_ref().unwrap_err().raw_os_error(), Some(libc::ESRCH));
        assert!(snapshot_from_stat(child.0.id(), read).unwrap().is_none());
    }
    assert_eq!(identity.state().unwrap(), ProcessState::Gone);
    assert!(ProcessIdentity::capture(child.0.id()).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn process_stat_reads_preserve_identity_and_observation_errors() {
    let stat = |state: &str| {
        format!(
            "41 (name with ) spaces) {state} 12 41 {} 987 0",
            "0 ".repeat(16)
        )
    };
    for state in ["R", "S", "D", "T", "t", "W", "K", "P", "I"] {
        let snapshot = snapshot_from_stat(41, Ok(stat(state))).unwrap().unwrap();
        assert_eq!(snapshot.started, Some((987, 0)));
        assert!(matches!(
            snapshot.state,
            ProcessState::Live {
                parent: 12,
                group: 41,
                ..
            }
        ));
    }
    for state in ["Z", "X", "x"] {
        assert_eq!(
            snapshot_from_stat(41, Ok(stat(state)))
                .unwrap()
                .unwrap()
                .state,
            ProcessState::Exited
        );
    }
    for invalid in [
        stat("?"),
        stat("S").replace("987", "bad"),
        "41 (short) S".into(),
    ] {
        assert!(snapshot_from_stat(41, Ok(invalid)).is_err());
    }
    assert!(snapshot_from_stat(42, Ok(stat("S"))).is_err());
    for number in [libc::EACCES, libc::EIO, libc::EINTR] {
        let error = snapshot_from_stat(41, Err(io::Error::from_raw_os_error(number)))
            .err()
            .expect("an unavailable observation became process disappearance");
        assert_eq!(error.raw_os_error(), Some(number));
    }
    let error = snapshot_from_stat(
        41,
        Err(io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 stat")),
    )
    .err()
    .expect("invalid stat data became process disappearance");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "non-UTF-8 stat");
}
