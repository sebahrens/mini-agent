//! Bounded discovery metadata for workspace-scoped interactive session selection.
//! Stored workspace text labels history; it is never opened or resolved.

use std::io::Read;
use std::path::{Component, Path};

use compact_str::CompactString;
use serde::{Deserialize, Deserializer};

use super::{SESSION_TITLE_CHARS, Session};

pub(crate) const MAX_ENTRIES: usize = 2_048;
pub(crate) const MAX_ROWS: usize = 100;
pub(crate) const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MESSAGES: usize = 100_000;

#[derive(Clone, Debug)]
pub(crate) struct SessionRow {
    pub id: CompactString,
    pub title: CompactString,
    pub updated_at: CompactString,
    pub working_dir: CompactString,
    pub message_count: usize,
    pub provider: CompactString,
    pub model: CompactString,
    pub current_workspace: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ResumeSelection {
    pub row: SessionRow,
    pub all: bool,
    pub workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
}

#[derive(Default)]
pub(crate) struct Catalogue {
    pub rows: Vec<SessionRow>,
    pub truncated: bool,
    pub skipped: usize,
}

#[derive(Default, Deserialize)]
struct MessagePeek {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

#[derive(Default)]
struct MessageSummary {
    count: usize,
    first_user: String,
}

fn messages<'de, D: Deserializer<'de>>(deserializer: D) -> Result<MessageSummary, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = MessageSummary;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a bounded session message array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut summary = MessageSummary::default();
            while let Some(message) = sequence.next_element::<MessagePeek>()? {
                summary.count += 1;
                if summary.count > MAX_MESSAGES {
                    return Err(serde::de::Error::custom(
                        "session message count exceeds catalogue budget",
                    ));
                }
                if summary.first_user.is_empty()
                    && message.role == "user"
                    && !message.content.trim().is_empty()
                {
                    summary.first_user = short_text(&message.content);
                }
            }
            Ok(summary)
        }
    }
    deserializer.deserialize_seq(Visitor)
}

#[derive(Default, Deserialize)]
struct Header {
    id: CompactString,
    #[serde(default)]
    name: String,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    updated_at: CompactString,
    #[serde(default)]
    working_dir: CompactString,
    #[serde(default)]
    provider: CompactString,
    #[serde(default)]
    model: CompactString,
    #[serde(default, deserialize_with = "messages")]
    messages: MessageSummary,
}

fn short_text(value: &str) -> String {
    let sanitized = crate::ui::events::sanitize_output(value);
    let text = sanitized
        .split_whitespace()
        .take(SESSION_TITLE_CHARS)
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = text.chars();
    let result: String = chars.by_ref().take(SESSION_TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

pub(crate) fn same_workspace(saved: &str, workspace: &crate::paths::WorkspaceBinding) -> bool {
    let saved = Path::new(saved);
    saved.is_absolute()
        && !saved
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        && saved == workspace.root()
}

fn open_catalogue_file(path: &Path) -> anyhow::Result<std::fs::File> {
    let file = crate::fs::open_private_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            file.metadata()?.nlink() == 1,
            "session catalogue rejects hard links"
        );
    }
    #[cfg(windows)]
    anyhow::ensure!(
        crate::fs::windows_file_link_count(&file)? == 1,
        "session catalogue rejects hard links"
    );
    Ok(file)
}

pub(crate) fn list(
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
) -> anyhow::Result<Catalogue> {
    workspace.validate().map_err(anyhow::Error::msg)?;
    let mut catalogue = Catalogue::default();
    if crate::paths::artifact_disabled("sessions") {
        return Ok(catalogue);
    }
    let Some(directory) = super::storage::existing_session_dir()? else {
        return Ok(catalogue);
    };
    let retained = crate::paths::WorkspaceBinding::capture(&directory)?;
    let mut entries = Vec::new();
    for (index, entry) in std::fs::read_dir(&directory)?.enumerate() {
        if index >= MAX_ENTRIES {
            catalogue.truncated = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                catalogue.skipped += 1;
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let modified = std::fs::symlink_metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        entries.push((modified, path));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut total = 0_u64;
    for (_, path) in entries {
        retained.validate().map_err(anyhow::Error::msg)?;
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            catalogue.skipped += 1;
            continue;
        };
        if uuid::Uuid::try_parse(stem).is_err() {
            catalogue.skipped += 1;
            continue;
        }
        let candidate = (|| -> anyhow::Result<Header> {
            let mut file = open_catalogue_file(&path)?;
            retained.validate().map_err(anyhow::Error::msg)?;
            let length = file.metadata()?.len();
            anyhow::ensure!(
                length <= MAX_FILE_BYTES,
                "oversized session catalogue entry"
            );
            if total.saturating_add(length) > MAX_TOTAL_BYTES {
                catalogue.truncated = true;
                anyhow::bail!("catalogue byte budget exhausted");
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take((MAX_TOTAL_BYTES - total).min(MAX_FILE_BYTES) + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_TOTAL_BYTES - total {
                catalogue.truncated = true;
                anyhow::bail!("catalogue byte budget exhausted");
            }
            total += bytes.len() as u64;
            anyhow::ensure!(
                bytes.len() as u64 <= MAX_FILE_BYTES,
                "session grew beyond catalogue budget"
            );
            let header: Header = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(header.id == stem, "session filename/id mismatch");
            Ok(header)
        })();
        let header = match candidate {
            Ok(header) => header,
            Err(_) => {
                catalogue.skipped += 1;
                if catalogue.truncated {
                    break;
                }
                continue;
            }
        };
        let current = same_workspace(&header.working_dir, workspace);
        if !all && !current {
            continue;
        }
        let title = if !header.name.trim().is_empty() {
            format!("[{}]", short_text(&header.name))
        } else {
            header
                .generated_title
                .as_deref()
                .map(short_text)
                .filter(|title| !title.is_empty())
                .unwrap_or(header.messages.first_user)
        };
        catalogue.rows.push(SessionRow {
            id: header.id,
            title: title.into(),
            updated_at: header.updated_at,
            working_dir: header.working_dir,
            message_count: header.messages.count,
            provider: header.provider,
            model: header.model,
            current_workspace: current,
        });
        if catalogue.rows.len() == MAX_ROWS {
            catalogue.truncated = true;
            break;
        }
    }
    retained.validate().map_err(anyhow::Error::msg)?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    Ok(catalogue)
}

/// An explicitly selected private session can transfer visible history across
/// workspaces, but its old approvals and concealed redo never follow it.
pub(crate) fn for_workspace(
    session: Session,
    workspace: &crate::paths::WorkspaceBinding,
) -> anyhow::Result<Session> {
    workspace.validate().map_err(anyhow::Error::msg)?;
    let mut session = if same_workspace(&session.working_dir, workspace) {
        session
    } else {
        let mut fork = super::lock::fork_session(&session)?;
        fork.permission_allowlist.clear();
        fork.rewind_undo = None;
        fork.retain_visible_replay_metadata();
        fork
    };
    session.working_dir = workspace.root().to_string_lossy().into();
    Ok(session)
}

/// Reload the exact private record, then decide scope again. A changed row
/// cannot silently turn a workspace-filtered selection into a foreign resume.
pub(crate) fn selected(
    row: &SessionRow,
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
) -> anyhow::Result<Session> {
    workspace.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        uuid::Uuid::try_parse(&row.id).is_ok(),
        "invalid selected session identity"
    );
    let directory = super::storage::existing_session_dir()?
        .ok_or_else(|| anyhow::anyhow!("selected session disappeared"))?;
    let retained = crate::paths::WorkspaceBinding::capture(&directory)?;
    let mut file = open_catalogue_file(&directory.join(format!("{}.json", row.id)))?;
    retained.validate().map_err(anyhow::Error::msg)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "selected session exceeds reload budget"
    );
    let session: Session = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        session.messages.len() <= MAX_MESSAGES,
        "selected session message count exceeds reload budget"
    );
    retained.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(session.id == row.id, "selected session identity changed");
    anyhow::ensure!(
        session.working_dir == row.working_dir,
        "selected session workspace changed; refresh the picker"
    );
    let current = same_workspace(&session.working_dir, workspace);
    anyhow::ensure!(
        all || current,
        "selected session belongs to another workspace"
    );
    let session = for_workspace(session, workspace)?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MessageRole;
    fn environment(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ])
    }
    fn stored(workspace: &Path, title: &str) -> Session {
        let mut session = Session::new("anthropic", "test", 128_000, "");
        session.working_dir = workspace.to_string_lossy().into();
        session.add_message(MessageRole::User, title);
        session.add_message(MessageRole::Assistant, "first answer");
        let file = super::super::storage::session_path(&session.id).unwrap();
        super::super::storage::atomic_write(&file, &serde_json::to_string(&session).unwrap())
            .unwrap();
        session
    }
    #[test]
    fn catalogue_filters_workspace_and_all_selection_forks_foreign_trust() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-scope");
        let _env = environment(&root);
        let here = root.path().join("here");
        let there = root.path().join("there");
        std::fs::create_dir(&here).unwrap();
        std::fs::create_dir(&there).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&here).unwrap();
        let current = stored(&here, "Current 雪");
        let mut foreign = stored(&there, "Foreign 雪");
        foreign.permission_allowlist = vec![crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "*".into(),
        }];
        foreign.add_message(MessageRole::User, "concealed");
        foreign.rewind_to(2);
        super::super::storage::atomic_write(
            &super::super::storage::session_path(&foreign.id).unwrap(),
            &serde_json::to_string(&foreign).unwrap(),
        )
        .unwrap();
        let scoped = list(&binding, false).unwrap();
        assert_eq!(scoped.rows.len(), 1);
        assert_eq!(scoped.rows[0].id, current.id);
        assert_eq!(scoped.rows[0].title, "Current 雪");
        assert_eq!(scoped.rows[0].message_count, 2);
        let all = list(&binding, true).unwrap();
        assert_eq!(all.rows.len(), 2);
        let row = all.rows.iter().find(|row| row.id == foreign.id).unwrap();
        assert!(!row.current_workspace);
        assert!(selected(row, &binding, false).is_err());
        let fork = selected(row, &binding, true).unwrap();
        assert_ne!(fork.id, foreign.id);
        assert_eq!(fork.messages.len(), 2);
        assert!(fork.permission_allowlist.is_empty());
        assert!(fork.rewind_undo.is_none());
        assert_eq!(fork.working_dir, current.working_dir);
        let loaded = selected(&scoped.rows[0], &binding, false).unwrap();
        assert_eq!(loaded.id, current.id);
    }
    #[test]
    fn catalogue_reloads_private_record_and_rejects_workspace_changes_and_large_files() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-reload");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let mut session = stored(root.path(), "fresh");
        let row = list(&binding, false).unwrap().rows.remove(0);
        session.working_dir = "/other/workspace".into();
        let path = super::super::storage::session_path(&session.id).unwrap();
        super::super::storage::atomic_write(&path, &serde_json::to_string(&session).unwrap())
            .unwrap();
        assert!(
            selected(&row, &binding, true)
                .unwrap_err()
                .to_string()
                .contains("workspace changed")
        );
        super::super::storage::atomic_write(&path, &"x".repeat(MAX_FILE_BYTES as usize + 1))
            .unwrap();
        assert!(
            selected(&row, &binding, true)
                .unwrap_err()
                .to_string()
                .contains("reload budget")
        );
        let catalogue = list(&binding, true).unwrap();
        assert!(catalogue.rows.is_empty());
        assert_eq!(catalogue.skipped, 1);
    }
    #[test]
    fn catalogue_limits_rows_sanitizes_titles_and_checks_filename_identity() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-budget");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        for _ in 0..MAX_ROWS + 1 {
            stored(root.path(), "label\u{1b}[31m雪\nsecond");
        }
        let catalogue = list(&binding, false).unwrap();
        assert_eq!(catalogue.rows.len(), MAX_ROWS);
        assert!(catalogue.truncated);
        assert!(
            catalogue
                .rows
                .iter()
                .all(|row| !row.title.contains('\u{1b}') && !row.title.contains('\n'))
        );
        let session = stored(root.path(), "id mismatch");
        let file = super::super::storage::session_path(&session.id).unwrap();
        let mut value = serde_json::to_value(&session).unwrap();
        value["id"] = uuid::Uuid::new_v4().to_string().into();
        super::super::storage::atomic_write(&file, &value.to_string()).unwrap();
        let mut row = SessionRow {
            id: session.id,
            title: "".into(),
            updated_at: "".into(),
            working_dir: session.working_dir,
            message_count: 2,
            provider: "".into(),
            model: "".into(),
            current_workspace: true,
        };
        assert!(
            selected(&row, &binding, false)
                .unwrap_err()
                .to_string()
                .contains("identity changed")
        );
        row.id = "../escape".into();
        assert!(selected(&row, &binding, true).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn catalogue_never_follows_symlink_or_hardlink_session_files() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-nofollow");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let session = stored(root.path(), "private target");
        let path = super::super::storage::session_path(&session.id).unwrap();
        let outside = root.path().join("outside.json");
        std::fs::rename(&path, &outside).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(list(&binding, true).unwrap().rows.is_empty());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert!(list(&binding, true).unwrap().rows.is_empty());
    }
    #[test]
    fn catalogue_total_bytes_and_directory_entry_limits_stop_discovery() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-total-budget");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let directory = super::super::storage::session_path(&uuid::Uuid::new_v4().to_string())
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        crate::paths::ensure_private_directory(&directory).unwrap();
        for _ in 0..9 {
            let id = uuid::Uuid::new_v4().to_string();
            let header=serde_json::json!({"id":id,"working_dir":root.path().to_string_lossy(),"messages":[]}).to_string();
            let mut payload = header.trim_end_matches('}').to_owned();
            payload.push_str(",\"padding\":\"");
            let room = MAX_FILE_BYTES as usize - payload.len() - 2;
            payload.extend(std::iter::repeat_n('x', room));
            payload.push_str("\"}");
            assert_eq!(payload.len(), MAX_FILE_BYTES as usize);
            super::super::storage::atomic_write(&directory.join(format!("{id}.json")), &payload)
                .unwrap();
        }
        let result = list(&binding, true).unwrap();
        assert_eq!(result.rows.len(), 8);
        assert!(result.truncated);
        for entry in std::fs::read_dir(&directory).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for index in 0..MAX_ENTRIES + 1 {
            std::fs::write(directory.join(format!("ignored-{index}.not-json")), []).unwrap();
        }
        let result = list(&binding, true).unwrap();
        assert!(result.rows.is_empty());
        assert!(result.truncated);
    }
    #[cfg(unix)]
    #[test]
    fn catalogue_selected_rejects_replaced_captured_workspace_before_private_reload() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-replaced-root");
        let parked = crate::tests::OwnedTestDirectory::new("session-catalogue-parked-root");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        stored(root.path(), "old root");
        let row = list(&binding, false).unwrap().rows.remove(0);
        std::fs::rename(root.path(), parked.path().join("original")).unwrap();
        symlink(parked.path(), root.path()).unwrap();
        assert!(selected(&row, &binding, true).is_err());
        assert!(list(&binding, true).is_err());
        std::fs::remove_file(root.path()).unwrap();
    }
}
