//! `/tasks` over ACP: list, read and cancel the session's background tasks.
//!
//! Like `/cron`, it is answered without a model turn and without turn
//! admission: it only reads or cancels pool entries, so an editor user can
//! see what a mode-change refusal is waiting for and act on it.

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Client, ConnectionTo, Responder};

use crate::extras::subagents::background::SavedTask;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    List,
    Result(String),
    Cancel(String),
}

const USAGE: &str = "usage: /tasks | /tasks result <id> | /tasks cancel <id>";

pub(super) fn parse(arguments: &str) -> Result<Request, String> {
    let parts: Vec<&str> = arguments.split_whitespace().collect();
    match parts.as_slice() {
        [] => Ok(Request::List),
        [action @ ("result" | "cancel"), id] => {
            // A full id or an unambiguous prefix of one; resolved against the
            // session's tasks by [`resolve_id`].
            if id.len() > 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                return Err("invalid task id; run /tasks to list them".to_owned());
            }
            Ok(if *action == "result" {
                Request::Result((*id).to_owned())
            } else {
                Request::Cancel((*id).to_owned())
            })
        }
        _ => Err(USAGE.to_owned()),
    }
}

/// The task `token` names: an exact id, or the only id it prefixes. An
/// ambiguous prefix lists its matches; no match says to run `/tasks`.
pub(super) fn resolve_id(rows: &[SavedTask], token: &str) -> Result<String, String> {
    if let Some(row) = rows.iter().find(|row| row.id == token) {
        return Ok(row.id.clone());
    }
    let matches: Vec<&SavedTask> = rows
        .iter()
        .filter(|row| row.id.starts_with(token))
        .collect();
    match matches.as_slice() {
        [only] => Ok(only.id.clone()),
        [] => Err("no background task has that id; run /tasks to list them".to_owned()),
        many => Err(format!(
            "ambiguous task id; {USAGE}\nmatches:\n{}",
            many.iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// What a task was asked to do, for a list row: the stored label (persona and
/// the start of its first prompt) and the persona when the label lacks it,
/// sanitised and clipped.
fn describe(row: &SavedTask) -> String {
    let mut parts = Vec::new();
    if let Some(label) = row
        .label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
    {
        parts.push(super::toolcall::clip(label, 80));
    }
    if let Some(agent) = row.agent_type.as_deref().filter(|agent| !agent.is_empty()) {
        let agent = super::toolcall::clip(agent, 40);
        if !parts.iter().any(|part| part.contains(&agent)) {
            parts.push(format!("[{agent}]"));
        }
    }
    if let Some(children) = row.children.filter(|children| *children > 1) {
        parts.push(format!("{children} children"));
    }
    parts.join(" ")
}

/// One `id  status  description` row per task, or a sentence when there are
/// none.
pub(super) fn format_list(rows: &[SavedTask]) -> String {
    if rows.is_empty() {
        return "no background tasks in this session".to_owned();
    }
    rows.iter()
        .map(|row| {
            let mut line = format!("{}  {}", row.id, row.status.as_str());
            let what = describe(row);
            if !what.is_empty() {
                line.push_str("  ");
                line.push_str(&what);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ids of the tasks still running, for refusals that wait on them.
pub(super) fn running_ids(rows: &[SavedTask]) -> Vec<String> {
    rows.iter()
        .filter(|row| row.status.is_active())
        .map(|row| row.id.clone())
        .collect()
}

fn format_row(row: &SavedTask, with_result: bool) -> String {
    let mut text = format!("{}: {}", row.id, row.status.as_str());
    if with_result && let Some(report) = &row.result {
        text.push('\n');
        text.push_str(&crate::ui::events::sanitize_output(report));
    }
    text
}

/// Messages for tasks that were running at an earlier call and are not now.
/// `seen_active` carries the ids still known to be running between calls, so a
/// task restored as finished (never seen running) is not announced.
pub(super) fn finished_messages(
    seen_active: &mut std::collections::HashSet<String>,
    rows: &[SavedTask],
) -> Vec<String> {
    let mut messages = Vec::new();
    for row in rows {
        if row.status.is_active() {
            seen_active.insert(row.id.clone());
        } else if seen_active.remove(&row.id) {
            let what = describe(row);
            messages.push(format!(
                "Background task {}{} {}. Send /tasks result {} to read its report.",
                row.id,
                if what.is_empty() {
                    String::new()
                } else {
                    format!(" ({what})")
                },
                row.status.as_str(),
                row.id
            ));
        }
    }
    messages
}

/// Tells the editor when a background task finishes while nobody is looking.
/// Holds only a weak pool handle and polls it every few seconds besides waking
/// on changes, so it ends shortly after the session (and its pool) is gone.
pub(super) async fn watch(
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    pool: std::sync::Weak<crate::extras::subagents::background::Pool>,
) -> Result<(), agent_client_protocol::Error> {
    let mut seen_active = std::collections::HashSet::new();
    loop {
        let Some(pool) = pool.upgrade() else {
            return Ok(());
        };
        for text in finished_messages(&mut seen_active, &pool.list()) {
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            if cx
                .send_notification(SessionNotification::new(
                    session_id.clone(),
                    SessionUpdate::AgentMessageChunk(chunk),
                ))
                .is_err()
            {
                return Ok(());
            }
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), pool.wait_changed()).await;
    }
}

pub(super) async fn handle_command(
    arguments: &str,
    session_id: &SessionId,
    responder: Responder<PromptResponse>,
    cx: ConnectionTo<Client>,
    state: &super::AcpState,
) -> Result<(), agent_client_protocol::Error> {
    let request =
        parse(arguments).map_err(|reason| agent_client_protocol::Error::new(-32602, reason))?;
    let pool = {
        let sessions = state.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown ACP session"))?;
        session.spend.background_pool()
    };
    let request = match (request, pool.as_ref()) {
        (Request::Result(token), Some(pool)) => {
            resolve_id(&pool.list(), &token).map(Request::Result)
        }
        (Request::Cancel(token), Some(pool)) => {
            resolve_id(&pool.list(), &token).map(Request::Cancel)
        }
        (request, _) => Ok(request),
    };
    let text = match (request, pool) {
        (_, None) => "no background tasks in this session".to_owned(),
        (Err(message), Some(_)) => message,
        (Ok(Request::List), Some(pool)) => format_list(&pool.list()),
        (Ok(Request::Result(id)), Some(pool)) => match pool.result(&id) {
            Ok(row) => format_row(&row, true),
            Err(error) => crate::ui::events::sanitize_output(&error.to_string()).to_string(),
        },
        (Ok(Request::Cancel(id)), Some(pool)) => match pool.request_cancel(&id) {
            Ok(row) => format_row(&row, false),
            Err(error) => crate::ui::events::sanitize_output(&error.to_string()).to_string(),
        },
    };
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    ))?;
    responder.respond(PromptResponse::new(StopReason::EndTurn))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::subagents::background::Status;

    fn row(id: &str, status: Status) -> SavedTask {
        SavedTask {
            status,
            result: Some("report".to_owned()),
            ..SavedTask::new(id.to_owned())
        }
    }

    #[test]
    fn parse_accepts_only_the_documented_forms() {
        let id = "0b9f3c3e-5f43-4a79-9d0e-6a3e2e1f4a10";
        assert_eq!(parse(""), Ok(Request::List));
        assert_eq!(
            parse(&format!("result {id}")),
            Ok(Request::Result(id.into()))
        );
        assert_eq!(
            parse(&format!("  cancel   {id} ")),
            Ok(Request::Cancel(id.into()))
        );
        assert!(parse("cancel not-an-id").unwrap_err().contains("invalid"));
        assert_eq!(parse("cancel").unwrap_err(), USAGE);
        assert_eq!(parse("clear").unwrap_err(), USAGE);
    }

    #[test]
    fn finished_messages_announce_only_tasks_seen_running_once() {
        let mut seen = std::collections::HashSet::new();
        // Restored as finished: never announced.
        assert!(finished_messages(&mut seen, &[row("old", Status::Completed)]).is_empty());
        assert!(finished_messages(&mut seen, &[row("a", Status::Running)]).is_empty());
        let done = finished_messages(&mut seen, &[row("a", Status::Failed)]);
        assert_eq!(done.len(), 1);
        assert!(done[0].contains("a failed") && done[0].contains("/tasks result a"));
        // A labelled task is named in its completion notice.
        let mut labelled = row("b", Status::Running);
        labelled.label = Some("explore: map the parser".into());
        assert!(finished_messages(&mut seen, &[labelled.clone()]).is_empty());
        labelled.status = Status::Completed;
        let done = finished_messages(&mut seen, &[labelled]);
        assert!(done[0].contains("(explore: map the parser)"), "{done:?}");
        assert!(finished_messages(&mut seen, &[row("a", Status::Failed)]).is_empty());
    }

    #[test]
    fn list_rows_and_running_ids() {
        assert_eq!(format_list(&[]), "no background tasks in this session");
        let rows = [row("a", Status::Running), row("b", Status::Completed)];
        assert_eq!(format_list(&rows), "a  running\nb  completed");
        assert_eq!(running_ids(&rows), vec!["a".to_owned()]);
        assert_eq!(format_row(&rows[1], true), "b: completed\nreport");
        assert_eq!(format_row(&rows[1], false), "b: completed");
    }

    #[test]
    fn rows_show_the_label_and_persona_and_ids_resolve_by_prefix() {
        let mut labelled = row("0b9f3c3e-5f43-4a79-9d0e-6a3e2e1f4a10", Status::Running);
        labelled.label = Some("explore: map the parser".into());
        labelled.agent_type = Some("explore".into());
        let mut other = row("0b9fffff-5f43-4a79-9d0e-6a3e2e1f4a10", Status::Completed);
        other.agent_type = Some("review".into());
        let rows = [labelled, other];
        let list = format_list(&rows);
        assert!(
            list.lines()
                .next()
                .unwrap()
                .ends_with("running  explore: map the parser"),
            "{list}"
        );
        assert!(
            list.lines()
                .nth(1)
                .unwrap()
                .ends_with("completed  [review]")
        );
        assert_eq!(
            resolve_id(&rows, "0b9f3c3e").unwrap(),
            "0b9f3c3e-5f43-4a79-9d0e-6a3e2e1f4a10"
        );
        let ambiguous = resolve_id(&rows, "0b9f").unwrap_err();
        assert!(ambiguous.contains("0b9f3c3e") && ambiguous.contains("0b9fffff"));
        assert!(
            resolve_id(&rows, "ffff")
                .unwrap_err()
                .contains("no background task")
        );
        assert_eq!(
            resolve_id(&rows, "0b9fffff-5f43-4a79-9d0e-6a3e2e1f4a10").unwrap(),
            rows[1].id
        );
    }
}
