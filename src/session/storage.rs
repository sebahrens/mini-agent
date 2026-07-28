use std::path::PathBuf;

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

pub(crate) fn config_path() -> PathBuf {
    app_paths().config_dir
}

/// Write `content` to `path` atomically: write to a temp file in the same
/// directory, then rename. On POSIX this is atomic; a crash mid-write leaves
/// the previous version intact.
pub fn atomic_write(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_directory(parent)?;
    }
    crate::fs::atomic_write_sync(path, content.as_bytes())?;
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
    crate::fs::atomic_create_sync(&path, output.as_bytes())?;
    Ok(path)
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    if disabled("sessions") {
        return Ok(());
    }
    let dir = session_dir();
    crate::paths::validate_portable_component(id)?;
    let path = dir.join(format!("{}.json", id));
    if path.exists() {
        std::fs::remove_file(&path)?;
        tracing::debug!("session deleted: id={}", id);
    } else {
        tracing::debug!("session delete skipped (not found): id={}", id);
    }
    Ok(())
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let dir = session_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let lower = prefix.to_lowercase();
    let mut sessions: Vec<Session> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
            && (stem.starts_with(prefix) || session.name.to_lowercase().contains(&lower))
        {
            sessions.push(session);
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
    let dir = session_dir();
    if !dir.exists() {
        return Ok(None);
    }
    let lower = name.to_lowercase();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(json) = std::fs::read_to_string(&path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
            && session.name.to_lowercase() == lower
        {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<Session>> {
    if disabled("sessions") {
        return Ok(Vec::new());
    }
    let dir = session_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Sort by filesystem mtime to avoid loading all sessions
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
        .map(|e| {
            let mtime = e.metadata().ok().and_then(|m| m.modified().ok());
            let path = e.path();
            (mtime, path)
        })
        .collect();

    // Sort newest first
    entries.sort_by_key(|b| std::cmp::Reverse(b.0));

    let mut sessions: Vec<Session> = Vec::new();
    for (_, path) in entries.iter().take(limit) {
        if let Ok(json) = std::fs::read_to_string(path)
            && let Ok(session) = serde_json::from_str::<Session>(&json)
        {
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
