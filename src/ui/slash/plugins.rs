//! `/plugins`: read-only list of installed plugins and their components.
//! Installing, removing and approving stay on the `mini-agent plugin` command
//! line, so nothing executable can be activated from inside a session.
use super::SlashCtx;

pub(crate) fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) {
    if parts.len() != 1 {
        super::write_usage(ctx.renderer, "usage: /plugins");
        return;
    }
    let Ok(paths) = crate::paths::process_paths_ref() else {
        super::write_error(ctx.renderer, "application paths are unavailable");
        return;
    };
    super::write_result(ctx.renderer, crate::extras::plugins::describe_for_paths(paths));
    super::write_ok(
        ctx.renderer,
        "Manage plugins with `mini-agent plugin install|remove|update|trust`.",
    );
}
