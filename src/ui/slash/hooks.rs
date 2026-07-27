use crate::ui::slash::{SlashCtx, write_ok, write_result};

pub async fn handle(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match crate::extras::hooks::get_dispatcher() {
        None => write_ok(
            ctx.renderer,
            "hooks: no dispatcher installed (--no-hooks, disableAllHooks, or no settings.json hooks found)",
        ),
        Some(dispatcher) => {
            let summary = dispatcher.summary();
            if summary.is_empty() {
                write_ok(ctx.renderer, "hooks: enabled, no hooks configured");
            } else {
                write_ok(ctx.renderer, "hooks: configured events");
                for (event, count) in summary {
                    write_result(
                        ctx.renderer,
                        format!(
                            "  {event}: {count} handler{}",
                            if count == 1 { "" } else { "s" }
                        ),
                    );
                }
            }
        }
    }
    Ok(())
}
