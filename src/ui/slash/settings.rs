use crate::agent::tools;
use crate::config::types::EditSystem;
use crate::permission::SecurityMode;
use crate::ui::slash::write_usage;
use crate::ui::slash::{SlashCtx, write_error, write_ok, write_result};

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/keys" => {
            if parts.len() != 1 {
                write_usage(ctx.renderer, "usage: /keys");
                return Ok(());
            }
            write_ok(
                ctx.renderer,
                "effective keyboard bindings (editor/App; modal choices retain raw keys):",
            );
            for line in ctx.input.keybindings.describe() {
                write_result(ctx.renderer, line);
            }
            Ok(())
        }
        "/top" | "/bottom" => {
            if parts.len() != 1 {
                write_error(ctx.renderer, format!("usage: {}", parts[0]));
                return Ok(());
            }
            // Key-independent fallback for terminals without Home/End or
            // PageUp/PageDown; scrolls without adding transcript output.
            if parts[0] == "/top" {
                ctx.renderer.scroll_to_top();
            } else {
                ctx.renderer.scroll_to_bottom()?;
            }
            Ok(())
        }
        "/vim" => {
            let enabled = match parts {
                ["/vim"] => !ctx.input.vim_enabled(),
                ["/vim", "on"] => true,
                ["/vim", "off"] => false,
                _ => {
                    write_usage(ctx.renderer, "usage: /vim [on|off]");
                    return Ok(());
                }
            };
            ctx.input.set_vim_enabled(enabled);
            write_ok(
                ctx.renderer,
                format!(
                    "vim editing: {} (this UI session)",
                    if enabled { "on" } else { "off" }
                ),
            );
            Ok(())
        }
        "/cost" => {
            if parts.len() != 1 {
                write_usage(ctx.renderer, "usage: /cost");
                return Ok(());
            }
            let result = match crate::paths::process_paths_ref() {
                Ok(paths) => {
                    crate::spend::cost_lines(
                        ctx.session,
                        crate::spend::Ledger::new(&paths.state_dir),
                    )
                    .await
                }
                Err(error) => Err(error.into()),
            };
            match result {
                Ok(lines) => {
                    for line in lines {
                        write_result(ctx.renderer, line);
                    }
                    if let Some(tier) =
                        crate::spend::configured_service_tier(ctx.cfg, &ctx.session.model)
                    {
                        write_result(ctx.renderer, crate::spend::service_tier_note(&tier));
                    }
                }
                Err(_) => write_error(
                    ctx.renderer,
                    "Cost tracking unavailable: private ledger could not be safely read",
                ),
            }
            Ok(())
        }
        "/reasoning" | "/thinking" => handle_reasoning(parts, ctx).await,
        "/mode" => handle_mode(parts, ctx).await,
        "/permissions" => handle_permissions(parts, ctx).await,
        "/sandbox" => handle_sandbox(parts, ctx).await,
        "/add-dir" => handle_add_dir(parts, ctx),
        "/toggle" => handle_toggle(parts, ctx).await,
        "/editsys" => handle_editsys(parts, ctx).await,
        "/advisor" => {
            #[cfg(feature = "advisor")]
            {
                handle_advisor(parts, ctx).await
            }
            #[cfg(not(feature = "advisor"))]
            {
                write_error(ctx.renderer, super::feature_missing("/advisor", "advisor"));
                Ok(())
            }
        }
        #[cfg(feature = "mcp")]
        "/mcp" => handle_mcp(parts, ctx).await,
        #[cfg(not(feature = "mcp"))]
        "/mcp" => {
            write_error(ctx.renderer, super::feature_missing("/mcp", "mcp"));
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The reasoning state `/reasoning [on|off]` asks for: explicit on/off, or a
/// toggle without an argument.
pub(crate) fn reasoning_target(arg: Option<&str>, current: bool) -> Result<bool, String> {
    match arg.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(!current),
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!("invalid: '{other}', use /reasoning [on|off]")),
    }
}

async fn handle_reasoning(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let target = match reasoning_target(parts.get(1).copied(), *ctx.reasoning_enabled) {
        Ok(target) => target,
        Err(error) => {
            write_error(ctx.renderer, error);
            return Ok(());
        }
    };
    if target == *ctx.reasoning_enabled {
        write_ok(
            ctx.renderer,
            format!("reasoning already {}", if target { "on" } else { "off" }),
        );
        return Ok(());
    }
    *ctx.reasoning_enabled = target;
    *ctx.show_reasoning =
        ctx.renderer.feed().reasoning_display() != crate::config::ReasoningDisplay::Off;
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
                    &ctx.cli.scoped_api_key(ctx.cfg),
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
                write_usage(ctx.renderer, "usage: /advisor model <name>");
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
                write_usage(
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
                write_usage(ctx.renderer, "usage: /advisor context-limit <kilobytes>");
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

/// `/add-dir DIR`: let the file tools reach DIR for this session (an allowed
/// external directory; read/edit rules, protected paths and denies still
/// apply). The shell sandbox is built at startup, so its write roots come
/// from `--add-workspace-dir`, not from this command (mini-agent-nvnkx.71).
fn handle_add_dir(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_usage(ctx.renderer, "usage: /add-dir <directory>");
        return Ok(());
    }
    let Some(permission) = ctx.permission.as_ref() else {
        write_error(ctx.renderer, "permission checks are off; nothing to grant");
        return Ok(());
    };
    let raw = parts[1..].join(" ");
    let expanded = std::path::PathBuf::from(crate::fs::expand_tilde(&raw));
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        ctx.workspace.root().join(expanded)
    };
    let dir = match absolute.canonicalize() {
        Ok(dir) if dir.is_dir() => dir,
        _ => {
            write_error(ctx.renderer, format!("{raw}: not an existing directory"));
            return Ok(());
        }
    };
    if dir.parent().is_none() {
        write_error(ctx.renderer, "refusing to grant the filesystem root");
        return Ok(());
    }
    if let Some(message) = crate::config::managed::active()
        .and_then(|policy| policy.sandbox_root_violation("/add-dir", &dir))
    {
        write_error(ctx.renderer, message);
        return Ok(());
    }
    permission
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .allow_external_directory(&dir);
    write_ok(
        ctx.renderer,
        format!(
            "file tools may now reach {} for this session (read/edit rules and denies still apply). Shell commands keep their sandbox roots; start with --add-workspace-dir to add a shell write root",
            dir.display()
        ),
    );
    Ok(())
}

async fn handle_sandbox(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::sandbox::SandboxProfile;
    if parts.len() == 1 {
        write_ok(ctx.renderer, ctx.sandbox.status_line());
        write_result(ctx.renderer, shell_network_line(ctx.sandbox));
        write_result(ctx.renderer, shell_auto_allow_line(ctx));
        for line in network_policy_lines(ctx.sandbox) {
            write_result(ctx.renderer, line);
        }
        write_result(
            ctx.renderer,
            format!(
                "  shell environment: {}",
                ctx.sandbox.shell_env_policy().summary()
            ),
        );
        for profile in [
            SandboxProfile::Workspace,
            SandboxProfile::HostRead,
            SandboxProfile::HostFull,
        ] {
            write_result(
                ctx.renderer,
                format!("  /sandbox {profile}: {}", profile.description()),
            );
        }
        write_result(ctx.renderer, format!("  {SANDBOX_NETWORK_USAGE_LINE}"));
        for line in recent_refusal_lines(ctx.sandbox) {
            write_result(ctx.renderer, line);
        }
        return Ok(());
    }
    if parts[1] == "network" {
        return handle_sandbox_network(&parts[2..], ctx).await;
    }
    if parts.len() != 2 {
        write_usage(
            ctx.renderer,
            "usage: /sandbox [workspace|host-read|host-full|network <deny|allowlist|allow>]",
        );
        return Ok(());
    }
    let Ok(profile) = <SandboxProfile as clap::ValueEnum>::from_str(parts[1], false) else {
        write_error(ctx.renderer, "expected workspace, host-read or host-full");
        return Ok(());
    };
    if let Some(refusal) = profile_switch_refusal(ctx.sandbox) {
        write_error(ctx.renderer, refusal);
        return Ok(());
    }
    #[cfg(feature = "subagents")]
    let background_active = ctx.session.background_task_counts().0 != 0;
    #[cfg(not(feature = "subagents"))]
    let background_active = false;
    if *ctx.is_running || background_active {
        write_error(
            ctx.renderer,
            "finish or cancel active turns/tasks before changing the sandbox profile",
        );
        return Ok(());
    }
    if let Err(error) = ctx.sandbox.set_profile(profile) {
        write_error(ctx.renderer, error);
        return Ok(());
    }
    ctx.rebuild_agent().await;
    write_ok(
        ctx.renderer,
        format!(
            "sandbox profile: {profile} (this session; permission mode and network policy unchanged)"
        ),
    );
    Ok(())
}

/// `/sandbox` line for `shell-auto-allow-sandboxed`: whether unmatched shell
/// commands currently run without a prompt because the sandbox contains them.
fn shell_auto_allow_line(ctx: &SlashCtx<'_>) -> String {
    let active = ctx.permission.as_ref().is_some_and(|permission| {
        permission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sandbox_auto_allow()
    });
    let configured = ctx.cfg.shell_auto_allow_sandboxed.unwrap_or(false);
    let detail = if active {
        "on: unmatched shell commands in standard mode run without a prompt while the sandbox contains them; deny/ask rules, destructive commands and escalation still prompt"
    } else if configured {
        "configured but inactive: needs an enforced sandbox with the workspace or host-read profile and network deny or allowlist"
    } else {
        "off (shell-auto-allow-sandboxed = false)"
    };
    format!("  shell auto-allow: {detail}")
}

/// `/sandbox` listing line for the network subcommand.
const SANDBOX_NETWORK_USAGE_LINE: &str = "/sandbox network <deny|allowlist|allow>: shell network policy for this session (allowlist asks per host; never saved; bounded by a managed ceiling)";

/// `/sandbox network [deny|allowlist|allow]`: switch the shell network policy
/// of an idle session (mini-agent-nvnkx.70).
async fn handle_sandbox_network(args: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use crate::sandbox::SandboxNetwork;
    let [mode] = args else {
        write_usage(
            ctx.renderer,
            "usage: /sandbox network <deny|allowlist|allow>",
        );
        return Ok(());
    };
    let Ok(network) = <SandboxNetwork as clap::ValueEnum>::from_str(mode, false) else {
        write_error(ctx.renderer, "expected deny, allowlist or allow");
        return Ok(());
    };
    if let Some(refusal) = profile_switch_refusal(ctx.sandbox) {
        write_error(
            ctx.renderer,
            refusal.replace(
                "the profile has no effect",
                "the network policy has no effect",
            ),
        );
        return Ok(());
    }
    #[cfg(feature = "subagents")]
    let background_active = ctx.session.background_task_counts().0 != 0;
    #[cfg(not(feature = "subagents"))]
    let background_active = false;
    if *ctx.is_running || background_active {
        write_error(
            ctx.renderer,
            "finish or cancel active turns/tasks before changing the sandbox network",
        );
        return Ok(());
    }
    if let Err(error) = ctx.sandbox.set_network(network) {
        write_error(ctx.renderer, error);
        return Ok(());
    }
    ctx.rebuild_agent().await;
    write_ok(
        ctx.renderer,
        format!("sandbox network: {network} (this session only; not saved to configuration)"),
    );
    write_result(ctx.renderer, shell_network_line(ctx.sandbox));
    for line in network_policy_lines(ctx.sandbox) {
        write_result(ctx.renderer, line);
    }
    Ok(())
}

/// Detail lines under the network line: preset expansions, approved project
/// hosts, intranet exceptions and the upstream proxy.
fn network_policy_lines(sandbox: &crate::sandbox::Sandbox) -> Vec<String> {
    let mut lines = Vec::new();
    if !sandbox.network().is_allowlist() {
        return lines;
    }
    for line in crate::sandbox::net_proxy::describe_presets(sandbox.network_hosts()) {
        lines.push(format!("    preset {line}"));
    }
    let project = sandbox.project_network_hosts();
    if !project.is_empty() {
        lines.push(format!(
            "    approved for this project: {}",
            project.join(", ")
        ));
    }
    if !sandbox.network_private_hosts().is_empty() {
        lines.push(format!(
            "    may resolve to private addresses: {}",
            sandbox.network_private_hosts().join(", ")
        ));
    }
    if let Some(upstream) = sandbox.network_upstream_display() {
        lines.push(format!("    upstream proxy: {upstream}"));
    }
    lines
}

/// Recent hosts the allowlist proxy refused, newest last, for `/sandbox`.
fn recent_refusal_lines(sandbox: &crate::sandbox::Sandbox) -> Vec<String> {
    let refusals = sandbox.recent_network_refusals();
    if refusals.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["  recently blocked by the network allowlist:".to_string()];
    for refusal in refusals.iter().rev().take(8) {
        let times = if refusal.count > 1 {
            format!(" x{}", refusal.count)
        } else {
            String::new()
        };
        lines.push(format!(
            "    {}{times} ({}, {}s ago)",
            crate::ui::events::sanitize_output(&refusal.host_port),
            refusal.reason.describe(),
            refusal.at.elapsed().as_secs()
        ));
    }
    lines
}

/// A profile switch changes nothing while the sandbox is off (or fell back to
/// unsandboxed), so it is refused rather than reported as applied.
fn profile_switch_refusal(sandbox: &crate::sandbox::Sandbox) -> Option<&'static str> {
    (!sandbox.containment_requested()).then_some("sandbox is off; the profile has no effect")
}

/// `/sandbox` line describing the `sandbox-network` setting and whether it
/// takes effect with the active backend.
fn shell_network_line(sandbox: &crate::sandbox::Sandbox) -> String {
    let network = sandbox.network();
    let effect = if !sandbox.containment_requested() {
        "sandbox disabled: shells use the host network"
    } else if sandbox.shell_network_effective() {
        "shell commands may reach the network; hooks and JS spawn stay denied"
    } else if sandbox.shell_network_proxied() {
        "shell commands reach only sandbox-network-hosts and prompted hosts, through a local proxy"
    } else if network.is_allowlist() || network.allows() {
        "not supported by this backend; shell network stays denied"
    } else {
        "shell commands have no network; /sandbox network allowlist|allow opens it for this session, or set sandbox-network in config"
    };
    let hosts = if network.is_allowlist() {
        format!(" [{}]", sandbox.network_hosts().join(", "))
    } else {
        String::new()
    };
    format!("  shell network: {network}{hosts} ({effect})")
}

async fn handle_mode(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let current_mode = ctx
        .permission
        .as_ref()
        .map(|p| p.lock().unwrap_or_else(|e| e.into_inner()).mode())
        .unwrap_or(SecurityMode::Standard);

    // The interactive picker (see `ui::pickers::handlers`) handles a bare
    // `/mode` typed into the TUI; this listing is the fallback wherever no
    // picker is available.
    if parts.len() < 2 {
        write_ok(ctx.renderer, "security mode:");
        for line in mode_listing(current_mode) {
            write_result(ctx.renderer, line);
        }
        return Ok(());
    }
    match parse_mode_arg(parts[1]) {
        Some(mode) => set_mode(ctx, mode).await,
        None => write_error(
            ctx.renderer,
            format!(
                "unknown mode: {} (expected one of {})",
                parts[1],
                SecurityMode::NAMES.join(", ")
            ),
        ),
    }
    Ok(())
}

/// Present the permission policies using approval-oriented names while keeping
/// `/mode` as the complete low-level security-mode interface.
/// Shared by `/permissions` and `/mode` when no permission checker exists.
const PERMISSION_SYSTEM_DISABLED: &str =
    "permission system disabled at startup; restart without --dangerously-skip-permissions";

async fn handle_permissions(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        // One list for this picker and for Tab/slash completion.
        if let Some((items, prefix)) =
            crate::ui::pickers::handlers::argument_picker_spec("/permissions")
        {
            ctx.input
                .start_argument_picker(prefix, items.iter().copied());
        }
        return Ok(());
    }

    if parts[1] == "session" {
        for line in session_grant_lines(&ctx.session.permission_allowlist) {
            write_result(ctx.renderer, crate::ui::events::sanitize_output(&line));
        }
        return Ok(());
    }
    if parts[1] == "revoke" {
        return revoke_session_grants(ctx, parts.get(2).copied());
    }
    if matches!(parts[1], "rules" | "forget") {
        let Ok(paths) = crate::paths::process_paths_ref() else {
            write_error(ctx.renderer, "application paths unavailable");
            return Ok(());
        };
        let root = ctx.workspace.root();
        let before =
            crate::permission::project_approvals::load(&paths.state_dir, root).unwrap_or_default();
        let mut lines = project_approval_lines(&paths.state_dir, root, &parts[1..]);
        if parts[1] == "forget" {
            // The running session drops what the store dropped.
            let after = crate::permission::project_approvals::load(&paths.state_dir, root)
                .unwrap_or_default();
            let removed: Vec<(String, String)> = before
                .into_iter()
                .filter(|rule| !after.contains(rule))
                .map(|rule| (rule.tool, rule.pattern))
                .collect();
            if !removed.is_empty() {
                apply_forgotten_approvals(ctx, &removed);
            }
        } else if let Some(permission) = ctx.permission {
            let checker = permission.lock().unwrap_or_else(|error| error.into_inner());
            let configured = checker.effective_rule_lines();
            if !configured.is_empty() {
                lines.push("  configured rules and built-ins (deny wins):".into());
                lines.extend(configured.into_iter().map(|line| format!("    {line}")));
            }
        }
        for line in lines {
            write_result(ctx.renderer, crate::ui::events::sanitize_output(&line));
        }
        return Ok(());
    }
    match parse_permission_mode(parts[1]) {
        Some((mode, label)) => set_permission_mode(ctx, mode, label),
        None => write_error(
            ctx.renderer,
            format!(
                "unknown permissions argument: {} (use yolo, auto, manual, rules, forget, session, or revoke)",
                parts[1]
            ),
        ),
    }
    Ok(())
}

/// Plain-words scope of one session grant entry.
pub(crate) fn session_grant_scope(entry: &crate::session::PermissionAllowEntry) -> String {
    let (tool, pattern) = (entry.tool.as_str(), entry.pattern.as_str());
    match tool {
        crate::permission::checker::SESSION_SHELL_PREFIX_TOOL => {
            format!("shell commands starting with \"{pattern}\"")
        }
        "shell" | "bash" => format!(
            "shell: {}",
            crate::permission::pattern::describe_allow_pattern(pattern)
        ),
        _ => crate::ui::permission_handler::grant_scope(tool, "", pattern),
    }
}

/// Numbered `/permissions session` listing.
pub(crate) fn session_grant_lines(entries: &[crate::session::PermissionAllowEntry]) -> Vec<String> {
    if entries.is_empty() {
        return vec![
            "  no grants in this session (they are made with (a), (x), (f), (w) at prompts)".into(),
        ];
    }
    let mut lines = vec!["  grants active in this session (restored on --resume):".to_string()];
    lines.extend(entries.iter().enumerate().map(|(index, entry)| {
        format!(
            "  {}. {}",
            index + 1,
            session_grant_scope(entry).replace(['\r', '\n'], " ")
        )
    }));
    lines.push("  remove with /permissions revoke <n|all>".into());
    lines
}

/// Remove entry `which` (1-based, or every entry with `all`) from `entries`.
pub(crate) fn take_session_grants(
    entries: &mut Vec<crate::session::PermissionAllowEntry>,
    which: Option<&str>,
) -> Result<Vec<crate::session::PermissionAllowEntry>, String> {
    match which {
        Some("all") => Ok(std::mem::take(entries)),
        Some(number) => match number.parse::<usize>() {
            Ok(index) if (1..=entries.len()).contains(&index) => {
                Ok(vec![entries.remove(index - 1)])
            }
            _ => Err(format!(
                "no session grant #{number}; see /permissions session"
            )),
        },
        None => Err("usage: /permissions revoke <n|all>".into()),
    }
}

/// Entries as the checker's `(tool, pattern)` pairs.
fn grant_pairs(entries: &[crate::session::PermissionAllowEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|entry| (entry.tool.to_string(), entry.pattern.to_string()))
        .collect()
}

/// `/permissions revoke <n|all>`: drop session grants, rebuild the checker's
/// lists from what is left and save the session.
fn revoke_session_grants(ctx: &mut SlashCtx<'_>, which: Option<&str>) -> anyhow::Result<()> {
    let Some(permission) = ctx.permission else {
        write_error(ctx.renderer, PERMISSION_SYSTEM_DISABLED);
        return Ok(());
    };
    let removed = match take_session_grants(&mut ctx.session.permission_allowlist, which) {
        Ok(removed) => removed,
        Err(message) => {
            write_error(ctx.renderer, message);
            return Ok(());
        }
    };
    permission
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .replace_session_allowlist(&grant_pairs(&ctx.session.permission_allowlist));
    if !*ctx.is_running && !ctx.cli.no_session {
        let renderer = &mut *ctx.renderer;
        if let Err(error) =
            crate::session::storage::save_session_recovering(ctx.session, |notice| {
                write_error(renderer, format!("warning: {notice}"));
            })
        {
            write_error(ctx.renderer, format!("failed to save session: {error}"));
        }
    }
    write_ok(
        ctx.renderer,
        format!("revoked {} session grant(s)", removed.len()),
    );
    Ok(())
}

/// Drop forgotten project approvals from the running checker, from the
/// session's own grant copies and from the allowlist proxy's stored hosts.
fn apply_forgotten_approvals(ctx: &mut SlashCtx<'_>, removed: &[(String, String)]) {
    ctx.session
        .permission_allowlist
        .retain(|entry| !removed.contains(&(entry.tool.to_string(), entry.pattern.to_string())));
    if let Some(permission) = ctx.permission {
        permission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .forget_project_allowlist(removed, &grant_pairs(&ctx.session.permission_allowlist));
    }
    let mut hosts = ctx.sandbox.project_network_hosts();
    hosts.retain(|host| {
        !removed.contains(&(
            crate::permission::project_approvals::NETWORK_TOOL.to_string(),
            host.clone(),
        ))
    });
    ctx.sandbox.set_project_network_hosts(hosts);
}

/// `/permissions rules` lists this project's persisted `(p)` approvals;
/// `/permissions forget <n|all>` removes them (effective from the next
/// interactive start; this session keeps its already-granted entries).
pub(crate) fn project_approval_lines(
    state_dir: &std::path::Path,
    root: &std::path::Path,
    arguments: &[&str],
) -> Vec<String> {
    use crate::permission::project_approvals;
    match arguments {
        ["rules"] => match project_approvals::load(state_dir, root) {
            Ok(rules) if rules.is_empty() => vec![
                "  no persisted project approvals (enable persist_project_approvals, then answer (p))".into(),
            ],
            Ok(rules) => {
                let mut lines = vec![format!("  project approvals for {}:", root.display())];
                lines.extend(
                    rules
                        .iter()
                        .enumerate()
                        .map(|(index, rule)| format!(
                            "  {}. {}: {}",
                            index + 1,
                            rule.tool,
                            rule.pattern.replace('\r', "\\r").replace('\n', "\\n")
                        )),
                );
                lines.push("  remove with /permissions forget <n|all>".into());
                lines
            }
            Err(error) => vec![format!("  error: {error}")],
        },
        ["forget", which] => {
            let index = match *which {
                "all" => None,
                other => match other.parse::<usize>() {
                    Ok(index) => Some(index),
                    Err(_) => return vec!["  usage: /permissions forget <n|all>".into()],
                },
            };
            match project_approvals::remove(state_dir, root, index) {
                Ok(removed) => vec![format!(
                    "  removed {} project approval(s) from this project and the running session (a network host approved in the proxy stays until restart)",
                    removed.len()
                )],
                Err(error) => vec![format!("  error: {error}")],
            }
        }
        _ => vec!["  usage: /permissions rules | forget <n|all> | session | revoke <n|all>".into()],
    }
}

fn parse_permission_mode(value: &str) -> Option<(SecurityMode, &'static str)> {
    match value {
        "yolo" => Some((SecurityMode::Yolo, "yolo")),
        "auto" | "auto-approval" => Some((SecurityMode::Standard, "auto-approval")),
        "manual" | "manual-approval" => Some((SecurityMode::Restrictive, "manual-approval")),
        _ => None,
    }
}

fn set_permission_mode(ctx: &mut SlashCtx<'_>, mode: SecurityMode, label: &str) {
    if let Some(refusal) = crate::config::managed::mode_violation(mode) {
        write_error(ctx.renderer, refusal);
        return;
    }
    if let Some(permission) = ctx.permission {
        permission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_mode(mode);
        write_ok(ctx.renderer, format!("permissions: {label}"));
    } else {
        write_error(ctx.renderer, PERMISSION_SYSTEM_DISABLED);
    }
}

/// The `/mode` listing: the current mode, then one line per mode in
/// [`SecurityMode::NAMES`] order with its description.
pub(crate) fn mode_listing(current: SecurityMode) -> Vec<String> {
    let width = SecurityMode::NAMES
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec![format!("  current: {current}"), String::new()];
    lines.extend(SecurityMode::all().map(|mode| {
        format!(
            "  /mode {:<width$}   {}",
            mode.to_string(),
            mode.description()
        )
    }));
    lines
}

/// Parse a `/mode` argument: every name in [`SecurityMode::NAMES`], plus the
/// `accept` alias for `standard` that `default_permission_mode` also takes.
pub(crate) fn parse_mode_arg(arg: &str) -> Option<SecurityMode> {
    SecurityMode::from_config_value(arg)
}

async fn set_mode(ctx: &mut SlashCtx<'_>, mode: SecurityMode) {
    if let Some(refusal) = crate::config::managed::mode_violation(mode) {
        write_error(ctx.renderer, refusal);
        return;
    }
    if let Some(p) = ctx.permission {
        p.lock().unwrap_or_else(|e| e.into_inner()).set_mode(mode);
        write_ok(ctx.renderer, format!("security mode: {mode}"));
    } else {
        write_error(ctx.renderer, PERMISSION_SYSTEM_DISABLED);
    }
}

async fn handle_toggle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 {
        write_usage(ctx.renderer, "usage: /toggle <feature> [on|off]");
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
        for line in default_runtime_status_lines(ctx.cfg, ctx.agent.as_ref()) {
            write_result(ctx.renderer, line);
        }
    } else {
        let new_state =
            match toggle_target(parts[1], parts.get(2).copied(), *ctx.todo_tools_enabled) {
                Ok(state) => state,
                Err(error) => {
                    write_error(ctx.renderer, error);
                    return Ok(());
                }
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

/// Uses only captured configuration and existing manager metadata. No PATH
/// search, server startup, hook execution, or provider request is performed.
fn default_runtime_status_lines(
    cfg: &crate::config::Config,
    agent: Option<&crate::provider::AnyAgent>,
) -> Vec<String> {
    #[cfg(feature = "lsp")]
    let lsp = if cfg.resolve_lsp().is_none() {
        "off (configured)".to_string()
    } else if let Some(agent) = agent {
        match agent.lsp_manager() {
            Some(manager) => format!(
                "on ({} registered servers; processes start lazily)",
                manager.diagnostic_snapshot().configured_count
            ),
            None => "off (no eligible servers or excluded tool)".to_string(),
        }
    } else {
        "not built (availability not checked)".to_string()
    };
    #[cfg(not(feature = "lsp"))]
    let lsp = {
        let _ = (cfg, agent);
        "not compiled".to_string()
    };
    vec![
        format!("  lsp     {lsp}"),
        format!(
            "  hooks   {}",
            if cfg!(feature = "hooks") {
                "compiled (only configured hooks run)"
            } else {
                "not compiled"
            }
        ),
        format!(
            "  media   {}",
            if cfg!(feature = "multimodal") {
                "compiled (provider support required)"
            } else {
                "not compiled"
            }
        ),
    ]
}

/// The todo-tools state `/toggle <feature> [on|off]` asks for. `todo` is the
/// only toggleable feature; the runtime lines bare `/toggle` prints are
/// read-only, so naming one (or anything else) is an error rather than a
/// silent todo toggle.
fn toggle_target(feature: &str, state: Option<&str>, current: bool) -> Result<bool, String> {
    if !feature.eq_ignore_ascii_case("todo") {
        return Err(format!(
            "'{feature}' cannot be toggled; toggleable features: todo (run /toggle to list)"
        ));
    }
    match state.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(!current),
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => Err(format!("invalid: '{other}', use on or off")),
    }
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

/// `checking` / `on` / `off (<verbatim reason>)` / `not compiled`, matching
/// the `on`/`off` vocabulary the rest of `/toggle` uses.
fn availability_label(state: &crate::provider::RuntimeAvailability) -> String {
    match state {
        crate::provider::RuntimeAvailability::Pending => "checking".to_string(),
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
            if tools::set_edit_system(EditSystem::Similarity) {
                ctx.rebuild_agent().await;
            }
            write_ok(ctx.renderer, "edit system: similarity (SEARCH/REPLACE)");
        }
        "hashedit" => {
            if tools::set_edit_system(EditSystem::Hashedit) {
                ctx.rebuild_agent().await;
            }
            write_ok(ctx.renderer, "edit system: hashedit (tag-based)");
        }
        _ => write_error(
            ctx.renderer,
            format!("unknown: '{}' (similarity|hashedit)", parts[1]),
        ),
    }
    Ok(())
}

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
            let defer = ctx
                .cfg
                .mcp_servers
                .as_ref()
                .and_then(|servers| servers.get(name))
                .map_or(crate::extras::mcp::config::McpDefer::Auto, |server| {
                    server.defer()
                });
            write_ok(ctx.renderer, format!("schema loading: {defer:?}"));
            if let Some((deferred, enabled)) = mgr.deferred_status(name) {
                write_ok(
                    ctx.renderer,
                    format!(
                        "last active build: {deferred} deferred schemas, {enabled} enabled by search"
                    ),
                );
            }

            let log_lines = handle.recent_log_lines(10);
            if !log_lines.is_empty() {
                write_ok(
                    ctx.renderer,
                    "recent server log lines (untrusted server output):",
                );
                for line in log_lines {
                    write_result(ctx.renderer, format!("  {line}"));
                }
            }

            match handle.cached_tools().await {
                Ok(listed) => {
                    let filtered = if listed.filtered_by_config > 0 {
                        format!(
                            " ({} excluded by enabled_tools/disabled_tools)",
                            listed.filtered_by_config
                        )
                    } else {
                        String::new()
                    };
                    if listed.entries.is_empty() {
                        write_ok(
                            ctx.renderer,
                            format!("server '{}' has no tools{filtered}", name),
                        );
                    } else {
                        write_ok(ctx.renderer, format!("tools on '{}'{filtered}:", name));
                        for entry in &listed.entries {
                            let tool = &entry.definition;
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
    }
    // Bare `/mcp` never reaches here: `handle_mcp_command` answers it from
    // `mcp_status()` without any RPC.
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
        write_usage(ctx.renderer, "usage: /mcp login <server>");
        return Ok(());
    };
    // Validate config here so errors get friendly messages. The actual login
    // (network discovery + browser wait) runs in the event loop, which spawns
    // the wait as a background task so the TUI never freezes.
    // A URL server without an oauth block may still sign in: login attempts
    // OAuth discovery and saves the block on success.
    let is_plain_url_server = matches!(
        ctx.cfg.mcp_servers.as_ref().and_then(|m| m.get(name)),
        Some(crate::extras::mcp::config::McpServerConfig::Url { oauth: None, .. })
    );
    if !is_plain_url_server && let Err(e) = resolve_oauth_server(ctx, name) {
        write_error(ctx.renderer, e);
        return Ok(());
    }
    Err(super::DeferredSlash::McpLogin(name.to_string()).into())
}

#[cfg(feature = "mcp")]
fn handle_mcp_logout(name: Option<&str>, ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        write_usage(ctx.renderer, "usage: /mcp logout <server>");
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
mod reasoning_tests {
    use super::{reasoning_target, toggle_target};

    /// mini-agent-x1dmg: only `todo` toggles; other names are errors.
    #[test]
    fn toggle_only_accepts_todo() {
        assert_eq!(toggle_target("todo", Some("off"), true), Ok(false));
        assert_eq!(toggle_target("TODO", Some("on"), false), Ok(true));
        assert_eq!(toggle_target("todo", None, true), Ok(false));
        assert!(toggle_target("todo", Some("sideways"), true).is_err());
        for other in ["skills", "js", "mcp", "lsp", "hooks", "media"] {
            let error = toggle_target(other, Some("off"), true).unwrap_err();
            assert!(error.contains(other) && error.contains("todo"), "{error}");
        }
    }

    /// mini-agent-dhq65: `/reasoning off` must never turn reasoning on.
    #[test]
    fn explicit_on_off_set_the_state_and_no_argument_toggles() {
        for current in [false, true] {
            assert_eq!(reasoning_target(Some("on"), current), Ok(true));
            assert_eq!(reasoning_target(Some("OFF"), current), Ok(false));
            assert_eq!(reasoning_target(None, current), Ok(!current));
            assert!(reasoning_target(Some("maybe"), current).is_err());
        }
    }
}

#[cfg(test)]
mod mode_tests {
    use super::{mode_listing, parse_mode_arg};
    use crate::permission::SecurityMode;

    #[test]
    fn every_documented_mode_name_round_trips_through_mode() {
        for name in SecurityMode::NAMES {
            let mode = parse_mode_arg(name).unwrap_or_else(|| panic!("/mode {name} rejected"));
            assert_eq!(mode.to_string(), name);
        }
        assert_eq!(parse_mode_arg("planwrite"), Some(SecurityMode::PlanWrite));
    }

    #[test]
    fn accept_is_an_alias_for_standard_and_unknown_names_are_rejected() {
        assert_eq!(parse_mode_arg("accept"), Some(SecurityMode::Standard));
        assert_eq!(parse_mode_arg("auto"), None);
        assert_eq!(parse_mode_arg(""), None);
    }

    #[test]
    fn listing_names_the_current_mode_and_every_mode_with_its_description() {
        let lines = mode_listing(SecurityMode::PlanWrite);
        assert_eq!(lines[0], "  current: planwrite");
        let modes: Vec<SecurityMode> = SecurityMode::all().collect();
        assert_eq!(modes.len(), SecurityMode::NAMES.len());
        for mode in modes {
            let prefix = format!("  /mode {mode} ");
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with(&prefix) && line.ends_with(mode.description())),
                "listing is missing {mode}: {lines:?}"
            );
        }
    }
}

#[cfg(test)]
mod runtime_status_tests {
    use super::{parse_permission_mode, runtime_status_lines};
    use crate::permission::SecurityMode;
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
    fn permission_command_names_map_to_existing_audited_modes() {
        for (name, expected_mode, expected_label) in [
            ("yolo", SecurityMode::Yolo, "yolo"),
            ("auto", SecurityMode::Standard, "auto-approval"),
            ("auto-approval", SecurityMode::Standard, "auto-approval"),
            ("manual", SecurityMode::Restrictive, "manual-approval"),
            (
                "manual-approval",
                SecurityMode::Restrictive,
                "manual-approval",
            ),
        ] {
            assert_eq!(
                parse_permission_mode(name),
                Some((expected_mode, expected_label))
            );
        }
        assert_eq!(parse_permission_mode("unknown"), None);
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
    fn pending_runtime_reports_checking_instead_of_off() {
        let report = JsRuntimeReport {
            javascript: RuntimeAvailability::Pending,
            learned_skills: RuntimeAvailability::Pending,
        };

        let lines = runtime_status_lines(
            &report,
            #[cfg(feature = "skills")]
            None,
        );

        assert!(
            lines.iter().all(|line| line.contains("checking")),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| !line.contains("off")), "{lines:?}");
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

#[cfg(test)]
mod default_runtime44_tests {
    #[test]
    fn default_runtime44_listing_reports_build_and_explicit_off_without_probes() {
        let mut cfg = crate::config::Config::default();
        let lines = super::default_runtime_status_lines(&cfg, None);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            if cfg!(feature = "lsp") {
                "  lsp     not built (availability not checked)"
            } else {
                "  lsp     not compiled"
            }
        );
        assert!(lines[1].contains(if cfg!(feature = "hooks") {
            "only configured hooks run"
        } else {
            "not compiled"
        }));
        assert!(lines[2].contains(if cfg!(feature = "multimodal") {
            "provider support required"
        } else {
            "not compiled"
        }));
        #[cfg(feature = "lsp")]
        {
            cfg.lsp = Some(crate::config::types::LspConfig {
                enabled: false.into(),
                ..Default::default()
            });
            assert_eq!(
                super::default_runtime_status_lines(&cfg, None)[0],
                "  lsp     off (configured)"
            );
        }
        #[cfg(not(feature = "lsp"))]
        let _ = &mut cfg;
    }
}

#[cfg(test)]
mod profile100_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test]
    async fn profile100_actual_slash_listing_switch_and_busy_refusal_are_session_only() {
        let root = crate::tests::OwnedTestDirectory::new("profile100-slash");
        let _env = crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let cli = crate::cli::Cli {
            no_sandbox: true,
            ..Default::default()
        };
        let cfg = crate::config::Config::default();
        let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
        let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
        let mut session = crate::session::Session::new("openrouter", "fixture", 128_000, "");
        let session_id = session.id.clone();
        let mut client = crate::provider::AnyClient::OpenRouter(
            rig::providers::openrouter::Client::builder()
                .api_key("unused-test-key")
                .base_url("http://127.0.0.1:9")
                .http_client(crate::provider_http::ProviderHttpClient::default())
                .build()
                .unwrap(),
        );
        let mut agent = None;
        let mut input = crate::ui::input::InputEditor::new();
        let invalidated = AtomicBool::new(false);
        let mut show_reasoning = false;
        let mut reasoning_enabled = false;
        let mut is_running = false;
        let mut todo_tools_enabled = false;
        // Selection/UI control only; actual OS effects are proved separately.
        let mut sandbox = crate::sandbox::Sandbox::new(false, "seatbelt")
            .with_workspace_binding(workspace.clone());
        let mut terminal_guard = crate::ui::terminal::TerminalGuard::detached_for_test();
        #[cfg(feature = "skills")]
        let skill_services = Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new());
        let mut ctx = SlashCtx {
            prebuild_invalidated: &invalidated,
            agent: &mut agent,
            client: &mut client,
            renderer: &mut renderer,
            session: &mut session,
            cli: &cli,
            cfg: &cfg,
            context: &mut context,
            workspace: &workspace,
            custom_commands: &mut Default::default(),
            show_reasoning: &mut show_reasoning,
            reasoning_enabled: &mut reasoning_enabled,
            is_running: &mut is_running,
            input: &mut input,
            permission: &None,
            ask_tx: &None,
            todo_tools_enabled: &mut todo_tools_enabled,
            sandbox: &mut sandbox,
            terminal_guard: &mut terminal_guard,
            #[cfg(feature = "skills")]
            skill_services: &skill_services,
            #[cfg(feature = "mcp")]
            mcp_manager: None,
        };
        handle(&["/sandbox"], &mut ctx).await.unwrap();
        assert!(!invalidated.load(Ordering::Relaxed));
        assert!(ctx.agent.is_none());
        handle(&["/sandbox", "typo"], &mut ctx).await.unwrap();
        assert_eq!(
            ctx.sandbox.profile(),
            crate::sandbox::SandboxProfile::Workspace
        );
        *ctx.is_running = true;
        handle(&["/sandbox", "host-full"], &mut ctx).await.unwrap();
        assert_eq!(
            ctx.sandbox.profile(),
            crate::sandbox::SandboxProfile::Workspace
        );
        assert!(!invalidated.load(Ordering::Relaxed));
        *ctx.is_running = false;
        handle(&["/sandbox", "host-full"], &mut ctx).await.unwrap();
        // mini-agent-sskdu.151: with the sandbox off the switch is refused, not
        // reported as applied; nothing is rebuilt.
        assert_eq!(
            ctx.sandbox.profile(),
            crate::sandbox::SandboxProfile::Workspace
        );
        assert!(!invalidated.load(Ordering::Relaxed));
        assert!(ctx.agent.is_none());
        assert_eq!(
            ctx.sandbox.policy(),
            crate::sandbox::SandboxPolicy::Disabled,
            "selection must not turn --no-sandbox into a new grant"
        );
        assert_eq!(ctx.session.id, session_id);
        assert!(ctx.permission.is_none());
        assert!(cfg.sandbox_profile.is_none(), "no config persistence");
        assert!(crate::ui::pickers::list::reserved_command("sandbox"));

        // mini-agent-nvnkx.70: the shell network policy switches for the idle
        // session only, and the listing advertises the subcommand.
        assert!(SANDBOX_NETWORK_USAGE_LINE.contains("/sandbox network"));
        *ctx.sandbox = crate::sandbox::Sandbox::new(true, "seatbelt")
            .with_workspace_binding(workspace.clone());
        assert_eq!(ctx.sandbox.network(), crate::sandbox::SandboxNetwork::Deny);
        handle(&["/sandbox", "network", "bogus"], &mut ctx)
            .await
            .unwrap();
        handle(&["/sandbox", "network"], &mut ctx).await.unwrap();
        assert_eq!(ctx.sandbox.network(), crate::sandbox::SandboxNetwork::Deny);
        *ctx.is_running = true;
        handle(&["/sandbox", "network", "allow"], &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.sandbox.network(), crate::sandbox::SandboxNetwork::Deny);
        *ctx.is_running = false;
        handle(&["/sandbox", "network", "allowlist"], &mut ctx)
            .await
            .unwrap();
        assert_eq!(
            ctx.sandbox.network(),
            crate::sandbox::SandboxNetwork::Allowlist
        );
        assert!(cfg.sandbox_network.is_none(), "no config persistence");
    }
}

#[cfg(test)]
mod i7aog4_tests {
    #[test]
    fn permissions_rules_and_forget_manage_persisted_project_approvals() {
        let root = crate::tests::OwnedTestDirectory::new("permissions-rules");
        let state = root.path().join("state");
        let project = root.path().join("project");
        let lines = super::project_approval_lines(&state, &project, &["rules"]);
        assert!(lines[0].contains("no persisted project approvals"));
        crate::permission::project_approvals::add(
            &state,
            &project,
            crate::permission::project_approvals::ProjectApproval {
                tool: "shell".into(),
                pattern: "cargo test".into(),
            },
        )
        .unwrap();
        let lines = super::project_approval_lines(&state, &project, &["rules"]);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("1. shell: cargo test"))
        );
        let lines = super::project_approval_lines(&state, &project, &["forget", "1"]);
        assert!(lines[0].contains("removed 1"));
        assert!(
            crate::permission::project_approvals::load(&state, &project)
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod nvnkx67_tests {
    use crate::session::PermissionAllowEntry;

    fn entry(tool: &str, pattern: &str) -> PermissionAllowEntry {
        PermissionAllowEntry {
            tool: tool.into(),
            pattern: pattern.into(),
        }
    }

    #[test]
    fn session_grants_are_listed_with_scope_and_revoked_by_number() {
        let mut entries = vec![entry("shell_prefix", "git push"), entry("edit", "src/**")];
        let lines = super::session_grant_lines(&entries);
        assert!(
            lines[1].starts_with("  1. shell commands starting with \"git push\""),
            "{lines:?}"
        );
        assert!(lines[2].starts_with("  2. "), "{lines:?}");
        assert!(super::take_session_grants(&mut entries, Some("9")).is_err());
        assert!(super::take_session_grants(&mut entries, None).is_err());
        let removed = super::take_session_grants(&mut entries, Some("1")).unwrap();
        assert_eq!(removed[0].pattern.as_str(), "git push");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            super::take_session_grants(&mut entries, Some("all"))
                .unwrap()
                .len(),
            1
        );
        assert!(super::session_grant_lines(&entries)[0].contains("no grants"));
    }

    #[test]
    fn a_revoked_prefix_asks_again_and_the_saved_entries_lose_it() {
        use crate::permission::SecurityMode;
        use crate::permission::checker::CheckResult;
        let mut checker = crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfigs::default(),
            SecurityMode::Standard,
            None,
            None,
        )
        .unwrap();
        let added = checker.add_session_shell_prefix("git push");
        let mut entries: Vec<PermissionAllowEntry> = added
            .into_iter()
            .map(|(tool, pattern)| entry(&tool, &pattern))
            .collect();
        assert_eq!(
            checker.check("shell", "git push origin x"),
            CheckResult::Allowed
        );
        super::take_session_grants(&mut entries, Some("1")).unwrap();
        checker.replace_session_allowlist(&super::grant_pairs(&entries));
        assert_eq!(
            checker.check("shell", "git push origin x"),
            CheckResult::Ask
        );
        assert!(entries.is_empty());
    }
}

#[cfg(test)]
mod sskdu151_tests {
    use super::profile_switch_refusal;
    use crate::sandbox::Sandbox;

    #[test]
    fn profile_switches_are_refused_while_the_sandbox_is_off() {
        assert_eq!(
            profile_switch_refusal(&Sandbox::new(false, "bwrap")),
            Some("sandbox is off; the profile has no effect")
        );
        assert_eq!(
            profile_switch_refusal(
                &Sandbox::new(false, "bwrap").with_unavailable_default_fallback()
            ),
            Some("sandbox is off; the profile has no effect")
        );
        assert_eq!(profile_switch_refusal(&Sandbox::new(true, "bwrap")), None);
    }
}
