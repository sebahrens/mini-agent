#![cfg(unix)]

use crate::extras::status_signals::StatusSignals;
use socket2::{Domain, SockAddr, Socket, Type};
use std::io::{ErrorKind, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

const FIXTURE_WAIT: Duration = Duration::from_secs(2);
type SignalCase = (fn(&StatusSignals), &'static str);
const SIGNALS: &[SignalCase] = &[
    (StatusSignals::send_start, "start\n"),
    (StatusSignals::send_stop, "stop\n"),
    (StatusSignals::send_git_conflict, "git-conflict\n"),
];

struct SocketFixture {
    path: std::path::PathBuf,
    listener: Option<UnixListener>,
}

impl SocketFixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("zs-status-{}", uuid::Uuid::new_v4()));
        crate::fs::ensure_private_directory(&directory).unwrap();
        let path = directory.join("s");
        let mut fixture = Self {
            path,
            listener: None,
        };
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
            .bind(&SockAddr::unix(&fixture.path).unwrap())
            .unwrap();
        socket.listen(1).unwrap();
        fixture.listener = Some(socket.into());
        fixture
    }

    fn signals(&self) -> StatusSignals {
        StatusSignals::new(self.path.to_string_lossy().into_owned())
    }

    fn accept(&self) -> UnixStream {
        let deadline = Instant::now() + FIXTURE_WAIT;
        loop {
            match self.listener.as_ref().unwrap().accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).unwrap();
                    return stream;
                }
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => {
                    panic!("status sender did not connect within fixture deadline: {error}")
                }
            }
        }
    }
    fn message(&self) -> String {
        let mut stream = self.accept();
        let deadline = Instant::now() + FIXTURE_WAIT;
        let mut message = Vec::new();
        let mut bytes = [0; 64];
        loop {
            assert!(
                Instant::now() < deadline,
                "status sender did not finish within fixture deadline"
            );
            match stream.read(&mut bytes) {
                Ok(0) => return String::from_utf8(message).unwrap(),
                Ok(length) => {
                    message.extend_from_slice(&bytes[..length]);
                    assert!(message.len() <= 64, "unexpected unbounded status message");
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => panic!("status message read failed: {error}"),
            }
        }
    }
}

impl Drop for SocketFixture {
    fn drop(&mut self) {
        self.listener.take();
        let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
    }
}

#[test]
fn status_messages_keep_the_existing_stream_protocol() {
    let fixture = SocketFixture::new();
    let signals = fixture.signals();
    for &(send, expected) in SIGNALS {
        send(&signals);
        assert_eq!(fixture.message(), expected);
    }
}

#[test]
fn unavailable_status_endpoints_are_best_effort_and_never_created() {
    let fixture = SocketFixture::new();
    let missing = fixture.path.with_file_name("missing");
    let regular = fixture.path.with_file_name("regular");
    std::fs::write(&regular, b"keep this file").unwrap();
    for path in [&missing, &regular] {
        let signals = StatusSignals::new(path.to_string_lossy().into_owned());
        for &(send, _) in SIGNALS {
            send(&signals);
        }
    }
    assert!(!missing.exists(), "signals must not create an endpoint");
    assert_eq!(std::fs::read(regular).unwrap(), b"keep this file");
}

#[test]
fn full_status_listener_queue_cannot_stall_turn_lifecycle() {
    let mut fixture = SocketFixture::new();
    let address = SockAddr::unix(&fixture.path).unwrap();
    let mut queued = Vec::new();
    let mut saturated = false;
    for _ in 0..128 {
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        socket.set_nonblocking(true).unwrap();
        let result = socket.connect(&address);
        queued.push(socket);
        match result {
            Ok(()) => {}
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    || error.kind() == ErrorKind::ConnectionRefused
                    || error.raw_os_error() == Some(libc::EINPROGRESS) =>
            {
                saturated = true;
                break;
            }
            Err(error) => panic!("could not fill listener queue: {error}"),
        }
    }
    assert!(
        saturated && queued.len() > 1,
        "fixture must fill the queue before exercising lifecycle sends"
    );
    let signals = fixture.signals();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let sender = std::thread::spawn(move || {
        for &(send, _) in SIGNALS {
            send(&signals);
        }
        finished_tx.send(()).unwrap();
    });
    let timely = finished_rx.recv_timeout(FIXTURE_WAIT).is_ok();
    // Close the listener before asserting so the old blocking implementation
    // is released, joined, and reported as a failure rather than hanging tests.
    fixture.listener.take();
    drop(queued);
    if !timely {
        finished_rx
            .recv_timeout(FIXTURE_WAIT)
            .expect("sender must exit after listener closure");
    }
    sender.join().unwrap();
    assert!(timely, "a saturated status listener blocked turn lifecycle");
}
