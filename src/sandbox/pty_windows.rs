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
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};

pub(crate) use super::pty_common::*;

/// Bytes handed to one blocking `WriteFile`. Small, so the bytes a cancelled
/// write may already have placed in the pipe stay a bounded, reported amount.
const WRITE_CHUNK: usize = 1024;
/// How long a timed-out writer gets to acknowledge its cancellation.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Writable handle to a running job's terminal. Empty until the helper
/// request feeder hands over the write end of the request pipe.
#[derive(Clone, Debug, Default)]
pub(crate) struct PtyInput {
    slot: Arc<Mutex<Option<File>>>,
    /// A writer that could not be cancelled still holds `slot`: later input
    /// fails at once instead of queueing behind it.
    stuck: Arc<AtomicBool>,
}

/// The blocking writer's thread, so a timed-out write can be cancelled with
/// `CancelSynchronousIo`. Cleared (under the lock) before the task ends, so a
/// cancel can never reach an unrelated job that reuses the pool thread.
#[derive(Default)]
struct WriterThread(Mutex<Option<OwnedHandle>>);

impl WriterThread {
    fn register(&self) {
        // SAFETY: plain Win32 calls; a null handle (failure) is not stored.
        let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, GetCurrentThreadId()) };
        if !handle.is_null() {
            *self.0.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
        }
    }

    fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    /// Abort the write the registered thread is blocked in, if any.
    fn cancel(&self) {
        if let Some(handle) = &*self.0.lock().unwrap_or_else(|error| error.into_inner()) {
            // SAFETY: the handle is a live thread handle owned by `self`.
            unsafe { CancelSynchronousIo(handle.as_raw_handle()) };
        }
    }
}

struct ClearOnDrop(Arc<WriterThread>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.clear();
    }
}

impl PtyInput {
    /// Take the write end of the helper's request pipe, positioned after the
    /// launch request, so later bytes reach the terminal.
    pub(crate) fn attach(&self, writer: File) {
        *self.slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(writer);
    }

    /// Write all of `bytes` to the terminal, as if typed, giving up after
    /// `timeout` when the program is not reading its input. On timeout the
    /// blocked write is cancelled and joined before this returns, so no byte
    /// is delivered after the error and the next call does not wait behind it.
    /// `written` counts the chunks fully accepted (a lower bound: a cancelled
    /// chunk may have been partly accepted).
    pub(crate) async fn write_all(
        &self,
        bytes: &[u8],
        timeout: std::time::Duration,
    ) -> Result<(), PtyWriteError> {
        if self.stuck.load(Ordering::Acquire) {
            return Err(PtyWriteError {
                written: 0,
                failure: PtyWriteFailure::Io(io::Error::other(
                    "an earlier terminal write could not be cancelled and still holds the input",
                )),
            });
        }
        let slot = self.slot.clone();
        let data = bytes.to_vec();
        let written = Arc::new(AtomicUsize::new(0));
        let progress = written.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let thread = Arc::new(WriterThread::default());
        let registered = thread.clone();
        let mut write = tokio::task::spawn_blocking(move || -> io::Result<()> {
            let _clear = ClearOnDrop(registered.clone());
            let mut guard = slot.lock().unwrap_or_else(|error| error.into_inner());
            let file = guard.as_mut().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the terminal is not connected yet",
                )
            })?;
            registered.register();
            let mut offset = 0usize;
            while offset < data.len() {
                if stop.load(Ordering::Acquire) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "input cancelled",
                    ));
                }
                let end = data.len().min(offset + WRITE_CHUNK);
                offset += file.write(&data[offset..end])?;
                progress.store(offset, Ordering::Relaxed);
            }
            Ok(())
        });
        let failure = match tokio::time::timeout(timeout, &mut write).await {
            Ok(Ok(Ok(()))) => return Ok(()),
            Ok(Ok(Err(error))) => PtyWriteFailure::Io(error),
            Ok(Err(join)) => PtyWriteFailure::Io(io::Error::other(join)),
            Err(_elapsed) => {
                // Stop the writer: flag first (it checks between chunks), then
                // abort a blocked WriteFile, repeating because the cancel can
                // land just before the thread enters the call.
                cancelled.store(true, Ordering::Release);
                let deadline = std::time::Instant::now() + CANCEL_GRACE;
                loop {
                    thread.cancel();
                    match tokio::time::timeout(std::time::Duration::from_millis(20), &mut write)
                        .await
                    {
                        Ok(_) => break,
                        Err(_) if std::time::Instant::now() >= deadline => {
                            self.stuck.store(true, Ordering::Release);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                PtyWriteFailure::TimedOut
            }
        };
        Err(PtyWriteError {
            written: written.load(Ordering::Relaxed),
            failure,
        })
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

    /// Bytes currently buffered in a pipe, without consuming them.
    fn pipe_available(reader: &std::io::PipeReader) -> u32 {
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                reader.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        available
    }

    /// A child that never reads: the oversized write times out, the blocked
    /// writer is cancelled, nothing more arrives afterwards (even once the
    /// pipe has room again) and a later input is not queued behind it.
    #[tokio::test]
    async fn timed_out_input_is_cancelled_and_never_delivered_late() {
        use std::io::Read;
        let (mut reader, writer) = std::io::pipe().unwrap();
        let input = PtyInput::default();
        input.attach(File::from(OwnedHandle::from(writer)));
        let big = vec![b'x'; 1024 * 1024];
        let error = input
            .write_all(&big, std::time::Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(
            matches!(error.failure, PtyWriteFailure::TimedOut),
            "{error:?}"
        );
        assert!(error.written < big.len());
        // Drain what was accepted before the error, then make sure the
        // cancelled writer does not refill the pipe.
        let mut drained = 0usize;
        let mut chunk = [0u8; 4096];
        while pipe_available(&reader) > 0 {
            drained += reader.read(&mut chunk).unwrap();
        }
        assert!(drained < big.len());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(pipe_available(&reader), 0, "late bytes after the timeout");
        // The next input goes straight through.
        input
            .write_all(b"ok\n", std::time::Duration::from_secs(2))
            .await
            .unwrap();
        let mut seen = [0u8; 3];
        reader.read_exact(&mut seen).unwrap();
        assert_eq!(&seen, b"ok\n");
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
