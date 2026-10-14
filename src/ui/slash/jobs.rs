//! Operator views and cancellation requests for this session's owned background jobs.
use super::SlashCtx;
use crate::ui::C_TOOL;

fn summary(job: &crate::sandbox::BackgroundJobSummary) -> String {
    let command: String = crate::ui::events::sanitize_output(&job.command)
        .chars()
        .take(512)
        .collect();
    let command = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let exit = job
        .exit_code
        .map(|code| format!(" exit={code}"))
        .unwrap_or_default();
    format!("{}  {}{exit}  {command}", job.id, job.status.as_str())
}

pub(super) fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts {
        ["/jobs"] => {
            let jobs = ctx.sandbox.background_job_summaries();
            if jobs.is_empty() {
                ctx.renderer
                    .write_line("no background jobs in this session", C_TOOL)?;
            } else {
                for job in jobs.into_iter().take(32) {
                    ctx.renderer.write_line(&summary(&job), C_TOOL)?;
                }
            }
        }
        ["/jobs", "stop", id] => match ctx.sandbox.request_stop_background_job(id) {
            Ok(job) => {
                ctx.renderer
                    .write_line(&format!("{}: {}", job.id, job.status.as_str()), C_TOOL)?;
                if !job.status.is_terminal() {
                    ctx.renderer.write_line(
                        "stop requested; the job remains owned until cleanup finishes",
                        C_TOOL,
                    )?;
                }
            }
            Err(error) => ctx
                .renderer
                .write_line(&crate::ui::events::sanitize_output(&error), super::C_ERROR)?,
        },
        _ => {
            ctx.renderer
                .write_line("usage: /jobs | /jobs stop <job-id>", super::C_ERROR)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn summaries_are_bounded_inert_metadata_without_output_contents() {
        let job = crate::sandbox::BackgroundJobSummary {
            id: "job-fixture".into(),
            command: format!("雪\x1b[2J\n{}", "x".repeat(2000)),
            status: crate::sandbox::BackgroundJobStatus::Stopping,
            exit_code: None,
        };
        let text = super::summary(&job);
        assert!(text.starts_with("job-fixture  stopping  雪"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\n'));
        assert!(text.chars().count() < 560);
    }
}
