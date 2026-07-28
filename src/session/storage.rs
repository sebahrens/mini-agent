use std::io::Read;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::session::Session;

fn session_dir() -> PathBuf {
    app_paths().sessions_dir()
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
    crate::paths::validate_portable_component(&session.id)?;
    let path = dir.join(format!("{}.json", session.id));
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

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(Vec::new());
    };
    let lower = prefix.to_lowercase();
    let mut sessions: Vec<Session> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            let json = read_private_string(&path)?;
            if let Ok(session) = serde_json::from_str::<Session>(&json)
                && (stem.starts_with(prefix) || session.name.to_lowercase().contains(&lower))
            {
                sessions.push(session);
            }
        }
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.dedup_by(|a, b| a.id == b.id);
    tracing::debug!(
        "find_sessions_by_prefix('{}'): {} results",
        prefix,
        sessions.len(),
    );
    Ok(sessions)
}

pub fn find_session_by_name(name: &str) -> anyhow::Result<Option<Session>> {
    if disabled("sessions") {
        return Ok(None);
    }
    let Some(dir) = existing_session_dir()? else {
        return Ok(None);
    };
    let lower = name.to_lowercase();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let json = read_private_string(&path)?;
            if let Ok(session) = serde_json::from_str::<Session>(&json)
                && session.name.to_lowercase() == lower
            {
                return Ok(Some(session));
            }
        }
    }
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
    for (_, path) in entries.iter().take(limit) {
        let json = read_private_string(path)?;
        if let Ok(session) = serde_json::from_str::<Session>(&json) {
            sessions.push(session);
        }
    }
    tracing::debug!(
        "find_recent_sessions(limit={}): {} results",
        limit,
        sessions.len(),
    );
    Ok(sessions)
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
