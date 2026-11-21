//! Dead-peer detection for the ACP transports.
//!
//! A client that vanishes without a FIN (a laptop asleep, a dropped network)
//! never closes the socket by itself, and the SDK queues every outgoing
//! message in an unbounded channel. Two bounds keep that from running forever:
//! TCP keepalive makes the kernel notice a silent peer, and [`StallGuard`]
//! fails a write that makes no progress for [`WRITE_STALL_TIMEOUT`], which ends
//! the connection (teardown then cancels every turn). A stdio client that
//! stops reading is bounded the same way.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// Idle time before the first keepalive probe.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// Time between keepalive probes.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// How long a write may make no progress before the connection is dropped.
pub(super) const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(120);

fn apply_keepalive(socket: socket2::SockRef<'_>) {
    let keepalive = socket2::TcpKeepalive::new().with_time(KEEPALIVE_IDLE);
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "windows"
    ))]
    let keepalive = keepalive.with_interval(KEEPALIVE_INTERVAL);
    if let Err(error) = socket.set_tcp_keepalive(&keepalive) {
        tracing::warn!("ACP TCP keepalive could not be enabled: {error}");
    }
}

/// Enables TCP keepalive on an accepted connection.
pub(super) fn enable_keepalive(stream: &tokio::net::TcpStream) {
    apply_keepalive(socket2::SockRef::from(stream));
}

/// [`enable_keepalive`] for a standard-library stream.
#[cfg(test)]
pub(super) fn enable_keepalive_std(stream: &std::net::TcpStream) {
    apply_keepalive(socket2::SockRef::from(stream));
}

/// Whether keepalive is on, for tests.
#[cfg(test)]
pub(super) fn keepalive_enabled(stream: &std::net::TcpStream) -> bool {
    socket2::SockRef::from(stream).keepalive().unwrap_or(false)
}

/// A writer that fails with `TimedOut` when a write, flush or close stays
/// pending for `timeout` without any progress, instead of waiting forever on a
/// peer that stopped reading.
pub(super) struct StallGuard<W> {
    inner: W,
    timeout: Duration,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<W> StallGuard<W> {
    pub(super) fn new(inner: W, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: None,
        }
    }

    fn stalled<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        let timeout = self.timeout;
        let deadline = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        match deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the ACP client stopped reading; closing the connection",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

macro_rules! guarded {
    ($this:ident, $cx:ident, $call:expr) => {{
        match $call {
            Poll::Ready(result) => {
                $this.deadline = None;
                Poll::Ready(result)
            }
            Poll::Pending => $this.stalled($cx),
        }
    }};
}

impl<W: futures::io::AsyncWrite + Unpin> futures::io::AsyncWrite for StallGuard<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        guarded!(this, cx, Pin::new(&mut this.inner).poll_write(cx, buffer))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        guarded!(this, cx, Pin::new(&mut this.inner).poll_flush(cx))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        guarded!(this, cx, Pin::new(&mut this.inner).poll_close(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::AsyncWriteExt;

    /// A peer that accepts a few bytes and then never reads again.
    struct StoppedReader {
        room: usize,
    }

    impl futures::io::AsyncWrite for StoppedReader {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.room == 0 {
                return Poll::Pending;
            }
            let taken = buffer.len().min(self.room);
            self.room -= taken;
            Poll::Ready(Ok(taken))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stops_reading_fails_the_write_instead_of_stalling() {
        let mut writer = StallGuard::new(StoppedReader { room: 8 }, Duration::from_secs(30));
        // Progress resets the clock: the first bytes are accepted.
        writer.write_all(b"12345678").await.unwrap();
        let started = tokio::time::Instant::now();
        let error = writer.write_all(&[0; 1024]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reading_peer_never_trips_the_guard() {
        let mut writer =
            StallGuard::new(StoppedReader { room: usize::MAX }, Duration::from_secs(1));
        for _ in 0..4 {
            writer.write_all(&[1; 64]).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        writer.flush().await.unwrap();
    }

    #[test]
    fn accepted_connections_get_keepalive() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        assert!(!keepalive_enabled(&server));
        enable_keepalive_std(&server);
        assert!(keepalive_enabled(&server));
        drop(client);
    }
}
