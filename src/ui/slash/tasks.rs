//! Views and cancellation for the current session's bounded Task pool.
use super::{C_ERROR, C_RESULT, SlashCtx};
use crate::extras::subagents::background::TaskDetails;
use crate::ui::{C_TOOL, events::sanitize_output};

/// Lines of the `/tasks <id>` view: the status, then each child's bounded
/// tool log (name, arguments, result size, error).
fn detail_lines(id: &str, status: &str, details: Option<&TaskDetails>) -> Vec<String> {
    let mut lines = vec![format!("{id}: {status}")];
    match details {
        Some(details) if !details.children.is_empty() => lines.extend(details.log_lines()),
        _ => lines.push("no tool log recorded for this task".to_string()),
    }
    lines
}

/// Lines appended to `/tasks result <id>` for raw child text the host
/// replaced: marked unverified and shown to the user only.
fn unverified_lines(details: Option<&TaskDetails>) -> Vec<String> {
    details
        .map(TaskDetails::unverified_lines)
        .unwrap_or_default()
}

pub(super) fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts {
        ["/tasks"] => {
            let rows = ctx.session.background_tasks();
            if rows.is_empty() {
                ctx.renderer
                    .write_line("no background tasks in this session", C_TOOL)?;
            }
            for row in rows {
                ctx.renderer
                    .write_line(&format!("{}  {}", row.id, row.status.as_str()), C_TOOL)?;
            }
            if let Some(pool) = &ctx.session.background_pool {
                for recent in pool.recent_foreground() {
                    let status = if recent.failed {
                        "foreground, failed"
                    } else {
                        "foreground"
                    };
                    ctx.renderer.write_line(
                        &sanitize_output(&format!("{}  {status}  {}", recent.id, recent.label)),
                        C_TOOL,
                    )?;
                }
            }
        }
        ["/tasks", id] if uuid::Uuid::parse_str(id).is_ok() => {
            let recent = ctx
                .session
                .background_pool
                .as_ref()
                .and_then(|pool| pool.recent_foreground().into_iter().find(|r| r.id == *id));
            let lines = if let Some(recent) = recent {
                let status = if recent.failed {
                    "foreground, failed"
                } else {
                    "foreground"
                };
                detail_lines(id, status, Some(&recent.details))
            } else {
                let row = if let Some(pool) = &ctx.session.background_pool {
                    pool.result(id)
                } else {
                    ctx.session
                        .background_tasks()
                        .into_iter()
                        .find(|row| row.id == *id)
                        .ok_or_else(|| anyhow::anyhow!("unknown task in this session"))
                };
                match row {
                    Ok(row) => detail_lines(id, row.status.as_str(), row.details.as_ref()),
                    Err(error) => vec![error.to_string()],
                }
            };
            for line in lines {
                ctx.renderer.write_line(&sanitize_output(&line), C_TOOL)?;
            }
        }
        ["/tasks", action @ ("cancel" | "result"), id] => {
            if uuid::Uuid::parse_str(id).is_err() {
                ctx.renderer.write_line("invalid task id", C_ERROR)?;
                return Ok(());
            }
            if *action == "result"
                && let Some(recent) = ctx
                    .session
                    .background_pool
                    .as_ref()
                    .and_then(|pool| pool.recent_foreground().into_iter().find(|r| r.id == *id))
            {
                ctx.renderer
                    .write_line(&format!("{id}: foreground"), C_TOOL)?;
                for line in unverified_lines(Some(&recent.details)) {
                    ctx.renderer.write_line(&sanitize_output(&line), C_RESULT)?;
                }
                return Ok(());
            }
            let row = if *action == "cancel" {
                ctx.session
                    .background_pool
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("this saved task has no live owner"))
                    .and_then(|pool| pool.request_cancel(id))
            } else if let Some(pool) = &ctx.session.background_pool {
                pool.result(id)
            } else {
                ctx.session
                    .background_tasks()
                    .into_iter()
                    .find(|row| row.id == *id)
                    .ok_or_else(|| anyhow::anyhow!("unknown task in this session"))
            };
            match row {
                Ok(row) => {
                    ctx.renderer
                        .write_line(&format!("{}: {}", row.id, row.status.as_str()), C_TOOL)?;
                    if *action == "result"
                        && let Some(report) = row.result
                    {
                        for line in sanitize_output(&report).lines() {
                            ctx.renderer.write_line(line, C_RESULT)?;
                        }
                    }
                    if *action == "result" {
                        for line in unverified_lines(row.details.as_ref()) {
                            ctx.renderer.write_line(&sanitize_output(&line), C_RESULT)?;
                        }
                    }
                }
                Err(error) => ctx
                    .renderer
                    .write_line(&sanitize_output(&error.to_string()), C_ERROR)?,
            }
        }
        _ => ctx.renderer.write_line(
            "usage: /tasks | /tasks <id> | /tasks result <id> | /tasks cancel <id>",
            C_ERROR,
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::subagents::background::{ChildDetails, ToolLogEntry};

    fn sample() -> TaskDetails {
        let mut read = ToolLogEntry::new("read", "{\"path\":\"src/a.rs\"}");
        read.result_bytes = Some(1234);
        let mut grep = ToolLogEntry::new("grep", "{\"pattern\":\"x\"}")
            .with_error("denied by policy\nsecond line");
        grep.result_bytes = Some(20);
        TaskDetails {
            children: vec![ChildDetails {
                index: 0,
                turns: 3,
                tool_calls: 30,
                log: vec![read, grep],
                unverified: Some("raw\nchild text".into()),
            }],
        }
    }

    #[test]
    fn task_view_lists_tool_calls_with_sizes_errors_and_unlogged_count() {
        let lines = detail_lines("id", "failed", Some(&sample())).join("\n");
        assert!(lines.contains("id: failed"), "{lines}");
        assert!(lines.contains("3 turns, 30 tool calls"), "{lines}");
        assert!(
            lines.contains("read {\"path\":\"src/a.rs\"}  -> 1234 bytes"),
            "{lines}"
        );
        assert!(lines.contains("ERROR: denied by policy"), "{lines}");
        assert!(
            !lines.contains("second line"),
            "error keeps its first line only"
        );
        assert!(lines.contains("28 more tool calls not logged"), "{lines}");
        assert!(detail_lines("id", "running", None)[1].contains("no tool log"));
    }

    #[test]
    fn result_view_marks_raw_child_text_unverified_and_user_only() {
        let lines = unverified_lines(Some(&sample()));
        assert!(lines[0].contains("UNVERIFIED") && lines[0].contains("not sent to the model"));
        assert_eq!(lines[1], "  raw");
        assert!(unverified_lines(None).is_empty());
    }
}
