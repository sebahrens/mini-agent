//! Live diagnostics without warming an agent or touching stored history.
use super::SlashCtx;

pub(crate) fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() != 1 {
        super::write_error(ctx.renderer, "usage: /doctor");
        return Ok(());
    }
    let Ok(paths) = crate::paths::process_paths() else {
        super::write_error(ctx.renderer, "application paths are unavailable");
        return Ok(());
    };
    let mut report = crate::diagnostics::DoctorReport::capture(
        ctx.cli,
        ctx.cfg,
        &paths,
        Some(ctx.session),
        Some(ctx.sandbox),
        false,
    );
    #[cfg(feature = "lsp")]
    {
        report.section("LSP cached manager state (no RPC)");
        if let Some(manager) = ctx.agent.as_ref().and_then(|agent| agent.lsp_manager()) {
            let snapshot = manager.diagnostic_snapshot();
            report.row("configured", snapshot.configured_count.to_string());
            for name in snapshot.configured_names {
                report.row("configured server", name);
            }
            report.row(
                "retained live clients",
                snapshot
                    .live_count
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "metadata busy".into()),
            );
            for name in snapshot.live_names.unwrap_or_default() {
                report.row("retained client", name);
            }
            report.row(
                "cached failures",
                snapshot
                    .failure_count
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "metadata busy".into()),
            );
            for failure in snapshot.failures.unwrap_or_default() {
                report.row(
                    "failure",
                    format!(
                        "{}: {} consecutive; retry in {} ms",
                        failure.name, failure.consecutive, failure.retry_in_ms
                    ),
                );
            }
        } else {
            report.row(
                "status",
                "current agent has no captured manager; no server started for diagnostics",
            );
        }
    }
    #[cfg(feature = "mcp")]
    {
        report.section("MCP connection metadata (no liveness RPC)");
        if let Some(manager) = ctx.mcp_manager {
            report.row("connected handles", manager.handles.len().to_string());
            for handle in manager.handles.iter().take(32) {
                report.row(
                    "server",
                    format!(
                        "{}: handle present; liveness not queried",
                        handle.server_name
                    ),
                );
            }
            report.row(
                "pending connection notices",
                manager.notices.len().to_string(),
            );
        } else {
            report.row("status", "not initialized");
        }
    }
    report.section("Terminal attachment");
    let (attached, paste, keyboard) = ctx.terminal_guard.diagnostic_protocols();
    report.row("attached", attached.to_string());
    report.row("bracketed paste enable emitted", paste.to_string());
    report.row("keyboard enhancement enable emitted", keyboard.to_string());
    report.row(
        "true color",
        if std::env::var("COLORTERM")
            .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        {
            "environment advertises true color; support not negotiated"
        } else {
            "support not negotiated"
        },
    );
    report.row(
        "protocol note",
        "successful emission is not terminal support confirmation",
    );
    super::write_result(ctx.renderer, report.render());
    Ok(())
}
