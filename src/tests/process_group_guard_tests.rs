//! Post-reap process-group cleanup (mini-agent-7xrej.1).
//!
//! Once a group leader has been reaped its pid no longer pins the pgid: after
//! the last descendant leaves, the kernel may hand the number to an unrelated
//! process. These tests use the test-only record of `kill_process_group` calls
//! to prove a reaped guard leaves an empty group alone, without depending on a
//! real pid wrap.

use std::collections::HashSet;
use std::io::BufRead as _;
use std::os::unix::process::CommandExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, killpg};
use nix::unistd::Pid;

use crate::sandbox::{
    ProcessGroupGuard, drop_reaped_output_lifecycle_guard, take_signalled_groups,
};

fn spawn_own_group(script: &str) -> std::process::Child {
    std::process::Command::new("/bin/sh")
        .args(["-c", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap()
}

fn group_is_live(pid: u32) -> bool {
    killpg(Pid::from_raw(i32::try_from(pid).unwrap()), None).is_ok()
}

#[test]
fn reaped_process_group_guard_does_not_signal_an_empty_group() {
    let mut child = spawn_own_group("exit 0");
    let pid = child.id();
    child.wait().unwrap();
    assert!(!group_is_live(pid), "the only member was reaped");
    let groups = Arc::new(Mutex::new(HashSet::new()));
    take_signalled_groups();

    let mut guard = ProcessGroupGuard::new(Some(pid), groups.clone());
    guard.mark_leader_reaped();
    drop(guard);

    assert!(
        !take_signalled_groups().contains(&pid),
        "a guard whose leader was reaped signalled an empty, recyclable pgid {pid}"
    );
    assert!(groups.lock().unwrap().is_empty());
}

#[test]
fn reaped_output_lifecycle_guard_does_not_signal_an_empty_group() {
    let mut child = spawn_own_group("exit 0");
    let pid = child.id();
    child.wait().unwrap();
    let groups = Arc::new(Mutex::new(HashSet::new()));
    take_signalled_groups();

    drop_reaped_output_lifecycle_guard(pid, groups.clone());

    assert!(!take_signalled_groups().contains(&pid));
    assert!(groups.lock().unwrap().is_empty());
}

#[test]
fn unreaped_process_group_guard_still_signals_its_group() {
    let mut child = spawn_own_group("exec /bin/sleep 60");
    let pid = child.id();
    take_signalled_groups();

    drop(ProcessGroupGuard::new(
        Some(pid),
        Arc::new(Mutex::new(HashSet::new())),
    ));

    assert!(take_signalled_groups().contains(&pid));
    let status = child.wait().unwrap();
    assert!(!status.success(), "the unreaped leader was not terminated");
}

#[test]
fn reaped_process_group_guard_still_terminates_a_lingering_descendant() {
    let mut child = spawn_own_group("/bin/sleep 60 </dev/null >/dev/null 2>&1 & echo $!");
    let pid = child.id();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let descendant: i32 = line.trim().parse().unwrap();
    child.wait().unwrap();
    assert!(
        group_is_live(pid),
        "the descendant should still hold the group"
    );
    take_signalled_groups();

    let mut guard = ProcessGroupGuard::new(Some(pid), Arc::new(Mutex::new(HashSet::new())));
    guard.mark_leader_reaped();
    drop(guard);

    assert!(take_signalled_groups().contains(&pid));
    let deadline = Instant::now() + Duration::from_secs(5);
    while kill(Pid::from_raw(descendant), None).is_ok() {
        assert!(
            Instant::now() < deadline,
            "lingering descendant {descendant} survived post-reap cleanup"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
