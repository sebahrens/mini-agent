#[cfg(any(feature = "loop", feature = "git-worktree"))]
use crate::ui::slash::write_ok;
#[cfg(feature = "loop")]
use crate::ui::slash::write_result;
#[cfg(any(feature = "loop", feature = "git-worktree"))]
use crate::ui::slash::write_usage;
use crate::ui::slash::{SlashCtx, write_error};

/// Canonical `/loop` usage; `status` is accepted too.
#[cfg(feature = "loop")]
pub(crate) const LOOP_USAGE: &str = "usage: /loop <prompt>  |  /loop status  |  /loop stop";

pub async fn handle(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    match parts[0] {
        "/compress" | "/compact" => handle_compress(parts, ctx),
        "/loop" => handle_loop(parts, ctx).await,
        "/worktree" => handle_worktree(parts, ctx).await,
        "/wt-merge" => handle_wt_merge(parts, ctx).await,
        "/wt-exit" => handle_wt_exit(parts, ctx).await,
        "/wt-discard" => handle_wt_discard(parts, ctx).await,
        _ => Ok(()),
    }
}

fn handle_compress(_parts: &[&str], _ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    // `show` is a verb only when bare; `/compact show more detail on the
    // API` is an instruction, as in /goal.
    if _parts.len() == 2 && _parts[1] == "show" {
        super::show_compaction_summary(_ctx.renderer, _ctx.session);
        return Ok(());
    }
    if _parts.len() == 2 && _parts[1] == "undo" {
        super::undo_compaction(_ctx);
        return Ok(());
    }
    let instructions = if _parts.len() > 1 {
        Some(_parts[1..].join(" "))
    } else {
        None
    };
    Err(super::DeferredSlash::Compress(instructions.filter(|text| !text.trim().is_empty())).into())
}

#[cfg(feature = "loop")]
async fn handle_loop(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    // `/loop` is a goal preset: an amnesiac round per iteration, the plan file
    // re-read each round, and the validator run every round as feedback. One
    // round engine serves both, so they cannot drift on what an iteration is.
    let store = ctx.session.goal_store.clone();
    let is_loop = |goal: &crate::extras::goal::Goal| goal.context_file.is_some();

    if parts.len() < 2 || (parts.len() == 2 && parts[1] == "status") {
        match store.snapshot().filter(is_loop) {
            Some(goal) => {
                write_ok(
                    ctx.renderer,
                    format!(
                        "loop {}: round {}/{} ({})",
                        goal.status.label(),
                        goal.progress.rounds,
                        goal.bounds.max_rounds,
                        goal.context_file
                            .as_deref()
                            .unwrap_or_else(|| std::path::Path::new("?"))
                            .display()
                    ),
                );
            }
            None => {
                write_ok(ctx.renderer, "no active loop");
                write_result(ctx.renderer, LOOP_USAGE);
            }
        }
        return Ok(());
    }

    if parts.len() == 2 && parts[1] == "stop" {
        match store.snapshot().filter(is_loop) {
            Some(_) => {
                store.clear();
                write_ok(ctx.renderer, "loop stopped");
                if *ctx.is_running {
                    // The current runner owns its own agent clone. Invalidate
                    // the cached agent so the next turn builds without the
                    // cleared loop, without rebuilding during this run.
                    ctx.prebuild_invalidated
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    *ctx.agent = None;
                } else {
                    ctx.rebuild_agent().await;
                }
            }
            None => write_ok(ctx.renderer, "no active loop"),
        }
        return Ok(());
    }

    // `/loop` starts round 1 at once, so `/loop 5m check CI` meant as a
    // schedule would burn a goal round on "5m check CI". The whole first
    // token must match the interval grammar, so `5minute-plan` still passes.
    if crate::ui::cron::is_interval_token(parts[1]) {
        let rest = parts
            .get(2)
            .map(|rest| format!(" {rest}"))
            .unwrap_or_default();
        write_error(
            ctx.renderer,
            format!(
                "/loop takes a prompt, not an interval; did you mean /cron {}{rest}?",
                parts[1]
            ),
        );
        return Ok(());
    }

    let prompt = parts[1..].join(" ");
    if prompt.trim().is_empty() {
        write_error(ctx.renderer, LOOP_USAGE);
        return Ok(());
    }
    if let Some(refusal) = super::goal::native_goal_refusal(ctx) {
        write_error(ctx.renderer, refusal);
        return Ok(());
    }
    // The plan lives in the workspace the agent edits, not wherever the
    // process happened to start: after `/worktree` the two differ, and a
    // relative path would have every round re-read the original checkout's
    // plan while the agent kept the worktree's current.
    let plan_file = ctx
        .workspace
        .root()
        .join(crate::extras::r#loop::DEFAULT_PLAN_FILENAME);
    let preset = match crate::extras::goal::preset::loop_goal(
        &prompt,
        &plan_file,
        Some(crate::extras::r#loop::DEFAULT_TUI_MAX_ITERATIONS),
        None,
        crate::extras::goal::GoalDefaults {
            cfg: ctx.cfg,
            provider: &ctx.session.provider,
            model: &ctx.session.model,
        },
    ) {
        Ok(preset) => preset,
        Err(error) => {
            write_error(ctx.renderer, error);
            return Ok(());
        }
    };
    if let Err(error) = store.set(preset.goal, false) {
        write_error(ctx.renderer, error);
        return Ok(());
    }
    ctx.rebuild_agent().await;
    write_ok(
        ctx.renderer,
        "loop started — iteration 1 will run after this message",
    );
    Ok(())
}

#[cfg(not(feature = "loop"))]
async fn handle_loop(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    write_error(ctx.renderer, super::feature_missing("/loop", "loop"));
    Ok(())
}

#[cfg(feature = "git-worktree")]
async fn handle_worktree(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    if parts.len() < 2 || (parts.len() == 2 && parts[1] == "list") {
        return list_worktrees(ctx).await;
    }
    let name = parts[1].trim();
    if name.is_empty() || name.contains(' ') || name.contains('/') {
        write_error(
            ctx.renderer,
            "invalid name: use a single word without spaces or slashes",
        );
        return Ok(());
    }

    // A worktree this harness created under that name is re-entered, not an
    // error (mini-agent-nvnkx.52).
    if let Ok(Some(existing)) =
        crate::extras::git_worktree::find_managed(ctx.workspace.root(), name).await
        && existing.info.worktree_path.exists()
    {
        write_ok(
            ctx.renderer,
            format!("re-entering existing worktree '{name}'"),
        );
        return Err(anyhow::Error::new(
            crate::extras::git_worktree::DeferredWorktreeAction::Switch {
                path: existing.info.worktree_path,
                branch: existing.info.branch,
            },
        ));
    }

    let wt_base_dir = ctx.cli.resolve_wt_base_dir(ctx.cfg);
    match crate::extras::git_worktree::create(ctx.workspace.root(), name, wt_base_dir.as_deref())
        .await
    {
        Ok((path, info)) => {
            let submodules = crate::extras::git_worktree::uninitialised_submodules(&path).await;
            if !submodules.is_empty() {
                write_ok(
                    ctx.renderer,
                    format!(
                        "note: submodule{} not populated in the new worktree: {}. Run `git submodule update --init` there (outside the sandbox) before building",
                        if submodules.len() == 1 { "" } else { "s" },
                        submodules.join(", ")
                    ),
                );
            }
            return Err(anyhow::Error::new(
                crate::extras::git_worktree::DeferredWorktreeAction::Switch {
                    path,
                    branch: info.branch,
                },
            ));
        }
        Err(e) => {
            write_error(ctx.renderer, format!("failed: {}", e));
        }
    }
    Ok(())
}

/// `/worktree` with no name: the worktrees `/worktree` created, with branch,
/// base, dirty state and age.
#[cfg(feature = "git-worktree")]
async fn list_worktrees(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let worktrees = match crate::extras::git_worktree::list_managed(ctx.workspace.root()).await {
        Ok(worktrees) => worktrees,
        Err(error) => {
            write_error(ctx.renderer, format!("cannot list worktrees: {error}"));
            return Ok(());
        }
    };
    if worktrees.is_empty() {
        write_usage(
            ctx.renderer,
            "no worktrees created by /worktree; usage: /worktree <name>  |  /worktree list  |  /wt-discard <name>",
        );
        return Ok(());
    }
    for worktree in &worktrees {
        let age = worktree.age_secs.map_or_else(
            || "unknown age".to_string(),
            |secs| match secs {
                0..=3_599 => format!("{}m old", secs / 60),
                3_600..=86_399 => format!("{}h old", secs / 3_600),
                _ => format!("{}d old", secs / 86_400),
            },
        );
        let state = match worktree.dirty {
            Some(true) => "dirty",
            Some(false) => "clean",
            None => "status unknown",
        };
        write_ok(
            ctx.renderer,
            format!(
                "{}  base {}  {state}  {age}  {}",
                worktree.info.branch,
                worktree.base.as_deref().unwrap_or("(none)"),
                worktree.info.worktree_path.display()
            ),
        );
    }
    write_ok(
        ctx.renderer,
        "enter one with /worktree <name>; remove one with /wt-discard <name>",
    );
    Ok(())
}

#[cfg(not(feature = "git-worktree"))]
async fn handle_worktree(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    write_error(
        ctx.renderer,
        super::feature_missing("/worktree", "git-worktree"),
    );
    Ok(())
}

#[cfg(feature = "git-worktree")]
async fn handle_wt_merge(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let info = match crate::extras::git_worktree::detect(ctx.workspace.root()).await {
        Some(i) => i,
        None => {
            write_error(ctx.renderer, "not in a git worktree");
            return Ok(());
        }
    };
    // `/wt-merge [--pull] [branch] [-m message...]`
    let mut options = crate::extras::git_worktree::MergeOptions::default();
    let mut given_target: Option<&str> = None;
    let mut index = 1;
    while index < parts.len() {
        match parts[index] {
            "--pull" => options.pull = true,
            "-m" | "--message" => {
                let message = parts[index + 1..].join(" ");
                if message.trim().is_empty() {
                    write_usage(
                        ctx.renderer,
                        "usage: /wt-merge [--pull] [branch] [-m <commit message>]",
                    );
                    return Ok(());
                }
                options.message = Some(message);
                break;
            }
            other if other.starts_with('-') => {
                write_usage(
                    ctx.renderer,
                    "usage: /wt-merge [--pull] [branch] [-m <commit message>]",
                );
                return Ok(());
            }
            other => given_target = Some(other),
        }
        index += 1;
    }
    let (target, source) = if let Some(target) = given_target {
        (target.trim().to_string(), "given on the command line")
    } else {
        match crate::extras::git_worktree::resolve_merge_target(&info).await {
            Some((b, source)) => (b, source.describe()),
            None => {
                write_error(
                    ctx.renderer,
                    "no target branch specified and none could be detected (no recorded base, checked-out branch, origin/HEAD, init.defaultBranch, main or master); pass one: /wt-merge <branch>",
                );
                return Ok(());
            }
        }
    };
    let repo_name = crate::extras::git_worktree::repo_name(&info.main_repo_path);
    write_ok(
        ctx.renderer,
        format!(
            "merging '{}' into '{}' ({source}) in {}...",
            info.branch, target, repo_name
        ),
    );
    Err(anyhow::Error::new(
        crate::extras::git_worktree::DeferredWorktreeAction::Merge {
            info,
            target,
            options,
        },
    ))
}

#[cfg(not(feature = "git-worktree"))]
async fn handle_wt_merge(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    write_error(
        ctx.renderer,
        "/wt-merge requires a build with the git-worktree feature",
    );
    Ok(())
}

#[cfg(feature = "git-worktree")]
async fn handle_wt_exit(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let info = match crate::extras::git_worktree::detect(ctx.workspace.root()).await {
        Some(i) => i,
        None => {
            write_error(ctx.renderer, "not in a git worktree");
            return Ok(());
        }
    };
    let main_path = info.main_repo_path;
    write_ok(
        ctx.renderer,
        format!(
            "returning to main repo at {}; worktree '{}' and its branch are kept (/worktree {} re-enters it, /wt-merge merges it, /wt-discard {} removes it)",
            main_path.display(),
            info.branch,
            info.branch,
            info.branch,
        ),
    );
    Err(anyhow::Error::new(
        crate::extras::git_worktree::DeferredWorktreeAction::Exit { main_path },
    ))
}

/// `/wt-discard [name] [--force]`: remove a worktree and its branch without
/// merging. Refuses uncommitted or unmerged work unless `--force` confirms.
#[cfg(feature = "git-worktree")]
async fn handle_wt_discard(parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let force = parts[1..].contains(&"--force");
    let name = parts[1..]
        .iter()
        .find(|part| !part.starts_with('-'))
        .copied();
    let target = match name {
        Some(name) => name.to_string(),
        None => match crate::extras::git_worktree::detect(ctx.workspace.root()).await {
            Some(info) => info.branch,
            None => {
                write_usage(
                    ctx.renderer,
                    "usage: /wt-discard <name> [--force]  (or run it inside the worktree to discard)",
                );
                return Ok(());
            }
        },
    };
    let worktree =
        match crate::extras::git_worktree::find_managed(ctx.workspace.root(), &target).await {
            Ok(Some(worktree)) => worktree,
            Ok(None) => {
                write_error(
                    ctx.renderer,
                    format!(
                        "no worktree named '{target}' was created by /worktree; see /worktree list"
                    ),
                );
                return Ok(());
            }
            Err(error) => {
                write_error(ctx.renderer, format!("cannot list worktrees: {error}"));
                return Ok(());
            }
        };
    if worktree.info.worktree_path.as_path() == ctx.workspace.root() {
        write_error(
            ctx.renderer,
            format!(
                "'{target}' is the current workspace; run /wt-exit first, then /wt-discard {target}"
            ),
        );
        return Ok(());
    }
    if !force {
        match crate::extras::git_worktree::discard_blockers(&worktree).await {
            Ok(blockers) if blockers.is_empty() => {}
            Ok(blockers) => {
                write_error(
                    ctx.renderer,
                    format!(
                        "not discarding '{target}': {}. Merge it with /wt-merge, or run /wt-discard {target} --force to throw the work away",
                        blockers.join("; ")
                    ),
                );
                return Ok(());
            }
            Err(error) => {
                write_error(ctx.renderer, format!("cannot check '{target}': {error}"));
                return Ok(());
            }
        }
    }
    match crate::extras::git_worktree::discard_worktree(&worktree.info).await {
        Ok(()) => write_ok(
            ctx.renderer,
            format!("discarded worktree '{target}' and its branch"),
        ),
        Err(error) => write_error(ctx.renderer, format!("discard failed: {error}")),
    }
    Ok(())
}

#[cfg(not(feature = "git-worktree"))]
async fn handle_wt_discard(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    write_error(
        ctx.renderer,
        super::feature_missing("/wt-discard", "git-worktree"),
    );
    Ok(())
}

#[cfg(not(feature = "git-worktree"))]
async fn handle_wt_exit(_parts: &[&str], ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    write_error(
        ctx.renderer,
        "/wt-exit requires a build with the git-worktree feature",
    );
    Ok(())
}
