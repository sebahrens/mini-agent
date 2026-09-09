//! Bounded cleanup of private Windows containment-test artifacts.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn remove_file(path: &Path, deadline: Instant) -> io::Result<()> {
    retry_sharing_errors(deadline, || std::fs::remove_file(path))
}

pub(super) fn remove_dir(path: &Path, deadline: Instant) -> io::Result<()> {
    retry_sharing_errors(deadline, || std::fs::remove_dir(path))
}

fn retry_sharing_errors(
    deadline: Instant,
    mut remove: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    loop {
        match remove() {
            Ok(()) => return Ok(()),
            // Explicit cleanup may already have removed the file before a
            // directory error triggers the owner's final Drop attempt.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                // Win32 ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION. Do not
                // retry access denial or weaken permissions to force cleanup.
                if !matches!(error.raw_os_error(), Some(32 | 33)) || Instant::now() >= deadline {
                    return Err(error);
                }
                std::thread::sleep(
                    Duration::from_millis(20)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("artifact-cleanup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn sharing_retry_preserves_errors_and_respects_an_expired_deadline() {
    for code in [32, 33] {
        let mut attempts = 0;
        retry_sharing_errors(Instant::now() + Duration::from_secs(2), || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from_raw_os_error(code))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 3);

        let mut attempts = 0;
        let error = retry_sharing_errors(Instant::now(), || {
            attempts += 1;
            assert_eq!(attempts, 1, "an expired deadline must not retry");
            Err(io::Error::from_raw_os_error(code))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(code));
        assert_eq!(attempts, 1, "an expired deadline must not be renewed");
    }
    for code in [5, 13, 87] {
        let mut attempts = 0;
        let error = retry_sharing_errors(Instant::now() + Duration::from_secs(2), || {
            attempts += 1;
            Err(io::Error::from_raw_os_error(code))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(code));
        assert_eq!(attempts, 1, "non-contention errors must fail immediately");
    }
}

#[test]
fn artifact_cleanup_can_resume_after_only_the_file_was_removed() {
    let root = TestDirectory::new();
    let directory = root.path().join("artifact");
    std::fs::create_dir(&directory).unwrap();
    let executable = directory.join("probe.exe");
    let obstruction = directory.join("unexpected");
    std::fs::write(&executable, "fixture").unwrap();
    std::fs::write(&obstruction, "must not remove unrelated content").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    remove_file(&executable, deadline).unwrap();
    assert!(remove_dir(&directory, deadline).is_err());
    assert!(obstruction.exists());
    std::fs::remove_file(&obstruction).unwrap();
    remove_file(&executable, deadline).unwrap();
    remove_dir(&directory, deadline).unwrap();
    remove_dir(&directory, deadline).unwrap();
    assert!(!directory.exists());
}

#[cfg(windows)]
#[test]
fn native_sharing_holder_must_release_before_artifact_cleanup_succeeds() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let root = TestDirectory::new();
    let executable = root.path().join("probe.exe");
    std::fs::write(&executable, "fixture").unwrap();
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&executable)
        .unwrap();
    assert_eq!(
        std::fs::remove_file(&executable)
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    let error = remove_file(&executable, Instant::now()).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(32));
    assert!(
        executable.exists(),
        "persistent sharing must not be reported as cleanup success"
    );
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(holder);
    });
    let result = remove_file(&executable, Instant::now() + Duration::from_secs(2));
    releaser.join().unwrap();
    result.unwrap();
    assert!(!executable.exists());
}
