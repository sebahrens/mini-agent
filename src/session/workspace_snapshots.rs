//! Whole-workspace snapshots: the optional second `/rewind` tier that also
//! undoes what shell commands did (formatters, codegen, `git mv`, `sed -i`).
//!
//! Before the first shell command of a turn the workspace is recorded as a
//! content-addressed manifest in the session's private tool-output directory:
//! every regular file the project's ignore rules do not exclude, hashed, with
//! its bytes stored once per distinct content. Ignored files, symbolic links
//! and `.git` are never recorded and never touched by a restore. A workspace
//! over the file-count or byte caps is refused (and says so) rather than
//! partially recorded; a single oversized file is left out of the snapshot and
//! out of any restore. The user's own Git metadata is never used.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

/// `[file_checkpoints] workspace_snapshots`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Snapshot unless the workspace exceeds the caps (default).
    #[default]
    Auto,
    /// Always attempt; a refusal is announced to the model and the user.
    On,
    Off,
}
static MODE: RwLock<Mode> = RwLock::new(Mode::Auto);
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn configure(mode: Mode) {
    *MODE.write().unwrap_or_else(|e| e.into_inner()) = mode;
}
pub(crate) fn mode() -> Mode {
    *MODE.read().unwrap_or_else(|e| e.into_inner())
}

/// Caps of one snapshot and of one session's accumulated snapshot storage.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Caps {
    pub files: usize,
    pub bytes: u64,
    pub file_bytes: u64,
    pub session_bytes: u64,
}
impl Caps {
    pub(crate) const DEFAULT: Self = Self {
        files: 20_000,
        bytes: 256 * 1024 * 1024,
        file_bytes: 16 * 1024 * 1024,
        session_bytes: 1024 * 1024 * 1024,
    };
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub digest: String,
    pub len: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub files: BTreeMap<String, Entry>,
    /// Files left out because they exceed the per-file cap.
    #[serde(default)]
    pub omitted: Vec<String>,
}

/// What `/rewind` will do for a workspace restore.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Present in the snapshot but changed or deleted since.
    pub restore: Vec<String>,
    /// Created since the snapshot (and not ignored).
    pub remove: Vec<String>,
}
impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.restore.is_empty() && self.remove.is_empty()
    }
    pub(crate) fn summary(&self) -> String {
        let names = self
            .restore
            .iter()
            .chain(&self.remove)
            .take(5)
            .map(|path| format!("{path:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} to restore, {} to delete: {names}{}",
            self.restore.len(),
            self.remove.len(),
            if self.restore.len() + self.remove.len() > 5 {
                ", …"
            } else {
                ""
            }
        )
    }
}

/// The manifest spelling of a workspace-relative path: `/` separators on every
/// platform, so a snapshot's keys do not depend on the host's separator. On
/// Unix this is the identity (a backslash is an ordinary file name byte).
fn manifest_key(relative: &str) -> String {
    if cfg!(windows) {
        relative.replace('\\', "/")
    } else {
        relative.to_owned()
    }
}

/// Windows names are case-insensitive: a path the plan wants to delete and
/// one it wants to restore that differ only in case (a case-only rename since
/// the snapshot) address the same file, so restoring one and removing the other
/// would destroy the restored bytes. Returns one such pair.
fn case_collision(plan: &Plan) -> Option<(String, String)> {
    let restored: HashMap<String, &String> = plan
        .restore
        .iter()
        .map(|path| (path.to_lowercase(), path))
        .collect();
    plan.remove.iter().find_map(|path| {
        restored
            .get(&path.to_lowercase())
            .map(|other| ((*other).clone(), path.clone()))
    })
}

fn valid_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
fn hex_digest(bytes: &[u8]) -> String {
    crate::hex::encode_lower(Sha256::digest(bytes))
}

/// Stat-keyed hashes of the previous scan of one root, so an unchanged file is
/// not read again. Entries touched in the last two seconds are re-read: a
/// same-size rewrite within one timestamp tick would otherwise be missed.
type StatCache = HashMap<String, (u128, u64, String)>;
static STAT_CACHE: Mutex<Option<HashMap<PathBuf, StatCache>>> = Mutex::new(None);
const RACY_WINDOW_NANOS: u128 = 2_000_000_000;

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// One scanned file: relative path, length, and (when requested) its bytes.
struct Scanned {
    path: String,
    len: u64,
    digest: String,
    bytes: Option<Vec<u8>>,
}
struct Scan {
    files: Vec<Scanned>,
    omitted: Vec<String>,
}

/// Lists and hashes the workspace. `keep_bytes` retains the content of files
/// whose digest the cache could not vouch for (the capture path needs them).
fn scan(root: &Path, caps: Caps, keep_bytes: bool) -> Result<Scan, String> {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .follow_links(false)
        .git_global(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    let mut cache = STAT_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|caches| caches.remove(root))
        .unwrap_or_default();
    let mut next = StatCache::new();
    let mut files = Vec::new();
    let mut omitted = Vec::new();
    let mut total = 0u64;
    let now = now_nanos();
    for entry in walker.build() {
        let entry = entry.map_err(|error| format!("workspace walk failed: {error}"))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(path) = relative.to_str().map(manifest_key) else {
            omitted.push(manifest_key(&relative.to_string_lossy()));
            continue;
        };
        let metadata = entry
            .metadata()
            .map_err(|error| format!("workspace stat failed: {error}"))?;
        let len = metadata.len();
        if len > caps.file_bytes {
            omitted.push(path);
            continue;
        }
        if files.len() >= caps.files {
            return Err(format!(
                "the workspace has more than {} files (snapshot file cap)",
                caps.files
            ));
        }
        total = total.saturating_add(len);
        if total > caps.bytes {
            return Err(format!(
                "the workspace is larger than {} bytes (snapshot byte cap)",
                caps.bytes
            ));
        }
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let cached = cache.remove(&path).filter(|(m, l, _)| {
            *m == mtime && *l == len && now.saturating_sub(mtime) > RACY_WINDOW_NANOS
        });
        let (digest, bytes) = match cached {
            Some((_, _, digest)) if !keep_bytes => (digest, None),
            cached => {
                let mut bytes = Vec::with_capacity(len as usize);
                std::fs::File::open(entry.path())
                    .and_then(|file| file.take(caps.file_bytes + 1).read_to_end(&mut bytes))
                    .map_err(|error| format!("could not read {path}: {error}"))?;
                if bytes.len() as u64 > caps.file_bytes {
                    omitted.push(path);
                    continue;
                }
                let digest = match cached {
                    Some((_, _, digest)) => digest,
                    None => hex_digest(&bytes),
                };
                (digest, Some(bytes))
            }
        };
        next.insert(path.clone(), (mtime, len, digest.clone()));
        files.push(Scanned {
            path,
            len,
            digest,
            bytes,
        });
    }
    STAT_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(root.to_path_buf(), next);
    omitted.sort();
    Ok(Scan { files, omitted })
}

/// Records the workspace under `dir`; returns the manifest digest and the new
/// bytes written. `Err` carries the refusal reason.
pub(crate) fn capture(root: &Path, dir: &Path, caps: Caps) -> Result<(String, u64), String> {
    let scanned = scan(root, caps, false)?;
    crate::paths::ensure_private_directory(dir).map_err(|e| format!("snapshot storage: {e}"))?;
    let mut manifest = Manifest {
        omitted: scanned.omitted,
        ..Default::default()
    };
    let mut written = 0u64;
    for file in scanned.files {
        let blob = dir.join(format!("{}.bin", file.digest));
        if std::fs::symlink_metadata(&blob).is_err() {
            // The cache vouched for the digest but the blob is gone: re-read.
            let bytes = match file.bytes {
                Some(bytes) => bytes,
                None => std::fs::read(root.join(&file.path))
                    .map_err(|error| format!("could not read {}: {error}", file.path))?,
            };
            if hex_digest(&bytes) != file.digest {
                return Err(format!("{} changed while it was recorded", file.path));
            }
            crate::fs::private_atomic_create_sync(&blob, &bytes)
                .map_err(|error| format!("snapshot storage: {error}"))?;
            written = written.saturating_add(file.len);
        }
        manifest.files.insert(
            file.path,
            Entry {
                digest: file.digest,
                len: file.len,
            },
        );
    }
    let json = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    let id = hex_digest(&json);
    let path = dir.join(format!("m-{id}.json"));
    if std::fs::symlink_metadata(&path).is_err() {
        crate::fs::private_atomic_create_sync(&path, &json)
            .map_err(|error| format!("snapshot storage: {error}"))?;
    }
    Ok((id, written))
}

pub(crate) fn read_manifest(dir: &Path, id: &str) -> Result<Manifest, String> {
    if !valid_digest(id) {
        return Err("invalid snapshot id".into());
    }
    let file = crate::fs::open_private_file(&dir.join(format!("m-{id}.json")))
        .map_err(|error| format!("snapshot manifest unavailable: {error}"))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if hex_digest(&bytes) != id {
        return Err("snapshot manifest changed".into());
    }
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

/// What a restore of `manifest` would change right now.
pub(crate) fn plan(root: &Path, manifest: &Manifest, caps: Caps) -> Result<Plan, String> {
    let current = scan(root, caps, false)?;
    let mut plan = Plan::default();
    let mut seen = std::collections::BTreeSet::new();
    for file in &current.files {
        seen.insert(file.path.as_str());
        match manifest.files.get(&file.path) {
            None if !manifest.omitted.contains(&file.path) => plan.remove.push(file.path.clone()),
            Some(entry) if entry.digest != file.digest => plan.restore.push(file.path.clone()),
            _ => {}
        }
    }
    for path in manifest.files.keys() {
        if !seen.contains(path.as_str()) {
            plan.restore.push(path.clone());
        }
    }
    plan.restore.sort();
    plan.remove.sort();
    if cfg!(windows)
        && let Some((restore, remove)) = case_collision(&plan)
    {
        return Err(format!(
            "{remove} was renamed from {restore} by changing only its letter case since the snapshot; \
             the workspace restore refuses rather than delete the file it would restore"
        ));
    }
    Ok(plan)
}

fn read_blob(dir: &Path, entry: &Entry) -> Result<Vec<u8>, String> {
    if !valid_digest(&entry.digest) {
        return Err("invalid snapshot digest".into());
    }
    let file = crate::fs::open_private_file(&dir.join(format!("{}.bin", entry.digest)))
        .map_err(|error| format!("snapshot content unavailable: {error}"))?;
    let mut bytes = Vec::new();
    file.take(Caps::DEFAULT.file_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if hex_digest(&bytes) != entry.digest {
        return Err("snapshot content changed".into());
    }
    Ok(bytes)
}

/// Windows: refuse before the first write when a path the restore would
/// replace or delete sits behind a link or is held open for writing (or
/// without delete sharing) by another process, so the common blockers leave
/// the workspace untouched instead of half restored. The probe opens the name
/// with DELETE access only and sharing that excludes writers; it writes
/// nothing and the checked publications below still re-verify every file.
#[cfg(windows)]
fn preflight_windows(
    workspace: &crate::paths::WorkspaceBinding,
    plan: &Plan,
) -> Result<(), String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ};
    for path in plan.restore.iter().chain(&plan.remove) {
        let relative = Path::new(path);
        if workspace.symlink_component(relative).is_some() {
            return Err(format!(
                "nothing restored; {path} sits behind a symbolic link or junction"
            ));
        }
        let probe = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(workspace.root().join(relative));
        match probe {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "nothing restored; {path} is open in another program or cannot be replaced: {error}"
                ));
            }
        }
    }
    Ok(())
}

/// Applies `plan` through the bound workspace's checked primitives. Every
/// stored blob is verified before the first write, so a damaged snapshot
/// changes nothing; each publication re-checks identity and content.
pub(crate) fn apply(
    workspace: &crate::paths::WorkspaceBinding,
    dir: &Path,
    manifest: &Manifest,
    plan: &Plan,
) -> Result<usize, String> {
    let mut contents = Vec::new();
    for path in &plan.restore {
        let entry = manifest
            .files
            .get(path)
            .ok_or_else(|| format!("{path} is not in the snapshot"))?;
        contents.push((path, read_blob(dir, entry)?));
    }
    #[cfg(windows)]
    preflight_windows(workspace, plan)?;
    let mut done = 0usize;
    for (path, bytes) in contents {
        let relative = Path::new(path);
        if workspace.symlink_component(relative).is_some() {
            return Err(format!(
                "restored {done} files; {path} now sits behind a symbolic link"
            ));
        }
        let result = match workspace.open_regular_relative(relative) {
            Ok(mut file) => {
                let expected =
                    crate::fs::checked_file_metadata(&file).map_err(|e| e.to_string())?;
                let mut current = Vec::new();
                file.by_ref()
                    .take(Caps::DEFAULT.file_bytes + 1)
                    .read_to_end(&mut current)
                    .map_err(|e| e.to_string())?;
                workspace.replace_relative_atomic_expecting(
                    relative,
                    &bytes,
                    &expected,
                    &crate::fs::ContentDigest::of(&current),
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                workspace.create_relative_atomic(relative, &bytes)
            }
            Err(error) => Err(error),
        };
        result.map_err(|error| format!("restored {done} files; stopped at {path}: {error}"))?;
        done += 1;
    }
    for path in &plan.remove {
        let relative = Path::new(path);
        let mut file = match workspace.open_regular_relative(relative) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("restored {done} files; {path}: {error}")),
        };
        let expected = crate::fs::checked_file_metadata(&file).map_err(|e| e.to_string())?;
        let mut current = Vec::new();
        file.by_ref()
            .take(Caps::DEFAULT.file_bytes + 1)
            .read_to_end(&mut current)
            .map_err(|e| e.to_string())?;
        workspace
            .retire_relative_expecting(relative, &expected, &crate::fs::ContentDigest::of(&current))
            .map_err(|error| format!("restored {done} files; stopped at {path}: {error}"))?;
        done += 1;
    }
    Ok(done)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn caps() -> Caps {
        Caps::DEFAULT
    }

    #[test]
    fn nvnkx140_snapshot_restores_edits_creations_and_deletions_but_not_ignored_files() {
        let root = crate::tests::OwnedTestDirectory::new("nvnkx140-fixture");
        let store = crate::tests::OwnedTestDirectory::new("nvnkx140-store");
        let dir = store.path().join("snapshots");
        let ws = root.path();
        std::fs::write(ws.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(ws.join("src/nested")).unwrap();
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("edited.rs"), "fn main() {}\n").unwrap();
        std::fs::write(ws.join("deleted.rs"), "// gone soon\n").unwrap();
        std::fs::write(ws.join("src/nested/keep.rs"), "keep\n").unwrap();
        std::fs::write(ws.join("target/build.o"), "object\n").unwrap();
        std::fs::write(ws.join("trace.log"), "log\n").unwrap();
        let (id, _) = capture(ws, &dir, caps()).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        assert!(manifest.files.contains_key("edited.rs"));
        assert!(!manifest.files.contains_key("target/build.o"));
        assert!(!manifest.files.contains_key("trace.log"));

        // What a shell command might do.
        std::fs::write(ws.join("edited.rs"), "fn main() { println!(); }\n").unwrap();
        std::fs::remove_file(ws.join("deleted.rs")).unwrap();
        std::fs::write(ws.join("created.rs"), "new\n").unwrap();
        std::fs::create_dir_all(ws.join("gen/deep")).unwrap();
        std::fs::write(ws.join("gen/deep/out.rs"), "generated\n").unwrap();
        std::fs::write(ws.join("target/build.o"), "ignored change\n").unwrap();
        std::fs::write(ws.join("trace.log"), "ignored log change\n").unwrap();

        let plan = plan(ws, &manifest, caps()).unwrap();
        assert_eq!(plan.restore, vec!["deleted.rs", "edited.rs"]);
        assert_eq!(plan.remove, vec!["created.rs", "gen/deep/out.rs"]);
        assert!(plan.summary().contains("2 to restore, 2 to delete"));
        let binding = crate::paths::WorkspaceBinding::capture(ws).unwrap();
        assert_eq!(apply(&binding, &dir, &manifest, &plan).unwrap(), 4);
        assert_eq!(
            std::fs::read_to_string(ws.join("edited.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("deleted.rs")).unwrap(),
            "// gone soon\n"
        );
        assert!(!ws.join("created.rs").exists());
        assert!(!ws.join("gen/deep/out.rs").exists());
        assert_eq!(
            std::fs::read_to_string(ws.join("target/build.o")).unwrap(),
            "ignored change\n",
            "ignored files are never touched"
        );
        assert!(plan_after(ws, &manifest).is_empty());
    }

    fn plan_after(ws: &Path, manifest: &Manifest) -> Plan {
        plan(ws, manifest, caps()).unwrap()
    }

    #[test]
    fn nvnkx140_exceeding_a_cap_refuses_instead_of_recording_partially() {
        let root = crate::tests::OwnedTestDirectory::new("nvnkx140-cap");
        let store = crate::tests::OwnedTestDirectory::new("nvnkx140-cap-store");
        let dir = store.path().join("snapshots");
        for n in 0..5 {
            std::fs::write(root.path().join(format!("f{n}")), "x").unwrap();
        }
        let tight = Caps { files: 3, ..caps() };
        let error = capture(root.path(), &dir, tight).unwrap_err();
        assert!(error.contains("more than 3 files"), "{error}");
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().count() == 0);
        let bytes = Caps { bytes: 2, ..caps() };
        assert!(
            capture(root.path(), &dir, bytes)
                .unwrap_err()
                .contains("byte cap")
        );
        // One oversized file is left out, not fatal, and never restored.
        std::fs::write(root.path().join("big.bin"), vec![1u8; 64]).unwrap();
        let small_files = Caps {
            file_bytes: 8,
            ..caps()
        };
        let (id, _) = capture(root.path(), &dir, small_files).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        assert_eq!(manifest.omitted, vec!["big.bin"]);
        std::fs::write(root.path().join("big.bin"), vec![2u8; 64]).unwrap();
        let plan = plan(root.path(), &manifest, small_files).unwrap();
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn nvnkx140_a_damaged_blob_changes_nothing() {
        let root = crate::tests::OwnedTestDirectory::new("nvnkx140-damage");
        let store = crate::tests::OwnedTestDirectory::new("nvnkx140-damage-store");
        let dir = store.path().join("snapshots");
        std::fs::write(root.path().join("a"), "one").unwrap();
        std::fs::write(root.path().join("b"), "two").unwrap();
        let (id, _) = capture(root.path(), &dir, caps()).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        std::fs::write(root.path().join("a"), "ONE").unwrap();
        std::fs::write(root.path().join("b"), "TWO").unwrap();
        let blob = dir.join(format!("{}.bin", manifest.files["b"].digest));
        std::fs::remove_file(&blob).unwrap();
        let plan = plan(root.path(), &manifest, caps()).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        assert!(apply(&binding, &dir, &manifest, &plan).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("a")).unwrap(),
            "ONE"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    fn fixture(
        label: &str,
    ) -> (
        crate::tests::OwnedTestDirectory,
        crate::tests::OwnedTestDirectory,
    ) {
        (
            crate::tests::OwnedTestDirectory::new(&format!("rb4w2-{label}")),
            crate::tests::OwnedTestDirectory::new(&format!("rb4w2-{label}-store")),
        )
    }

    fn junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");
    }

    #[test]
    fn rb4w2_restores_modified_deleted_and_created_files_with_portable_keys() {
        let (root, store) = fixture("restore");
        let dir = store.path().join("snapshots");
        let ws = root.path();
        std::fs::write(ws.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(ws.join("src").join("nested")).unwrap();
        std::fs::create_dir(ws.join("ignored")).unwrap();
        std::fs::write(ws.join("edited.txt"), "before\n").unwrap();
        std::fs::write(ws.join("deleted.txt"), "doomed\n").unwrap();
        std::fs::write(ws.join("src").join("nested").join("keep.rs"), "keep\n").unwrap();
        std::fs::write(ws.join("ignored").join("cache"), "cache\n").unwrap();
        let (id, _) = capture(ws, &dir, Caps::DEFAULT).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        assert!(
            manifest.files.contains_key("src/nested/keep.rs"),
            "{manifest:?}"
        );

        std::fs::write(ws.join("edited.txt"), "after\n").unwrap();
        std::fs::remove_file(ws.join("deleted.txt")).unwrap();
        std::fs::remove_file(ws.join("src").join("nested").join("keep.rs")).unwrap();
        std::fs::create_dir_all(ws.join("gen").join("deep")).unwrap();
        std::fs::write(ws.join("gen").join("deep").join("out.rs"), "new\n").unwrap();
        std::fs::write(ws.join("created.txt"), "new\n").unwrap();
        std::fs::write(ws.join("ignored").join("cache"), "rebuilt\n").unwrap();

        let plan = plan(ws, &manifest, Caps::DEFAULT).unwrap();
        assert_eq!(
            plan.restore,
            vec!["deleted.txt", "edited.txt", "src/nested/keep.rs"]
        );
        assert_eq!(plan.remove, vec!["created.txt", "gen/deep/out.rs"]);
        let binding = crate::paths::WorkspaceBinding::capture(ws).unwrap();
        assert_eq!(apply(&binding, &dir, &manifest, &plan).unwrap(), 5);
        assert_eq!(
            std::fs::read_to_string(ws.join("edited.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("deleted.txt")).unwrap(),
            "doomed\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("src").join("nested").join("keep.rs")).unwrap(),
            "keep\n"
        );
        assert!(!ws.join("created.txt").exists());
        assert!(!ws.join("gen").join("deep").join("out.rs").exists());
        assert_eq!(
            std::fs::read_to_string(ws.join("ignored").join("cache")).unwrap(),
            "rebuilt\n"
        );
        assert!(plan_after(ws, &manifest).is_empty());
    }

    fn plan_after(ws: &Path, manifest: &Manifest) -> Plan {
        plan(ws, manifest, Caps::DEFAULT).unwrap()
    }

    #[test]
    fn rb4w2_a_read_only_file_is_restored() {
        let (root, store) = fixture("readonly");
        let dir = store.path().join("snapshots");
        let ws = root.path();
        std::fs::write(ws.join("ro.txt"), "before\n").unwrap();
        let (id, _) = capture(ws, &dir, Caps::DEFAULT).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        std::fs::write(ws.join("ro.txt"), "after\n").unwrap();
        let mut permissions = std::fs::metadata(ws.join("ro.txt")).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(ws.join("ro.txt"), permissions).unwrap();
        let plan = plan(ws, &manifest, Caps::DEFAULT).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(ws).unwrap();
        let result = apply(&binding, &dir, &manifest, &plan);
        eprintln!("read-only restore result: {result:?}");
        let mut permissions = std::fs::metadata(ws.join("ro.txt")).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(ws.join("ro.txt"), permissions).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("ro.txt")).unwrap(),
            "before\n"
        );
    }

    #[test]
    fn rb4w2_a_file_held_open_for_writing_refuses_the_whole_restore() {
        let (root, store) = fixture("held");
        let dir = store.path().join("snapshots");
        let ws = root.path();
        std::fs::write(ws.join("a.txt"), "a before\n").unwrap();
        std::fs::write(ws.join("b.txt"), "b before\n").unwrap();
        std::fs::write(ws.join("c.txt"), "c before\n").unwrap();
        let (id, _) = capture(ws, &dir, Caps::DEFAULT).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(ws.join(name), "changed\n").unwrap();
        }
        // A process keeping b.txt open for writing without sharing delete.
        let holder = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .open(ws.join("b.txt"))
            .unwrap();
        let plan = plan(ws, &manifest, Caps::DEFAULT).unwrap();
        assert_eq!(plan.restore, vec!["a.txt", "b.txt", "c.txt"]);
        let binding = crate::paths::WorkspaceBinding::capture(ws).unwrap();
        let error = apply(&binding, &dir, &manifest, &plan).unwrap_err();
        assert!(error.contains("nothing restored"), "{error}");
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                std::fs::read_to_string(ws.join(name)).unwrap(),
                "changed\n",
                "{name} must be untouched"
            );
        }
        drop(holder);
        assert_eq!(apply(&binding, &dir, &manifest, &plan).unwrap(), 3);
        assert_eq!(
            std::fs::read_to_string(ws.join("b.txt")).unwrap(),
            "b before\n"
        );
    }

    #[test]
    fn rb4w2_a_junction_is_not_followed_out_of_the_workspace() {
        let (root, store) = fixture("junction");
        let outside = crate::tests::OwnedTestDirectory::new("rb4w2-junction-outside");
        let dir = store.path().join("snapshots");
        let ws = root.path();
        std::fs::write(outside.path().join("secret.txt"), "outside\n").unwrap();
        std::fs::create_dir(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub").join("f.txt"), "inside\n").unwrap();
        junction(&ws.join("link"), outside.path());
        let (id, _) = capture(ws, &dir, Caps::DEFAULT).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        assert!(
            manifest.files.keys().all(|key| !key.starts_with("link")),
            "{manifest:?}"
        );
        // The junction's target changes: not part of the workspace.
        std::fs::write(outside.path().join("secret.txt"), "changed\n").unwrap();
        std::fs::write(outside.path().join("new.txt"), "new\n").unwrap();
        assert!(plan(ws, &manifest, Caps::DEFAULT).unwrap().is_empty());

        // A directory swapped for a junction after the snapshot.
        std::fs::remove_dir_all(ws.join("sub")).unwrap();
        junction(&ws.join("sub"), outside.path());
        std::fs::write(outside.path().join("f.txt"), "victim\n").unwrap();
        let plan = plan(ws, &manifest, Caps::DEFAULT).unwrap();
        assert_eq!(plan.restore, vec!["sub/f.txt"]);
        let binding = crate::paths::WorkspaceBinding::capture(ws).unwrap();
        let error = apply(&binding, &dir, &manifest, &plan).unwrap_err();
        assert!(error.contains("nothing restored"), "{error}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("f.txt")).unwrap(),
            "victim\n"
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "changed\n"
        );
        std::fs::remove_dir(ws.join("sub")).unwrap();
        std::fs::remove_dir(ws.join("link")).unwrap();
    }

    #[test]
    fn rb4w2_a_case_only_rename_is_refused_instead_of_deleting_the_restored_file() {
        let (root, store) = fixture("case");
        let dir = store.path().join("snapshots");
        let ws = root.path();
        std::fs::write(ws.join("Readme.txt"), "before\n").unwrap();
        let (id, _) = capture(ws, &dir, Caps::DEFAULT).unwrap();
        let manifest = read_manifest(&dir, &id).unwrap();
        std::fs::rename(ws.join("Readme.txt"), ws.join("readme.txt")).unwrap();
        std::fs::write(ws.join("readme.txt"), "after\n").unwrap();
        let error = plan(ws, &manifest, Caps::DEFAULT).unwrap_err();
        assert!(error.contains("letter case"), "{error}");
        assert_eq!(
            std::fs::read_to_string(ws.join("readme.txt")).unwrap(),
            "after\n"
        );
    }
}
