use std::io::Read;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::session::Session;

pub(crate) const MAX_SESSION_BYTES: usize = 32 * 1024 * 1024;
const MAX_SESSION_MESSAGES: usize = 100_000;

/// The persisted-storage error remains static and never includes session data.
/// Frontends can use the attached counts for an actionable, deduplicated notice.
#[derive(Debug)]
pub(crate) struct SessionSizeError {
    pub(crate) encoded_bytes: usize,
    pub(crate) limit_bytes: usize,
}

impl std::fmt::Display for SessionSizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("persisted session exceeds the byte limit")
    }
}

impl std::error::Error for SessionSizeError {}

pub(crate) fn is_session_size_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SessionSizeError>().is_some()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum SaveSizeState {
    #[default]
    Normal,
    NearLimit,
    Oversized,
}

/// Ephemeral presentation state: excluded from storage, canonical digests and
/// imports. Warn on size-state transitions, rather than on every later turn.
#[derive(Debug, Default, Clone)]
pub(crate) struct SaveNoticeState {
    size: SaveSizeState,
    encoded_bytes: Option<usize>,
    pending: Vec<String>,
}

/// Decode only bounded data. Provider/workspace compatibility and fresh runtime
/// grants belong to the host's import controller, never saved labels.
pub(crate) fn decode_session_json(json: &str) -> anyhow::Result<Session> {
    anyhow::ensure!(
        json.len() <= MAX_SESSION_BYTES,
        "persisted session exceeds the byte limit"
    );
    let session: Session = serde_json::from_str(json)
        .map_err(|_| anyhow::anyhow!("invalid persisted session JSON"))?;
    anyhow::ensure!(
        session.messages.len() <= MAX_SESSION_MESSAGES,
        "persisted session exceeds the message limit"
    );
    session
        .validate_native_persistence()
        .map_err(anyhow::Error::from)?;
    Ok(session)
}

#[cfg(test)]
thread_local! {
    /// Counts whole-session serializations on this test thread so tests can
    /// pin that per-event paths (tool calls/results) do not re-encode it.
    pub(crate) static SERIALIZE_BOUNDED_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Whole-file session reads on this test thread (`read_private_string`),
    /// so tests can pin that head peeks avoid them.
    pub(crate) static FULL_SESSION_READS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Session file bytes. Identical to [`serialize_bounded_session`] except that a
/// tool-result row whose display `content` is `prefix + tool.output` writes the
/// output once (`content_prefix`), roughly halving tool-heavy files and the
/// session byte budget they consume (mini-agent-buv2v). Loading restores `content`
/// exactly, and every digest is taken over the canonical form, never this one.
pub(super) fn serialize_session_for_storage(session: &Session) -> anyhow::Result<String> {
    let _storage = super::StorageEncodingGuard::enable();
    serialize_bounded_session(session)
}

pub(super) fn serialize_bounded_session(session: &Session) -> anyhow::Result<String> {
    #[cfg(test)]
    SERIALIZE_BOUNDED_CALLS.with(|calls| calls.set(calls.get() + 1));
    // Encode the shared stores' coherent observation cut through a thread-local
    // override instead of deep-cloning the whole Session (mini-agent-hcb3s).
    #[cfg(feature = "subagents")]
    let _frozen = super::background_observation::SharedObservations::capture(session)
        .map(|observations| observations.install_frozen_serialization());
    session.validate_user_messages()?;
    anyhow::ensure!(
        session.messages.len() <= MAX_SESSION_MESSAGES,
        "persisted session exceeds the message limit"
    );
    struct BoundedJson {
        bytes: Vec<u8>,
        encoded_bytes: usize,
    }
    impl std::io::Write for BoundedJson {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.encoded_bytes = self.encoded_bytes.saturating_add(bytes.len());
            // Continue counting after overflow without retaining more bytes.
            // This reports the encoded size (including JSON escaping), while
            // keeping the buffer bounded and never writing a partial session.
            if self.encoded_bytes <= MAX_SESSION_BYTES {
                self.bytes.extend_from_slice(bytes);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Encode shared task/account owners once. A separate count pass would not
    // constrain a second snapshot if one of those owners changed meanwhile.
    let mut json = BoundedJson {
        bytes: Vec::new(),
        encoded_bytes: 0,
    };
    serde_json::to_writer(&mut json, session)?;
    if json.encoded_bytes > MAX_SESSION_BYTES {
        return Err(SessionSizeError {
            encoded_bytes: json.encoded_bytes,
            limit_bytes: MAX_SESSION_BYTES,
        }
        .into());
    }
    String::from_utf8(json.bytes).map_err(|_| anyhow::anyhow!("invalid persisted session UTF-8"))
}

/// Exact, noncreating and nonrepairing import. Both retained directories are
/// supplied by the host; stored working_dir is compared as text, never opened.
#[cfg(any(feature = "acp", test))]
#[allow(
    dead_code,
    reason = "shared API is intentionally dormant before its frontend consumer lands"
)]
pub(crate) fn load_session_exact_read_only(
    sessions_root: &crate::paths::WorkspaceBinding,
    requested_id: &str,
    workspace: &crate::paths::WorkspaceBinding,
) -> anyhow::Result<Option<Session>> {
    crate::paths::validate_portable_component(requested_id)?;
    sessions_root.validate_private_root()?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    let relative = PathBuf::from(format!("{requested_id}.json"));
    let mut file = match sessions_root.open_regular_relative(&relative) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    crate::fs::validate_private_file(&file)?;
    let identity = crate::fs::checked_file_metadata(&file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SESSION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_SESSION_BYTES,
        "persisted session exceeds the byte limit"
    );
    crate::fs::validate_private_file(&file)?;
    let current = sessions_root.open_regular_relative(&relative)?;
    crate::fs::validate_private_file(&current)?;
    crate::fs::ensure_same_file(
        &sessions_root.logical_relative_path(&relative)?,
        &identity,
        &crate::fs::checked_file_metadata(&current)?,
    )?;
    sessions_root.validate_private_root()?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    let json = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid persisted session UTF-8"))?;
    let mut session = decode_session_json(json)?;
    anyhow::ensure!(
        session.id == requested_id,
        "persisted session identity mismatch"
    );
    anyhow::ensure!(
        super::catalogue::same_workspace(&session.working_dir, workspace),
        "persisted session belongs to another workspace"
    );
    // Import history/task data, never explicit grants or concealed redo.
    session.permission_allowlist.clear();
    session.rewind_undo = None;
    Ok(Some(session))
}

fn session_dir() -> PathBuf {
    app_paths().sessions_dir()
}

pub(crate) fn session_path(session_id: &str) -> anyhow::Result<PathBuf> {
    crate::paths::validate_portable_component(session_id)?;
    Ok(session_dir().join(format!("{session_id}.json")))
}

pub fn tool_output_dir(session_id: &str) -> PathBuf {
    app_paths()
        .tool_outputs_dir()
        .join(crate::paths::opaque_name(
            "session-tool-output-directory",
            &[session_id.as_bytes()],
        ))
}

fn app_paths() -> &'static crate::paths::AppPaths {
    crate::paths::process_paths_ref().expect("startup must initialize application paths")
}

fn disabled(artifact: &'static str) -> bool {
    crate::paths::artifact_disabled(artifact)
}

pub(super) fn existing_session_dir() -> anyhow::Result<Option<PathBuf>> {
    let dir = session_dir();
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {
            crate::paths::ensure_private_directory(&dir)?;
            Ok(Some(dir))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_private_string(path: &Path) -> anyhow::Result<String> {
    #[cfg(test)]
    FULL_SESSION_READS.with(|reads| reads.set(reads.get() + 1));
    let file = crate::fs::open_private_file(path).map_err(|error| {
        anyhow::anyhow!("refusing unsafe session file {}: {}", path.display(), error)
    })?;
    // Reject an oversized file from its recorded length before allocating,
    // then bound the read itself in case the file grows concurrently.
    let recorded = file.metadata()?.len();
    anyhow::ensure!(
        recorded <= MAX_SESSION_BYTES as u64,
        "persisted session {} exceeds the session size limit ({} byte limit)",
        path.display(),
        MAX_SESSION_BYTES
    );
    let mut bytes = Vec::with_capacity(recorded as usize);
    file.take((MAX_SESSION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_SESSION_BYTES,
        "persisted session {} exceeds the session size limit ({} byte limit) while reading",
        path.display(),
        MAX_SESSION_BYTES
    );
    String::from_utf8(bytes)
        .map_err(|error| anyhow::anyhow!("session file {} is not UTF-8: {error}", path.display()))
}

#[cfg(test)]
pub(crate) fn config_path() -> PathBuf {
    app_paths().config_dir.clone()
}

/// Write `content` privately and atomically via a same-directory temporary
/// file. A crash mid-write leaves the previous version intact, and once this
/// returns `Ok` the new version is durable: the temporary file is `fsync`ed
/// before the rename and the directory after it (mini-agent-p73n1), so a
/// power loss cannot leave an undecodable session for `--continue` to skip.
pub fn atomic_write(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    crate::fs::private_atomic_write_sync(path, content.as_bytes())?;
    Ok(())
}

#[cfg(all(test, unix))]
pub(crate) fn atomic_write_with_failure(
    path: &Path,
    content: &str,
    fail_rename: bool,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    crate::fs::private_atomic_write_with_failure_sync(path, content.as_bytes(), fail_rename)?;
    Ok(())
}

// Strict writes are retained for persistence and stale-snapshot probes. Ordinary
// production consumers must use the shared recovering preparation path.
#[cfg(test)]
pub fn save_session(session: &Session) -> anyhow::Result<()> {
    prepare_save(session)?.commit()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SaveEncoding {
    pub(crate) encoded_bytes: usize,
    pub(crate) undo_dropped: bool,
}

/// The single bounded shrink ladder. Callers must already own any mutation
/// authority required by their persistence domain; strict digests never call it.
fn encode_with_recovery<T>(
    session: &mut Session,
    mut encode: impl FnMut(&Session) -> anyhow::Result<T>,
    allow_undo_drop: bool,
) -> (anyhow::Result<T>, bool) {
    let mut result = encode(session);
    let undo_dropped = result.as_ref().is_err_and(is_session_size_error)
        && allow_undo_drop
        && session.rewind_undo.take().is_some();
    if undo_dropped {
        result = encode(session);
    }
    (result, undo_dropped)
}

/// Only an exact native actor may call this before sealing a new digest.
/// Immutable raw snapshots and replay hashes keep the strict serializer.
pub(crate) fn encode_owned_session(
    session: &mut Session,
    in_flight: bool,
) -> anyhow::Result<(String, Option<String>, SaveEncoding)> {
    let encode = |session: &Session| {
        let canonical = serialize_bounded_session(session)?;
        let denied = if in_flight {
            let _projection = super::native_replay::InFlightEncoding::install();
            Some(serialize_bounded_session(session)?)
        } else {
            None
        };
        Ok((canonical, denied))
    };
    let (result, undo_dropped) = encode_with_recovery(session, encode, true);
    let (canonical, denied) = result?;
    let encoding = SaveEncoding {
        encoded_bytes: denied.as_ref().unwrap_or(&canonical).len(),
        undo_dropped,
    };
    Ok((canonical, denied, encoding))
}

pub(crate) fn record_native_undo_drop(session: &mut Session, encoding: SaveEncoding) {
    if encoding.undo_dropped && session.rewind_undo.take().is_some() {
        session
            .save_notice_state
            .pending
            .push("undo snapshot dropped to stay within the session size limit".into());
    }
}

/// Adopt only an acknowledged native owner's encoding decision. Notices are
/// queued for its frontend, rather than printed by a blocking storage worker.
pub(crate) fn adopt_native_encoding(session: &mut Session, encoding: SaveEncoding) {
    record_native_undo_drop(session, encoding);
    let mut pending = Vec::new();
    session.save_notice_state.encoded_bytes = Some(encoding.encoded_bytes);
    report_save_success(session, |notice| pending.push(notice.to_owned()));
    session.save_notice_state.pending.extend(pending);
}

pub(crate) fn queue_native_save_failure(session: &mut Session, error: &anyhow::Error) {
    let mut pending = Vec::new();
    report_save_failure(session, error, |notice| pending.push(notice.to_owned()));
    session.save_notice_state.pending.extend(pending);
}

pub(crate) fn report_pending_save_notices(session: &mut Session, mut notice: impl FnMut(&str)) {
    for pending in std::mem::take(&mut session.save_notice_state.pending) {
        notice(&pending);
    }
}

/// Save with the bounded recovery policy shared by ordinary frontends.
/// Only optional rewind undo may be discarded; history is never truncated.
pub(crate) fn save_session_recovering(
    session: &mut Session,
    mut notice: impl FnMut(&str),
) -> anyhow::Result<()> {
    let prepared = prepare_save_recovering(session, &mut notice)?;
    prepared.commit()?;
    report_save_success(session, notice);
    Ok(())
}

/// Capture bytes before any disk publication. Native snapshots stay strict:
/// removing undo would invalidate their canonical replay/publication digest.
pub(crate) fn prepare_save_recovering(
    session: &mut Session,
    mut notice: impl FnMut(&str),
) -> anyhow::Result<PreparedSave> {
    report_pending_save_notices(session, &mut notice);
    let native = super::native_publication::is_native_bearing(session);
    let prepare = |session: &Session| -> anyhow::Result<(PreparedSave, Option<usize>)> {
        // Native publication uses canonical bytes; ordinary files use their
        // compact storage representation. Never change either digest domain.
        let native_size = if native {
            Some(serialize_bounded_session(session)?.len())
        } else {
            None
        };
        let prepared = prepare_save(session)?;
        let size = match &prepared {
            PreparedSave::Legacy { json, .. } => Some(json.len()),
            PreparedSave::Native(_) => native_size,
            PreparedSave::Disabled { .. } => None,
        };
        Ok((prepared, size))
    };
    let (result, undo_dropped) = encode_with_recovery(session, prepare, !native);
    if undo_dropped {
        notice("undo snapshot dropped to stay within the session size limit");
    }
    match result {
        Ok((prepared, size)) => {
            session.save_notice_state.encoded_bytes = size;
            Ok(prepared)
        }
        Err(error) => {
            report_save_failure(session, &error, notice);
            Err(error)
        }
    }
}

pub(crate) fn report_save_failure(
    session: &mut Session,
    error: &anyhow::Error,
    mut notice: impl FnMut(&str),
) {
    if let Some(size) = error.downcast_ref::<SessionSizeError>() {
        if session.save_notice_state.size != SaveSizeState::Oversized {
            notice(&format!(
                "session was not saved: encoded size {} bytes exceeds the {} byte limit; use /compact to reduce history before saving again",
                size.encoded_bytes, size.limit_bytes
            ));
        }
        session.save_notice_state.size = SaveSizeState::Oversized;
    }
}

pub(crate) fn report_save_success(session: &mut Session, mut notice: impl FnMut(&str)) {
    let Some(bytes) = session.save_notice_state.encoded_bytes else {
        return;
    };
    let size = if bytes >= MAX_SESSION_BYTES * 3 / 4 {
        SaveSizeState::NearLimit
    } else {
        SaveSizeState::Normal
    };
    if size == SaveSizeState::NearLimit && session.save_notice_state.size != size {
        notice(&format!(
            "session encoded size is {} bytes of the {} byte limit (75% or more); use /compact to make room",
            bytes, MAX_SESSION_BYTES
        ));
    }
    session.save_notice_state.size = size;
}

/// Turn-end form of the same recovery policy. The captured snapshot is handed
/// to the existing single-flight writer; no unbounded clone or model work is
/// introduced for legacy sessions.
pub(crate) fn save_session_deferred_recovering(
    session: &mut Session,
    mut notice: impl FnMut(&str),
) -> anyhow::Result<()> {
    let Some(runtime) = deferred::runtime() else {
        return save_session_recovering(session, notice);
    };
    let prepared = prepare_save_recovering(session, &mut notice)?;
    let result = match prepared {
        PreparedSave::Legacy { id, json } => {
            deferred::queue(&runtime, &id, deferred::Snapshot::Legacy(json.into_bytes()))
        }
        PreparedSave::Native(snapshot) => {
            super::native_publication::ensure_snapshot_owner(&snapshot)?;
            deferred::queue(
                &runtime,
                &snapshot.id.clone(),
                deferred::Snapshot::Native(snapshot),
            )
        }
        disabled @ PreparedSave::Disabled { .. } => disabled.commit(),
    };
    result?;
    report_save_success(session, notice);
    Ok(())
}

/// A session snapshot captured for a later blocking write. A legacy session is
/// fully encoded (borrowing the live session, no deep clone); a native-bearing
/// session needs the write-time digest/revision/lease checks against a whole
/// `Session`, so it keeps an owned clone (mini-agent-twon3).
pub(crate) enum PreparedSave {
    Native(Box<Session>),
    Legacy { id: String, json: String },
    Disabled { id: String },
}

/// Validate and capture `session` without the optional-state recovery in
/// [`prepare_save_recovering`], and without
/// touching the disk. The result is `Send` and is written by [`PreparedSave::commit`].
pub(crate) fn prepare_save(session: &Session) -> anyhow::Result<PreparedSave> {
    if super::native_publication::is_native_bearing(session) {
        return Ok(PreparedSave::Native(Box::new(session.clone())));
    }
    if disabled("sessions") {
        return Ok(PreparedSave::Disabled {
            id: session.id.to_string(),
        });
    }
    session
        .validate_native_persistence()
        .map_err(anyhow::Error::from)?;
    Ok(PreparedSave::Legacy {
        id: session.id.to_string(),
        json: serialize_session_for_storage(session)?,
    })
}

impl PreparedSave {
    pub(crate) fn commit(self) -> anyhow::Result<()> {
        match self {
            // A synchronous save supersedes any queued turn-end snapshot and
            // must not race an in-flight one: drop the queued bytes and join
            // the writer first.
            Self::Native(session) => {
                deferred::supersede(&session.id);
                super::native_publication::save_snapshot(&session)
            }
            Self::Legacy { id, json } => {
                deferred::supersede(&id);
                write_legacy_snapshot(&id, json.as_bytes())
            }
            Self::Disabled { id } => {
                deferred::supersede(&id);
                Ok(())
            }
        }
    }
}

/// Turn-end save: the session is serialized here, on the caller's task, and
/// the bytes are handed to a per-session single-flight blocking writer
/// (latest snapshot wins). The write itself is the same atomic, fsynced,
/// lock-owned publication as [`save_session`]. Without a Tokio runtime, or
/// with persistence disabled, this is exactly [`save_session`].
///
/// A native-bearing session is not serialized here at all: its exact owner is
/// checked, then a clone goes to the same writer, which runs the ordinary
/// native snapshot (digest, revision/phase and lease checks, projection,
/// fsync) off the UI task. The write-time revision check still refuses to
/// overwrite a newer checkpoint, and a snapshot an acknowledged newer
/// checkpoint of the same owner has superseded is dropped (latest wins).
///
/// A failure of an earlier deferred write is returned by the next call (and by
/// [`flush_deferred_sessions`]), so callers still warn about it.
#[cfg(test)]
pub fn save_session_deferred(session: &Session) -> anyhow::Result<()> {
    let Some(runtime) = deferred::runtime() else {
        return save_session(session);
    };
    if super::native_publication::is_native_bearing(session) {
        super::native_publication::ensure_snapshot_owner(session)?;
        return deferred::queue(
            &runtime,
            &session.id,
            deferred::Snapshot::Native(Box::new(session.clone())),
        );
    }
    if disabled("sessions") {
        return save_session(session);
    }
    session
        .validate_native_persistence()
        .map_err(anyhow::Error::from)?;
    let json = serialize_session_for_storage(session)?;
    deferred::queue(
        &runtime,
        &session.id,
        deferred::Snapshot::Legacy(json.into_bytes()),
    )
}

/// Join every deferred session write; returns the first failure. Exit paths
/// call this so saves are complete before the process returns.
pub fn flush_deferred_sessions() -> anyhow::Result<()> {
    deferred::flush(None)
}

/// Join the deferred writer of one session (session switch / lock release).
pub fn flush_deferred_session(session_id: &str) -> anyhow::Result<()> {
    deferred::flush(Some(session_id))
}

/// Opt this test thread into deferred turn-end saves until dropped.
#[cfg(test)]
pub(crate) struct DeferSavesForTest(());

#[cfg(test)]
pub(crate) fn defer_saves_for_test() -> DeferSavesForTest {
    deferred::ENABLED_FOR_TEST.with(|enabled| enabled.set(true));
    DeferSavesForTest(())
}

#[cfg(test)]
impl Drop for DeferSavesForTest {
    fn drop(&mut self) {
        deferred::ENABLED_FOR_TEST.with(|enabled| enabled.set(false));
    }
}

/// What this process last published for a session file. A save whose bytes
/// are identical and whose file is still the exact inode this process wrote
/// (same length and mtime) changes nothing on disk, so it skips the claim,
/// the legacy pre-read and both fsyncs (mini-agent-3bri2). Any difference,
/// including a deleted or replaced file, falls through to the full write.
#[derive(Clone, PartialEq, Eq)]
struct PublishedSnapshot {
    digest: [u8; 32],
    file: FileStamp,
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    device: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        device: metadata.dev(),
    })
}

fn published_snapshots()
-> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, PublishedSnapshot>> {
    static PUBLISHED: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, PublishedSnapshot>>,
    > = std::sync::LazyLock::new(Default::default);
    PUBLISHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn snapshot_digest(json: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(json).into()
}

fn write_legacy_snapshot(session_id: &str, json: &[u8]) -> anyhow::Result<()> {
    let dir = session_dir();
    crate::paths::ensure_private_directory(&dir)?;
    let path = session_path(session_id)?;
    let digest = snapshot_digest(json);
    if let Some(published) = published_snapshots().get(&path).cloned()
        && published.digest == digest
        && file_stamp(&path).as_ref() == Some(&published.file)
    {
        return Ok(());
    }
    // The snapshot replaces the whole file, so only the process that owns the
    // session may write it; another owner's turns would otherwise be lost
    // (mini-agent-e42za). Without an established lock, only an atomic
    // create-only publication of a missing target is eligible.
    match crate::session::lock::claim_session(session_id) {
        Ok(crate::session::lock::SessionClaim::Owned) => {
            super::lock::publish_legacy_owned_id(session_id, json)?;
            match file_stamp(&path) {
                Some(file) => {
                    published_snapshots().insert(path, PublishedSnapshot { digest, file });
                }
                None => {
                    published_snapshots().remove(&path);
                }
            }
            Ok(())
        }
        Ok(crate::session::lock::SessionClaim::HeldElsewhere) => {
            Err(crate::session::lock::held_elsewhere_error(session_id))
        }
        Err(error) => {
            // Without exact ownership, classification cannot exclude an
            // existing or concurrently activated native checkpoint. Preserve
            // only the legacy new-target fallback through create-only CAS.
            tracing::warn!(
                "session lock unavailable; refusing replacement and trying create-only persistence: {}",
                error
            );
            let root = crate::paths::WorkspaceBinding::capture(&dir)?;
            root.validate_private_root()?;
            let parent = crate::fs::checked_file_metadata(&root.try_clone_directory_file()?)?;
            crate::fs::private_atomic_create_checked_sync(&path, json, &parent)?;
            root.validate_private_root()?;
            Ok(())
        }
    }
}

mod deferred {
    use std::collections::HashMap;
    use std::sync::{Condvar, LazyLock, Mutex};

    /// One queued turn-end snapshot. Latest wins, whichever kind it is.
    pub(super) enum Snapshot {
        Legacy(Vec<u8>),
        Native(Box<crate::session::Session>),
    }

    #[derive(Default)]
    struct Slot {
        queued: Option<Snapshot>,
        writing: bool,
        error: Option<String>,
    }

    #[derive(Default)]
    struct Writers {
        slots: Mutex<HashMap<String, Slot>>,
        idle: Condvar,
    }

    static WRITERS: LazyLock<Writers> = LazyLock::new(Writers::default);

    #[cfg(test)]
    thread_local! {
        /// Deferral is opt-in per test thread so existing tests keep reading
        /// the disk synchronously after a save.
        pub(super) static ENABLED_FOR_TEST: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    pub(super) fn runtime() -> Option<tokio::runtime::Handle> {
        #[cfg(test)]
        if !ENABLED_FOR_TEST.with(std::cell::Cell::get) {
            return None;
        }
        tokio::runtime::Handle::try_current().ok()
    }

    fn slots() -> std::sync::MutexGuard<'static, HashMap<String, Slot>> {
        WRITERS
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn queue(
        runtime: &tokio::runtime::Handle,
        session_id: &str,
        snapshot: Snapshot,
    ) -> anyhow::Result<()> {
        let (start, previous) = {
            let mut slots = slots();
            let slot = slots.entry(session_id.to_string()).or_default();
            slot.queued = Some(snapshot);
            let start = !slot.writing;
            slot.writing = true;
            (start, slot.error.take())
        };
        if start {
            let id = session_id.to_string();
            runtime.spawn_blocking(move || drain(&id));
        }
        match previous {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }

    /// Clears `writing` if a write panics, so joins never hang.
    struct PanicReset<'a>(&'a str);
    impl Drop for PanicReset<'_> {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                return;
            }
            let mut slots = slots();
            if let Some(slot) = slots.get_mut(self.0) {
                slot.writing = false;
                slot.error = Some("deferred session save panicked".into());
            }
            WRITERS.idle.notify_all();
        }
    }

    fn drain(session_id: &str) {
        let _reset = PanicReset(session_id);
        loop {
            let snapshot = {
                let mut slots = slots();
                let Some(slot) = slots.get_mut(session_id) else {
                    WRITERS.idle.notify_all();
                    return;
                };
                match slot.queued.take() {
                    Some(snapshot) => snapshot,
                    None => {
                        // Cleared under the same lock `queue` checks, so a
                        // snapshot queued now starts a new writer.
                        slot.writing = false;
                        WRITERS.idle.notify_all();
                        return;
                    }
                }
            };
            let result = match snapshot {
                Snapshot::Legacy(bytes) => super::write_legacy_snapshot(session_id, &bytes),
                Snapshot::Native(session) => {
                    super::super::native_publication::save_snapshot_deferred(&session)
                }
            };
            let mut slots = slots();
            if let Some(slot) = slots.get_mut(session_id) {
                slot.error = result.err().map(|error| format!("{error:#}"));
            }
        }
    }

    fn wait_idle(
        mut slots: std::sync::MutexGuard<'static, HashMap<String, Slot>>,
        session_id: Option<&str>,
    ) -> std::sync::MutexGuard<'static, HashMap<String, Slot>> {
        loop {
            let busy = match session_id {
                Some(id) => slots.get(id).is_some_and(|slot| slot.writing),
                None => slots.values().any(|slot| slot.writing),
            };
            if !busy {
                return slots;
            }
            slots = WRITERS
                .idle
                .wait(slots)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(super) fn supersede(session_id: &str) {
        let mut slots = slots();
        let Some(slot) = slots.get_mut(session_id) else {
            return;
        };
        slot.queued = None;
        let mut slots = wait_idle(slots, Some(session_id));
        slots.remove(session_id);
    }

    pub(super) fn flush(session_id: Option<&str>) -> anyhow::Result<()> {
        let mut slots = wait_idle(slots(), session_id);
        let mut first = None;
        slots.retain(|id, slot| {
            if session_id.is_some_and(|wanted| wanted != id) {
                return true;
            }
            if first.is_none() {
                first = slot.error.take();
            }
            false
        });
        match first {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }
}

pub fn save_tool_output(
    session_id: &str,
    tool_name: &str,
    output: &str,
) -> anyhow::Result<PathBuf> {
    if disabled("tool outputs") {
        anyhow::bail!("tool-output persistence is disabled by a legacy-path conflict");
    }
    let dir = tool_output_dir(session_id);
    crate::paths::ensure_private_directory(&dir)?;
    let nonce = Uuid::new_v4().to_string();
    let filename = crate::paths::digest_filename(
        "session-tool-output",
        &[
            session_id.as_bytes(),
            tool_name.as_bytes(),
            nonce.as_bytes(),
        ],
        "txt",
    )?;
    let path = dir.join(filename);
    crate::fs::private_atomic_create_sync(&path, output.as_bytes())?;
    Ok(path)
}

/// Remove a tool-output artifact created by an uncommitted UI turn. The path
/// is accepted only when it is an immediate child of this session's private,
/// opaque output directory; callers cannot use rollback bookkeeping as an
/// arbitrary-file deletion primitive.
pub(crate) fn delete_uncommitted_tool_output(session_id: &str, path: &Path) -> anyhow::Result<()> {
    let expected_dir = tool_output_dir(session_id);
    if path.parent() != Some(expected_dir.as_path()) {
        anyhow::bail!("refusing tool-output rollback outside the session directory");
    }
    crate::paths::ensure_private_directory(&expected_dir)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!("refusing non-regular tool-output rollback target");
            }
            drop(crate::fs::open_private_file(path)?);
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    if disabled("sessions") {
        return Ok(());
    }
    crate::paths::validate_portable_component(id)?;
    let Some(dir) = existing_session_dir()? else {
        tracing::debug!("session delete skipped (not found): id={}", id);
        return Ok(());
    };
    let path = dir.join(format!("{}.json", id));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            drop(crate::fs::open_private_file(&path)?);
            std::fs::remove_file(&path)?;
            tracing::debug!("session deleted: id={}", id);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!("session delete skipped (not found): id={}", id);
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// At most this many per-entry diagnostics are emitted for one discovery call;
/// the rest are summarized so a corrupted directory cannot flood the log.
const MAX_DISCOVERY_DIAGNOSTICS: usize = 5;

/// Per-entry failures encountered while enumerating saved sessions.
#[derive(Default)]
struct SkippedEntries {
    reported: usize,
    total: usize,
}

impl SkippedEntries {
    fn note(&mut self, path: &Path, error: &dyn std::fmt::Display) {
        self.total += 1;
        if self.reported < MAX_DISCOVERY_DIAGNOSTICS {
            self.reported += 1;
            tracing::warn!("session discovery skipped {}: {}", path.display(), error);
        }
    }

    fn finish(&self, operation: &str) {
        if self.total > self.reported {
            tracing::warn!(
                "session discovery ({}) skipped {} entries in total",
                operation,
                self.total
            );
        }
    }
}

/// Decode one candidate session file during multi-session discovery.
///
/// Discovery must stay available when an unrelated entry is unsafe, unreadable,
/// invalid or disappears mid-iteration, so those entries are skipped with a
/// bounded diagnostic instead of aborting the whole search. Exact-identifier
/// loading (`load_session_exact`) stays fail-closed.
fn read_discovered_session(path: &Path, skipped: &mut SkippedEntries) -> Option<Session> {
    let json = match read_private_string(path) {
        Ok(json) => json,
        Err(error) => {
            skipped.note(path, &error);
            return None;
        }
    };
    match decode_session_json(&json) {
        Ok(session) => {
            if path.file_stem().and_then(|stem| stem.to_str()) == Some(session.id.as_str()) {
                session.file_checkpoints.trust_owned_load();
            }
            Some(session)
        }
        Err(error) => {
            skipped.note(path, &error);
            None
        }
    }
}

/// Bytes of a session file inspected for its `name`. `id` and `name` are the
/// first serialized fields, so a few KiB covers every realistic file.
const NAME_PEEK_BYTES: usize = 4 * 1024;

/// Read a top-level string field (`name`, `working_dir`) from the head of a
/// session file without reading or parsing the rest. `None` means
/// "undetermined" (unsafe or unreadable file, key absent or not a string before
/// the cut), and the caller then decodes the full file.
fn peek_session_string(path: &Path, key: &str) -> Option<String> {
    use serde::de::{Deserializer as _, Error as _, IgnoredAny, MapAccess, Visitor};
    struct Head<'a>(&'a str, &'a mut Option<String>);
    impl<'de> Visitor<'de> for Head<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a session object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                if key == self.0 {
                    *self.1 = Some(map.next_value::<String>()?);
                    // Found; stop before reaching the truncated tail.
                    return Err(A::Error::custom("key found"));
                }
                map.next_value::<IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let file = crate::fs::open_private_file(path).ok()?;
    let (head, _) = crate::paths::read_utf8_prefix(file, NAME_PEEK_BYTES).ok()??;
    let mut value = None;
    let _ = serde_json::Deserializer::from_str(&head).deserialize_map(Head(key, &mut value));
    value
}

fn peek_session_name(path: &Path) -> Option<String> {
    peek_session_string(path, "name")
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(Vec::new());
    };
    let lower = prefix.to_lowercase();
    let mut sessions: Vec<Session> = Vec::new();
    let mut skipped = SkippedEntries::default();
    for entry in std::fs::read_dir(&dir)? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                skipped.note(&dir, &error);
                continue;
            }
        };
        if path.extension().is_some_and(|e| e == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            // The stem alone decides a prefix match. Otherwise the name near
            // the head of the file decides, so a non-matching session is never
            // read in full or decoded (mini-agent-hf54t). A name that cannot
            // be peeked falls back to the full decode below.
            if !stem.starts_with(prefix)
                && peek_session_name(&path)
                    .is_some_and(|name| !name.to_lowercase().contains(&lower))
            {
                continue;
            }
            if let Some(session) = read_discovered_session(&path, &mut skipped)
                && (stem.starts_with(prefix) || session.name.to_lowercase().contains(&lower))
            {
                sessions.push(session);
            }
        }
    }
    skipped.finish("prefix search");
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.dedup_by(|a, b| a.id == b.id);
    tracing::debug!(
        "find_sessions_by_prefix('{}'): {} results",
        prefix,
        sessions.len(),
    );
    Ok(sessions)
}

/// Load one private session by its complete portable identifier. This exact
/// lookup is also used to reconcile the visible state when an atomic save
/// reports an error after publication may already have occurred.
pub fn load_session_exact(id: &str) -> anyhow::Result<Option<Session>> {
    if disabled("sessions") {
        return Ok(None);
    }
    crate::paths::validate_portable_component(id)?;
    let Some(dir) = existing_session_dir()? else {
        return Ok(None);
    };
    let path = dir.join(format!("{id}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let json = read_private_string(&path)?;
            let session = decode_session_json(&json)
                .map_err(|error| anyhow::anyhow!("invalid persisted session {id}: {error}"))?;
            session
                .validate_native_persistence()
                .map_err(|reason| anyhow::anyhow!(reason))?;
            anyhow::ensure!(
                session.id == id,
                "persisted session ID mismatch: requested {id}, found {}",
                session.id
            );
            session.file_checkpoints.trust_owned_load();
            Ok(Some(session))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
pub fn find_session_by_name(name: &str) -> anyhow::Result<Option<Session>> {
    if disabled("sessions") {
        return Ok(None);
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(None);
    };
    let lower = name.to_lowercase();
    let mut skipped = SkippedEntries::default();
    for entry in std::fs::read_dir(&dir)? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                skipped.note(&dir, &error);
                continue;
            }
        };
        if path.extension().is_some_and(|e| e == "json")
            && let Some(session) = read_discovered_session(&path, &mut skipped)
            && session.name.to_lowercase() == lower
        {
            skipped.finish("name search");
            return Ok(Some(session));
        }
    }
    skipped.finish("name search");
    Ok(None)
}

/// How a `--session <query>` argument resolved against the saved sessions.
pub enum SessionSelection {
    One(Box<Session>),
    NoMatch,
    Ambiguous(Vec<Session>),
}

/// Pick the session a `--session <query>` means from the prefix-search
/// candidates (id prefix or name substring, newest first).
///
/// An exact id wins, then an exact (case-insensitive) name, then a single
/// prefix/substring match. Without the exact-name step a session named `work`
/// could never be selected while another was named `homework`
/// (mini-agent-6wvoz).
pub fn select_session(query: &str, candidates: Vec<Session>) -> SessionSelection {
    if let Some(index) = candidates.iter().position(|s| s.id == query) {
        return SessionSelection::One(Box::new(candidates.into_iter().nth(index).unwrap()));
    }
    let lower = query.to_lowercase();
    let exact_names = candidates
        .iter()
        .filter(|s| !s.name.is_empty() && s.name.to_lowercase() == lower)
        .count();
    if exact_names > 0 {
        let mut named: Vec<Session> = candidates
            .into_iter()
            .filter(|s| !s.name.is_empty() && s.name.to_lowercase() == lower)
            .collect();
        return if named.len() == 1 {
            SessionSelection::One(Box::new(named.remove(0)))
        } else {
            SessionSelection::Ambiguous(named)
        };
    }
    match candidates.len() {
        0 => SessionSelection::NoMatch,
        1 => SessionSelection::One(Box::new(candidates.into_iter().next().unwrap())),
        _ => SessionSelection::Ambiguous(candidates),
    }
}

/// Saved session files, newest modification first.
fn session_paths_newest_first(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
        .map(|e| {
            let path = e.path();
            let mtime = std::fs::symlink_metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            (mtime, path)
        })
        .collect();
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));
    Ok(entries.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(Vec::new());
    };
    // Sort by filesystem mtime to avoid loading all sessions
    let entries = session_paths_newest_first(&dir)?;

    let mut sessions: Vec<Session> = Vec::new();
    let mut skipped = SkippedEntries::default();
    // The limit applies to sessions that actually decoded: a run of broken
    // recent files must not hide older valid sessions.
    for path in entries.iter() {
        if sessions.len() == limit {
            break;
        }
        if let Some(session) = read_discovered_session(path, &mut skipped) {
            sessions.push(session);
        }
    }
    skipped.finish("recent sessions");
    tracing::debug!(
        "find_recent_sessions(limit={}): {} results",
        limit,
        sessions.len(),
    );
    Ok(sessions)
}

/// Return recent sessions whose persisted workspace is exactly the captured
/// workspace. `--continue` uses this instead of silently importing history and
/// approvals from whichever project happened to run most recently.
///
/// Files are visited newest first, and only a file whose `working_dir`
/// matches is fully decoded, so `--continue` stops at the first match instead
/// of deserializing every saved session (mini-agent-5s9me).
pub fn find_recent_sessions_for_workspace(
    limit: usize,
    workspace: &Path,
) -> anyhow::Result<Vec<Session>> {
    #[derive(serde::Deserialize)]
    struct WorkspacePeek {
        #[serde(default)]
        working_dir: String,
    }

    let workspace = std::fs::canonicalize(workspace)?;
    let mut matches = Vec::new();
    if limit == 0 || disabled("sessions") {
        return Ok(matches);
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(matches);
    };
    let mut skipped = SkippedEntries::default();
    let mut canonical_matches = std::collections::HashMap::<String, bool>::new();
    for path in session_paths_newest_first(&dir)? {
        // `working_dir` serializes right after `name`, so the head normally
        // answers without reading the file. A legacy file (field written late)
        // or an unparseable head falls back to the full read (mini-agent-vb5vw.58).
        let (working_dir, mut json) = match peek_session_string(&path, "working_dir") {
            Some(working_dir) => (working_dir, None),
            None => {
                let json = match read_private_string(&path) {
                    Ok(json) => json,
                    Err(error) => {
                        skipped.note(&path, &error);
                        continue;
                    }
                };
                let peek = match serde_json::from_str::<WorkspacePeek>(&json) {
                    Ok(peek) => peek,
                    Err(error) => {
                        skipped.note(&path, &error);
                        continue;
                    }
                };
                (peek.working_dir, Some(json))
            }
        };
        // Many sessions share a few workspaces: resolve each distinct stored
        // label once per scan.
        let same_workspace =
            *canonical_matches
                .entry(working_dir)
                .or_insert_with_key(|working_dir| {
                    std::fs::canonicalize(Path::new(working_dir.as_str()))
                        .ok()
                        .as_deref()
                        == Some(workspace.as_path())
                });
        if !same_workspace {
            continue;
        }
        let json = match json.take() {
            Some(json) => json,
            None => match read_private_string(&path) {
                Ok(json) => json,
                Err(error) => {
                    skipped.note(&path, &error);
                    continue;
                }
            },
        };
        match decode_session_json(&json) {
            Ok(session) => matches.push(session),
            Err(error) => {
                skipped.note(&path, &error);
                continue;
            }
        }
        if matches.len() == limit {
            break;
        }
    }
    skipped.finish("workspace sessions");
    Ok(matches)
}

pub fn agents_path() -> PathBuf {
    app_paths().global_agents_file()
}

#[cfg(feature = "archmd")]
pub fn architecture_path() -> PathBuf {
    app_paths().global_architecture_file()
}

pub fn suffix_path() -> PathBuf {
    app_paths().suffix_file()
}

pub fn load_suffix() -> Option<String> {
    let path = suffix_path();
    if path.exists() {
        std::fs::read_to_string(path)
            .ok()
            .filter(|s| !s.trim().is_empty())
    } else {
        None
    }
}

fn theme_file_path() -> PathBuf {
    app_paths().theme_selection_file()
}

pub fn save_theme_name(name: Option<&str>) -> anyhow::Result<()> {
    let path = theme_file_path();
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    let value = match name {
        Some(n) => serde_json::json!({ "theme": n }),
        None => serde_json::json!({ "theme": null }),
    };
    atomic_write(&path, &serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

pub fn load_theme_name() -> Option<String> {
    let path = theme_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value.get("theme")?.as_str().map(|s| s.to_string())
}

#[cfg(test)]
mod readonly_import_tests {
    use super::*;
    use crate::paths::WorkspaceBinding;

    fn snapshot(
        root: &crate::tests::OwnedTestDirectory,
    ) -> (
        std::sync::Arc<WorkspaceBinding>,
        WorkspaceBinding,
        Session,
        PathBuf,
    ) {
        let state = root.path().join("state");
        crate::paths::ensure_private_directory(&state).unwrap();
        let workspace = std::sync::Arc::new(WorkspaceBinding::capture(root.path()).unwrap());
        let sessions = WorkspaceBinding::capture(&state).unwrap();
        let mut session = Session::new("untrusted-label", "data-model", 32_000, "");
        session.working_dir = workspace.root().to_string_lossy().into();
        session.add_message(crate::session::MessageRole::User, "literal saved history");
        session
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: "write".into(),
                pattern: "**".into(),
            });
        let path = state.join(format!("{}.json", session.id));
        crate::fs::private_atomic_write_sync(
            &path,
            serde_json::to_string(&session).unwrap().as_bytes(),
        )
        .unwrap();
        (workspace, sessions, session, path)
    }

    #[test]
    fn readonly_session_import_is_exact_bounded_and_never_recreates_grants_or_missing_artifacts() {
        let root = crate::tests::OwnedTestDirectory::new("readonly-session-import");
        let (workspace, sessions, mut original, path) = snapshot(&root);
        let before = crate::fs::checked_path_metadata(&path).unwrap();
        let loaded = load_session_exact_read_only(&sessions, &original.id, &workspace)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.messages[0].content, "literal saved history");
        assert!(loaded.permission_allowlist.is_empty());
        assert!(loaded.rewind_undo.is_none());
        crate::fs::ensure_same_file(
            &path,
            &before,
            &crate::fs::checked_path_metadata(&path).unwrap(),
        )
        .unwrap();
        // The readonly identity proof is complete. Retire its held descriptor
        // before this fixture deliberately replaces the snapshot for negatives.
        drop(before);
        let absent = uuid::Uuid::new_v4().to_string();
        assert!(
            load_session_exact_read_only(&sessions, &absent, &workspace)
                .unwrap()
                .is_none()
        );
        assert!(!sessions.root().join(format!("{absent}.json")).exists());
        assert!(load_session_exact_read_only(&sessions, "../credentials", &workspace).is_err());
        original.id = uuid::Uuid::new_v4().to_string().into();
        crate::fs::private_atomic_write_sync(
            &path,
            serde_json::to_string(&original).unwrap().as_bytes(),
        )
        .unwrap();
        assert!(
            load_session_exact_read_only(&sessions, &loaded.id, &workspace)
                .unwrap_err()
                .to_string()
                .contains("identity mismatch")
        );
        original.id = loaded.id.clone();
        original.working_dir = sessions.root().to_string_lossy().into();
        crate::fs::private_atomic_write_sync(
            &path,
            serde_json::to_string(&original).unwrap().as_bytes(),
        )
        .unwrap();
        assert!(
            load_session_exact_read_only(&sessions, &loaded.id, &workspace)
                .unwrap_err()
                .to_string()
                .contains("another workspace")
        );
        crate::fs::private_atomic_write_sync(&path, &vec![b' '; MAX_SESSION_BYTES + 1]).unwrap();
        assert!(
            load_session_exact_read_only(&sessions, &loaded.id, &workspace)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn readonly_session_import_refuses_unsafe_file_and_root_without_permission_repair() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let root = crate::tests::OwnedTestDirectory::new("readonly-session-policy");
        let (workspace, sessions, original, path) = snapshot(&root);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias = sessions.root().join("hardlink.json");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
        std::fs::remove_file(alias).unwrap();
        std::fs::set_permissions(sessions.root(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
        assert_eq!(
            std::fs::metadata(sessions.root()).unwrap().mode() & 0o777,
            0o755
        );
        std::fs::set_permissions(sessions.root(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(root.path().join("outside.json"), &path).unwrap();
        assert!(load_session_exact_read_only(&sessions, &original.id, &workspace).is_err());
    }

    #[test]
    fn session_storage_global_limit_refuses_before_creating_and_keeps_errors_static() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-bound");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let mut invalid_projection = Session::new("provider", "model", 32_000, "");
        invalid_projection.add_user_message(
            crate::session::user::PersistedUserMessage::new(vec![
                crate::session::user::UserBlock::Text {
                    text: "canonical typed user text".into(),
                },
            ])
            .unwrap(),
        );
        invalid_projection.messages[0].content = "divergent public display projection".into();
        assert!(save_session(&invalid_projection).is_err());
        assert!(!root.path().join("state").exists());

        let mut session = Session::new("provider", "model", 32_000, "");
        session.add_message(
            crate::session::MessageRole::User,
            &"x".repeat(MAX_SESSION_BYTES),
        );
        assert!(
            serialize_bounded_session(&session)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert!(
            save_session(&session)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert!(!root.path().join("state").exists());
        let path = session_path(&session.id).unwrap();
        let previous =
            serde_json::to_string(&Session::new("provider", "model", 32_000, "previous")).unwrap();
        atomic_write(&path, &previous).unwrap();
        assert!(
            save_session(&session)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        // UTF-8 source size is not the encoded size: escaped content must be
        // checked as it is written, before the existing snapshot is replaced.
        session.messages.clear();
        session.add_message(
            crate::session::MessageRole::User,
            &"\"".repeat(MAX_SESSION_BYTES / 2),
        );
        assert!(session.messages[0].content.len() < MAX_SESSION_BYTES);
        assert!(
            save_session(&session)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        let oversized = serde_json::to_string(&session).unwrap();
        assert!(
            decode_session_json(&oversized)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert_eq!(
            decode_session_json("{\"messages\": [\"opaque-secret\"]}")
                .unwrap_err()
                .to_string(),
            "invalid persisted session JSON"
        );
        invalid_projection.id = session.id.clone();
        assert!(save_session(&invalid_projection).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        invalid_projection.messages[0].content = "canonical typed user text".into();
        invalid_projection.messages[0].role = crate::session::MessageRole::Assistant;
        assert!(save_session(&invalid_projection).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
    }

    #[test]
    fn session_save_recovery_drops_only_undo_and_notifies_once() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-recovery-undo");
        let _environment = scoped_state(&root);
        let mut session = Session::new("provider", "model", 32_000, "undo");
        session.add_message(super::super::MessageRole::User, "keep this history");
        session.add_message(
            super::super::MessageRole::Assistant,
            &"x".repeat(MAX_SESSION_BYTES),
        );
        session.rewind_to(1);
        assert!(session.rewind_undo.is_some());
        let history = serde_json::to_value(&session.messages).unwrap();
        let mut notices = Vec::new();
        SERIALIZE_BOUNDED_CALLS.with(|calls| calls.set(0));
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert_eq!(SERIALIZE_BOUNDED_CALLS.with(|calls| calls.get()), 2);
        assert_eq!(
            notices,
            ["undo snapshot dropped to stay within the session size limit"]
        );
        assert!(session.rewind_undo.is_none());
        assert_eq!(serde_json::to_value(&session.messages).unwrap(), history);
        let loaded = load_session_exact(&session.id).unwrap().unwrap();
        assert!(loaded.rewind_undo.is_none());
        assert_eq!(serde_json::to_value(&loaded.messages).unwrap(), history);
        notices.clear();
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert!(notices.is_empty());
    }

    #[test]
    fn session_save_recovery_refuses_oversize_without_creating_or_replacing_and_compact_recovers() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-recovery-refuse");
        let _environment = scoped_state(&root);
        let mut session = Session::new("provider", "model", 32_000, "oversize");
        session.add_message(
            super::super::MessageRole::User,
            &"\"".repeat(MAX_SESSION_BYTES / 2),
        );
        let encoded_size = serde_json::to_string(&session).unwrap().len();
        let mut notices = Vec::new();
        let error = save_session_recovering(&mut session, |notice| notices.push(notice.to_owned()))
            .unwrap_err();
        let size = error.downcast_ref::<SessionSizeError>().unwrap();
        assert_eq!(size.encoded_bytes, encoded_size);
        assert_eq!(size.limit_bytes, MAX_SESSION_BYTES);
        assert_eq!(
            error.to_string(),
            "persisted session exceeds the byte limit"
        );
        assert!(!root.path().join("state").exists());
        assert_eq!(session.messages.len(), 1, "history must never be truncated");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(&encoded_size.to_string()));
        assert!(notices[0].contains(&MAX_SESSION_BYTES.to_string()));
        assert!(notices[0].contains("/compact"));
        let path = session_path(&session.id).unwrap();
        let previous =
            serde_json::to_string(&Session::new("provider", "model", 32_000, "previous")).unwrap();
        atomic_write(&path, &previous).unwrap();
        notices.clear();
        // Even a later turn remains the same oversized state: no warning storm.
        session.add_message(super::super::MessageRole::Assistant, "another turn");
        assert!(
            save_session_recovering(&mut session, |notice| notices.push(notice.to_owned()))
                .is_err()
        );
        assert!(notices.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);
        session.apply_compaction("explicit compact summary".into(), 2, 0, 16 * 1024);
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert!(notices.is_empty());
        let loaded = load_session_exact(&session.id).unwrap().unwrap();
        assert!(
            loaded.messages[0]
                .content
                .contains("explicit compact summary")
        );
        assert_eq!(loaded.compactions.len(), 1);
    }

    #[test]
    fn session_save_recovery_undo_retry_still_oversized_is_bounded_and_keeps_history() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-recovery-still-oversize");
        let _environment = scoped_state(&root);
        let mut session = Session::new("provider", "model", 32_000, "still oversized");
        session.add_message(
            super::super::MessageRole::User,
            &"x".repeat(MAX_SESSION_BYTES),
        );
        session.add_message(
            super::super::MessageRole::Assistant,
            "optional removed tail",
        );
        session.rewind_to(1);
        let mut notices = Vec::new();
        SERIALIZE_BOUNDED_CALLS.with(|calls| calls.set(0));
        assert!(
            save_session_recovering(&mut session, |notice| notices.push(notice.to_owned()))
                .is_err()
        );
        assert_eq!(SERIALIZE_BOUNDED_CALLS.with(|calls| calls.get()), 2);
        assert!(session.rewind_undo.is_none());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content.len(), MAX_SESSION_BYTES);
        assert_eq!(notices.len(), 2);
        assert_eq!(
            notices[0],
            "undo snapshot dropped to stay within the session size limit"
        );
        assert!(notices[1].contains("/compact"));
        assert!(!root.path().join("state").exists());
        notices.clear();
        assert!(
            save_session_recovering(&mut session, |notice| notices.push(notice.to_owned()))
                .is_err()
        );
        assert!(notices.is_empty());
    }

    #[test]
    fn session_save_recovery_warns_once_per_near_limit_crossing_and_never_serializes_notice_state()
    {
        let root = crate::tests::OwnedTestDirectory::new("session-save-recovery-threshold");
        let _environment = scoped_state(&root);
        let mut session = Session::new("provider", "model", 32_000, "threshold");
        session.add_message(
            super::super::MessageRole::User,
            &"x".repeat(MAX_SESSION_BYTES * 3 / 4),
        );
        let mut notices = Vec::new();
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("75%"));
        assert!(notices[0].contains("/compact"));
        let canonical = serde_json::to_string(&session).unwrap();
        assert!(!canonical.contains("save_notice_state"));
        notices.clear();
        session.add_message(super::super::MessageRole::Assistant, "small next turn");
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert!(notices.is_empty());
        session.apply_compaction("summary".into(), 2, 0, 16 * 1024);
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert!(notices.is_empty());
        session.add_message(
            super::super::MessageRole::User,
            &"x".repeat(MAX_SESSION_BYTES * 3 / 4),
        );
        save_session_recovering(&mut session, |notice| notices.push(notice.to_owned())).unwrap();
        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn session_save_recovery_does_not_drop_undo_for_non_size_validation_failure() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-recovery-validation");
        let _environment = scoped_state(&root);
        let mut session = Session::new("provider", "model", 32_000, "invalid");
        session.add_user_message(
            super::super::user::PersistedUserMessage::new(vec![
                super::super::user::UserBlock::Text {
                    text: "canonical user text".into(),
                },
            ])
            .unwrap(),
        );
        session.add_message(super::super::MessageRole::Assistant, "tail");
        session.rewind_to(1);
        session.messages[0].content = "mismatched user projection".into();
        let mut notices = Vec::new();
        assert!(
            save_session_recovering(&mut session, |notice| notices.push(notice.to_owned()))
                .is_err()
        );
        assert!(session.rewind_undo.is_some());
        assert!(notices.is_empty());
        assert!(!root.path().join("state").exists());
    }

    #[tokio::test]
    async fn session_save_recovery_deferred_uses_the_same_undo_ladder() {
        let root = crate::tests::OwnedTestDirectory::new("session-save-recovery-deferred");
        let _environment = scoped_state(&root);
        let _defer = defer_saves_for_test();
        let mut session = Session::new("provider", "model", 32_000, "deferred");
        session.add_message(super::super::MessageRole::User, "kept");
        session.add_message(
            super::super::MessageRole::Assistant,
            &"x".repeat(MAX_SESSION_BYTES),
        );
        session.rewind_to(1);
        let mut notices = Vec::new();
        save_session_deferred_recovering(&mut session, |notice| notices.push(notice.to_owned()))
            .unwrap();
        flush_deferred_session(&session.id).unwrap();
        assert_eq!(
            notices,
            ["undo snapshot dropped to stay within the session size limit"]
        );
        assert!(
            load_session_exact(&session.id)
                .unwrap()
                .unwrap()
                .rewind_undo
                .is_none()
        );
    }

    fn scoped_state(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        // These must be the canonical `MINI_AGENT_*_DIR` override names that
        // `paths::resolve_override` actually reads (see `process_override`);
        // the stale `ZS_*` prefix used here previously matched nothing, so
        // every test using this helper silently fell through to the real
        // default path roots and shared on-disk state with every other test
        // (and process) doing the same, instead of this directory.
        crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ])
    }

    // mini-agent-3bri2: an unchanged session costs no fsync; any change,
    // or a vanished file, is still written.
    #[cfg(unix)]
    #[test]
    fn unchanged_save_skips_the_durable_write_but_changes_and_deleted_files_do_not() {
        let root = crate::tests::OwnedTestDirectory::new("session-unchanged-save");
        let _environment = scoped_state(&root);
        let mut session = Session::new("provider", "model", 32_000, "unchanged");
        session.add_message(crate::session::MessageRole::User, "hello");
        save_session(&session).unwrap();
        let syncs = || crate::fs::DURABLE_SYNCS.with(std::cell::Cell::get);
        let path = session_path(&session.id).unwrap();

        let before = syncs();
        save_session(&session).unwrap();
        assert_eq!(syncs(), before, "identical bytes must not be rewritten");

        session.add_message(crate::session::MessageRole::Assistant, "changed");
        save_session(&session).unwrap();
        assert!(syncs() > before, "a changed session must be written");
        assert!(std::fs::read_to_string(&path).unwrap().contains("changed"));

        std::fs::remove_file(&path).unwrap();
        let before = syncs();
        save_session(&session).unwrap();
        assert!(syncs() > before, "a deleted snapshot must be recreated");
        assert!(path.exists());
    }

    // mini-agent-hcb3s: the borrowed-session encoding with frozen stores is
    // identical to encoding a frozen clone, and leaves no override behind.
    #[cfg(feature = "subagents")]
    #[test]
    fn serialization_with_bound_observations_matches_the_frozen_clone_exactly() {
        let mut session = Session::new("provider", "model", 32_000, "frozen");
        session.initialize_spend_account(None).unwrap();
        session.add_message(crate::session::MessageRole::User, "hello");
        let _owner =
            super::super::background_observation::ObservationOwner::bind(&mut session).unwrap();
        let observations =
            super::super::background_observation::SharedObservations::capture(&session).unwrap();
        let expected = serde_json::to_string(&observations.frozen_session(&session)).unwrap();
        let actual = serialize_bounded_session(&session).unwrap();
        assert_eq!(actual, expected);
    }

    // mini-agent-twon3: a legacy session is captured as the exact bytes
    // `save_session` would write (borrowed, no Session clone); only a
    // native-bearing session keeps an owned snapshot.
    #[test]
    fn prepared_save_of_a_legacy_session_is_the_exact_serialized_bytes_without_a_clone() {
        let mut session = Session::new("provider", "model", 32_000, "prepared");
        session.add_message(crate::session::MessageRole::User, "hello");
        session.add_tool_call("read", &serde_json::json!({"path": "a"}));
        session.add_tool_result("read", "contents");
        let expected = serialize_bounded_session(&session).unwrap();
        match prepare_save(&session).unwrap() {
            PreparedSave::Legacy { id, json } => {
                assert_eq!(id, session.id);
                assert_eq!(json, expected);
            }
            _ => panic!("a legacy session must be captured as bytes, not cloned"),
        }
        session.messages[0].content = "x".repeat(MAX_SESSION_BYTES).into();
        assert!(prepare_save(&session).is_err());
    }

    // mini-agent-vb5vw.58: `--continue` answers foreign sessions from the file
    // head; only the matching file is read whole, and a legacy file that
    // serialized `working_dir` late still falls back to the full read.
    #[test]
    fn workspace_lookup_peeks_working_dir_and_reads_only_matching_files() {
        let root = crate::tests::OwnedTestDirectory::new("session-workspace-peek");
        let _environment = scoped_state(&root);
        let here = root.path().join("here");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let here = std::fs::canonicalize(here).unwrap();
        let elsewhere = std::fs::canonicalize(elsewhere).unwrap();
        let big = |name: &str, dir: &Path| {
            let mut session = Session::new("provider", "model", 32_000, name);
            session.working_dir = dir.to_string_lossy().into_owned().into();
            session.add_message(
                crate::session::MessageRole::User,
                "x".repeat(3 * NAME_PEEK_BYTES).as_str(),
            );
            session
        };
        let wanted = big("wanted", &here);
        save_session(&wanted).unwrap();
        // A legacy-layout file: `working_dir` far beyond the peeked head.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let legacy = big("legacy", &here);
        let mut value = serde_json::to_value(&legacy).unwrap();
        let object = value.as_object_mut().unwrap();
        let working_dir = object.remove("working_dir").unwrap();
        let mut late = serde_json::Map::new();
        late.extend(std::mem::take(object));
        late.insert("working_dir".into(), working_dir);
        let legacy_path = session_dir().join(format!("{}.json", legacy.id));
        atomic_write(&legacy_path, &serde_json::Value::Object(late).to_string()).unwrap();
        assert!(
            peek_session_string(&legacy_path, "working_dir").is_none(),
            "a late working_dir must be undetermined by the head peek"
        );
        for index in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            save_session(&big(&format!("foreign-{index}"), &elsewhere)).unwrap();
        }
        let head = |session: &Session| {
            peek_session_string(
                &session_dir().join(format!("{}.json", session.id)),
                "working_dir",
            )
        };
        assert_eq!(
            head(&wanted).as_deref(),
            Some(here.to_string_lossy().as_ref())
        );

        FULL_SESSION_READS.with(|reads| reads.set(0));
        let found = find_recent_sessions_for_workspace(1, &here).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, legacy.id);
        // 3 foreign heads are peeked (no read), then the legacy file is read
        // for the fallback and decoded from that same read.
        assert_eq!(FULL_SESSION_READS.with(|reads| reads.get()), 1);

        FULL_SESSION_READS.with(|reads| reads.set(0));
        let both = find_recent_sessions_for_workspace(2, &here).unwrap();
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].id, wanted.id);
        // legacy: one read; wanted: peeked, then read once for the decode.
        assert_eq!(FULL_SESSION_READS.with(|reads| reads.get()), 2);
    }

    // mini-agent-hf54t: a prefix lookup decodes only the files it can match.
    #[test]
    fn prefix_lookup_peeks_names_and_decodes_only_matching_sessions() {
        let root = crate::tests::OwnedTestDirectory::new("session-prefix-peek");
        let _environment = scoped_state(&root);
        let mut wanted = Vec::new();
        for index in 0..20 {
            let name = if index == 7 { "Deploy Plan" } else { "other" };
            let mut session = Session::new("provider", "model", 32_000, name);
            session.add_message(
                crate::session::MessageRole::User,
                "x".repeat(2_000).as_str(),
            );
            save_session(&session).unwrap();
            if index == 7 {
                wanted.push(session.id.to_string());
            }
        }
        let dir = session_dir();
        // Not decodable as a session, but its head says the name does not match.
        let unrelated = dir.join("zz-unrelated.json");
        std::fs::write(
            &unrelated,
            "{\"id\":\"zz-unrelated\",\"name\":\"nothing\",\"messages\":[BROKEN",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(peek_session_name(&unrelated).as_deref(), Some("nothing"));
        let found = find_sessions_by_prefix("deploy").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id.as_str(), wanted[0]);
        let by_stem = find_sessions_by_prefix(&wanted[0][..12]).unwrap();
        assert_eq!(by_stem.len(), 1);
    }
}
