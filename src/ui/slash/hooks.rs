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
                            "  {event}: {count} handler{}{}",
                            if count == 1 { "" } else { "s" },
                            event_note(&event)
                        ),
                    );
                }
            }
            for line in support_lines() {
                write_result(ctx.renderer, line);
            }
        }
    }
    Ok(())
}

fn event_note(event: &str) -> &'static str {
    use crate::extras::hooks::settings::SUPPORTED_HOOK_EVENTS;
    if SUPPORTED_HOOK_EVENTS.contains(&event) {
        ""
    } else {
        " (not dispatched by this build; inert)"
    }
}

/// Supported vs unsupported events and handler types, so ported Claude Code
/// hooks are never silently assumed to fire.
pub(crate) fn support_lines() -> Vec<String> {
    use crate::extras::hooks::settings::{
        KNOWN_UNDISPATCHED_HOOK_EVENTS, SUPPORTED_HOOK_EVENTS, SUPPORTED_HOOK_TYPES,
    };
    vec![
        format!("  supported events: {}", SUPPORTED_HOOK_EVENTS.join(", ")),
        format!(
            "  not dispatched (accepted, inert): {}",
            KNOWN_UNDISPATCHED_HOOK_EVENTS.join(", ")
        ),
        format!(
            "  handler types: {} (http {}); prompt, agent and mcp_tool handlers are skipped with a warning",
            SUPPORTED_HOOK_TYPES.join(", "),
            if crate::extras::hooks::http_hooks_enabled() {
                "enabled"
            } else {
                "disabled: set hooks_http_enabled = true"
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn ytyt3_hooks_listing_names_supported_and_unsupported_surface() {
        let lines = super::support_lines().join("\n");
        for event in [
            "PostCompact",
            "StopFailure",
            "InstructionsLoaded",
            "FileChanged",
            "CwdChanged",
        ] {
            assert!(lines.contains(event), "{event}");
        }
        assert!(lines.contains("PermissionDenied"));
        assert!(lines.contains("http"));
        assert_eq!(
            super::event_note("FutureEvent"),
            " (not dispatched by this build; inert)"
        );
        assert_eq!(super::event_note("PostCompact"), "");
    }
}
