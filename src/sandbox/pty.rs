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
/// How long one `job_status` `input` write waits for the program to read.
pub(crate) const PTY_INPUT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Terminal size used when `LINES`/`COLUMNS` are unset or unusable.
pub(crate) const DEFAULT_PTY_SIZE: (u16, u16) = (40, 120);
const PTY_ROWS_RANGE: std::ops::RangeInclusive<u16> = 5..=500;
const PTY_COLS_RANGE: std::ops::RangeInclusive<u16> = 20..=1000;

/// Rows and columns for a new terminal from the operator's `LINES` and
/// `COLUMNS`. A value that is not a plain decimal number inside the sane range
/// falls back to the default for that axis, so the environment can resize but
/// never break the terminal.
pub(crate) fn pty_size(lines: Option<&str>, columns: Option<&str>) -> (u16, u16) {
    fn parse(value: Option<&str>, range: &std::ops::RangeInclusive<u16>, default: u16) -> u16 {
        value
            .and_then(|text| text.trim().parse::<u16>().ok())
            .filter(|number| range.contains(number))
            .unwrap_or(default)
    }
    (
        parse(lines, &PTY_ROWS_RANGE, DEFAULT_PTY_SIZE.0),
        parse(columns, &PTY_COLS_RANGE, DEFAULT_PTY_SIZE.1),
    )
}

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

/// Why a bounded terminal write stopped early.
#[derive(Debug)]
pub(crate) enum PtyWriteFailure {
    /// The program did not drain its input before the deadline.
    TimedOut,
    Io(io::Error),
}

/// A bounded terminal write that did not complete, with the bytes accepted.
#[derive(Debug)]
pub(crate) struct PtyWriteError {
    pub(crate) written: usize,
    pub(crate) failure: PtyWriteFailure,
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

    #[test]
    fn pty_size_honours_sane_lines_and_columns_and_ignores_the_rest() {
        assert_eq!(pty_size(None, None), DEFAULT_PTY_SIZE);
        assert_eq!(pty_size(Some("24"), Some("80")), (24, 80));
        assert_eq!(pty_size(Some(" 50 "), None), (50, 120));
        assert_eq!(pty_size(None, Some("200")), (40, 200));
        for bad in [
            "",
            "abc",
            "-3",
            "4",
            "501",
            "0",
            "99999999999",
            "1e3",
            "80 cols",
        ] {
            assert_eq!(pty_size(Some(bad), None).0, 40, "lines={bad:?}");
        }
        for bad in ["", "x", "-1", "19", "1001", "65536"] {
            assert_eq!(pty_size(None, Some(bad)).1, 120, "columns={bad:?}");
        }
    }

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
