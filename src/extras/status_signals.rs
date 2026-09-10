#[cfg(unix)]
use std::io::Write;

#[derive(Clone)]
pub struct StatusSignals {
    path: String,
}

impl StatusSignals {
    #[cfg(any(feature = "status-signals", all(test, unix)))]
    pub fn new(path: String) -> Self {
        Self { path }
    }

    #[cfg(unix)]
    pub fn send_start(&self) {
        self.send(b"start\n");
    }

    #[cfg(not(unix))]
    pub fn send_start(&self) {}

    #[cfg(unix)]
    pub fn send_stop(&self) {
        self.send(b"stop\n");
    }

    #[cfg(not(unix))]
    pub fn send_stop(&self) {}

    #[cfg(all(unix, any(feature = "git-worktree", test)))]
    pub fn send_git_conflict(&self) {
        self.send(b"git-conflict\n");
    }

    /// Status consumers must never hold up a turn or its cleanup. A full
    /// listener queue can block Unix stream connect on Linux, so set the mode
    /// before connecting; a write timeout alone does not cover that wait.
    /// An unavailable/busy endpoint loses this best-effort notification.
    #[cfg(unix)]
    fn send(&self, message: &[u8]) {
        let _ = (|| -> std::io::Result<()> {
            use socket2::{Domain, SockAddr, Socket, Type};
            let mut socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
            socket.set_nonblocking(true)?;
            socket.connect(&SockAddr::unix(&self.path)?)?;
            socket.write_all(message)
        })();
    }

    #[cfg(all(not(unix), feature = "git-worktree"))]
    pub fn send_git_conflict(&self) {}
}
