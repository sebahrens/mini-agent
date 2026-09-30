//! `/loop` start, stop and usage through the real slash dispatcher state.

use super::*;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

/// A private temporary directory whose application roots are isolated from
/// the user's for as long as the fixture lives.
struct FixtureRoot {
    path: PathBuf,
    _environment: crate::tests::ScopedProcessEnv,
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture_root(label: &str) -> FixtureRoot {
    let path = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("mini-agent-{label}-{}", uuid::Uuid::new_v4()));
    let environment = crate::tests::ScopedProcessEnv::set(&[
        ("ZS_CONFIG_DIR", Some(path.join("config").into_os_string())),
        ("ZS_DATA_DIR", Some(path.join("data").into_os_string())),
        ("ZS_STATE_DIR", Some(path.join("state").into_os_string())),
        ("ZS_CACHE_DIR", Some(path.join("cache").into_os_string())),
    ]);
    std::fs::create_dir_all(&path).unwrap();
    FixtureRoot {
        path,
        _environment: environment,
    }
}

fn context_for(root: &std::path::Path) -> ContextFiles {
    ContextFiles {
        workspace_root: root.to_path_buf(),
        agents: None,
        prompts: Default::default(),
        current_prompt: None,
        current_prompt_name: None,
        agent_definitions: Default::default(),
        current_agent_name: None,
        current_agent_explicit: false,
        themes: Default::default(),
        current_theme_name: None,
        extra_files: Vec::new(),
        extra_file_contents: Default::default(),
        one_shot_restore: None,
        chain_declined: Vec::new(),
        #[cfg(feature = "memory")]
        memory: None,
        #[cfg(feature = "archmd")]
        architecture: None,
    }
}

/// Run one `/loop` command line against `session` in `workspace`, exactly as
/// `handle_slash` splits and routes it, and return the lines it wrote.
async fn run_loop_command(
    text: &str,
    session: &mut Session,
    context: &mut ContextFiles,
    workspace: &Arc<crate::paths::WorkspaceBinding>,
) -> Vec<String> {
    run_loop_parts(&split_command(text), session, context, workspace).await
}

/// Route already-split `/loop` fields to the command handler.
async fn run_loop_parts(
    parts: &[&str],
    session: &mut Session,
    context: &mut ContextFiles,
    workspace: &Arc<crate::paths::WorkspaceBinding>,
) -> Vec<String> {
    run_loop_parts_with_state(parts, session, context, workspace, false)
        .await
        .0
}

async fn run_loop_parts_with_state(
    parts: &[&str],
    session: &mut Session,
    context: &mut ContextFiles,
    workspace: &Arc<crate::paths::WorkspaceBinding>,
    running: bool,
) -> (Vec<String>, bool, bool, bool) {
    let cli = Cli::parse_from([
        "mini-agent",
        "--no-session",
        "--api-key",
        "unused-test-key",
        "--no-sandbox",
    ]);
    let cfg = Config::default();
    let mut client = AnyClient::OpenRouter(
        rig::providers::openrouter::Client::builder()
            .api_key("unused-test-key")
            .base_url("http://127.0.0.1:9")
            .build()
            .unwrap(),
    );
    let sandbox = Sandbox::new(false, "none").with_workspace_binding(workspace.clone());
    let mut agent = None;
    let mut renderer = Renderer::new().unwrap();
    let mut input = InputEditor::new();
    let mut terminal_guard = TerminalGuard::detached_for_test();
    let invalidated = std::sync::atomic::AtomicBool::new(false);
    let mut show_reasoning = false;
    let mut reasoning_enabled = false;
    let mut is_running = running;
    let mut todo_tools_enabled = true;
    #[cfg(feature = "skills")]
    let skill_services = Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new());
    let mut ctx = SlashCtx {
        prebuild_invalidated: &invalidated,
        agent: &mut agent,
        client: &mut client,
        renderer: &mut renderer,
        session,
        cli: &cli,
        cfg: &cfg,
        context,
        workspace,
        show_reasoning: &mut show_reasoning,
        reasoning_enabled: &mut reasoning_enabled,
        is_running: &mut is_running,
        input: &mut input,
        permission: &None,
        ask_tx: &None,
        todo_tools_enabled: &mut todo_tools_enabled,
        sandbox: &sandbox,
        terminal_guard: &mut terminal_guard,
        #[cfg(feature = "skills")]
        skill_services: &skill_services,
        #[cfg(feature = "mcp")]
        mcp_manager: None,
    };
    features::handle(parts, &mut ctx).await.unwrap();
    let feed = renderer.feed();
    let lines = (0..feed.block_count())
        .map(|index| feed.block_text(index).unwrap().to_string())
        .collect();
    (
        lines,
        is_running,
        invalidated.load(std::sync::atomic::Ordering::Relaxed),
        agent.is_some(),
    )
}

/// After `/worktree` rebinds the workspace, the plan a new loop re-reads each
/// round is the rebound workspace's, never the original checkout's or the
/// process working directory's.
#[tokio::test]
async fn loop_after_workspace_rebind_reads_the_plan_from_the_rebound_root() {
    let root = fixture_root("loop-rebind");
    let checkout = root.path.join("checkout");
    let worktree = root.path.join("worktree");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    let original = Arc::new(crate::paths::WorkspaceBinding::capture(&checkout).unwrap());
    let rebound = Arc::new(crate::paths::WorkspaceBinding::capture(&worktree).unwrap());
    assert_ne!(original.root(), rebound.root());
    let mut context = context_for(original.root());
    let mut session = Session::new("openrouter", "loop-model", 128_000, "loop-rebind");

    let lines = run_loop_command(
        "/loop keep the tests green",
        &mut session,
        &mut context,
        &rebound,
    )
    .await;

    assert!(
        lines.iter().any(|line| line.starts_with("loop started")),
        "{lines:?}"
    );
    let goal = session.goal_store.snapshot().expect("a loop goal is set");
    let plan = goal
        .context_file
        .expect("a loop goal carries its plan file");
    assert_eq!(
        plan,
        rebound
            .root()
            .join(crate::extras::r#loop::DEFAULT_PLAN_FILENAME)
    );
    assert!(plan.is_absolute());
    assert!(!plan.starts_with(original.root()));
    assert_ne!(
        plan,
        std::env::current_dir()
            .unwrap()
            .join(crate::extras::r#loop::DEFAULT_PLAN_FILENAME)
    );
    assert_eq!(goal.objective, "keep the tests green");
}

/// `/loop stop` ends a loop, but an ordinary `/goal` (no plan file) is not a
/// loop and survives it.
#[tokio::test]
async fn loop_stop_clears_only_a_loop_goal() {
    let root = fixture_root("loop-stop");
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&root.path).unwrap());
    let mut context = context_for(workspace.root());
    let mut session = Session::new("openrouter", "loop-model", 128_000, "loop-stop");

    let ordinary = crate::extras::goal::Goal::new("ship the release", Vec::new()).unwrap();
    assert!(ordinary.context_file.is_none());
    session.goal_store.set(ordinary, false).unwrap();
    let lines = run_loop_command("/loop stop", &mut session, &mut context, &workspace).await;
    assert_eq!(lines, ["no active loop"]);
    let kept = session
        .goal_store
        .snapshot()
        .expect("an ordinary goal survives /loop stop");
    assert_eq!(kept.objective, "ship the release");
    session.goal_store.clear();

    run_loop_command(
        "/loop refactor the parser",
        &mut session,
        &mut context,
        &workspace,
    )
    .await;
    assert!(
        session
            .goal_store
            .snapshot()
            .is_some_and(|goal| goal.context_file.is_some())
    );
    let lines = run_loop_command("/loop stop", &mut session, &mut context, &workspace).await;
    assert_eq!(lines, ["loop stopped"]);
    assert!(session.goal_store.snapshot().is_none());
}

#[tokio::test]
async fn loop_stop_during_a_run_clears_future_rounds_without_rebuilding_current_runner() {
    let root = fixture_root("loop-active-stop");
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&root.path).unwrap());
    let mut context = context_for(workspace.root());
    let mut session = Session::new("openrouter", "loop-model", 128_000, "loop-active-stop");
    run_loop_command(
        "/loop refactor the parser",
        &mut session,
        &mut context,
        &workspace,
    )
    .await;

    let (status, running, invalidated, _) = run_loop_parts_with_state(
        &["/loop", "status"],
        &mut session,
        &mut context,
        &workspace,
        true,
    )
    .await;
    assert!(status.iter().any(|line| line.starts_with("loop active:")));
    assert!(running);
    assert!(!invalidated);

    let (stopped, running, invalidated, rebuilt) = run_loop_parts_with_state(
        &["/loop", "stop"],
        &mut session,
        &mut context,
        &workspace,
        true,
    )
    .await;
    assert_eq!(stopped, ["loop stopped"]);
    assert!(running, "the current round remains owned by its runner");
    assert!(
        invalidated,
        "the next turn must rebuild without loop context"
    );
    assert!(!rebuilt, "stopping cannot rebuild the active runner");
    assert!(session.goal_store.snapshot().is_none());
}

/// A `/loop` whose prompt is blank sets no goal and prints the usage line.
#[tokio::test]
async fn loop_with_an_empty_prompt_prints_usage_and_sets_no_goal() {
    let root = fixture_root("loop-usage");
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(&root.path).unwrap());
    let mut context = context_for(workspace.root());
    let mut session = Session::new("openrouter", "loop-model", 128_000, "loop-usage");

    // The dispatcher trims a bare `/loop   ` down to `/loop`, which reports
    // status and the usage.
    let lines = run_loop_command("/loop   ", &mut session, &mut context, &workspace).await;
    assert_eq!(
        lines,
        ["no active loop", "usage: /loop <prompt>  |  /loop stop"]
    );
    assert!(session.goal_store.snapshot().is_none());

    // A prompt field that is only whitespace (which the dispatcher's own split
    // never produces, but another caller could) is refused rather than
    // becoming a loop with an empty objective.
    for blank in [" ", "\u{3000}\t"] {
        let lines = run_loop_parts(&["/loop", blank], &mut session, &mut context, &workspace).await;
        assert_eq!(lines, ["usage: /loop <prompt>"], "{blank:?}");
        assert!(session.goal_store.snapshot().is_none(), "{blank:?}");
    }
}
