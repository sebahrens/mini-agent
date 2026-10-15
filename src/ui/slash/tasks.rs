//! Views and cancellation for the current session's bounded Task pool.
use super::{C_ERROR, C_RESULT, SlashCtx};
use crate::ui::{C_TOOL, events::sanitize_output};

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
        }
        ["/tasks", action @ ("cancel" | "result"), id] => {
            if uuid::Uuid::parse_str(id).is_err() {
                ctx.renderer.write_line("invalid task id", C_ERROR)?;
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
                }
                Err(error) => ctx
                    .renderer
                    .write_line(&sanitize_output(&error.to_string()), C_ERROR)?,
            }
        }
        _ => ctx.renderer.write_line(
            "usage: /tasks | /tasks result <id> | /tasks cancel <id>",
            C_ERROR,
        )?,
    }
    Ok(())
}
