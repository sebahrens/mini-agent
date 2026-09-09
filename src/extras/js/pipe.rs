//! Parent pipe reads that drain buffered bytes after producer exit without a scheduling deadline.

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

const RUNNING: u8 = 0;
const DRAINING: u8 = 1;
const STOPPED: u8 = 2;

#[derive(Clone, Default)]
pub(super) struct ReadControl(Arc<AtomicU8>);

impl ReadControl {
    pub(super) fn producer_exited(&self) {
        self.0.store(DRAINING, Ordering::Release);
    }

    pub(super) fn stop_on_drop(&self) -> StopReadOnDrop {
        StopReadOnDrop(self.clone())
    }

    pub(super) fn reader<'a>(&'a self, file: &'a mut File) -> ControlledReader<'a> {
        ControlledReader {
            file,
            control: &self.0,
        }
    }
}

pub(super) struct StopReadOnDrop(ReadControl);

impl Drop for StopReadOnDrop {
    fn drop(&mut self) {
        self.0.0.store(STOPPED, Ordering::Release);
    }
}

pub(super) struct ControlledReader<'a> {
    file: &'a mut File,
    control: &'a AtomicU8,
}

impl Read for ControlledReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut delay = Duration::from_millis(1);
        loop {
            // Sample before checking the pipe: an exit observed after an empty
            // poll must trigger another poll, since the final bytes may have
            // arrived between those two observations.
            let state = self.control.load(Ordering::Acquire);
            if state == STOPPED {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if let Some(count) = read_available(self.file, buffer, state == RUNNING, delay)? {
                return Ok(count);
            }
            if state == DRAINING {
                // The producer cannot add any more bytes. A foreign writer
                // retaining an empty pipe must not keep this frame open.
                return Ok(0);
            }
            delay = (delay * 2).min(Duration::from_millis(16));
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn read_available(
    file: &mut File,
    buffer: &mut [u8],
    wait: bool,
    _delay: Duration,
) -> io::Result<Option<usize>> {
    use std::os::fd::AsRawFd;

    let mut descriptor = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: the mutex-owned file keeps this descriptor live and has one reader.
    // poll also reports HUP/ERR, for which read returns EOF or the native error.
    let ready = unsafe { libc::poll(&mut descriptor, 1, if wait { 250 } else { 0 }) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 {
        return Ok(None);
    }
    file.read(buffer).map(Some)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn read_available(
    file: &mut File,
    buffer: &mut [u8],
    wait: bool,
    delay: Duration,
) -> io::Result<Option<usize>> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, HANDLE};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let mut available = 0u32;
    // SAFETY: the anonymous-pipe handle is owned by the exclusively locked file;
    // the optional output buffers are null and available is a live DWORD.
    if unsafe {
        PeekNamedPipe(
            file.as_raw_handle() as HANDLE,
            null_mut(),
            0,
            null_mut(),
            &mut available,
            null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32)
        {
            return Ok(Some(0));
        }
        return Err(error);
    }
    if available > 0 {
        let count = buffer.len().min(available as usize);
        return file.read(&mut buffer[..count]).map(Some);
    }
    if wait {
        // Anonymous Windows pipes do not provide overlapped readiness. Back off
        // idle peeks while keeping short request/response exchanges responsive.
        std::thread::sleep(delay);
    }
    Ok(None)
}

#[cfg(not(any(unix, windows)))]
fn read_available(_: &mut File, _: &mut [u8], _: bool, _: Duration) -> io::Result<Option<usize>> {
    Err(io::ErrorKind::Unsupported.into())
}
