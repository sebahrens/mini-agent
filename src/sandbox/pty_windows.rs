//! Parent side of the Windows ConPTY path for shell commands that need a TTY
//! (`pty = true`).
//!
//! The pseudoconsole itself lives in the AppContainer helper (see
//! `conpty.rs`): it is the only process that creates the target and attests
//! the handles the target inherits. The parent therefore keeps no terminal
//! object. It reads the merged terminal output from the helper's stdout pipe
//! and, for background jobs, writes bounded input into the helper's private
//! request pipe once the launch request has been delivered.

use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) use super::pty_common::*;

/// Writable handle to a running job's terminal. Empty until the helper
/// request feeder hands over the write end of the request pipe.
#[derive(Clone, Debug, Default)]
pub(crate) struct PtyInput(Arc<Mutex<Option<File>>>);

impl PtyInput {
    /// Take the write end of the helper's request pipe, positioned after the
    /// launch request, so later bytes reach the terminal.
    pub(crate) fn attach(&self, writer: File) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some(writer);
    }

    /// Write all of `bytes` to the terminal, as if typed, giving up after
    /// `timeout` when the program is not reading its input.
    pub(crate) async fn write_all(
        &self,
        bytes: &[u8],
        timeout: std::time::Duration,
    ) -> Result<(), PtyWriteError> {
        let slot = self.0.clone();
        let data = bytes.to_vec();
        let written = Arc::new(AtomicUsize::new(0));
        let progress = written.clone();
        let write = tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut guard = slot.lock().unwrap_or_else(|error| error.into_inner());
            let file = guard.as_mut().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the terminal is not connected yet",
                )
            })?;
            let mut offset = 0usize;
            while offset < data.len() {
                offset += file.write(&data[offset..])?;
                progress.store(offset, Ordering::Relaxed);
            }
            Ok(())
        });
        match tokio::time::timeout(timeout, write).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(PtyWriteError {
                written: written.load(Ordering::Relaxed),
                failure: PtyWriteFailure::Io(error),
            }),
            Ok(Err(join)) => Err(PtyWriteError {
                written: written.load(Ordering::Relaxed),
                failure: PtyWriteFailure::Io(io::Error::other(join)),
            }),
            Err(_elapsed) => Err(PtyWriteError {
                written: written.load(Ordering::Relaxed),
                failure: PtyWriteFailure::TimedOut,
            }),
        }
    }
}

/// What the helper needs to host one terminal: its size and where to deliver
/// the input end.
#[derive(Clone, Debug)]
pub(crate) struct PtyLaunch {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) input: PtyInput,
}

/// A terminal awaiting a spawn.
#[derive(Debug)]
pub(crate) struct PtyPair {
    launch: PtyLaunch,
}

impl PtyPair {
    /// Resolve the size (40x120 unless `LINES`/`COLUMNS` in the mini-agent
    /// process environment say otherwise, see [`pty_size`]). The pseudoconsole
    /// is created by the helper at launch.
    pub(crate) fn open() -> io::Result<Self> {
        let (rows, cols) = pty_size(
            std::env::var("LINES").ok().as_deref(),
            std::env::var("COLUMNS").ok().as_deref(),
        );
        Ok(Self {
            launch: PtyLaunch {
                rows,
                cols,
                input: PtyInput::default(),
            },
        })
    }

    pub(crate) fn input(&self) -> PtyInput {
        self.launch.input.clone()
    }

    pub(crate) fn launch(&self) -> PtyLaunch {
        self.launch.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::windows::io::OwnedHandle;

    #[tokio::test]
    async fn input_before_the_request_is_delivered_is_refused_with_nothing_written() {
        let error = PtyInput::default()
            .write_all(b"hello\n", std::time::Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.written, 0);
        assert!(matches!(error.failure, PtyWriteFailure::Io(_)));
    }

    #[tokio::test]
    async fn attached_input_reaches_the_other_end_of_the_pipe() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let input = PtyInput::default();
        input.attach(File::from(OwnedHandle::from(writer)));
        input
            .write_all(b"hello\n", std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let mut seen = [0u8; 6];
        reader.read_exact(&mut seen).unwrap();
        assert_eq!(&seen, b"hello\n");
    }

    #[test]
    fn open_resolves_the_environment_size() {
        let pair = PtyPair::open().unwrap();
        let (rows, cols) = pty_size(
            std::env::var("LINES").ok().as_deref(),
            std::env::var("COLUMNS").ok().as_deref(),
        );
        assert_eq!((pair.launch().rows, pair.launch().cols), (rows, cols));
    }
}
