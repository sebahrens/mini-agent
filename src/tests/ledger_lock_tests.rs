//! Owned pre-exec witnesses for actual private-ledger lock retirement.

use crate::spend::{Account, Ledger, PriceSnapshot, Receipt, RequestKind};
use std::io::{Read, Write};
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::sync::{Arc, mpsc};
use std::time::Duration;

struct HeldPreExec {
    release: UnixStream,
    task: Option<std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
}
impl HeldPreExec {
    fn new(ledger_fd: std::os::fd::RawFd) -> Self {
        use std::os::fd::AsRawFd;
        let (release, child_socket) = UnixStream::pair().unwrap();
        release
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        release
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let child_fd = child_socket.as_raw_fd();
        let task = std::thread::spawn(move || {
            let mut command = std::process::Command::new("/usr/bin/true");
            command
                .env_clear()
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // Only async-signal-safe syscalls and stack scalars execute after
            // fork. No Rust locks, allocation, logging or user code runs here.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(ledger_fd, libc::F_GETFD);
                    let ready = [u8::from(flags >= 0 && flags & libc::FD_CLOEXEC != 0)];
                    if libc::write(child_fd, ready.as_ptr().cast(), 1) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut released = [0_u8; 1];
                    loop {
                        let count = libc::read(child_fd, released.as_mut_ptr().cast(), 1);
                        if count == 1 {
                            return Ok(());
                        }
                        if count == 0 {
                            return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
                        }
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::EINTR) {
                            return Err(error);
                        }
                    }
                });
            }
            let status = command.spawn()?.wait();
            // Retain the parent socket through the exact owned spawn/exit.
            drop(child_socket);
            status
        });
        let mut owned = Self {
            release,
            task: Some(task),
        };
        let mut ready = [0_u8; 1];
        owned
            .release
            .read_exact(&mut ready)
            .expect("bounded pre-exec readiness");
        assert_eq!(
            ready,
            [1],
            "actual ledger descriptor was not inherited with CLOEXEC"
        );
        owned
    }
    fn finish(mut self) {
        self.release.write_all(&[1]).unwrap();
        assert!(self.task.take().unwrap().join().unwrap().unwrap().success());
    }
}
impl Drop for HeldPreExec {
    fn drop(&mut self) {
        let _ = self.release.write_all(&[1]);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

struct GatedOperation<T> {
    release: mpsc::SyncSender<()>,
    task: Option<std::thread::JoinHandle<anyhow::Result<T>>>,
}
impl<T> GatedOperation<T> {
    fn finish(mut self) -> anyhow::Result<T> {
        self.release.send(()).unwrap();
        self.task
            .take()
            .unwrap()
            .join()
            .expect("owned ledger operation")
    }
}
impl<T> Drop for GatedOperation<T> {
    fn drop(&mut self) {
        let _ = self.release.try_send(());
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn receipt() -> Receipt {
    Receipt::new(
        "lock-test",
        "owned",
        "fixture",
        RequestKind::Turn,
        crate::event::UsageDelta {
            input_tokens: 50,
            output_tokens: 10,
            total_tokens: 60,
            ..Default::default()
        },
        PriceSnapshot::capture(Some((1.0, 2.0)), Default::default(), false),
    )
}
fn gate(
    root: &std::path::Path,
) -> (
    Arc<Ledger>,
    mpsc::Receiver<std::os::fd::RawFd>,
    mpsc::SyncSender<()>,
) {
    let (locked, observed) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    (
        Arc::new(Ledger::with_lock_test_gate(root, locked, released)),
        observed,
        release,
    )
}

#[test]
fn ledger109_completed_writer_releases_inherited_lock_before_child_exec() {
    let root = super::OwnedTestDirectory::new("ledger109-writer");
    let (ledger, observed, release) = gate(root.path());
    let account = Account::new("lock-test", None, 0.0).unwrap();
    account.observe(&receipt()).unwrap();
    let writer = ledger.clone();
    let charged = account.clone();
    let operation = GatedOperation {
        release,
        task: Some(std::thread::spawn(move || writer.flush(&charged))),
    };
    let fd = observed
        .recv_timeout(Duration::from_secs(5))
        .expect("actual writer acquired lock");
    let child = HeldPreExec::new(fd);
    operation.finish().unwrap();
    // Child is still in pre_exec: close-only releases the mutex but leaves
    // the inherited OFD locked, so this exact production summary used to fail.
    let summary = ledger
        .summary_for_session("lock-test")
        .expect("completed writer lock was inherited past its owner");
    assert_eq!(summary.session.records, 1);
    assert_eq!(summary.session.input_tokens, 50);
    assert_eq!(summary.session.output_tokens, 10);
    assert!((summary.session.known_cost - 0.00007).abs() < 1e-12);
    assert!((account.snapshot().recorded_spend() - 0.00007).abs() < 1e-12);
    assert!(!account.has_pending());
    assert!(!account.snapshot().tracking_error);
    let bytes = std::fs::read(root.path().join("usage.jsonl")).unwrap();
    ledger.flush(&account).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("usage.jsonl")).unwrap(),
        bytes
    );
    child.finish();
}

#[test]
fn ledger109_failed_writer_releases_inherited_lock_and_keeps_corrupt_tail_closed() {
    let root = super::OwnedTestDirectory::new("ledger109-failed-writer");
    crate::fs::private_atomic_create_sync(&root.path().join("usage.jsonl"), b"incomplete").unwrap();
    let (ledger, observed, release) = gate(root.path());
    let account = Account::new("lock-test", None, 0.0).unwrap();
    account.observe(&receipt()).unwrap();
    let writer = ledger.clone();
    let charged = account.clone();
    let operation = GatedOperation {
        release,
        task: Some(std::thread::spawn(move || writer.flush(&charged))),
    };
    let child = HeldPreExec::new(observed.recv_timeout(Duration::from_secs(5)).unwrap());
    assert!(
        operation
            .finish()
            .unwrap_err()
            .to_string()
            .contains("incomplete tail")
    );
    assert!(
        ledger
            .summary_for_session("lock-test")
            .unwrap_err()
            .to_string()
            .contains("incomplete tail"),
        "failed writer left inherited exclusive lock"
    );
    assert_eq!(
        std::fs::read(root.path().join("usage.jsonl")).unwrap(),
        b"incomplete"
    );
    assert!(account.has_pending());
    assert!(account.snapshot().tracking_error);
    child.finish();
}

fn reader_case(corrupt: bool) {
    let root = super::OwnedTestDirectory::new("ledger109-reader");
    let account = Account::new("lock-test", None, 0.0).unwrap();
    if corrupt {
        crate::fs::private_atomic_create_sync(&root.path().join("usage.jsonl"), b"incomplete")
            .unwrap();
    } else {
        account.observe(&receipt()).unwrap();
        Ledger::new(root.path()).flush(&account).unwrap();
    }
    let (ledger, observed, release) = gate(root.path());
    let reader = ledger.clone();
    let operation = GatedOperation {
        release,
        task: Some(std::thread::spawn(move || {
            reader.summary_for_session("lock-test")
        })),
    };
    let child = HeldPreExec::new(observed.recv_timeout(Duration::from_secs(5)).unwrap());
    let result = operation.finish();
    if corrupt {
        assert!(result.unwrap_err().to_string().contains("incomplete tail"));
    } else {
        assert_eq!(result.unwrap().session.records, 1);
    }
    account.observe(&receipt()).unwrap();
    let result = ledger.flush(&account);
    if corrupt {
        assert!(
            result.unwrap_err().to_string().contains("incomplete tail"),
            "failed reader left inherited shared lock"
        );
        assert_eq!(
            std::fs::read(root.path().join("usage.jsonl")).unwrap(),
            b"incomplete"
        );
        assert!(account.has_pending());
    } else {
        result.expect("completed reader lock was inherited past its owner");
        let summary = ledger.summary_for_session("lock-test").unwrap();
        assert_eq!(summary.session.records, 2);
        assert!((summary.session.known_cost - 0.00014).abs() < 1e-12);
        assert!(!account.has_pending());
        assert!(!account.snapshot().tracking_error);
    }
    child.finish();
}
#[test]
fn ledger109_completed_reader_releases_inherited_lock_before_child_exec() {
    reader_case(false);
}
#[test]
fn ledger109_failed_reader_releases_inherited_lock_and_keeps_corrupt_tail_closed() {
    reader_case(true);
}
