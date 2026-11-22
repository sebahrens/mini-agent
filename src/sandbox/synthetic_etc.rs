//! Minimal synthetic `/etc/passwd` and `/etc/group` for the Linux `bwrap`
//! workspace profile.
//!
//! The workspace profile mounts a synthetic `/etc`, so `id -un`, `whoami`,
//! `ssh` and git's identity fallback could not resolve the current uid. The
//! host account database is deliberately not exposed (it lists every local
//! account). Instead a two-line file naming only the current user is staged in
//! a private owner-only directory and bound read-only. `/etc/os-release` is a
//! plain host file and is bound by the caller.

use std::path::{Path, PathBuf};

/// Render `(passwd, group)` for exactly one user.
pub(crate) fn render(uid: u32, gid: u32, name: &str, home: &str) -> (String, String) {
    let name = sanitize_name(name);
    let home = if home.starts_with('/') && !home.contains([':', '\n', '\0']) {
        home
    } else {
        "/"
    };
    (
        format!("{name}:x:{uid}:{gid}:{name}:{home}:/bin/sh\n"),
        format!("{name}:x:{gid}:\n"),
    )
}

/// Account names go into a colon-separated file: keep only portable
/// characters and fall back to `user`.
fn sanitize_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(32)
        .collect();
    if clean.is_empty() || clean.starts_with('-') {
        "user".to_string()
    } else {
        clean
    }
}

/// Paths of the staged host files, ready for `--ro-bind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyntheticAccountFiles {
    pub(crate) passwd: PathBuf,
    pub(crate) group: PathBuf,
}

/// Stage the files for the current user under `<base>/mini-agent-etc-<uid>/`.
/// Names carry a content hash, so concurrent launches converge on the same
/// immutable files. Returns `None` when staging fails; the sandbox then simply
/// lacks the files (the prior behaviour).
#[cfg(unix)]
pub(crate) fn stage_current_user(base: &Path) -> Option<SyntheticAccountFiles> {
    let (uid, gid) = current_ids();
    let name = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    stage(base, uid, gid, &name, &home)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn current_ids() -> (u32, u32) {
    // SAFETY: geteuid and getegid take no arguments and cannot fail.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

#[cfg(unix)]
fn stage(base: &Path, uid: u32, gid: u32, name: &str, home: &str) -> Option<SyntheticAccountFiles> {
    use std::hash::{Hash, Hasher};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::sync::Mutex;

    static STAGING: Mutex<()> = Mutex::new(());
    let _guard = STAGING.lock().unwrap_or_else(|p| p.into_inner());
    let dir = base.join(format!("mini-agent-etc-{uid}"));
    if std::fs::symlink_metadata(&dir).is_err() {
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    }
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    // A directory another user pre-created, a symlink, or a group/world
    // writable one could substitute the files: refuse it.
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
        return None;
    }
    let (passwd, group) = render(uid, gid, name, home);
    let put = |kind: &str, body: &str| -> Option<PathBuf> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        let path = dir.join(format!("{kind}-{:016x}", hasher.finish()));
        match std::fs::symlink_metadata(&path) {
            Ok(existing) => {
                let intact = existing.is_file()
                    && existing.uid() == uid
                    && std::fs::read_to_string(&path).is_ok_and(|text| text == body);
                intact.then_some(path)
            }
            Err(_) => {
                let temporary = dir.join(format!("{kind}.tmp-{}", std::process::id()));
                let _ = std::fs::remove_file(&temporary);
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o444)
                    .open(&temporary)
                    .ok()?;
                file.write_all(body.as_bytes()).ok()?;
                drop(file);
                std::fs::rename(&temporary, &path).ok()?;
                Some(path)
            }
        }
    };
    Some(SyntheticAccountFiles {
        passwd: put("passwd", &passwd)?,
        group: put("group", &group)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_only_the_current_user() {
        let (passwd, group) = render(1000, 1001, "alice", "/home/alice");
        assert_eq!(passwd, "alice:x:1000:1001:alice:/home/alice:/bin/sh\n");
        assert_eq!(group, "alice:x:1001:\n");
    }

    #[test]
    fn render_neutralises_hostile_fields() {
        let (passwd, _) = render(1, 1, "a:b\nroot:x:0:0::/:/bin/sh", "relative:dir");
        assert_eq!(passwd.lines().count(), 1, "{passwd:?}");
        assert!(passwd.starts_with("abrootx00"), "{passwd:?}");
        assert!(passwd.contains(":/:/bin/sh"), "{passwd:?}");
        assert!(render(1, 1, "", "/h").0.starts_with("user:x:"));
        assert!(render(1, 1, "-x", "/h").0.starts_with("user:x:"));
    }

    #[cfg(unix)]
    #[test]
    fn stage_writes_private_read_only_files_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mini-agent-synth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let (uid, gid) = current_ids();
        let first = stage(&base, uid, gid, "alice", "/home/alice").unwrap();
        let second = stage(&base, uid, gid, "alice", "/home/alice").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(&first.passwd).unwrap(),
            "alice:x:".to_string() + &format!("{uid}:{gid}:alice:/home/alice:/bin/sh\n")
        );
        let dir_mode = std::fs::metadata(first.passwd.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o077, 0);
        let file_mode = std::fs::metadata(&first.passwd)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o222, 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn stage_refuses_a_group_accessible_directory() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mini-agent-synth-{}", uuid::Uuid::new_v4()));
        let (uid, gid) = current_ids();
        let dir = base.join(format!("mini-agent-etc-{uid}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(stage(&base, uid, gid, "alice", "/home/alice").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
