//! Pseudo-terminal stdio for shell commands that need a TTY (`pty = true`).
//!
//! The slave end becomes the child's stdin, stdout and stderr; the parent
//! keeps the master to read the merged terminal output and, for background
//! jobs, to write bounded input. Both ends are close-on-exec in the parent so
//! no other child inherits them. The child stays inside the same sandbox
//! wrapper: the pty only replaces its pipes.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, ReadBuf};

/// Largest input one `job_status` `input` action may write.
pub(crate) const MAX_PTY_INPUT_BYTES: usize = 16 * 1024;

/// An opened pseudo-terminal pair awaiting a spawn.
#[derive(Debug)]
pub(crate) struct PtyPair {
    master: Arc<AsyncFd<OwnedFd>>,
    slave: OwnedFd,
}

/// Writable handle to a running job's terminal.
#[derive(Clone, Debug)]
pub(crate) struct PtyInput(Arc<AsyncFd<OwnedFd>>);

#[allow(unsafe_code)]
fn set_flag(fd: &OwnedFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: `fd` is a valid open descriptor owned by the caller for the
    // duration of both calls; F_GETFD/F_GETFL and F_SETFD/F_SETFL take and
    // return plain integers.
    unsafe {
        let current = libc::fcntl(fd.as_raw_fd(), get);
        if current < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd.as_raw_fd(), set, current | flag) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[allow(unsafe_code)]
fn register(fd: OwnedFd) -> io::Result<AsyncFd<OwnedFd>> {
    // SAFETY: the `OwnedFd` is a valid open descriptor and is owned by the
    // `AsyncFd` until it is dropped, so it stays open and keeps referring to
    // the same file description; `as_raw_fd` on an `OwnedFd` never changes.
    unsafe { AsyncFd::register(fd) }.map_err(io::Error::from)
}

impl PtyPair {
    /// Open a 40x120 terminal pair. Must run inside a Tokio runtime.
    pub(crate) fn open() -> io::Result<Self> {
        let size = nix::pty::Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pair = nix::pty::openpty(&size, None).map_err(io::Error::from)?;
        for fd in [&pair.master, &pair.slave] {
            set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
        }
        set_flag(&pair.master, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
        Ok(Self {
            master: Arc::new(register(pair.master)?),
            slave: pair.slave,
        })
    }

    /// Three descriptors of the slave end for the child's stdio.
    pub(crate) fn child_stdio(&self) -> io::Result<[std::process::Stdio; 3]> {
        Ok([
            self.slave.try_clone()?.into(),
            self.slave.try_clone()?.into(),
            self.slave.try_clone()?.into(),
        ])
    }

    pub(crate) fn input(&self) -> PtyInput {
        PtyInput(self.master.clone())
    }

    /// Close the parent's slave copy (after spawn) and return the reader.
    pub(crate) fn into_reader(self) -> PtyReader {
        drop(self.slave);
        PtyReader(self.master)
    }
}

/// Reads the merged terminal output. End of output (`EIO` once every slave
/// descriptor is closed) reads as EOF.
#[derive(Debug)]
pub(crate) struct PtyReader(Arc<AsyncFd<OwnedFd>>);

impl AsyncRead for PtyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|fd| {
                nix::unistd::read(fd.get_ref().as_fd(), unfilled).map_err(io::Error::from)
            }) {
                Ok(Ok(read)) => {
                    buf.advance(read);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => {
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl PtyInput {
    /// Write all of `bytes` to the terminal, as if typed.
    pub(crate) async fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut guard = self.0.writable().await?;
            match guard.try_io(|fd| {
                nix::unistd::write(fd.get_ref().as_fd(), bytes).map_err(io::Error::from)
            }) {
                Ok(Ok(written)) => bytes = &bytes[written..],
                Ok(Err(error)) => return Err(error),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }
}
