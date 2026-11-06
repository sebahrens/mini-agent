//! A native Anthropic session whose thinking chain can no longer be replayed
//! (finite bounds, another model, a compacted transcript) restarts from the
//! display transcript as a new epoch instead of refusing every later prompt.

use super::*;

struct Release(String);
impl Drop for Release {
    fn drop(&mut self) {
        crate::session::lock::release_session(&self.0);
    }
}

fn response(signature: &str, answer: &str) -> (bool, String) {
    let thinking = event(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}}),
    ) + &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    );
    (true, start() + &thinking + &text(1, answer) + &finish())
}

struct Run {
    saved: Vec<crate::session::Session>,
    bodies: Vec<Value>,
}

/// Drive a live TUI App through `steps` against a scripted Anthropic wire.
async fn drive(
    name: &str,
    responses: Vec<(bool, String)>,
    steps: Vec<String>,
    observe: bool,
    extra_config: Value,
) -> Run {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new(name);
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "MINI_AGENT_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let wire = Wire::new_tolerating_model_listing(responses);
    let mut config = json!({
        "session_title_model":"off",
        "custom_providers":{"restart-tui":{"provider_type":"anthropic","base_url":wire.base}},
        // Pinned models keep the App from probing `/v1/models` for windows.
        "quick_models":{
            "model-a":{"provider":"restart-tui","model":"claude-opus-5-5","context_window":200000,"input_token_cost":2.0,"output_token_cost":4.0},
            "model-b":{"provider":"restart-tui","model":"claude-sonnet-5-0","context_window":200000,"input_token_cost":2.0,"output_token_cost":4.0}
        },
    });
    for (key, value) in extra_config.as_object().unwrap() {
        config[key] = value.clone();
    }
    let cfg: crate::config::Config = serde_json::from_value(config).unwrap();
    let cli = crate::cli::Cli {
        api_key: Some("unused-native-key".into()),
        no_tools: true,
        no_context_files: true,
        no_sandbox: true,
        ..Default::default()
    };
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    #[cfg(feature = "agent-skills")]
    let _immutable_cleanup =
        crate::extras::skills::session::ImmutableFixtureCleanup(roots.path().to_path_buf());
    let mut session = crate::session::Session::new(
        "restart-tui",
        "claude-opus-5-5",
        200_000,
        workspace.root().to_str().unwrap(),
    );
    #[cfg(feature = "agent-skills")]
    {
        session.instruction_owner = Arc::new(
            crate::extras::skills::session::InstructionOwner::for_test(None),
        );
    }
    session.working_dir = workspace.root().to_string_lossy().into();
    session.set_input_output_prices(Some((2.0, 4.0)));
    let _release = Release(session.id.to_string());
    let mut context = crate::context::load(true).for_workspace_binding(true, &workspace);
    let client = crate::provider::create_client(
        "restart-tui",
        cli.api_key.as_deref(),
        cfg.custom_providers_map(),
        cfg.api_keys.as_ref(),
    )
    .unwrap();
    let ui = crate::ui::state::UiContext::new(
        &cli,
        &cfg,
        &mut session,
        &mut context,
        workspace.clone(),
        client,
        None,
        None,
        crate::sandbox::Sandbox::new(false, "none").with_workspace_binding(workspace.clone()),
        None,
    );
    let saved =
        crate::startup::heap_future(|| crate::ui::drive_native_script(ui, &steps, observe)).await;
    let bodies = wire.finish();
    Run { saved, bodies }
}

fn signatures(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter_map(|block| block["signature"].as_str().map(str::to_owned))
        .collect()
}

// mini-agent-9szqw: /model on a native session restarts the chain for the new
// model; the next prompt completes and replays the transcript, not the old
// signed thinking block.
#[tokio::test]
async fn model_switch_restarts_the_native_chain_and_the_next_prompt_completes() {
    let run = drive(
        "native-model-switch",
        vec![
            response("model-a-signature", "answer from model a"),
            response("model-b-signature", "answer from model b"),
        ],
        vec![
            "first prompt".into(),
            "/model claude-sonnet-5-0".into(),
            "second prompt".into(),
        ],
        true,
        json!({}),
    )
    .await;
    assert_eq!(run.bodies.len(), 2, "exactly one request per prompt");
    assert_eq!(run.bodies[0]["model"], "claude-opus-5-5");
    assert_eq!(run.bodies[1]["model"], "claude-sonnet-5-0");
    // The old model's signed thinking is not replayed under the new model.
    assert_eq!(signatures(&run.bodies[0]), Vec::<String>::new());
    assert!(
        !signatures(&run.bodies[1]).contains(&"model-a-signature".to_string()),
        "{}",
        run.bodies[1]
    );
    // The transcript itself is.
    let replay = run.bodies[1]["messages"].to_string();
    assert!(replay.contains("first prompt") && replay.contains("answer from model a"));
    // Disk: restart published, the new model's chain follows it.
    let after_switch = &run.saved[1];
    assert!(after_switch.native_history_snapshot().is_none());
    assert_eq!(after_switch.native_frontier.refusal(), None);
    let last = &run.saved[2];
    assert_eq!(last.native_frontier.refusal(), None);
    let history = last.native_history_snapshot().expect("new chain");
    assert_eq!(history.frames().len(), 1);
    assert_eq!(history.frames()[0].configured_model, "claude-sonnet-5-0");
    assert!(last.native_frontier.current() > run.saved[0].native_frontier.current());
}

// mini-agent-ydugt: /compact on a native session succeeds, re-anchors the
// chain on the compacted transcript, and the following prompt completes.
#[tokio::test]
async fn compact_restarts_the_native_chain_and_the_next_prompt_completes() {
    let summary = (
        true,
        start() + &text(0, "SUMMARY OF THE EARLIER CONVERSATION") + &finish(),
    );
    let run = drive(
        "native-compact",
        vec![
            response("compact-signature-one", "first answer"),
            response("compact-signature-two", "second answer"),
            summary,
            response("compact-signature-three", "third answer"),
        ],
        vec![
            "first prompt".into(),
            "second prompt".into(),
            "/compact".into(),
            "third prompt".into(),
        ],
        true,
        json!({"keep_recent_tokens": 1}),
    )
    .await;
    assert_eq!(run.bodies.len(), 4, "turn, turn, summarizer, turn");
    let after_compact = &run.saved[2];
    assert!(
        !after_compact.compactions.is_empty(),
        "the compaction was applied"
    );
    assert!(after_compact.native_history_snapshot().is_none());
    assert_eq!(after_compact.native_frontier.refusal(), None);
    // The third request replays the compacted transcript without stale chain.
    let replay = run.bodies[3]["messages"].to_string();
    assert!(
        replay.contains("SUMMARY OF THE EARLIER CONVERSATION"),
        "{replay}"
    );
    assert!(replay.contains("third prompt"));
    assert!(signatures(&run.bodies[3]).is_empty(), "{replay}");
    let last = &run.saved[3];
    assert_eq!(last.native_history_snapshot().unwrap().frames().len(), 1);
    assert_eq!(last.native_frontier.refusal(), None);
}

// mini-agent-9xucp: a persisted session runs past both finite bounds (128
// frames) without a refusal. The chain restarts in a new epoch near the bound.
#[tokio::test]
async fn native_session_survives_more_than_the_frame_bound() {
    const TURNS: usize = 150;
    let responses = (0..TURNS)
        .map(|turn| {
            response(
                &format!("bound-signature-{turn}"),
                &format!("answer {turn}"),
            )
        })
        .collect();
    let steps = (0..TURNS).map(|turn| format!("prompt {turn}")).collect();
    let run = drive(
        "native-frame-bound",
        responses,
        steps,
        false,
        json!({"compact_enabled": false}),
    )
    .await;
    assert_eq!(run.bodies.len(), TURNS);
    // The 150th request carried the whole conversation text...
    let last = run.bodies[TURNS - 1]["messages"].to_string();
    assert!(last.contains("prompt 149") && last.contains("answer 148"));
    // ...but never more than the restarted chain's signed blocks.
    let max_signatures = run.bodies.iter().map(|body| signatures(body).len()).max();
    assert!(
        max_signatures.unwrap() <= crate::session::native_replay::MAX_NATIVE_FRAMES / 2,
        "{max_signatures:?}"
    );
    assert!(signatures(&run.bodies[TURNS - 1]).len() < TURNS - 1);
}

/// Two `-p` invocations of one saved session through the real startup. A
/// resumed session keeps its persisted model, so `switch_model_between_runs`
/// edits the saved session's model on disk, as `/model` would have before a
/// quit. Returns (request bodies, session saved after the second run).
async fn headless_resume(
    name: &str,
    switch_model_between_runs: Option<&str>,
    responses: Vec<(bool, String)>,
    extra_config: Value,
) -> (Vec<Value>, crate::session::Session) {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let roots = crate::tests::OwnedTestDirectory::new(name);
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "MINI_AGENT_CONFIG_DIR",
            Some(roots.path().join("config").into_os_string()),
        ),
        (
            "MINI_AGENT_DATA_DIR",
            Some(roots.path().join("data").into_os_string()),
        ),
        (
            "MINI_AGENT_STATE_DIR",
            Some(roots.path().join("state").into_os_string()),
        ),
        (
            "MINI_AGENT_CACHE_DIR",
            Some(roots.path().join("cache").into_os_string()),
        ),
    ]);
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let wire = Wire::new(responses);
    let mut config = json!({
        "session_title_model":"off",
        "custom_providers":{"restart-headless":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{
            "model-a":{"provider":"restart-headless","model":"claude-opus-5-5","context_window":200000,"input_token_cost":2.0,"output_token_cost":4.0},
            "model-b":{"provider":"restart-headless","model":"claude-sonnet-5-0","context_window":200000,"input_token_cost":2.0,"output_token_cost":4.0}
        },
    });
    for (key, value) in extra_config.as_object().unwrap() {
        config[key] = value.clone();
    }
    let cfg: crate::config::Config = serde_json::from_value(config).unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths_ref().unwrap();
    let mut release = Release(String::new());
    let mut id: Option<String> = None;
    for prompt in ["first prompt", "second prompt"] {
        if let (Some(model), Some(sid)) = (switch_model_between_runs, id.as_ref()) {
            let path = crate::session::storage::session_path(sid).unwrap();
            let mut saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            saved["model"] = model.into();
            std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        }
        let cli = crate::cli::Cli {
            api_key: Some("unused-native-key".into()),
            quick_model: id.is_none().then(|| "model-a".to_owned()),
            session: id.clone(),
            print: true,
            quiet: true,
            message: vec![prompt.into()],
            no_context_files: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let mut startup = crate::startup::heap_future(|| async {
            Box::new(
                crate::startup::Startup::init(
                    cli,
                    cfg.clone(),
                    paths.clone(),
                    workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap(),
            )
        })
        .await;
        let sid = startup.session.id.to_string();
        release.0 = sid.clone();
        startup.init_features().await.unwrap();
        startup.resolve_prompts().await.unwrap();
        crate::startup::heap_future(|| startup.dispatch())
            .await
            .unwrap();
        crate::session::lock::release_session(&sid);
        id = Some(sid);
    }
    let saved = crate::session::storage::load_session_exact(id.as_ref().unwrap())
        .unwrap()
        .unwrap();
    (wire.finish(), saved)
}

// mini-agent-9szqw / mini-agent-ydugt (headless): resuming a native session on
// another model starts a new chain from the transcript instead of refusing.
#[tokio::test]
async fn headless_resume_on_another_model_restarts_the_native_chain() {
    let (bodies, saved) = headless_resume(
        "native-headless-model-switch",
        Some("claude-sonnet-5-0"),
        vec![
            response("headless-a-signature", "headless answer a"),
            response("headless-b-signature", "headless answer b"),
        ],
        json!({}),
    )
    .await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["model"], "claude-opus-5-5");
    assert_eq!(bodies[1]["model"], "claude-sonnet-5-0");
    assert!(!signatures(&bodies[1]).contains(&"headless-a-signature".to_string()));
    let replay = bodies[1]["messages"].to_string();
    assert!(replay.contains("first prompt") && replay.contains("headless answer a"));
    assert_eq!(saved.native_frontier.refusal(), None);
    let history = saved.native_history_snapshot().unwrap();
    assert_eq!(history.frames().len(), 1);
    assert_eq!(history.frames()[0].configured_model, "claude-sonnet-5-0");
}

// Control: the same model keeps and replays the signed chain (no restart).
#[tokio::test]
async fn headless_resume_on_the_same_model_replays_the_native_chain() {
    let (bodies, saved) = headless_resume(
        "native-headless-same-model",
        None,
        vec![
            response("headless-a-signature", "headless answer a"),
            response("headless-a2-signature", "headless answer a2"),
        ],
        json!({}),
    )
    .await;
    assert_eq!(
        signatures(&bodies[1]),
        vec!["headless-a-signature".to_string()]
    );
    assert_eq!(saved.native_history_snapshot().unwrap().frames().len(), 2);
}
