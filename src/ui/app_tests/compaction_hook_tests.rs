//! mini-agent-sskdu.239: the compaction hooks fire the same way in every
//! front end. A recording command hook sees `PreCompact` once, then
//! `PostCompact` carrying the stored summary, then `SessionStart(compact)`.
use super::*;

/// Events a recording hook saw, in order, as parsed envelopes.
fn recorded(log: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn names(events: &[serde_json::Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| event["hook_event_name"].as_str())
        .collect()
}

/// Installs a dispatcher whose three compaction events append their envelope
/// to the returned log. Hold `dispatcher_guard::acquire()` first: the
/// dispatcher is process-global and the guard hides it from other tests.
fn install_recording_hooks(root: &std::path::Path) -> std::path::PathBuf {
    let log = root.join("compaction-hooks.log");
    let handler = serde_json::json!([{"hooks":[{
        "type":"command","command":"/bin/sh",
        "args":["-c", "{ cat; printf '\\n'; } >> \"$1\"", "record", log],
        "trust":"trusted","timeout":5
    }]}]);
    let settings = serde_json::json!({
        "PreCompact": handler.clone(),
        "PostCompact": handler.clone(),
        "SessionStart": handler,
    });
    let hooks = crate::extras::hooks::settings::parse_hooks_config(&settings).unwrap();
    crate::extras::hooks::init_dispatcher(
        crate::extras::hooks::dispatcher::HookDispatcher::from_config(&hooks).unwrap(),
    );
    crate::extras::hooks::set_active_workspace(root);
    log
}

fn assert_one_committed_compaction(events: &[serde_json::Value], session: &Session, trigger: &str) {
    assert_eq!(
        names(events),
        ["PreCompact", "PostCompact", "SessionStart"],
        "{events:?}"
    );
    assert_eq!(events[0]["trigger"], trigger);
    assert_eq!(events[1]["trigger"], trigger);
    let stored = session.compacted_context().0.expect("a summary was stored");
    assert!(stored.contains("HOOK_SUMMARY"), "{stored}");
    assert_eq!(
        events[1]["compact_summary"], stored,
        "PostCompact carries the stored summary"
    );
    assert_eq!(events[2]["source"], "compact");
}

fn long_session(workspace: &crate::paths::WorkspaceBinding) -> Session {
    let mut session = Session::new(
        "plan80",
        "owned-model",
        1_000_000,
        workspace.root().to_str().unwrap(),
    );
    session.add_message(MessageRole::User, &"x".repeat(40_000));
    session.add_message(MessageRole::Assistant, &"y".repeat(40_000));
    session.add_message(MessageRole::User, &"z".repeat(8_000));
    session
}

#[cfg(unix)]
#[tokio::test]
async fn tui_manual_compress_fires_the_compaction_hooks_once() {
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let _workspace = crate::extras::hooks::retain_test_workspace_selection();
    let roots = Roots::new();
    let log = install_recording_hooks(roots.directory.path());
    let provider = Provider::new(vec!["HOOK_SUMMARY"]).await;
    let mut cfg = provider.config();
    cfg.keep_recent_tokens = Some(1_000);
    let cli = crate::cli::Cli {
        no_tools: true,
        no_session: true,
        ..Default::default()
    };
    let workspace = roots.workspace();
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = long_session(&workspace);
    let ui = ui(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        SecurityMode::Standard,
    );
    let mut app = owned_app(ui);
    app.run_slash_command("/compress").await.unwrap();
    assert_eq!(app.ui.session.compactions.len(), 1);
    assert_one_committed_compaction(&recorded(&log), app.ui.session, "manual");
    provider.finish().await;
}

#[cfg(unix)]
#[tokio::test]
async fn headless_compaction_fires_the_compaction_hooks_once() {
    let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
    let _workspace = crate::extras::hooks::retain_test_workspace_selection();
    let roots = Roots::new();
    let log = install_recording_hooks(roots.directory.path());
    let provider = Provider::new(vec!["HOOK_SUMMARY"]).await;
    let mut cfg = provider.config();
    cfg.compact_enabled = Some(true);
    cfg.keep_recent_tokens = Some(1_000);
    let client = crate::provider::create_client(
        "plan80",
        Some("unused-local-key"),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .unwrap();
    let workspace = roots.workspace();
    let mut session = long_session(&workspace);
    let (_, happened) = crate::startup::compact_headless_session(
        &mut session,
        &client,
        &cfg,
        #[cfg(feature = "memory")]
        None,
        0,
        None,
        true,
        None,
    )
    .await
    .unwrap();
    assert!(happened);
    assert_one_committed_compaction(&recorded(&log), &session, "auto");
    provider.finish().await;
}
