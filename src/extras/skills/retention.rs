//! Retention of superseded Agent Skill digests.
//!
//! Every import publishes an immutable `<data-dir>/agent-skills/<name>/<digest>`
//! tree and repoints `ACTIVE`; the previous digest stays on disk because a
//! running session may still read it. This module decides when such a
//! superseded digest can go. A digest is removed only when **all** of these
//! hold:
//!
//! * its package has a valid `ACTIVE` pointer naming a different, installed
//!   digest (legacy packages without a pointer are never pruned);
//! * that pointer has not changed for [`RETENTION_WINDOW`], so nothing was
//!   superseded or re-activated in that package recently;
//! * it is not one of the [`RETAINED_SUPERSEDED`] most recently installed
//!   superseded digests of its package;
//! * it was installed more than [`RETENTION_WINDOW`] ago; and
//! * no running session holds a lease naming it.
//!
//! A session lease is a `<data-dir>/agent-skill-leases/<id>.lock` file the
//! owning catalog keeps exclusively locked for its lifetime, beside an
//! `<id>.digests` list of the `<name>/<digest>` trees its current and previous
//! index generations read. The pruner treats a lease it cannot lock as live;
//! one it can lock belonged to a process that exited without cleaning up and is
//! removed. When the lease directory cannot be read reliably, nothing is pruned.
//!
//! Pruning renames a digest to a hidden `.import-pruned-*` name before deleting
//! it, so a concurrent reader never sees a half-deleted tree, a killed prune is
//! cleaned by the stale-import sweep, and a pointer that moved back to the
//! digest during the rename restores it.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Superseded digests per package that are always kept, newest install first.
pub(super) const RETAINED_SUPERSEDED: usize = 2;
/// Minimum age of both a superseded digest and its package's pointer change.
pub(super) const RETENTION_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const LEASE_DIRECTORY: &str = "agent-skill-leases";
const LOCK_SUFFIX: &str = ".lock";
const DIGESTS_SUFFIX: &str = ".digests";
const PENDING_SUFFIX: &str = ".pending";
const MAX_LEASE_BYTES: u64 = 1024 * 1024;
const MAX_POINTER_BYTES: u64 = 128;
/// Unlocked lease leftovers younger than this may belong to a lease being created.
const ORPHAN_LEASE_AGE: Duration = Duration::from_secs(60 * 60);
const PRUNED_PREFIX: &str = ".import-pruned-";

pub(super) fn lease_root(data_dir: &Path) -> PathBuf {
    data_dir.join(LEASE_DIRECTORY)
}

/// A running session's claim on the installed digests its catalog published.
pub(super) struct SessionLease {
    // Held open (and locked) until drop; closing it releases the lock.
    _lock: fs::File,
    lock_path: PathBuf,
    digests_path: PathBuf,
    previous: BTreeSet<String>,
}

impl SessionLease {
    pub(super) fn acquire(lease_root: &Path) -> io::Result<Self> {
        crate::fs::ensure_private_directory(lease_root)?;
        let id = uuid::Uuid::new_v4();
        // Lock under a name the pruner ignores, then rename into place: a
        // pruner can never observe the lease unlocked and remove it as stale.
        let pending = lease_root.join(format!(".{id}{PENDING_SUFFIX}"));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&pending)?;
        if let Err(error) = lock.try_lock() {
            let _ = fs::remove_file(&pending);
            return Err(match error {
                fs::TryLockError::Error(error) => error,
                fs::TryLockError::WouldBlock => {
                    io::Error::other("new lease file is already locked")
                }
            });
        }
        let lock_path = lease_root.join(format!("{id}{LOCK_SUFFIX}"));
        if let Err(error) = fs::rename(&pending, &lock_path) {
            let _ = fs::remove_file(&pending);
            return Err(error);
        }
        Ok(Self {
            _lock: lock,
            lock_path,
            digests_path: lease_root.join(format!("{id}{DIGESTS_SUFFIX}")),
            previous: BTreeSet::new(),
        })
    }

    /// Record the digests of a newly published generation. The previous
    /// generation stays leased too, because an in-flight turn may still hold it.
    pub(super) fn record<'a>(
        &mut self,
        digests: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let current = digests
            .into_iter()
            .map(|(name, digest)| format!("{name}/{digest}"))
            .collect::<BTreeSet<_>>();
        let mut body = String::new();
        for entry in self.previous.union(&current) {
            body.push_str(entry);
            body.push('\n');
        }
        crate::fs::private_atomic_write_sync(&self.digests_path, body.as_bytes())?;
        self.previous = current;
        Ok(())
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.digests_path);
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// `<name>/<digest>` entries of every live lease, or `None` when the leases
/// cannot be determined (then nothing may be pruned). Leases whose owner
/// exited are removed.
fn live_leased_digests(lease_root: &Path, now: SystemTime) -> Option<BTreeSet<String>> {
    let entries = match fs::read_dir(lease_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(BTreeSet::new()),
        Err(_) => return None,
    };
    let mut leased = BTreeSet::new();
    for entry in entries {
        let entry = entry.ok()?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let path = entry.path();
        if let Some(id) = file_name.strip_suffix(LOCK_SUFFIX) {
            let digests_path = lease_root.join(format!("{id}{DIGESTS_SUFFIX}"));
            let lock = fs::OpenOptions::new().read(true).write(true).open(&path);
            let lock = match lock {
                Ok(lock) => lock,
                // Removed by its owner since the listing.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            match lock.try_lock() {
                Ok(()) => {
                    // The owner exited without cleaning up.
                    drop(lock);
                    let _ = fs::remove_file(&digests_path);
                    let _ = fs::remove_file(&path);
                }
                Err(fs::TryLockError::WouldBlock) => {
                    let bytes = match super::import::read_stable_file(
                        &digests_path,
                        MAX_LEASE_BYTES,
                        false,
                    ) {
                        Ok(bytes) => bytes,
                        // A live session that has not recorded a generation yet.
                        Err(super::ImportError::Io(error))
                            if error.kind() == io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(_) => return None,
                    };
                    let text = String::from_utf8(bytes).ok()?;
                    leased.extend(
                        text.lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty())
                            .map(str::to_string),
                    );
                }
                Err(fs::TryLockError::Error(_)) => return None,
            }
        } else if file_name.ends_with(PENDING_SUFFIX) || file_name.ends_with(DIGESTS_SUFFIX) {
            let orphaned = file_name.ends_with(PENDING_SUFFIX)
                || !lease_root
                    .join(format!(
                        "{}{LOCK_SUFFIX}",
                        file_name.trim_end_matches(DIGESTS_SUFFIX)
                    ))
                    .exists();
            if orphaned && older_than(&path, ORPHAN_LEASE_AGE, now) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Some(leased)
}

fn older_than(path: &Path, age: Duration, now: SystemTime) -> bool {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .is_some_and(|modified| !recent(modified, age, now))
}

/// Future timestamps count as recent, so a skewed clock only keeps more.
fn recent(time: SystemTime, window: Duration, now: SystemTime) -> bool {
    !now.duration_since(time).is_ok_and(|age| age >= window)
}

/// Remove superseded digests that the retention policy no longer protects.
/// Best effort: every failure keeps the digest. Returns the removed trees.
pub(super) fn prune_superseded_digests(
    install_root: &Path,
    lease_root: &Path,
    now: SystemTime,
) -> Vec<PathBuf> {
    let mut pruned = Vec::new();
    let Some(leased) = live_leased_digests(lease_root, now) else {
        tracing::debug!("Agent Skill leases are unreadable; skipping digest pruning");
        return pruned;
    };
    let Ok(packages) = fs::read_dir(install_root) else {
        return pruned;
    };
    for package in packages.flatten() {
        if !package.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = package.file_name().to_str().map(str::to_string) else {
            continue;
        };
        prune_package(&package.path(), &name, &leased, now, &mut pruned);
    }
    pruned
}

fn read_pointer(pointer: &Path) -> Option<String> {
    let bytes = super::import::read_stable_file(pointer, MAX_POINTER_BYTES, false).ok()?;
    let digest = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    is_digest(&digest).then_some(digest)
}

fn is_digest(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn prune_package(
    name_root: &Path,
    name: &str,
    leased: &BTreeSet<String>,
    now: SystemTime,
    pruned: &mut Vec<PathBuf>,
) {
    let pointer = name_root.join("ACTIVE");
    let Ok(pointer_metadata) = fs::symlink_metadata(&pointer) else {
        // Legacy package: nothing records which digest is current.
        return;
    };
    let Ok(pointer_modified) = pointer_metadata.modified() else {
        return;
    };
    if !pointer_metadata.is_file() || recent(pointer_modified, RETENTION_WINDOW, now) {
        return;
    }
    let Some(active) = read_pointer(&pointer) else {
        return;
    };
    if !name_root.join(&active).join("SKILL.md").is_file() {
        return;
    }
    let Ok(entries) = fs::read_dir(name_root) else {
        return;
    };
    let mut superseded = Vec::new();
    for entry in entries.flatten() {
        let digest = entry.file_name().to_string_lossy().to_string();
        if digest == active || !is_digest(&digest) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(installed) = metadata.modified() else {
            // Without an install time it cannot be ranked; keep it.
            continue;
        };
        superseded.push((installed, digest));
    }
    superseded.sort_by(|left, right| right.cmp(left));
    for (rank, (installed, digest)) in superseded.into_iter().enumerate() {
        if rank < RETAINED_SUPERSEDED
            || recent(installed, RETENTION_WINDOW, now)
            || leased.contains(&format!("{name}/{digest}"))
        {
            continue;
        }
        if let Some(path) = prune_digest(name_root, &pointer, pointer_modified, &active, &digest) {
            pruned.push(path);
        }
    }
}

fn prune_digest(
    name_root: &Path,
    pointer: &Path,
    pointer_modified: SystemTime,
    active: &str,
    digest: &str,
) -> Option<PathBuf> {
    let digest_root = name_root.join(digest);
    let doomed = name_root.join(format!("{PRUNED_PREFIX}{}", uuid::Uuid::new_v4()));
    // A read-only directory cannot always be moved (it updates its "..").
    let _ = super::import::make_directory_writable(&digest_root);
    if let Err(error) = fs::rename(&digest_root, &doomed) {
        // On Windows an open file inside the tree also refuses the rename.
        tracing::debug!(
            "kept superseded Agent Skill digest {}: {error}",
            digest_root.display()
        );
        let _ = super::import::make_directory_read_only(&digest_root);
        return None;
    }
    // An import may have re-activated this digest while it was being moved.
    let unchanged = fs::symlink_metadata(pointer)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified == pointer_modified)
        && read_pointer(pointer).as_deref() == Some(active);
    if !unchanged {
        if fs::rename(&doomed, &digest_root).is_ok() {
            let _ = super::import::make_directory_read_only(&digest_root);
        }
        return None;
    }
    if let Err(error) = super::import::remove_tree_no_follow(&doomed) {
        // The stale-import sweep removes the hidden tree later.
        tracing::debug!(
            "could not finish removing pruned Agent Skill digest {}: {error}",
            doomed.display()
        );
    }
    Some(digest_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mini-agent-retention-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = super::super::import::remove_tree_no_follow(&self.0);
        }
    }

    fn digest(seed: char) -> String {
        std::iter::repeat_n(seed, 64).collect()
    }

    fn set_modified(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[cfg(unix)]
    fn set_directory_modified(path: &Path, time: SystemTime) {
        fs::File::open(path).unwrap().set_modified(time).unwrap();
    }

    /// Install `digests` (oldest first, one day apart, all `age` old at the
    /// newest) with the last one ACTIVE and its pointer written `pointer_age` ago.
    #[cfg(unix)]
    fn package(
        root: &Path,
        name: &str,
        digests: &[String],
        now: SystemTime,
        age: Duration,
        pointer_age: Duration,
    ) -> PathBuf {
        let name_root = root.join(name);
        fs::create_dir_all(&name_root).unwrap();
        let count = digests.len() as u32;
        for (index, digest) in digests.iter().enumerate() {
            let tree = name_root.join(digest);
            fs::create_dir_all(&tree).unwrap();
            fs::write(
                tree.join("SKILL.md"),
                b"---\nname: x\ndescription: y\n---\n",
            )
            .unwrap();
            super::super::import::make_directory_read_only(&tree).unwrap();
            let installed = now - age - DAY * (count - 1 - index as u32);
            set_directory_modified(&tree, installed);
        }
        let pointer = name_root.join("ACTIVE");
        fs::write(&pointer, format!("{}\n", digests.last().unwrap())).unwrap();
        set_modified(&pointer, now - pointer_age);
        name_root
    }

    fn remaining(name_root: &Path) -> BTreeSet<String> {
        fs::read_dir(name_root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| is_digest(name))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn prunes_only_old_unleased_digests_beyond_the_newest_two() {
        let temp = Temp::new();
        let root = temp.0.join("agent-skills");
        let leases = temp.0.join("leases");
        let now = SystemTime::now();
        let digests = ['a', 'b', 'c', 'd', 'e'].map(digest).to_vec();
        let name_root = package(&root, "old-skill", &digests, now, 30 * DAY, 30 * DAY);

        let pruned = prune_superseded_digests(&root, &leases, now);

        // e is ACTIVE; d and c are the two newest superseded; a and b go.
        assert_eq!(pruned.len(), 2, "{pruned:?}");
        assert_eq!(
            remaining(&name_root),
            ['c', 'd', 'e'].map(digest).into_iter().collect()
        );
        assert!(
            !fs::read_dir(&name_root)
                .unwrap()
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PRUNED_PREFIX)),
            "the hidden pruning tree must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn keeps_everything_while_the_pointer_or_digests_are_recent() {
        let temp = Temp::new();
        let root = temp.0.join("agent-skills");
        let leases = temp.0.join("leases");
        let now = SystemTime::now();
        let digests = ['a', 'b', 'c', 'd'].map(digest).to_vec();
        // Superseded within the window: a pointer written two days ago.
        let recent_pointer = package(&root, "recent-pointer", &digests, now, 30 * DAY, 2 * DAY);
        // Old pointer, but every digest installed within the window.
        let recent_installs = package(&root, "recent-installs", &digests, now, DAY, 30 * DAY);

        assert!(prune_superseded_digests(&root, &leases, now).is_empty());
        assert_eq!(remaining(&recent_pointer).len(), 4);
        assert_eq!(remaining(&recent_installs).len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_and_broken_pointer_packages_are_never_pruned() {
        let temp = Temp::new();
        let root = temp.0.join("agent-skills");
        let leases = temp.0.join("leases");
        let now = SystemTime::now();
        let digests = ['a', 'b', 'c', 'd'].map(digest).to_vec();
        let legacy = package(&root, "legacy", &digests, now, 30 * DAY, 30 * DAY);
        fs::remove_file(legacy.join("ACTIVE")).unwrap();
        let broken = package(&root, "broken", &digests, now, 30 * DAY, 30 * DAY);
        let pointer = broken.join("ACTIVE");
        fs::write(&pointer, format!("{}\n", digest('f'))).unwrap();
        set_modified(&pointer, now - 30 * DAY);

        assert!(prune_superseded_digests(&root, &leases, now).is_empty());
        assert_eq!(remaining(&legacy).len(), 4);
        assert_eq!(remaining(&broken).len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_live_session_lease_protects_its_digests_and_a_dead_one_does_not() {
        let temp = Temp::new();
        let root = temp.0.join("agent-skills");
        let leases = temp.0.join("leases");
        let now = SystemTime::now();
        let digests = ['a', 'b', 'c', 'd', 'e'].map(digest).to_vec();
        let name_root = package(&root, "leased", &digests, now, 30 * DAY, 30 * DAY);

        let mut live = SessionLease::acquire(&leases).unwrap();
        live.record([("leased", digest('a').as_str())]).unwrap();
        // A lease whose owner exited: present on disk but unlocked.
        let dead_id = "dead";
        fs::write(leases.join(format!("{dead_id}{LOCK_SUFFIX}")), b"").unwrap();
        fs::write(
            leases.join(format!("{dead_id}{DIGESTS_SUFFIX}")),
            format!("leased/{}\n", digest('b')),
        )
        .unwrap();

        let pruned = prune_superseded_digests(&root, &leases, now);

        assert_eq!(pruned, vec![name_root.join(digest('b'))]);
        assert!(
            name_root.join(digest('a')).is_dir(),
            "live lease protects a"
        );
        assert!(!leases.join(format!("{dead_id}{LOCK_SUFFIX}")).exists());
        assert!(!leases.join(format!("{dead_id}{DIGESTS_SUFFIX}")).exists());

        drop(live);
        assert_eq!(
            fs::read_dir(&leases).unwrap().count(),
            0,
            "a dropped lease removes its files"
        );
        let pruned = prune_superseded_digests(&root, &leases, now);
        assert_eq!(pruned, vec![name_root.join(digest('a'))]);
    }

    #[test]
    fn lease_records_the_current_and_previous_generation() {
        let temp = Temp::new();
        let leases = temp.0.join("leases");
        let mut lease = SessionLease::acquire(&leases).unwrap();
        let a = digest('a');
        let b = digest('b');
        let c = digest('c');
        lease.record([("s", a.as_str())]).unwrap();
        lease.record([("s", b.as_str())]).unwrap();
        lease.record([("s", c.as_str())]).unwrap();

        let leased = live_leased_digests(&leases, SystemTime::now()).unwrap();

        assert_eq!(
            leased,
            [format!("s/{b}"), format!("s/{c}")].into_iter().collect()
        );
        // Only the renamed, locked file and its list exist; no pending name.
        let names = fs::read_dir(&leases)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|name| !name.ends_with(PENDING_SUFFIX)));
    }

    #[test]
    fn orphaned_lease_leftovers_are_removed_only_when_old() {
        let temp = Temp::new();
        let leases = temp.0.join("leases");
        fs::create_dir_all(&leases).unwrap();
        let pending = leases.join(format!(".x{PENDING_SUFFIX}"));
        let digests = leases.join(format!("y{DIGESTS_SUFFIX}"));
        fs::write(&pending, b"").unwrap();
        fs::write(&digests, b"s/x\n").unwrap();
        let now = SystemTime::now();

        let leased = live_leased_digests(&leases, now).unwrap();
        assert!(
            leased.is_empty(),
            "a digests list without a lock is not a lease"
        );
        assert!(pending.exists() && digests.exists());

        live_leased_digests(&leases, now + ORPHAN_LEASE_AGE * 2).unwrap();
        assert!(!pending.exists() && !digests.exists());
    }

    #[test]
    fn a_session_catalog_leases_what_it_published_until_dropped() {
        use crate::extras::js::skills::embed::Embedder;
        use crate::extras::skills::catalog::AgentSkillCatalog;
        use crate::paths::AppPaths;

        let temp = Temp::new();
        let paths = AppPaths {
            config_dir: temp.0.join("config"),
            data_dir: temp.0.join("data"),
            local_data_dir: temp.0.join("local-data"),
            state_dir: temp.0.join("state"),
            cache_dir: temp.0.join("cache"),
            credentials_dir: temp.0.join("credentials"),
            project_dir: None,
        };
        let source = temp.0.join("source").join("kept-skill");
        fs::create_dir_all(&source).unwrap();
        let import_version = |version: u32| {
            fs::write(
                source.join("SKILL.md"),
                format!(
                    "---\nname: kept-skill\ndescription: Version {version}.\n---\n\n# V{version}\n"
                ),
            )
            .unwrap();
            super::super::import_agent_skill(&source, &paths)
                .unwrap()
                .identity
                .digest
        };
        let install_root = paths.data_dir.join("agent-skills");
        let leases = lease_root(&paths.data_dir);
        let name_root = install_root.join("kept-skill");

        let first = import_version(1);
        let embedder = Embedder::new().unwrap();
        let mut catalog = AgentSkillCatalog::new(&paths);
        let index = catalog.refresh(&embedder).unwrap();
        assert_eq!(index.generation(), 1);
        for version in 2..=4 {
            import_version(version);
        }
        assert_eq!(
            remaining(&name_root).len(),
            4,
            "imports never prune digests superseded within the retention window"
        );

        // Well past the window, only the leased first version is beyond the
        // two newest superseded digests, and the running session protects it.
        let later = SystemTime::now() + RETENTION_WINDOW * 4;
        assert!(prune_superseded_digests(&install_root, &leases, later).is_empty());
        assert!(name_root.join(&first).is_dir());

        drop(index);
        drop(catalog);
        assert_eq!(
            prune_superseded_digests(&install_root, &leases, later),
            vec![name_root.join(&first)]
        );
        assert_eq!(remaining(&name_root).len(), 3);
    }
}
