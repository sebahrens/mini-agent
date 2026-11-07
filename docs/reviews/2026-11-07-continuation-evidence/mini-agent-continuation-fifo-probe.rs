
#[cfg(all(test, unix))]
#[allow(unsafe_code)]
mod continuation_bound_fifo_probe {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;

    #[test]
    fn continuation_bound_file_open_must_refuse_fifo_before_a_writer_arrives() {
        let root = crate::tests::OwnedTestDirectory::new("continuation-bound-fifo");
        let target = root.path().join("input.txt");
        let cpath = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: cpath is an owned valid NUL-terminated fixture path.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let binding = WorkspaceBinding::capture(root.path()).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let job = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = binding.open_relative(std::path::Path::new("input.txt"));
            done_tx.send(result.is_err()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let observed = done_rx.recv_timeout(Duration::from_secs(2));
        let blocked_until_writer = observed.is_err();
        if blocked_until_writer {
            // Controlled cleanup: release the read-only FIFO open before asserting.
            let release = std::fs::OpenOptions::new().read(true).write(true)
                .custom_flags(libc::O_NONBLOCK).open(&target).unwrap();
            done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(release);
        }
        job.join().unwrap();
        assert!(!blocked_until_writer, "workspace-bound file open waited for the controlled FIFO writer");
        assert!(observed.unwrap(), "non-regular target must be rejected");
    }
}
