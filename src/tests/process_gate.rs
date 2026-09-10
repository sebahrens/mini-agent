//! Owned Unix FIFO release channel shared by subprocess fixtures.

use std::io;

/// A fixture-owned release channel for a shell read. Keeping both ends open
/// lets cleanup release even a command that has not opened its reader yet.
pub(crate) struct ProcessGate {
    file: std::fs::File,
    released: bool,
}

impl ProcessGate {
    pub(crate) fn new(path: &std::path::Path) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: path_c remains a valid NUL-terminated path for the call.
        if unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(|file| Self {
                file,
                released: false,
            })
    }

    pub(crate) fn release(&mut self) -> io::Result<()> {
        use std::io::Write;
        if !self.released {
            self.file.write_all(b"release\n")?;
            self.released = true;
        }
        Ok(())
    }
}

impl Drop for ProcessGate {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
