use super::test_support::fixture_app;
use super::*;
use crate::agent::tools::mutation_diff::MutationDiff;
use crate::agent::tools::{ReadArgs, ReadTool, ReadTracker, WriteArgs, WriteTool};
use rig::tool::{Tool, ToolCallExtensions};

#[tokio::test]
async fn actual_app_channel_renders_only_matching_committed_diff_and_replays_same_preview() {
    let root = crate::tests::OwnedTestDirectory::new("mutation-app");
    let bindings = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
    ]
    .map(|name| (name, Some(root.path().join(name).into_os_string())));
    let _env = crate::tests::ScopedProcessEnv::set(&bindings);
    let cli = crate::cli::Cli {
        no_session: true,
        no_tools: true,
        no_sandbox: true,
        ..Default::default()
    };
    let cfg = crate::config::Config::default();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let mut session = Session::new("openrouter", "test-model", 128000, "");
    let ui = UiContext {
        native_startup: None,
        native_ticket: None,
        #[cfg(feature = "hooks")]
        pending_session_start_context: None,
        cli: &cli,
        cfg: &cfg,
        session: &mut session,
        context: &mut context,
        workspace: workspace.clone(),
        client: crate::provider::create_client(
            "openrouter",
            Some("unused-test-key"),
            cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
        .unwrap(),
        permission: None,
        ask_tx: None,
        sandbox: crate::sandbox::Sandbox::new(false, "none"),
        prebuild_invalidated: Arc::new(AtomicBool::new(false)),
        status_signals: None,
        #[cfg(feature = "skills")]
        skill_services: Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
        #[cfg(feature = "mcp")]
        mcp_manager: None,
    };
    let mut app = fixture_app(ui);
    std::fs::write(root.path().join("target.txt"), "old🙂\n").unwrap();
    let tracker = ReadTracker::new(true);
    ReadTool::new_with_tracker(None, None, None, 100, tracker.clone())
        .with_workspace(root.path())
        .call(ReadArgs {
            pages: None,
            raw_notebook: false,
            visual: false,
            full_resolution: false,
            character_offset: None,
            character_limit: None,
            path: "target.txt".into(),
            offset: None,
            limit: None,
        })
        .await
        .unwrap();
    let result = WriteTool::new_with_tracker(None, None, None, tracker)
        .with_workspace(root.path())
        .call_structured(
            WriteArgs {
                path: "target.txt".into(),
                content: "newα\x1b]52;c;ZXZpbA==\x07\n".into(),
                overwrite: true,
            },
            &ToolCallExtensions::new(),
        )
        .await
        .unwrap();
    let diff = result.extensions.get::<MutationDiff>().unwrap().clone();
    let (sender, mut receiver) = mpsc::channel(8);
    for event in [
        AgentEvent::ToolCall {
            id: "actual".into(),
            name: "write".into(),
            args: serde_json::json!({"path":"target.txt"}),
        },
        AgentEvent::FileMutation {
            id: "wrong".into(),
            name: "write".into(),
            diff: diff.clone(),
        },
        AgentEvent::FileMutation {
            id: "actual".into(),
            name: "edit".into(),
            diff: diff.clone(),
        },
        AgentEvent::FileMutation {
            id: "actual".into(),
            name: "write".into(),
            diff: diff.clone(),
        },
        AgentEvent::ToolResult {
            id: "actual".into(),
            name: "write".into(),
            output: result.output.as_str().into(),
            images: Vec::new(),
            is_error: false,
        },
    ] {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
    {
        app.handle_agent_event(event).await.unwrap();
    }
    assert_eq!(app.ui.session.messages.len(), 2);
    assert_eq!(
        app.ui.session.messages[0].recorded_diff.as_ref(),
        Some(&diff)
    );
    assert!(app.ui.session.messages[1].recorded_diff.is_none());
    let live = app
        .renderer
        .feed()
        .selected_text(
            180,
            0,
            app.renderer.feed().line_count(180).saturating_sub(1),
        )
        .unwrap();
    assert_eq!(live.matches("+newα").count(), 1);
    assert_eq!(live.matches("-old🙂").count(), 1);
    let painted = app.renderer.captured_output();
    assert!(painted.contains("+newα") && painted.contains("-old🙂"));
    assert!(!painted.contains("]52;") && !painted.contains("ZXZpbA=="));
    let mut replay = crate::ui::feed::Feed::new();
    crate::ui::events::push_session_messages(&mut replay, &app.ui.session.messages, &cfg);
    let replayed = replay
        .selected_text(180, 0, replay.line_count(180).saturating_sub(1))
        .unwrap();
    assert!(replayed.contains("+newα") && replayed.contains("-old🙂"));
    let off: crate::config::Config =
        serde_json::from_value(serde_json::json!({"show_tool_details":false})).unwrap();
    let mut hidden = crate::ui::feed::Feed::new();
    crate::ui::events::push_session_messages(&mut hidden, &app.ui.session.messages, &off);
    let hidden = hidden
        .selected_text(180, 0, hidden.line_count(180).saturating_sub(1))
        .unwrap();
    assert!(hidden.contains("write") && !hidden.contains("+newα") && !hidden.contains("-old🙂"));
}
