//! Accept-loop plumbing shared by the plaintext and TLS ACP TCP listeners.
//!
//! Both listeners accept connections until one peer authenticates. A failed
//! `accept()` for one incoming connection (the peer reset or aborted it before
//! it was accepted, the call was interrupted, or the process briefly ran out
//! of descriptors or buffers) says nothing about the listening socket, so it
//! is logged and the loop continues; only an error about the listener itself
//! ends it. Resource exhaustion additionally pauses accepting for
//! [`ACCEPT_BACKOFF`], because the pending connection stays queued and an
//! immediate retry would spin.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

/// Pause before accepting again after the process ran out of descriptors,
/// buffers or memory.
pub(super) const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// A source of accepted TCP connections: the Tokio listener in production,
/// a stub that injects accept failures in tests.
pub(super) trait AcceptSource: Send {
    fn accept(
        &mut self,
    ) -> impl Future<Output = io::Result<(tokio::net::TcpStream, SocketAddr)>> + Send;
}

impl AcceptSource for tokio::net::TcpListener {
    fn accept(
        &mut self,
    ) -> impl Future<Output = io::Result<(tokio::net::TcpStream, SocketAddr)>> + Send {
        tokio::net::TcpListener::accept(self)
    }
}

/// Registers a bound standard listener with the Tokio reactor.
pub(super) fn tokio_listener(
    listener: std::net::TcpListener,
) -> io::Result<tokio::net::TcpListener> {
    listener.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(listener)
}

/// How an accept loop treats one failed `accept()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcceptFailure {
    /// A failure of that one incoming connection: accept the next one.
    Retry,
    /// The process is out of descriptors, buffers or memory: pause, then
    /// accept again.
    Backoff,
    /// The listening socket itself failed.
    Fatal,
}

pub(super) fn classify_accept_error(error: &io::Error) -> AcceptFailure {
    use io::ErrorKind;
    match error.kind() {
        ErrorKind::ConnectionAborted
        | ErrorKind::ConnectionReset
        | ErrorKind::Interrupted
        | ErrorKind::WouldBlock => AcceptFailure::Retry,
        ErrorKind::OutOfMemory => AcceptFailure::Backoff,
        _ if is_resource_exhaustion(error) => AcceptFailure::Backoff,
        _ => AcceptFailure::Fatal,
    }
}

fn is_resource_exhaustion(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
        )
    }
    #[cfg(windows)]
    {
        // WSAEMFILE, WSAENOBUFS.
        matches!(error.raw_os_error(), Some(10024 | 10055))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

/// Logs one failed accept and decides how the loop continues: `Ok(None)`
/// accepts again at once, `Ok(Some(deadline))` pauses until `deadline`, and
/// `Err` ends the loop with the listener's error.
pub(super) fn after_accept_error(
    transport: &str,
    error: io::Error,
) -> io::Result<Option<tokio::time::Instant>> {
    match classify_accept_error(&error) {
        AcceptFailure::Retry => {
            tracing::warn!("{transport} accept failed for one connection: {error}");
            Ok(None)
        }
        AcceptFailure::Backoff => {
            tracing::warn!(
                "{transport} accept failed for lack of resources ({error}); retrying in {} ms",
                ACCEPT_BACKOFF.as_millis()
            );
            Ok(Some(tokio::time::Instant::now() + ACCEPT_BACKOFF))
        }
        AcceptFailure::Fatal => Err(error),
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use std::collections::VecDeque;

    use super::*;

    /// A listener whose first accepts fail with the queued errors before
    /// real connections are handed out.
    pub(in crate::extras::acp) struct FlakyListener {
        listener: tokio::net::TcpListener,
        failures: VecDeque<io::Error>,
    }

    impl FlakyListener {
        pub(in crate::extras::acp) fn bind(failures: Vec<io::Error>) -> (Self, SocketAddr) {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            (
                Self {
                    listener: tokio_listener(listener).unwrap(),
                    failures: failures.into(),
                },
                address,
            )
        }
    }

    impl AcceptSource for FlakyListener {
        fn accept(
            &mut self,
        ) -> impl Future<Output = io::Result<(tokio::net::TcpStream, SocketAddr)>> + Send {
            let failure = self.failures.pop_front();
            let listener = &self.listener;
            async move {
                match failure {
                    Some(error) => Err(error),
                    None => listener.accept().await,
                }
            }
        }
    }

    /// One of each per-connection failure an accept loop must survive.
    pub(in crate::extras::acp) fn per_connection_failures() -> Vec<io::Error> {
        let exhausted = {
            #[cfg(unix)]
            {
                io::Error::from_raw_os_error(libc::EMFILE)
            }
            #[cfg(windows)]
            {
                io::Error::from_raw_os_error(10024)
            }
            #[cfg(not(any(unix, windows)))]
            {
                io::Error::from(io::ErrorKind::OutOfMemory)
            }
        };
        vec![
            io::Error::from(io::ErrorKind::ConnectionAborted),
            io::Error::from(io::ErrorKind::ConnectionReset),
            io::Error::from(io::ErrorKind::Interrupted),
            exhausted,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_connection_failures_are_not_fatal_to_the_listener() {
        use io::ErrorKind;
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::Interrupted,
            ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                classify_accept_error(&io::Error::from(kind)),
                AcceptFailure::Retry,
                "{kind:?}"
            );
        }
        assert_eq!(
            classify_accept_error(&io::Error::from(ErrorKind::OutOfMemory)),
            AcceptFailure::Backoff
        );
        #[cfg(unix)]
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert_eq!(
                classify_accept_error(&io::Error::from_raw_os_error(code)),
                AcceptFailure::Backoff,
                "errno {code}"
            );
        }
        #[cfg(unix)]
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(libc::ECONNABORTED)),
            AcceptFailure::Retry
        );
        for kind in [ErrorKind::InvalidInput, ErrorKind::PermissionDenied] {
            assert_eq!(
                classify_accept_error(&io::Error::from(kind)),
                AcceptFailure::Fatal,
                "{kind:?}"
            );
        }
    }
}
