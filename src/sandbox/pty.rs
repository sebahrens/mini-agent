//! Pseudo-terminal stdio for shell commands that need a TTY (`pty = true`).
//!
//! The slave end becomes the child's stdin, stdout and stderr; the parent
//! keeps the master to read the merged terminal output and, for background
//! jobs, to write bounded input. Both ends are created close-on-exec (atomically
//! on Linux and macOS) so no other child, however concurrently spawned,
//! inherits them. The child stays inside the same sandbox
//! wrapper: the pty only replaces its pipes.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, ReadBuf};

pub(crate) use super::pty_common::*;

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

/// Open a terminal pair whose descriptors are close-on-exec from creation.
///
/// `openpty` returns inheritable descriptors, and setting `FD_CLOEXEC`
/// afterwards leaves a window in which a concurrent fork+exec on another
/// thread inherits the slave and keeps the master from ever seeing EOF. The
/// `O_CLOEXEC` flag on `posix_openpt` and on the slave `open` closes it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
#[allow(unsafe_code)]
fn open_cloexec_pair(size: &nix::pty::Winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::FromRawFd;

    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: plain libc calls; every descriptor returned is wrapped in an
    // `OwnedFd` immediately, so each error path closes it, and the name
    // buffer outlives the calls that fill and read it.
    unsafe {
        let raw = libc::posix_openpt(flags);
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = OwnedFd::from_raw_fd(raw);
        if libc::grantpt(raw) != 0 || libc::unlockpt(raw) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut name = [0 as libc::c_char; 256];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let code = libc::ptsname_r(raw, name.as_mut_ptr(), name.len());
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code));
            }
        }
        #[cfg(target_os = "macos")]
        if libc::ioctl(raw, libc::TIOCPTYGNAME as _, name.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let slave = libc::open(name.as_ptr(), flags);
        if slave < 0 {
            return Err(io::Error::last_os_error());
        }
        let slave = OwnedFd::from_raw_fd(slave);
        if libc::ioctl(raw, libc::TIOCSWINSZ as _, size as *const nix::pty::Winsize) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((master, slave))
    }
}

/// Fallback for other Unix targets: `openpty` then `FD_CLOEXEC` (not atomic).
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn open_cloexec_pair(size: &nix::pty::Winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    let pair = nix::pty::openpty(size, None).map_err(io::Error::from)?;
    for fd in [&pair.master, &pair.slave] {
        set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    }
    Ok((pair.master, pair.slave))
}

impl PtyPair {
    /// Open a terminal pair sized 40x120 unless `LINES`/`COLUMNS` in the
    /// mini-agent process environment say otherwise (see [`pty_size`]). Must
    /// run inside a Tokio runtime.
    pub(crate) fn open() -> io::Result<Self> {
        let (rows, cols) = pty_size(
            std::env::var("LINES").ok().as_deref(),
            std::env::var("COLUMNS").ok().as_deref(),
        );
        let size = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = open_cloexec_pair(&size)?;
        set_flag(&master, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
        Ok(Self {
            master: Arc::new(register(master)?),
            slave,
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
    /// Write all of `bytes` to the terminal, as if typed, giving up after
    /// `timeout` when the program is not reading its input.
    pub(crate) async fn write_all(
        &self,
        bytes: &[u8],
        timeout: std::time::Duration,
    ) -> Result<(), PtyWriteError> {
        let mut written = 0usize;
        let write = async {
            while written < bytes.len() {
                let mut guard = self.0.writable().await?;
                match guard.try_io(|fd| {
                    nix::unistd::write(fd.get_ref().as_fd(), &bytes[written..])
                        .map_err(io::Error::from)
                }) {
                    Ok(Ok(count)) => written += count,
                    Ok(Err(error)) => return Err(error),
                    Err(_would_block) => continue,
                }
            }
            Ok(())
        };
        match tokio::time::timeout(timeout, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(PtyWriteError {
                written,
                failure: PtyWriteFailure::Io(error),
            }),
            Err(_elapsed) => Err(PtyWriteError {
                written,
                failure: PtyWriteFailure::TimedOut,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opened_terminal_reports_the_default_size() {
        let pair = PtyPair::open().unwrap();
        let mut size = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the descriptor is the live master of this pair and `size` is a valid out-pointer.
        #[allow(unsafe_code)]
        let result = unsafe {
            libc::ioctl(
                pair.master.get_ref().as_raw_fd(),
                libc::TIOCGWINSZ,
                &mut size,
            )
        };
        assert_eq!(result, 0);
        let (rows, cols) = pty_size(
            std::env::var("LINES").ok().as_deref(),
            std::env::var("COLUMNS").ok().as_deref(),
        );
        assert_eq!((size.ws_row, size.ws_col), (rows, cols));
    }

    #[tokio::test]
    async fn both_terminal_ends_are_close_on_exec() {
        let pair = PtyPair::open().unwrap();
        for fd in [pair.master.get_ref().as_raw_fd(), pair.slave.as_raw_fd()] {
            // SAFETY: both descriptors are live and owned by `pair`.
            #[allow(unsafe_code)]
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        }
    }

    /// A tty line discipline may drop or buffer instead of blocking, so the
    /// deadline is exercised on a non-blocking pipe nobody reads.
    #[tokio::test]
    async fn write_to_a_descriptor_nobody_reads_times_out_and_reports_bytes_written() {
        let (read, write) = nix::unistd::pipe().unwrap();
        set_flag(&write, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK).unwrap();
        let input = PtyInput(Arc::new(register(write).unwrap()));
        let data = vec![b'x'; 8 * 1024 * 1024];
        let started = std::time::Instant::now();
        let error = input
            .write_all(&data, std::time::Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(matches!(error.failure, PtyWriteFailure::TimedOut));
        assert!(error.written > 0 && error.written < data.len());
        drop(read);
    }

    #[tokio::test]
    async fn write_to_a_reading_terminal_completes() {
        let pair = PtyPair::open().unwrap();
        let input = pair.input();
        input
            .write_all(b"hello\n", std::time::Duration::from_secs(2))
            .await
            .unwrap();
        drop(pair);
    }
}
