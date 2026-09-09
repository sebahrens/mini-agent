use crate::agent::tools;
use crate::config::types::EditSystem;
use crate::permission::SecurityMode;
use crate::ui::slash::{SlashCtx, write_error, write_ok, write_result};

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/reasoning" | "/thinking" => handle_reasoning(parts, ctx).await,
        "/mode" => handle_mode(parts, ctx).await,
        "/toggle" => handle_toggle(parts, ctx).await,
        "/editsys" => handle_editsys(parts, ctx).await,
        "/advisor" => {
            #[cfg(feature = "advisor")]
            {
                handle_advisor(parts, ctx).await
            }
            #[cfg(not(feature = "advisor"))]
            {
                write_error(
                    ctx.renderer,
                    "Advisor support not enabled (build with --features advisor)",
                );
                Ok(())
            }
        }
        #[cfg(feature = "mcp")]
        "/mcp" => handle_mcp(parts, ctx).await,
        #[cfg(not(feature = "mcp"))]
        "/mcp" => {
            write_error(
                ctx.renderer,
                "MCP support not enabled (build with --features mcp)",
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn handle_reasoning(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.reasoning_enabled = !*ctx.reasoning_enabled;
    *ctx.show_reasoning = *ctx.reasoning_enabled;
    ctx.rebuild_agent().await;
    write_ok(
        ctx.renderer,
        format!(
            "reasoning: {}",
            if *ctx.reasoning_enabled { "on" } else { "off" }
        ),
    );
    Ok(())
}

#[cfg(feature = "advisor")]
async fn handle_advisor(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::extras::advisor;

    let current = advisor::with_config(|c| c.clone())?;

    if parts.len() < 2 {
        write_ok(ctx.renderer, "advisor:");
        write_result(
            ctx.renderer,
            format!("  enabled: {}", if current.enabled { "yes" } else { "no" }),
        );
        write_result(
            ctx.renderer,
            format!(
                "  mode: {}",
                if current.human_handoff {
                    "human handoff"
                } else {
                    "model"
                }
            ),
        );
        write_result(ctx.renderer, format!("  model: {}", current.advisor_model));
        write_result(
            ctx.renderer,
            format!("  provider: {}", current.advisor_provider),
        );
        write_result(
            ctx.renderer,
            format!(
                "  max uses: {}",
                current
                    .max_uses
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unlimited".to_string())
            ),
        );
        write_result(ctx.renderer, "");
        write_result(
            ctx.renderer,
            format!("  context limit: {} KB", current.kilobytes_limit),
        );
        write_result(ctx.renderer, "");
        write_result(ctx.renderer, "  /advisor on|off");
        write_result(ctx.renderer, "  /advisor handoff [on|off]");
        write_result(ctx.renderer, "  /advisor model <name>");
        write_result(ctx.renderer, "  /advisor max-uses <n>");
        write_result(ctx.renderer, "  /advisor context-limit <kilobytes>");
        return Ok(());
    }

    match parts[1] {
        "on" => {
            let mut cfg = current;
            cfg.enabled = true;
            advisor::init_config(cfg);
            ctx.rebuild_agent().await;
            write_ok(ctx.renderer, "advisor: on");
        }
        "off" => {
            let mut cfg = current;
            cfg.enabled = false;
            advisor::init_config(cfg);
            ctx.rebuild_agent().await;
            write_ok(ctx.renderer, "advisor: off");
        }
        "handoff" => {
            let new_state = match parts.get(2).copied() {
                Some("on") | None => true,
                Some("off") => false,
                Some(other) => {
                    write_error(ctx.renderer, format!("invalid: '{}', use on or off", other));
                    return Ok(());
                }
            };
            let mut cfg = current;
            cfg.human_handoff = new_state;
            // Interactive startup owns a channel independently of the initial mode.
            if new_state && cfg.handoff_tx.is_none() {
                write_error(
                    ctx.renderer,
                    "Human handoff requires an interactive session",
                );
                return Ok(());
            }
            advisor::init_config(cfg);
            ctx.rebuild_agent().await;
            write_ok(
                ctx.renderer,
                format!("advisor handoff: {}", if new_state { "on" } else { "off" }),
            );
        }
        "model" => {
            if let Some(model) = parts.get(2) {
                let mut cfg = current;
                if let Err(error) = cfg.select_model(
                    model,
                    &ctx.session.provider,
                    ctx.client,
                    ctx.cfg,
                    ctx.cli.api_key.as_deref(),
                ) {
                    write_error(
                        ctx.renderer,
                        format!("cannot select advisor model: {error}"),
                    );
                    return Ok(());
                }
                let selected = format!("{}/{}", cfg.advisor_provider, cfg.advisor_model);
                advisor::init_config(cfg);
                ctx.rebuild_agent().await;
                write_ok(ctx.renderer, format!("advisor model: {selected}"));
            } else {
                write_error(ctx.renderer, "usage: /advisor model <name>");
            }
        }
        "max-uses" => {
            if let Some(n) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) {
                let mut cfg = current;
                cfg.max_uses = if n == 0 { None } else { Some(n) };
                let label = cfg
                    .max_uses
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unlimited".to_string());
                advisor::init_config(cfg);
                write_ok(ctx.renderer, format!("advisor max uses: {}", label));
            } else {
                write_error(
                    ctx.renderer,
                    "usage: /advisor max-uses <number|0=unlimited>",
                );
            }
        }
        "context-limit" => {
            if let Some(n) = parts.get(2).and_then(|s| s.parse::<u32>().ok()) {
                let mut cfg = current;
                cfg.kilobytes_limit = n;
                advisor::init_config(cfg);
                write_ok(
                    ctx.renderer,
                    format!(
                        "advisor context limit: {} KB ({} chars head / {} chars tail)",
                        n,
                        n as usize * 1024 / 2,
                        n as usize * 1024 / 2,
                    ),
                );
            } else {
                write_error(ctx.renderer, "usage: /advisor context-limit <kilobytes>");
            }
        }
        _ => {
            write_error(
                ctx.renderer,
                format!(
                    "unknown: '{}' (on|off|handoff|model|max-uses|context-limit)",
                    parts[1]
                ),
            );
        }
    }
    Ok(())
}

async fn handle_mode(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let current_mode = ctx
        .permission
        .as_ref()
        .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()).mode())
        .unwrap_or(SecurityMode::Standard);

    if parts.len() < 2 {
        write_ok(ctx.renderer, "security mode:");
        write_result(ctx.renderer, format!("  current: {}", current_mode));
        write_result(ctx.renderer, "");
        write_result(
            ctx.renderer,
            "  /mode standard      allow within CWD, ask for external",
        );
        write_result(ctx.renderer, "  /mode restrictive   ask for all operations");
        write_result(
            ctx.renderer,
            "  /mode readonly      allow reads, deny everything else",
        );
        write_result(
            ctx.renderer,
            "  /mode guarded       allow reads, ask for everything else",
        );
        write_result(
            ctx.renderer,
            "  /mode yolo          allow all, ask for destructive bash",
        );
        return Ok(());
    }
    match parts[1] {
        "standard" => set_mode(ctx, SecurityMode::Standard, "standard").await,
        "restrictive" => set_mode(ctx, SecurityMode::Restrictive, "restrictive").await,
        "readonly" => set_mode(ctx, SecurityMode::ReadOnly, "readonly").await,
        "guarded" => set_mode(ctx, SecurityMode::Guarded, "guarded").await,
        "yolo" => set_mode(ctx, SecurityMode::Yolo, "yolo").await,
        _ => write_error(ctx.renderer, format!("unknown mode: {}", parts[1])),
    }
    Ok(())
}

async fn set_mode(ctx: &mut SlashCtx<'_>, mode: SecurityMode, label: &str) {
    if let Some(p) = ctx.permission {
        p.lock().unwrap_or_else(|e| e.into_inner()).set_mode(mode);
        write_ok(ctx.renderer, format!("security mode: {}", label));
    } else {
        write_error(ctx.renderer, "permission system not active");
    }
}

async fn handle_toggle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_ok(ctx.renderer, "usage: /toggle <feature> [on|off]");
        write_ok(ctx.renderer, "features:");
        write_result(
            ctx.renderer,
            format!(
                "  todo  {}",
                if *ctx.todo_tools_enabled { "on" } else { "off" }
            ),
        );
        // Not toggleable: the JavaScript worker containment preflight and the
        // learned-skill subsystem it gates are decided at startup. They are
        // listed here because otherwise they fail completely silently — the tool
        // is simply absent and `--learned-skill-stats` keeps answering, so the
        // operator has no way to learn that skills are off or why.
        write_ok(ctx.renderer, "runtime availability (read-only):");
        #[cfg(feature = "skills")]
        let service_failure = ctx.skill_services.disabled_diagnostic(ctx.workspace.root());
        for line in runtime_status_lines(
            &crate::provider::js_runtime_report(),
            #[cfg(feature = "skills")]
            service_failure.as_ref(),
        ) {
            write_result(ctx.renderer, line);
        }
    } else {
        let new_state = match parts.get(2).copied() {
            Some("on") => true,
            Some("off") => false,
            Some(other) => {
                write_error(ctx.renderer, format!("invalid: '{}', use on or off", other));
                return Ok(());
            }
            None => !*ctx.todo_tools_enabled,
        };
        if new_state == *ctx.todo_tools_enabled {
            write_ok(
                ctx.renderer,
                format!(
                    "todo tools already {}",
                    if new_state { "on" } else { "off" }
                ),
            );
        } else {
            *ctx.todo_tools_enabled = new_state;
            ctx.rebuild_agent().await;
            write_ok(
                ctx.renderer,
                format!(
                    "todo tools: {}",
                    if *ctx.todo_tools_enabled { "on" } else { "off" }
                ),
            );
        }
    }
    Ok(())
}

/// Renders the read-only runtime availability lines shown by bare `/toggle`.
///
/// Kept pure and separate from the renderer so the verbatim containment refusal
/// reason is unit-testable without a terminal.
fn runtime_status_lines(
    report: &crate::provider::JsRuntimeReport,
    #[cfg(feature = "skills")] service_failure: Option<
        &crate::extras::js::skills::session::SkillServiceFailure,
    >,
) -> Vec<String> {
    let lines = vec![
        format!("  {:<8}{}", "js", availability_label(&report.javascript)),
        format!(
            "  {:<8}{}",
            "skills",
            availability_label(&report.learned_skills)
        ),
    ];
    #[cfg(feature = "skills")]
    let lines = {
        let mut lines = lines;
        if let Some(failure) = service_failure {
            lines.push(crate::ui::events::sanitize_output(&format!("  {failure}")).to_string());
        }
        lines
    };
    lines
}

/// `on` / `off (<verbatim reason>)` / `not compiled`, matching the `on`/`off`
/// vocabulary the rest of `/toggle` uses.
fn availability_label(state: &crate::provider::RuntimeAvailability) -> String {
    match state {
        crate::provider::RuntimeAvailability::Available => "on".to_string(),
        crate::provider::RuntimeAvailability::Unavailable { reason } => format!("off ({reason})"),
        crate::provider::RuntimeAvailability::NotCompiled => "not compiled".to_string(),
    }
}

async fn handle_editsys(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let current = tools::edit_system();
    if parts.len() < 2 {
        write_ok(ctx.renderer, format!("edit system: {}", current));
        write_result(
            ctx.renderer,
            "  /editsys similarity   SEARCH/REPLACE with fuzzy matching",
        );
        write_result(
            ctx.renderer,
            "  /editsys hashedit     tag-based (CRC-32 line hashes)",
        );
        return Ok(());
    }
    match parts[1] {
        "similarity" => {
            tools::set_edit_system(EditSystem::Similarity);
            ctx.rebuild_agent().await;
            write_ok(ctx.renderer, "edit system: similarity (SEARCH/REPLACE)");
        }
        "hashedit" => {
            tools::set_edit_system(EditSystem::Hashedit);
            ctx.rebuild_agent().await;
            write_ok(ctx.renderer, "edit system: hashedit (tag-based)");
        }
        _ => write_error(
            ctx.renderer,
            format!("unknown: '{}' (similarity|hashedit)", parts[1]),
        ),
    }
    Ok(())
}

/// Prefix understood by the caller in `ui::mod` to start the interactive OAuth
/// login for a server. The login (browser wait) runs there as a background task
/// so the TUI stays responsive; on success the server is reconnected.
#[cfg(feature = "mcp")]
pub(crate) const DEFER_MCP_LOGIN: &str = "DEFER_MCP_LOGIN:";

#[cfg(feature = "mcp")]
async fn handle_mcp(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() >= 2 && parts[1] == "login" {
        return handle_mcp_login(parts.get(2).map(|s| s.trim()), ctx).await;
    }
    if parts.len() >= 2 && parts[1] == "logout" {
        return handle_mcp_logout(parts.get(2).map(|s| s.trim()), ctx);
    }

    let Some(mgr) = ctx.mcp_manager else {
        write_ok(ctx.renderer, "no MCP servers configured");
        return Ok(());
    };

    // `/mcp <server>` — list tools for one server.
    if parts.len() > 1 {
        let name = parts[1].trim();
        if let Some(handle) = mgr.handles.iter().find(|h| h.server_name == name) {
            match handle.list_tools().await {
                Ok(tools) => {
                    if tools.is_empty() {
                        write_ok(ctx.renderer, format!("server '{}' has no tools", name));
                    } else {
                        write_ok(ctx.renderer, format!("tools on '{}':", name));
                        for tool in &tools {
                            let desc = tool.description.as_deref().unwrap_or("");
                            write_result(ctx.renderer, format!("  {}  {}", tool.name, desc));
                        }
                    }
                }
                Err(e) => {
                    write_error(
                        ctx.renderer,
                        format!("error listing tools on '{}': {}", name, e),
                    );
                }
            }
        } else if is_oauth_server(ctx, name) {
            write_error(
                ctx.renderer,
                format!("server '{name}' is not connected (run /mcp login {name})"),
            );
        } else if server_is_configured(ctx, name) {
            write_error(ctx.renderer, format!("server '{name}' is not connected"));
        } else {
            write_error(ctx.renderer, format!("unknown MCP server: '{name}'"));
        }
        return Ok(());
    }

    // `/mcp` — list all configured servers, green = connected, red = not.
    let Some(servers) = ctx.cfg.mcp_servers.as_ref().filter(|s| !s.is_empty()) else {
        write_ok(ctx.renderer, "no MCP servers configured");
        return Ok(());
    };
    write_ok(ctx.renderer, "MCP servers:");
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    for name in names {
        if let Some(handle) = mgr
            .handles
            .iter()
            .find(|h| h.server_name.as_str() == name.as_str())
        {
            let label = match handle.list_tools().await {
                Ok(tools) => format!("  + {name} ({} tools)", tools.len()),
                Err(_) => format!("  + {name} (connected)"),
            };
            ctx.renderer
                .write_line(&label, crossterm::style::Color::Green)?;
        } else {
            let oauth = matches!(
                servers.get(name),
                Some(crate::extras::mcp::config::McpServerConfig::Url { oauth: Some(o), .. })
                    if o.settings().is_some()
            );
            let label = if oauth {
                format!("  - {name} (unauthenticated, run /mcp login {name})")
            } else {
                format!("  - {name} (not connected)")
            };
            ctx.renderer
                .write_line(&label, crossterm::style::Color::Red)?;
        }
    }
    Ok(())
}

/// True if a server name exists in config.
#[cfg(feature = "mcp")]
fn server_is_configured(ctx: &SlashCtx<'_>, name: &str) -> bool {
    ctx.cfg
        .mcp_servers
        .as_ref()
        .is_some_and(|m| m.contains_key(name))
}

/// True if a configured server is a URL server with OAuth enabled.
#[cfg(feature = "mcp")]
fn is_oauth_server(ctx: &SlashCtx<'_>, name: &str) -> bool {
    use crate::extras::mcp::config::McpServerConfig;
    matches!(
        ctx.cfg.mcp_servers.as_ref().and_then(|m| m.get(name)),
        Some(McpServerConfig::Url { oauth: Some(o), .. }) if o.settings().is_some()
    )
}

/// Resolve a URL-based server's OAuth settings + url from config, or report why not.
#[cfg(feature = "mcp")]
fn resolve_oauth_server(
    ctx: &SlashCtx<'_>,
    name: &str,
) -> Result<(String, crate::extras::mcp::config::OAuthSettings), String> {
    use crate::extras::mcp::config::McpServerConfig;
    let servers = ctx
        .cfg
        .mcp_servers
        .as_ref()
        .ok_or_else(|| "no MCP servers configured".to_string())?;
    let server = servers
        .get(name)
        .ok_or_else(|| format!("unknown MCP server: '{name}'"))?;
    match server {
        McpServerConfig::Url { url, oauth, .. } => {
            let settings = oauth
                .as_ref()
                .and_then(|o| o.settings())
                .ok_or_else(|| format!("server '{name}' does not have OAuth enabled"))?;
            Ok((url.clone(), settings))
        }
        McpServerConfig::Command { .. } => Err(format!(
            "server '{name}' is command-based; OAuth applies to URL servers"
        )),
        McpServerConfig::BuiltIn { .. } => Err(format!(
            "server '{name}' is a built-in server without OAuth"
        )),
    }
}

#[cfg(feature = "mcp")]
async fn handle_mcp_login(name: Option<&str>, ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        write_error(ctx.renderer, "usage: /mcp login <server>");
        return Ok(());
    };
    // Validate config here so errors get friendly messages. The actual login
    // (network discovery + browser wait) runs in the event loop, which spawns
    // the wait as a background task so the TUI never freezes.
    if let Err(e) = resolve_oauth_server(ctx, name) {
        write_error(ctx.renderer, e);
        return Ok(());
    }
    Err(anyhow::anyhow!("{}{}", DEFER_MCP_LOGIN, name))
}

#[cfg(feature = "mcp")]
fn handle_mcp_logout(name: Option<&str>, ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        write_error(ctx.renderer, "usage: /mcp logout <server>");
        return Ok(());
    };
    let (url, settings) = match resolve_oauth_server(ctx, name) {
        Ok(resolved) => resolved,
        Err(error) => {
            write_error(ctx.renderer, error);
            return Ok(());
        }
    };
    match crate::extras::mcp::oauth::logout(name, &url, &settings) {
        Ok(true) => write_ok(
            ctx.renderer,
            format!("removed stored OAuth token for '{name}' (effective next start)"),
        ),
        Ok(false) => write_ok(ctx.renderer, format!("no stored OAuth token for '{name}'")),
        Err(e) => write_error(ctx.renderer, format!("logout failed: {e}")),
    }
    Ok(())
}

#[cfg(test)]
mod runtime_status_tests {
    use super::runtime_status_lines;
    use crate::provider::{JsRuntimeReport, RuntimeAvailability};

    #[test]
    fn unavailable_runtime_reports_the_containment_reason_verbatim() {
        let reason = "macOS major version 15 is not a validated containment host";
        let report = JsRuntimeReport {
            javascript: RuntimeAvailability::Unavailable {
                reason: reason.to_string(),
            },
            learned_skills: RuntimeAvailability::Unavailable {
                reason: format!("requires the contained JavaScript worker: {reason}"),
            },
        };

        let lines = runtime_status_lines(
            &report,
            #[cfg(feature = "skills")]
            None,
        );

        assert_eq!(lines.len(), 2);
        let js = &lines[0];
        assert!(js.starts_with("  js"), "{js}");
        assert!(js.contains("off"), "{js}");
        assert!(js.contains(reason), "{js}");

        let skills = &lines[1];
        assert!(skills.starts_with("  skills"), "{skills}");
        assert!(skills.contains("off"), "{skills}");
        assert!(skills.contains(reason), "{skills}");
    }

    #[test]
    fn available_runtime_reports_on_without_a_reason() {
        let report = JsRuntimeReport {
            javascript: RuntimeAvailability::Available,
            learned_skills: RuntimeAvailability::Available,
        };

        let lines = runtime_status_lines(
            &report,
            #[cfg(feature = "skills")]
            None,
        );

        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(line.trim_end().ends_with("on"), "{line}");
            assert!(!line.contains('('), "{line}");
        }
    }

    #[test]
    fn not_compiled_runtime_is_distinct_from_a_contained_failure() {
        let report = JsRuntimeReport {
            javascript: RuntimeAvailability::Available,
            learned_skills: RuntimeAvailability::NotCompiled,
        };

        let lines = runtime_status_lines(
            &report,
            #[cfg(feature = "skills")]
            None,
        );

        assert!(lines[1].contains("not compiled"), "{}", lines[1]);
        assert!(!lines[1].contains("off"), "{}", lines[1]);
    }

    #[cfg(feature = "skills")]
    #[test]
    fn service_health_reports_partial_failure_and_retry_state_separately() {
        use crate::extras::js::skills::session::SkillServiceFailure;

        let report = JsRuntimeReport {
            javascript: RuntimeAvailability::Available,
            learned_skills: RuntimeAvailability::Available,
        };
        let healthy = runtime_status_lines(&report, None);
        for (degraded, exhausted, state) in [
            (false, false, "unavailable"),
            (false, true, "disabled"),
            (true, false, "degraded"),
            (true, true, "degraded"),
        ] {
            let failure = SkillServiceFailure {
                workspace_root: "workspace-a".into(),
                attempts: if exhausted { 4 } else { 1 },
                exhausted,
                degraded,
                reason: "\u{1b}[31mstore busy\u{1b}[0m".to_string(),
            };
            let lines = runtime_status_lines(&report, Some(&failure));
            assert_eq!(&lines[..2], healthy.as_slice());
            let retry = if exhausted {
                "retry budget exhausted after 4 initialization attempts"
            } else {
                "attempt 1 of 4, retrying"
            };
            assert_eq!(
                lines[2],
                format!("  learned skills are {state} for workspace-a ({retry}): store busy")
            );
            assert_eq!(lines.len(), 3);
        }
    }
}
