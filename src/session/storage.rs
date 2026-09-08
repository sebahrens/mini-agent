use std::io::Read;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::session::Session;

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

fn app_paths() -> crate::paths::AppPaths {
    crate::paths::process_paths().expect("startup must initialize application paths")
}

fn disabled(artifact: &'static str) -> bool {
    crate::paths::artifact_disabled(artifact)
}

fn existing_session_dir() -> anyhow::Result<Option<PathBuf>> {
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
    let mut file = crate::fs::open_private_file(path).map_err(|error| {
        anyhow::anyhow!("refusing unsafe session file {}: {}", path.display(), error)
    })?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

#[cfg(test)]
pub(crate) fn config_path() -> PathBuf {
    app_paths().config_dir
}

/// Write `content` privately and atomically via a same-directory temporary
/// file. A crash mid-write leaves the previous version intact.
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

pub fn save_session(session: &Session) -> anyhow::Result<()> {
    if disabled("sessions") {
        return Ok(());
    }
    let dir = session_dir();
    crate::paths::ensure_private_directory(&dir)?;
    let path = session_path(&session.id)?;
    let json = serde_json::to_string(session)?;
    let json_len = json.len();
    atomic_write(&path, &json)?;
    tracing::debug!(
        "session saved: id={}, msgs={}, size={}",
        session.id,
        session.messages.len(),
        json_len,
    );
    Ok(())
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
    match serde_json::from_str::<Session>(&json) {
        Ok(session) => Some(session),
        Err(error) => {
            skipped.note(path, &error);
            None
        }
    }
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
            && let Some(session) = read_discovered_session(&path, &mut skipped)
            && (stem.starts_with(prefix) || session.name.to_lowercase().contains(&lower))
        {
            sessions.push(session);
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
            let session: Session = serde_json::from_str(&json)
                .map_err(|error| anyhow::anyhow!("invalid persisted session {id}: {error}"))?;
            anyhow::ensure!(
                session.id == id,
                "persisted session ID mismatch: requested {id}, found {}",
                session.id
            );
            Ok(Some(session))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

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

pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(Vec::new());
    };
    // Sort by filesystem mtime to avoid loading all sessions
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
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

    // Sort newest first
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));

    let mut sessions: Vec<Session> = Vec::new();
    let mut skipped = SkippedEntries::default();
    // The limit applies to sessions that actually decoded: a run of broken
    // recent files must not hide older valid sessions.
    for (_, path) in entries.iter() {
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
pub fn find_recent_sessions_for_workspace(
    limit: usize,
    workspace: &Path,
) -> anyhow::Result<Vec<Session>> {
    let workspace = std::fs::canonicalize(workspace)?;
    let mut matches = Vec::new();
    for session in find_recent_sessions(usize::MAX)? {
        let saved = Path::new(session.working_dir.as_str());
        if std::fs::canonicalize(saved).ok().as_deref() == Some(workspace.as_path()) {
            matches.push(session);
            if matches.len() == limit {
                break;
            }
        }
    }
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
