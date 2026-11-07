//! Windows ConPTY hosting for the AppContainer helper (`pty = true`).
//!
//! The helper creates the pseudoconsole, hands it to the target through
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` in the same attribute list that
//! carries the exact inherited-handle list and the AppContainer security
//! capabilities, and relays between it and its own stdio: terminal output goes
//! to the helper's stdout (the parent's capture pipe), terminal input is read
//! from the helper's stdin right after the launch request.

use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr::{null, null_mut};
use std::thread::JoinHandle;

use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, UpdateProcThreadAttribute,
};

const ROWS: std::ops::RangeInclusive<u16> = 5..=500;
const COLS: std::ops::RangeInclusive<u16> = 20..=1000;

/// An open pseudoconsole and the parent's ends of its two pipes.
pub(crate) struct ConPty {
    hpc: HPCON,
    input: Option<File>,
    output: Option<File>,
}

fn create_pipe() -> Result<(File, File), String> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 0,
    };
    let mut read = null_mut();
    let mut write = null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &mut attributes, 0) } == 0 {
        return Err(format!(
            "sandbox: create pseudoconsole pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) })
}

impl ConPty {
    /// Create a pseudoconsole of `rows` x `cols`. The size comes from the
    /// launch request, so it is re-validated here.
    pub(crate) fn open(rows: u16, cols: u16) -> Result<Self, String> {
        if !ROWS.contains(&rows) || !COLS.contains(&cols) {
            return Err("sandbox: pseudoconsole size is out of range".into());
        }
        let (input_read, input_write) = create_pipe()?;
        let (output_read, output_write) = create_pipe()?;
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let mut hpc: HPCON = 0;
        let result = unsafe {
            CreatePseudoConsole(
                size,
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &mut hpc,
            )
        };
        if result < 0 {
            return Err(format!(
                "sandbox: create pseudoconsole: HRESULT {:#010x}",
                result as u32
            ));
        }
        // The console host keeps its own references to these two ends. Closing ours is what lets
        // the output pipe report end of file once the console is closed.
        drop(input_read);
        drop(output_write);
        Ok(Self {
            hpc,
            input: Some(input_write),
            output: Some(output_read),
        })
    }

    /// Add the pseudoconsole to a process attribute list that was initialised
    /// for one more attribute than it otherwise holds.
    pub(crate) fn set_attribute(&self, list: LPPROC_THREAD_ATTRIBUTE_LIST) -> Result<(), String> {
        if unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                self.hpc as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(format!(
                "sandbox: set pseudoconsole attribute: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Start copying: terminal output to this process's stdout, this
    /// process's stdin to the terminal. Dropping the relay closes the console
    /// and waits for the remaining output to be written.
    pub(crate) fn relay(mut self) -> ConPtyRelay {
        let mut output = self.output.take().expect("pseudoconsole output is owned");
        let mut input = self.input.take().expect("pseudoconsole input is owned");
        let output_thread = std::thread::Builder::new()
            .name("windows-conpty-output".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                let mut stdout = std::io::stdout();
                while let Ok(read) = output.read(&mut buffer) {
                    if read == 0 || stdout.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                }
            })
            .ok();
        // Detached on purpose: it ends when the parent closes the request pipe or the helper exits.
        let _ = std::thread::Builder::new()
            .name("windows-conpty-input".into())
            .spawn(move || {
                let mut buffer = [0u8; 4096];
                let mut stdin = std::io::stdin().lock();
                while let Ok(read) = stdin.read(&mut buffer) {
                    if read == 0 || input.write_all(&buffer[..read]).is_err() {
                        break;
                    }
                }
            });
        ConPtyRelay {
            console: self,
            output_thread,
        }
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        if self.hpc != 0 {
            unsafe { ClosePseudoConsole(self.hpc) };
            self.hpc = 0;
        }
    }
}

/// The running relay of one terminal; see [`ConPty::relay`].
pub(crate) struct ConPtyRelay {
    console: ConPty,
    output_thread: Option<JoinHandle<()>>,
}

impl Drop for ConPtyRelay {
    fn drop(&mut self) {
        // Closing the console ends its output pipe, which ends the copier.
        if self.console.hpc != 0 {
            unsafe { ClosePseudoConsole(self.console.hpc) };
            self.console.hpc = 0;
        }
        if let Some(thread) = self.output_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        InitializeProcThreadAttributeList, PROCESS_INFORMATION, STARTUPINFOEXW,
        WaitForSingleObject,
    };

    #[test]
    fn sizes_outside_the_terminal_range_are_refused() {
        for (rows, cols) in [
            (0, 120),
            (4, 120),
            (501, 120),
            (40, 0),
            (40, 19),
            (40, 1001),
        ] {
            assert!(ConPty::open(rows, cols).is_err(), "{rows}x{cols}");
        }
    }

    /// A real console program started under the pseudoconsole writes through
    /// it, and closing the console after the program exits ends the output.
    #[test]
    fn a_console_program_writes_through_the_pseudoconsole() {
        let mut pty = ConPty::open(40, 120).unwrap();
        let mut bytes = 0usize;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = storage.as_mut_ptr().cast();
        assert_ne!(
            unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) },
            0
        );
        pty.set_attribute(list).unwrap();
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = list;
        let mut command_line: Vec<u16> = std::ffi::OsStr::new("cmd.exe /c echo conpty-ok")
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut information = PROCESS_INFORMATION::default();
        let created = unsafe {
            CreateProcessW(
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                &startup.StartupInfo,
                &mut information,
            )
        };
        assert_ne!(created, 0, "{}", std::io::Error::last_os_error());
        unsafe { CloseHandle(information.hThread) };
        let mut output = pty.output.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut collected = Vec::new();
            let _ = output.read_to_end(&mut collected);
            collected
        });
        let waited = unsafe { WaitForSingleObject(information.hProcess, 30_000) };
        unsafe { CloseHandle(information.hProcess) };
        unsafe { DeleteProcThreadAttributeList(list) };
        assert_eq!(waited, WAIT_OBJECT_0);
        drop(pty);
        let collected = reader.join().unwrap();
        assert!(
            String::from_utf8_lossy(&collected).contains("conpty-ok"),
            "{:?}",
            String::from_utf8_lossy(&collected)
        );
    }
}
