use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use crate::config::load::read_config_content;
use crate::config::load::{atomic_config_write, parse_config_content};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "zerostack_config_permissions_{tag}_{}_{}",
            std::process::id(),
            counter
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn atomic_temp_residue(directory: &Path) -> usize {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".zswrite.") || name.starts_with(".zsconfig.")
        })
        .count()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn config_persistence_permissions_ignore_permissive_umask() {
    const CHILD_PATH: &str = "ZS_CONFIG_PERMISSION_UMASK_CHILD";
    if let Some(config) = std::env::var_os(CHILD_PATH) {
        atomic_config_write(Path::new(&config), "api_keys = { openai = \"secret\" }\n").unwrap();
        return;
    }

    use std::os::unix::process::CommandExt;

    let root = TempDir::new("umask");
    let config_dir = root.path().join("nested").join("config");
    let config = config_dir.join("config.toml");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "tests::config_persistence_permissions_tests::config_persistence_permissions_ignore_permissive_umask",
            "--nocapture",
        ])
        .env(CHILD_PATH, &config);
    #[allow(unsafe_code)]
    unsafe {
        child.pre_exec(|| {
            unsafe extern "C" {
                fn umask(mask: std::os::raw::c_uint) -> std::os::raw::c_uint;
            }
            umask(0);
            Ok(())
        });
    }
    assert!(child.status().unwrap().success());

    assert_eq!(mode(&root.path().join("nested")), 0o700);
    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config), 0o600);
    assert_eq!(atomic_temp_residue(&config_dir), 0);
}

#[cfg(unix)]
#[test]
fn config_persistence_permissions_restore_owner_read_after_restrictive_umask() {
    const CHILD_PATH: &str = "ZS_CONFIG_RESTRICTIVE_UMASK_CHILD";
    if let Some(config) = std::env::var_os(CHILD_PATH) {
        atomic_config_write(Path::new(&config), "api_keys = { openai = \"secret\" }\n").unwrap();
        return;
    }

    use std::os::unix::process::CommandExt;

    let root = TempDir::new("restrictive-umask");
    let config_dir = root.path().join("config");
    std::fs::create_dir(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "tests::config_persistence_permissions_tests::config_persistence_permissions_restore_owner_read_after_restrictive_umask",
            "--nocapture",
        ])
        .env(CHILD_PATH, &config);
    #[allow(unsafe_code)]
    unsafe {
        child.pre_exec(|| {
            unsafe extern "C" {
                fn umask(mask: std::os::raw::c_uint) -> std::os::raw::c_uint;
            }
            umask(0o400);
            Ok(())
        });
    }
    assert!(child.status().unwrap().success());

    assert_eq!(mode(&config), 0o600);
    assert_eq!(
        read_config_content(&config).unwrap(),
        "api_keys = { openai = \"secret\" }\n"
    );
    assert_eq!(atomic_temp_residue(&config_dir), 0);
}

#[cfg(unix)]
#[test]
fn config_persistence_permissions_repair_owned_regular_paths() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new("repair");
    let config_dir = root.path().join("config");
    let config = config_dir.join("config.toml");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(&config, "old").unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o666)).unwrap();

    assert_eq!(read_config_content(&config).unwrap(), "old");
    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config), 0o600);

    atomic_config_write(&config, "new").unwrap();
    assert_eq!(mode(&config_dir), 0o700);
    assert_eq!(mode(&config), 0o600);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "new");
}

#[cfg(unix)]
#[test]
fn config_persistence_permissions_reject_symlink_and_directory_targets() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new("wrong-kind");
    let config_dir = root.path().join("config");
    std::fs::create_dir(&config_dir).unwrap();
    let outside = root.path().join("outside.toml");
    std::fs::write(&outside, "unchanged").unwrap();

    let link = config_dir.join("link.toml");
    symlink(&outside, &link).unwrap();
    assert!(atomic_config_write(&link, "secret").is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");

    let directory = config_dir.join("directory.toml");
    std::fs::create_dir(&directory).unwrap();
    assert!(atomic_config_write(&directory, "secret").is_err());
    assert_eq!(atomic_temp_residue(&config_dir), 0);
}

#[cfg(unix)]
#[test]
fn config_persistence_permissions_clean_failure_residue_and_preserve_prior_file() {
    use crate::config::load::atomic_config_write_with_failure;

    let root = TempDir::new("failure");
    let config_dir = root.path().join("config");
    let config = config_dir.join("config.toml");
    atomic_config_write(&config, "old-secret").unwrap();

    for fail_rename in [false, true] {
        let error = atomic_config_write_with_failure(&config, "new-secret", fail_rename)
            .expect_err("injected failure must be surfaced");
        assert!(!error.to_string().contains("new-secret"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "old-secret");
        assert_eq!(mode(&config), 0o600);
        assert_eq!(atomic_temp_residue(&config_dir), 0);
    }
}

#[cfg(unix)]
#[test]
fn config_persistence_permissions_cover_lock_and_backup_artifact_policy() {
    let root = TempDir::new("siblings");
    let config_dir = root.path().join("config");
    for name in ["config.toml", "config.toml.lock", "config.toml.bak"] {
        atomic_config_write(&config_dir.join(name), "private").unwrap();
        assert_eq!(mode(&config_dir.join(name)), 0o600);
    }
    assert_eq!(mode(&config_dir), 0o700);
}

#[test]
fn config_persistence_permissions_errors_do_not_echo_secret_values() {
    let secret = "SENTINEL-PLAINTEXT-API-KEY";
    let invalid = format!("api_keys = {{ openai = \"{secret}\" \n");
    let error = parse_config_content(Path::new("config.toml"), &invalid).unwrap_err();
    assert!(!error.to_string().contains(secret));
}

#[cfg(windows)]
#[test]
fn config_persistence_permissions_windows_dacls_exclude_broad_principals() {
    let root = TempDir::new("windows-dacl");
    let config_dir = root.path().join("config");
    for name in ["config.toml", "config.toml.lock", "config.toml.bak"] {
        atomic_config_write(&config_dir.join(name), "private").unwrap();
        atomic_config_write(&config_dir.join(name), "replacement").unwrap();
        assert_eq!(
            std::fs::read_to_string(config_dir.join(name)).unwrap(),
            "replacement"
        );
    }

    for (path, directory) in [
        (config_dir.clone(), true),
        (config_dir.join("config.toml"), false),
        (config_dir.join("config.toml.lock"), false),
        (config_dir.join("config.toml.bak"), false),
    ] {
        let dacl = crate::fs::private_dacl_sddl(&path, directory).unwrap();
        assert!(
            dacl.starts_with("D:P"),
            "DACL inherits broad grants: {dacl}"
        );
        assert!(
            !dacl.contains(";;;WD)") && !dacl.contains("S-1-1-0"),
            "Everyone can access config content: {dacl}"
        );
        assert!(
            !dacl.contains(";;;BU)") && !dacl.contains("S-1-5-32-545"),
            "ordinary Users can access config content: {dacl}"
        );
    }
}

#[cfg(windows)]
fn open_without_delete_sharing(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .unwrap()
}

#[cfg(windows)]
#[test]
fn config_persistence_permissions_windows_retries_transient_replacement_lock() {
    let root = TempDir::new("windows-transient-lock");
    let config_dir = root.path().join("config");
    let config = config_dir.join("config.toml");
    atomic_config_write(&config, "old").unwrap();

    let locked = open_without_delete_sharing(&config);
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(locked);
    });
    atomic_config_write(&config, "new").unwrap();
    release.join().unwrap();

    assert_eq!(std::fs::read_to_string(&config).unwrap(), "new");
    assert_eq!(atomic_temp_residue(&config_dir), 0);
}

#[cfg(windows)]
#[test]
fn config_persistence_permissions_windows_persistent_lock_preserves_old_file() {
    let root = TempDir::new("windows-persistent-lock");
    let config_dir = root.path().join("config");
    let config = config_dir.join("config.toml");
    atomic_config_write(&config, "old-secret").unwrap();
    let original_dacl = crate::fs::private_dacl_sddl(&config, false).unwrap();
    let _locked = open_without_delete_sharing(&config);

    let error = atomic_config_write(&config, "new-secret").unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains(&config.display().to_string()));
    assert!(error.to_string().contains("temporarily locked"));
    assert!(!error.to_string().contains("new-secret"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "old-secret");
    assert_eq!(
        crate::fs::private_dacl_sddl(&config, false).unwrap(),
        original_dacl
    );
    assert_eq!(atomic_temp_residue(&config_dir), 0);
}

#[cfg(not(any(unix, windows)))]
#[test]
fn config_persistence_permissions_unsupported_platform_fails_closed() {
    let root = TempDir::new("unsupported");
    let error = atomic_config_write(&root.path().join("config.toml"), "private").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}
