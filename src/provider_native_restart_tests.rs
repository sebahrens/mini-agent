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

// mini-agent-ewkrr: an ACP native session over the window reserve is compacted
// like any other, and the thinking chain built over the old transcript is
// restarted from the compacted one so the next prompt reaches the provider.
#[cfg(feature = "acp")]
#[tokio::test]
async fn acp_native_compaction_restarts_the_chain_over_the_compacted_transcript() {
    let cfg = crate::config::Config {
        compact_enabled: Some(true),
        keep_recent_tokens: Some(1),
        ..Default::default()
    };
    // The ACP gate no longer exempts native history from token pressure.
    let limits = crate::extras::acp::acp_native_limits_for_test(&cfg, 20_000, 2_000);
    assert!(limits, "native ACP history must be subject to token pressure");
    let run = drive(
        "native-acp-compact",
        vec![
            response("acp-signature-one", "first answer"),
            response("acp-signature-two", "second answer"),
        ],
        vec!["first prompt".into(), "second prompt".into()],
        true,
        json!({"compact_enabled": false}),
    )
    .await;
    let mut session = run.saved.last().unwrap().clone();
    assert!(session.native_history_snapshot().is_some());
    assert!(session.native_restart_cause().is_none(), "healthy chain");
    // No compaction happened: restarting is a no-op that keeps the chain.
    crate::extras::acp::restart_native_after_compaction(&mut session, None)
        .await
        .unwrap();
    assert!(session.native_history_snapshot().is_some());
    // Compaction rewrites the transcript under the signed chain.
    let cut = session.messages.len();
    session.compress("COMPACTED SUMMARY".into(), cut, 0);
    assert!(
        session.native_restart_cause().is_some(),
        "compaction invalidates the chain"
    );
    let epoch = session.native_frontier.current();
    crate::extras::acp::restart_native_after_compaction(&mut session, None)
        .await
        .unwrap();
    assert!(session.native_history_snapshot().is_none());
    assert!(session.native_restart_cause().is_none());
    assert!(session.native_frontier.current() > epoch, "new epoch");
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

// ---- mini-agent-siyos: headless images and multi-turn input on a persisted
// provider-native session ----

/// A valid 1x1 PNG.
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn image_blocks(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["type"] == "image")
        .count()
}

struct Headless {
    /// Owns the temporary roots for the fixture's lifetime.
    #[cfg_attr(not(feature = "multimodal"), allow(dead_code))]
    roots: crate::tests::OwnedTestDirectory,
    wire: Wire,
    cfg: crate::config::Config,
    workspace: Arc<crate::paths::WorkspaceBinding>,
    paths: &'static crate::paths::AppPaths,
}

impl Headless {
    async fn startup(
        &self,
        id: Option<String>,
        prompt: &str,
        image: bool,
    ) -> Box<crate::startup::Startup> {
        let cli = crate::cli::Cli {
            api_key: Some("unused-native-key".into()),
            quick_model: id.is_none().then(|| "model-a".to_owned()),
            session: id,
            print: true,
            quiet: true,
            message: vec![prompt.into()],
            image: if image {
                vec![std::path::PathBuf::from("shot.png")]
            } else {
                Vec::new()
            },
            no_context_files: true,
            no_tools: true,
            no_sandbox: true,
            ..Default::default()
        };
        let mut startup = crate::startup::heap_future(|| async {
            Box::new(
                crate::startup::Startup::init(
                    cli,
                    self.cfg.clone(),
                    self.paths.clone(),
                    self.workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap(),
            )
        })
        .await;
        startup.init_features().await.unwrap();
        startup.resolve_prompts().await.unwrap();
        startup
    }
}

fn headless_fixture(
    name: &str,
    responses: Vec<(bool, String)>,
) -> (Headless, crate::tests::ScopedProcessEnv) {
    let roots = crate::tests::OwnedTestDirectory::new(name);
    let environment = crate::tests::ScopedProcessEnv::set(&[
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
    use base64::Engine as _;
    std::fs::write(
        roots.path().join("shot.png"),
        base64::engine::general_purpose::STANDARD
            .decode(PNG_1X1)
            .unwrap(),
    )
    .unwrap();
    let wire = Wire::new(responses);
    let cfg: crate::config::Config = serde_json::from_value(json!({
        "session_title_model":"off",
        "custom_providers":{"media-headless":{"provider_type":"anthropic","base_url":wire.base}},
        "quick_models":{"model-a":{"provider":"media-headless","model":"claude-opus-5-5",
            "context_window":200000,"input_token_cost":2.0,"output_token_cost":4.0}},
    }))
    .unwrap();
    let workspace = Arc::new(crate::paths::WorkspaceBinding::capture(roots.path()).unwrap());
    let paths = crate::paths::process_paths_ref().unwrap();
    (
        Headless {
            roots,
            wire,
            cfg,
            workspace,
            paths,
        },
        environment,
    )
}

// `-p -i shot.png` on a persisted Anthropic session sends the image with the
// prompt, saves it in the native feed, and `--continue` replays it exactly once.
#[cfg(feature = "multimodal")]
#[tokio::test]
async fn headless_image_turn_on_a_persisted_native_session_resumes() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let (fixture, _environment) = headless_fixture(
        "native-headless-image",
        vec![
            response("image-turn-signature", "I see a pixel"),
            response("follow-up-signature", "still a pixel"),
        ],
    );
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let first = fixture.startup(None, "describe the image", true).await;
    let sid = first.session.id.to_string();
    let _release = Release(sid.clone());
    crate::startup::heap_future(|| first.dispatch())
        .await
        .unwrap();
    crate::session::lock::release_session(&sid);
    let saved = crate::session::storage::load_session_exact(&sid)
        .unwrap()
        .unwrap();
    assert_eq!(saved.native_frontier.refusal(), None);
    assert_eq!(saved.native_history_snapshot().unwrap().frames().len(), 1);
    let user_rows: Vec<_> = saved
        .messages
        .iter()
        .filter(|row| row.role == crate::session::MessageRole::User)
        .collect();
    assert_eq!(user_rows.len(), 1, "the image and prompt are one user turn");
    assert!(
        user_rows[0]
            .user
            .as_ref()
            .is_some_and(|user| user.has_images())
    );
    assert_eq!(user_rows[0].content.as_str(), "describe the image");

    let second = fixture.startup(Some(sid.clone()), "and now?", false).await;
    crate::startup::heap_future(|| second.dispatch())
        .await
        .unwrap();
    crate::session::lock::release_session(&sid);
    let bodies = fixture.wire.finish();
    assert_eq!(bodies.len(), 2);
    assert_eq!(image_blocks(&bodies[0]), 1);
    assert_eq!(
        bodies[0]["messages"][0]["content"][0]["type"], "image",
        "images precede the prompt text: {}",
        bodies[0]["messages"][0]
    );
    assert_eq!(
        bodies[0]["messages"][0]["content"][1]["text"],
        "describe the image"
    );
    assert_eq!(
        image_blocks(&bodies[1]),
        1,
        "the resumed request replays the image once"
    );
    assert_eq!(
        signatures(&bodies[1]),
        vec!["image-turn-signature".to_string()]
    );
    let saved = crate::session::storage::load_session_exact(&sid)
        .unwrap()
        .unwrap();
    assert_eq!(saved.native_history_snapshot().unwrap().frames().len(), 2);
    assert_eq!(saved.native_frontier.refusal(), None);
    drop(fixture.roots);
}

// Several user turns (stream-json) in one process on one persisted native
// session, an image on the second, then a resumed process.
#[tokio::test]
async fn headless_multi_turn_input_on_a_persisted_native_session() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let (fixture, _environment) = headless_fixture(
        "native-headless-multi-turn",
        vec![
            response("turn-one-signature", "answer one"),
            response("turn-two-signature", "answer two"),
            response("turn-three-signature", "answer three"),
            response("turn-four-signature", "answer four"),
        ],
    );
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let mut startup = fixture.startup(None, "turn one", false).await;
    let sid = startup.session.id.to_string();
    let _release = Release(sid.clone());
    // The same steps `dispatch` takes before the stream-json loop.
    startup.join_native_startup().await.unwrap();
    startup.dispatch_print().await.unwrap();
    // Turn two arrives as a stream-json line with an image block.
    startup.cli.message = vec!["turn two".into()];
    #[cfg(feature = "multimodal")]
    startup.session.pending_media.extend(
        crate::headless_media::resolve_images(
            fixture.roots.path(),
            vec![crate::headless_media::ImageSource::Base64 {
                media_type: Some("image/png".into()),
                data: PNG_1X1.into(),
            }],
        )
        .unwrap(),
    );
    startup.reopen_native_turn().await.unwrap();
    startup.dispatch_print().await.unwrap();
    startup.cli.message = vec!["turn three".into()];
    startup.reopen_native_turn().await.unwrap();
    startup.dispatch_print().await.unwrap();
    crate::session::lock::release_session(&sid);
    drop(startup);

    let saved = crate::session::storage::load_session_exact(&sid)
        .unwrap()
        .unwrap();
    assert_eq!(saved.native_frontier.refusal(), None);
    assert_eq!(saved.native_history_snapshot().unwrap().frames().len(), 3);

    // `--continue`: a new process resumes the same chain.
    let resumed = fixture.startup(Some(sid.clone()), "turn four", false).await;
    crate::startup::heap_future(|| resumed.dispatch())
        .await
        .unwrap();
    crate::session::lock::release_session(&sid);
    let bodies = fixture.wire.finish();
    assert_eq!(bodies.len(), 4);
    assert_eq!(
        bodies.iter().map(image_blocks).collect::<Vec<_>>(),
        if cfg!(feature = "multimodal") {
            [0, 1, 1, 1]
        } else {
            [0, 0, 0, 0]
        },
        "the image of turn two is replayed, never duplicated"
    );
    assert_eq!(
        signatures(&bodies[3]),
        vec![
            "turn-one-signature".to_string(),
            "turn-two-signature".to_string(),
            "turn-three-signature".to_string()
        ]
    );
    let saved = crate::session::storage::load_session_exact(&sid)
        .unwrap()
        .unwrap();
    assert_eq!(saved.native_history_snapshot().unwrap().frames().len(), 4);
    assert_eq!(saved.native_frontier.refusal(), None);
}

// Multi-turn on `--no-session` (nothing saved) keeps working too.
#[tokio::test]
async fn headless_multi_turn_input_without_a_saved_session_keeps_the_chain_in_memory() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let (fixture, _environment) = headless_fixture(
        "native-headless-no-session",
        vec![
            response("memory-one-signature", "answer one"),
            response("memory-two-signature", "answer two"),
        ],
    );
    #[cfg(feature = "hooks")]
    let _workspace_selection = crate::extras::hooks::retain_test_workspace_selection();
    let mut startup = fixture.startup(None, "turn one", false).await;
    startup.cli.no_session = true;
    startup.join_native_startup().await.unwrap();
    startup.dispatch_print().await.unwrap();
    startup.cli.message = vec!["turn two".into()];
    startup.reopen_native_turn().await.unwrap();
    startup.dispatch_print().await.unwrap();
    let bodies = fixture.wire.finish();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        signatures(&bodies[1]),
        vec!["memory-one-signature".to_string()]
    );
}
