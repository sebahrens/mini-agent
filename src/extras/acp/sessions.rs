//! Read-only ACP session metadata discovery. Stored workspace labels are never opened.
use std::sync::Arc;

use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::*;

use super::AcpState;

const PAGE_SIZE: usize = 20;
const CURSOR_PREFIX: &str = "bounded-v1:";

fn cursor_offset(cursor: Option<&str>) -> Result<usize, agent_client_protocol::Error> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let offset = cursor
        .strip_prefix(CURSOR_PREFIX)
        .filter(|value| {
            !value.is_empty() && value.len() <= 3 && value.bytes().all(|byte| byte.is_ascii_digit())
        })
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|offset| *offset <= crate::session::catalogue::MAX_ROWS);
    offset
        .ok_or_else(|| agent_client_protocol::Error::new(-32602, "invalid ACP session list cursor"))
}

pub(super) async fn handle_list(
    req: ListSessionsRequest,
    responder: Responder<ListSessionsResponse>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _activity = state.idle.admit()?;
    let offset = cursor_offset(req.cursor.as_deref())?;
    let all = req.cwd.is_none();
    let requested_root = req
        .cwd
        .unwrap_or_else(|| state.context.workspace_root.clone());
    let scope = crate::agent::runner::AgentWorkScope::new();
    #[cfg(test)]
    let scope = super::lock_unpoisoned(&state.leases.storage_scope_for_test)
        .clone()
        .unwrap_or(scope);
    let _cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(Arc::clone(&scope));
    let result = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                let workspace = super::canonical_session_workspace(&requested_root)
                    .map_err(|_| anyhow::anyhow!("invalid ACP session-list workspace"))?;
                crate::session::catalogue::list_readonly(&workspace, all)
            })
            .await
        })
        .await;
    scope.wait_idle().await;
    let catalogue = result
        .map_err(|_| {
            agent_client_protocol::Error::new(-32603, "ACP session discovery task failed")
        })?
        .map_err(|_| {
            agent_client_protocol::Error::new(
                -32603,
                "ACP session discovery refused unsafe or unavailable storage",
            )
        })?;
    let total = catalogue.rows.len();
    let sessions = catalogue
        .rows
        .into_iter()
        .skip(offset)
        .take(PAGE_SIZE)
        .map(|row| {
            SessionInfo::new(SessionId::new(row.id.to_string()), row.working_dir.as_str())
                .title(row.title.to_string())
                .updated_at(row.updated_at.to_string())
        })
        .collect::<Vec<_>>();
    let next = offset.saturating_add(sessions.len());
    let mut response = ListSessionsResponse::new(sessions);
    if next < total {
        response.next_cursor = Some(format!("{CURSOR_PREFIX}{next}"));
    }
    response.meta = Some(serde_json::Map::from_iter([
        (
            "catalogue_truncated".to_owned(),
            serde_json::json!(catalogue.truncated),
        ),
        (
            "skipped_entries".to_owned(),
            serde_json::json!(catalogue.skipped),
        ),
    ]));
    responder.respond(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acp53_session_list_cursor_is_bounded_data_not_a_path() {
        assert_eq!(cursor_offset(None).unwrap(), 0);
        assert_eq!(cursor_offset(Some("bounded-v1:20")).unwrap(), 20);
        assert_eq!(cursor_offset(Some("bounded-v1:100")).unwrap(), 100);
        for value in [
            "",
            "../secret",
            "bounded-v1:101",
            "bounded-v1:-1",
            "bounded-v1:1/secret",
            "bounded-v1:1000",
            "bounded-v1:雪",
            "bounded-v1:",
        ] {
            assert!(cursor_offset(Some(value)).is_err(), "{value}");
        }
    }
}

/// Publication follows the exact model/effect/accounting retirement boundary.
/// The retained session lease owns the writer until the last history owner drops.
pub(super) async fn checkpoint(
    history: &mut super::SessionHistory,
    mut notice: impl FnMut(&str),
) -> anyhow::Result<()> {
    let lease = history
        ._lease
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ACP session has no persistence owner"))?;
    anyhow::ensure!(
        lease.writer.load(std::sync::atomic::Ordering::Acquire),
        "ACP session writer was not claimed"
    );
    // Legacy sessions are encoded here from the borrow (no deep clone of every
    // message); only native-bearing ones keep an owned snapshot for the writer.
    // A validation/size failure is carried into the blocking job so it is
    // reported (and scoped) exactly like a failed write.
    let snapshot =
        crate::session::storage::prepare_save_recovering(&mut history.session, &mut notice);
    let scope = crate::agent::runner::AgentWorkScope::new();
    #[cfg(test)]
    let scope = super::lock_unpoisoned(&lease.owner.storage_scope_for_test)
        .clone()
        .unwrap_or(scope);
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let result = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                let _lease = lease;
                anyhow::ensure!(
                    !crate::paths::artifact_disabled("sessions"),
                    "ACP session persistence is disabled"
                );
                snapshot?.commit()
            })
            .await
        })
        .await;
    cleanup.settle().await;
    let result = result.map_err(|_| anyhow::anyhow!("ACP session checkpoint task failed"))?;
    match result {
        Ok(()) => {
            crate::session::storage::report_save_success(&mut history.session, notice);
            Ok(())
        }
        Err(error) if crate::session::storage::is_session_size_error(&error) => Err(error),
        Err(_) => Err(anyhow::anyhow!(
            "ACP session checkpoint failed; observed progress remains in memory"
        )),
    }
}

/// Why `session/load` refused, each with its own bounded message. The
/// underlying error is logged, never returned, so no stored path or file
/// content beyond the saved workspace label reaches the client.
#[derive(Debug, PartialEq, Eq)]
enum LoadFailure {
    InvalidWorkspace,
    NotFound,
    WorkspaceMismatch { saved: String },
    Incompatible,
    Unavailable,
}

impl LoadFailure {
    fn unavailable(error: &dyn std::fmt::Display) -> Self {
        tracing::warn!("ACP session load could not use session storage: {error}");
        Self::Unavailable
    }

    fn from_error(error: &anyhow::Error) -> Self {
        use crate::session::storage::ExactLoadRefusal;
        match error.downcast_ref::<ExactLoadRefusal>() {
            Some(ExactLoadRefusal::WorkspaceMismatch { saved }) => Self::WorkspaceMismatch {
                saved: saved.clone(),
            },
            Some(ExactLoadRefusal::Incompatible | ExactLoadRefusal::IdentityMismatch) => {
                tracing::warn!("ACP session load refused an unreadable session file");
                Self::Incompatible
            }
            None => Self::unavailable(&format_args!("{error:#}")),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidWorkspace => {
                "session/load needs an absolute, existing workspace folder".to_owned()
            }
            Self::NotFound => "no saved session has that id".to_owned(),
            Self::WorkspaceMismatch { saved } => format!(
                "that session was saved for another workspace folder ({}); open that folder to resume it",
                super::toolcall::clip(saved, 120)
            ),
            Self::Incompatible => {
                "the saved session file is not readable by this version of mini-agent".to_owned()
            }
            Self::Unavailable => {
                "session storage is unsafe or unavailable; see the mini-agent log".to_owned()
            }
        }
    }

    fn into_error(self) -> agent_client_protocol::Error {
        agent_client_protocol::Error::new(-32602, self.message())
    }
}

pub(super) async fn handle_load(
    req: LoadSessionRequest,
    responder: Responder<LoadSessionResponse>,
    cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Client>,
    state: &AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let _activity = state.idle.admit()?;
    if !req.additional_directories.is_empty() {
        return Err(agent_client_protocol::Error::new(
            -32602,
            "additional ACP workspace directories are not supported",
        ));
    }
    // Fresh runtime lease/policy; no permission choice, waiter, hidden redo or
    // process-local skill/MCP authority can be adopted from saved data.
    let lease = state.leases.reserve(req.session_id.clone())?;
    #[cfg(feature = "mcp")]
    let client_mcp_servers = super::client_mcp_servers(state, &req.mcp_servers)?;
    #[cfg(not(feature = "mcp"))]
    super::client_mcp_servers(state, &req.mcp_servers)?;
    let requested_id = req.session_id.to_string();
    let requested_cwd = req.cwd;
    let scope = crate::agent::runner::AgentWorkScope::new();
    #[cfg(test)]
    let scope = super::lock_unpoisoned(&state.leases.storage_scope_for_test)
        .clone()
        .unwrap_or(scope);
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
    let loaded = scope
        .run(async move {
            crate::agent::runner::spawn_blocking_scoped(move || {
                let workspace = Arc::new(
                    super::canonical_session_workspace(&requested_cwd)
                        .map_err(|_| LoadFailure::InvalidWorkspace)?,
                );
                let paths = crate::paths::process_paths_ref()
                    .map_err(|error| LoadFailure::unavailable(&error))?;
                let sessions = crate::paths::WorkspaceBinding::capture(&paths.sessions_dir())
                    .map_err(|error| LoadFailure::unavailable(&error))?;
                let session = crate::session::storage::load_session_exact_read_only(
                    &sessions,
                    &requested_id,
                    &workspace,
                )
                .map_err(|error| LoadFailure::from_error(&error))?
                .ok_or(LoadFailure::NotFound)?;
                Ok::<_, LoadFailure>((session, workspace))
            })
            .await
        })
        .await;
    cleanup.settle().await;
    let (session, workspace) = loaded
        .map_err(|_| agent_client_protocol::Error::new(-32603, "ACP session load task failed"))?
        .map_err(LoadFailure::into_error)?;
    #[cfg(not(feature = "multimodal"))]
    if session
        .messages
        .iter()
        .any(|message| message.user.as_ref().is_some_and(|user| user.has_images()))
    {
        return Err(agent_client_protocol::Error::new(
            -32602,
            "saved ACP session contains images but multimodal support is not compiled",
        ));
    }
    if responder.cancellation().is_cancelled() {
        return Err(agent_client_protocol::Error::new(
            -32000,
            "ACP session load was cancelled",
        ));
    }
    let replay = replay_updates(&req.session_id, &session, workspace.root())?;
    let modes = super::install_session(
        state,
        req.session_id.clone(),
        workspace,
        session,
        lease,
        #[cfg(feature = "subagents")]
        super::BackgroundSessionClient::Protocol(cx.clone()),
        #[cfg(feature = "mcp")]
        client_mcp_servers,
    )
    .await?;
    // Replay literal saved data only. Opaque native signatures/redacted blocks
    // are never sent to the editor's display channel.
    for notification in replay {
        if let Err(error) = cx.send_notification(notification) {
            state.sessions.lock().await.remove(&req.session_id);
            super::lock_unpoisoned(&state.cancel_routes).remove(&req.session_id);
            return Err(error);
        }
    }
    let config_options = super::persona_config_options(state, &req.session_id).await;
    super::commands::announce(state, &req.session_id, &cx).await?;
    responder.respond(
        LoadSessionResponse::new()
            .modes(modes)
            .config_options(config_options),
    )
}

fn replay_updates(
    id: &SessionId,
    session: &crate::session::Session,
    workspace_root: &std::path::Path,
) -> Result<Vec<SessionNotification>, agent_client_protocol::Error> {
    use crate::session::MessageRole;
    let mut notifications = Vec::new();
    let mut open_calls: Vec<String> = Vec::new();
    for message in &session.messages {
        if message.role == MessageRole::User {
            let blocks = match &message.user {
                Some(user) => user
                    .blocks()
                    .iter()
                    .map(|block| match block {
                        crate::session::user::UserBlock::Text { text } => {
                            ContentBlock::Text(TextContent::new(text.clone()))
                        }
                        crate::session::user::UserBlock::Image { data, mime_type } => {
                            ContentBlock::Image(ImageContent::new(data.clone(), mime_type.as_str()))
                        }
                    })
                    .collect(),
                None => vec![ContentBlock::Text(TextContent::new(
                    message.content.to_string(),
                ))],
            };
            for block in blocks {
                notifications.push(SessionNotification::new(
                    id.clone(),
                    SessionUpdate::UserMessageChunk(ContentChunk::new(block)),
                ));
            }
        } else if message.role == MessageRole::Assistant {
            notifications.push(SessionNotification::new(
                id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(message.content.to_string()),
                ))),
            ));
        } else if message.role == MessageRole::ToolResult {
            if let Some(call_id) = &message.tool_call_id {
                open_calls.retain(|open| open != call_id.as_str());
                let fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::from(ContentBlock::Text(
                        TextContent::new(message.content.to_string()),
                    ))]);
                notifications.push(SessionNotification::new(
                    id.clone(),
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(call_id.to_string(), fields)),
                ));
            }
        } else if message.role == MessageRole::ToolCall
            && let (
                Some(call_id),
                Some(crate::session::PersistedToolMessage::Call { name, arguments }),
            ) = (&message.tool_call_id, &message.tool)
        {
            // The same announcement the live relay builds: title, kind and
            // absolute locations.
            open_calls.push(call_id.to_string());
            notifications.push(SessionNotification::new(
                id.clone(),
                SessionUpdate::ToolCall(super::announce_tool_call(
                    call_id,
                    name,
                    arguments,
                    workspace_root,
                )),
            ));
        }
    }
    // A call whose result was never saved (an interrupted turn) must not be
    // left Pending: the editor would show a spinner on yesterday's call.
    for call_id in open_calls {
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new("interrupted before a result was recorded"),
            ))]);
        notifications.push(SessionNotification::new(
            id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(call_id, fields)),
        ));
    }
    Ok(notifications)
}

#[cfg(test)]
mod canonical_tests {
    use super::*;
    #[test]
    fn acp53_replay_keeps_ordered_user_data_and_never_emits_opaque_reasoning() {
        let mut session = crate::session::Session::new("logical", "model", 128_000, "");
        let input = crate::session::user::PersistedUserMessage::new(vec![
            crate::session::user::UserBlock::Text {
                text: "before /undo".into(),
            },
            crate::session::user::UserBlock::Image {
                data: "aW5lcnQ=".into(),
                mime_type: crate::session::user::ImageMime::Png,
            },
            crate::session::user::UserBlock::Text {
                text: "after".into(),
            },
        ])
        .unwrap();
        session.add_user_message(input);
        session.add_message(crate::session::MessageRole::Assistant, "visible answer");
        let id = SessionId::new(session.id.to_string());
        let updates = replay_updates(&id, &session, std::path::Path::new("/work")).unwrap();
        assert_eq!(updates.len(), 4);
        assert!(
            matches!(&updates[0].update, SessionUpdate::UserMessageChunk(chunk) if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "before /undo"))
        );
        assert!(
            matches!(&updates[1].update, SessionUpdate::UserMessageChunk(chunk) if matches!(&chunk.content, ContentBlock::Image(image) if image.data == "aW5lcnQ=" && image.mime_type == "image/png"))
        );
        assert!(
            matches!(&updates[2].update, SessionUpdate::UserMessageChunk(chunk) if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "after"))
        );
        assert!(
            matches!(&updates[3].update, SessionUpdate::AgentMessageChunk(chunk) if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "visible answer"))
        );
    }

    #[test]
    fn replay_matches_live_announcements_and_closes_unfinished_calls() {
        let root = std::env::temp_dir().join("acp-replay");
        let mut session = crate::session::Session::new("logical", "model", 128_000, "");
        session.add_tool_call_with_id("done", "edit", &serde_json::json!({"path": "src/main.rs"}));
        session.add_tool_result_with_id("done", "edit", "ok");
        session.add_tool_call_with_id(
            "lost",
            "shell",
            &serde_json::json!({"command": "cargo test"}),
        );
        let id = SessionId::new(session.id.to_string());
        let updates = replay_updates(&id, &session, &root).unwrap();
        assert_eq!(updates.len(), 4);
        let SessionUpdate::ToolCall(call) = &updates[0].update else {
            panic!("expected a tool call");
        };
        assert_eq!(call.title, "edit src/main.rs");
        assert_eq!(call.kind, ToolKind::Edit);
        assert_eq!(call.locations[0].path, root.join("src/main.rs"));
        let SessionUpdate::ToolCallUpdate(done) = &updates[1].update else {
            panic!("expected a result update");
        };
        assert_eq!(done.fields.status, Some(ToolCallStatus::Completed));
        let SessionUpdate::ToolCall(lost) = &updates[2].update else {
            panic!("expected a tool call");
        };
        assert_eq!(lost.title, "shell cargo test");
        let SessionUpdate::ToolCallUpdate(closed) = &updates[3].update else {
            panic!("expected the unfinished call to be closed");
        };
        assert_eq!(closed.tool_call_id.0.as_ref(), "lost");
        assert_eq!(closed.fields.status, Some(ToolCallStatus::Failed));
    }

    #[test]
    fn load_failures_have_one_distinct_bounded_message_each() {
        use crate::session::storage::ExactLoadRefusal;
        let mismatch =
            LoadFailure::from_error(&anyhow::Error::new(ExactLoadRefusal::WorkspaceMismatch {
                saved: "/other/project".into(),
            }));
        assert_eq!(
            mismatch,
            LoadFailure::WorkspaceMismatch {
                saved: "/other/project".into()
            }
        );
        assert!(mismatch.message().contains("/other/project"));
        let incompatible =
            LoadFailure::from_error(&anyhow::Error::new(ExactLoadRefusal::Incompatible));
        assert_eq!(incompatible, LoadFailure::Incompatible);
        let other = LoadFailure::from_error(&anyhow::anyhow!("secret /etc/shadow detail"));
        assert_eq!(other, LoadFailure::Unavailable);
        let messages = [
            LoadFailure::InvalidWorkspace.message(),
            LoadFailure::NotFound.message(),
            mismatch.message(),
            incompatible.message(),
            other.message(),
        ];
        for (index, message) in messages.iter().enumerate() {
            assert!(!message.contains("shadow"));
            assert!(!message.contains("missing, incompatible or unsafe"));
            for later in &messages[index + 1..] {
                assert_ne!(message, later);
            }
        }
        let long = LoadFailure::WorkspaceMismatch {
            saved: "x".repeat(5000),
        };
        assert!(long.message().chars().count() < 300);
    }
}
