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
pub(super) async fn checkpoint(history: &super::SessionHistory) -> anyhow::Result<()> {
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
    let snapshot = crate::session::storage::prepare_save(&history.session);
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
    result
        .map_err(|_| anyhow::anyhow!("ACP session checkpoint task failed"))?
        .map_err(|_| {
            anyhow::anyhow!("ACP session checkpoint failed; observed progress remains in memory")
        })
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
                        .map_err(|_| anyhow::anyhow!("invalid ACP load workspace"))?,
                );
                let paths = crate::paths::process_paths_ref()?;
                let sessions = crate::paths::WorkspaceBinding::capture(&paths.sessions_dir())
                    .map_err(anyhow::Error::msg)?;
                let session = crate::session::storage::load_session_exact_read_only(
                    &sessions,
                    &requested_id,
                    &workspace,
                )?
                .ok_or_else(|| anyhow::anyhow!("saved ACP session not found"))?;
                Ok::<_, anyhow::Error>((session, workspace))
            })
            .await
        })
        .await;
    cleanup.settle().await;
    let (session, workspace) = loaded
        .map_err(|_| agent_client_protocol::Error::new(-32603, "ACP session load task failed"))?
        .map_err(|_| {
            agent_client_protocol::Error::new(
                -32602,
                "saved ACP session is missing, incompatible or unsafe",
            )
        })?;
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
    let replay = replay_updates(&req.session_id, &session)?;
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
    super::commands::announce(&req.session_id, &cx)?;
    responder.respond(LoadSessionResponse::new().modes(modes))
}

fn replay_updates(
    id: &SessionId,
    session: &crate::session::Session,
) -> Result<Vec<SessionNotification>, agent_client_protocol::Error> {
    use crate::session::MessageRole;
    let mut notifications = Vec::new();
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
            notifications.push(SessionNotification::new(
                id.clone(),
                SessionUpdate::ToolCall(
                    ToolCall::new(call_id.to_string(), name.to_string())
                        .raw_input(arguments.clone())
                        .status(ToolCallStatus::Pending),
                ),
            ));
        }
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
        let updates = replay_updates(&id, &session).unwrap();
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
}
