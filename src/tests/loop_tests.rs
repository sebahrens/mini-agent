use crate::extras::r#loop::{
    DEFAULT_PLAN_FILENAME, LoopState, SUMMARY_TRUNCATION_CHARS, plan, transcript,
};
use std::path::PathBuf;

struct LoopTestDataDir {
    path: PathBuf,
    _environment: crate::tests::ScopedProcessEnv,
}

impl Drop for LoopTestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn loop_test_data_dir() -> LoopTestDataDir {
    let dir = std::env::temp_dir().join(format!("zerostack-loop-tests-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let environment = crate::tests::ScopedProcessEnv::set(&[(
        "ZS_DATA_DIR",
        Some(dir.as_os_str().to_os_string()),
    )]);
    LoopTestDataDir {
        path: dir,
        _environment: environment,
    }
}

fn unique_plan_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("zerostack-{label}-{}", uuid::Uuid::new_v4()))
}

// --- LoopState tests ---

#[test]
fn test_loop_state_new_defaults() {
    let ls = LoopState::new("fix bugs".to_string(), PathBuf::from("plan.md"), None, None);
    assert!(ls.active);
    assert_eq!(ls.prompt, "fix bugs");
    assert_eq!(ls.plan_file, PathBuf::from("plan.md"));
    assert_eq!(ls.iteration, 0);
    assert_eq!(ls.max_iterations, None);
    assert!(ls.last_summary.is_none());
    assert!(ls.run_cmd.is_none());
    assert!(ls.last_run_output.is_none());
}

#[test]
fn test_loop_state_new_with_max() {
    let ls = LoopState::new(
        "test".to_string(),
        PathBuf::from("p.md"),
        Some(5),
        Some("make check".to_string()),
    );
    assert_eq!(ls.max_iterations, Some(5));
    assert_eq!(ls.run_cmd, Some("make check".to_string()));
}

#[test]
fn test_should_stop_no_max() {
    let ls = LoopState::new("x".to_string(), PathBuf::from("p.md"), None, None);
    assert!(!ls.should_stop());
}

#[test]
fn test_should_stop_with_max_not_reached() {
    let mut ls = LoopState::new("x".to_string(), PathBuf::from("p.md"), Some(3), None);
    ls.iteration = 2;
    assert!(!ls.should_stop());
}

#[test]
fn test_should_stop_with_max_exactly_reached_still_runs() {
    let mut ls = LoopState::new("x".to_string(), PathBuf::from("p.md"), Some(3), None);
    ls.iteration = 3;
    // iteration == max should NOT trigger stop (must exceed)
    assert!(!ls.should_stop());
}

#[test]
fn test_should_stop_with_max_exceeded() {
    let mut ls = LoopState::new("x".to_string(), PathBuf::from("p.md"), Some(3), None);
    ls.iteration = 4;
    assert!(ls.should_stop());
}

#[test]
fn test_iteration_label_no_max() {
    let mut ls = LoopState::new("x".to_string(), PathBuf::from("p.md"), None, None);
    ls.iteration = 5;
    assert_eq!(ls.iteration_label(), "LOOP 5/∞");
}

#[test]
fn test_iteration_label_with_max() {
    let mut ls = LoopState::new("x".to_string(), PathBuf::from("p.md"), Some(10), None);
    ls.iteration = 3;
    assert_eq!(ls.iteration_label(), "LOOP 3/10");
}

#[test]
fn test_build_prompt_contains_key_parts() {
    let plan_path = unique_plan_path("custom plan.md");
    std::fs::write(&plan_path, "- Fix the parser").unwrap();
    let mut ls = LoopState::new(
        "implement feature X".to_string(),
        plan_path.clone(),
        Some(5),
        Some("cargo test".to_string()),
    );
    ls.iteration = 2;
    ls.last_summary = Some("fixed parser bug".to_string());
    ls.last_run_output = Some("all tests passed".to_string());

    let prompt = ls.build_prompt();

    assert!(prompt.contains("implement feature X"));
    assert!(prompt.contains("Iteration 2/5"));
    assert!(prompt.contains(&format!(
        "Current plan ({}):\n- Fix the parser",
        plan_path.display()
    )));
    assert!(prompt.contains(&format!("Keep {} up to date", plan_path.display())));
    assert!(prompt.contains(&format!("document them in {}.", plan_path.display())));
    assert!(!prompt.contains(DEFAULT_PLAN_FILENAME));
    assert!(prompt.contains("fixed parser bug"));
    assert!(prompt.contains("all tests passed"));
    assert!(prompt.contains("Choose ONE task from the plan"));
    std::fs::remove_file(plan_path).unwrap();
}

#[test]
fn test_build_prompt_starting_fresh() {
    let ls = LoopState::new("task".to_string(), PathBuf::from("plan.md"), None, None);
    let prompt = ls.build_prompt();
    assert!(prompt.contains("starting fresh"));
    assert!(prompt.contains("(none)"));
    assert!(prompt.contains("Iteration 0/∞"));
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn test_summary_truncation_constant() {
    // Guards against the constant being set to a nonsensical zero value.
    assert!(SUMMARY_TRUNCATION_CHARS > 0);
}

// --- plan tests ---

#[test]
fn test_plan_exists_on_nonexistent() {
    let tmp = unique_plan_path("plan-nonexistent.md");
    let _ = std::fs::remove_file(&tmp);
    assert!(!plan::plan_exists(&tmp));
}

#[test]
fn test_plan_exists_on_existing() {
    let tmp = unique_plan_path("plan-exists.md");
    std::fs::write(&tmp, "# Test plan").unwrap();
    assert!(plan::plan_exists(&tmp));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_read_plan_returns_content() {
    let tmp = unique_plan_path("read-plan.md");
    std::fs::write(&tmp, "item 1\nitem 2").unwrap();
    let content = plan::read_plan(&tmp);
    assert_eq!(content, Some("item 1\nitem 2".to_string()));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_read_plan_nonexistent_returns_none() {
    let tmp = unique_plan_path("read-nonexistent.md");
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(plan::read_plan(&tmp), None);
}

#[test]
fn test_delete_plan_removes_file() {
    let tmp = unique_plan_path("delete-plan.md");
    std::fs::write(&tmp, "data").unwrap();
    assert!(tmp.exists());
    plan::delete_plan(&tmp);
    assert!(!tmp.exists());
}

#[test]
fn test_delete_plan_nonexistent_does_not_panic() {
    let tmp = unique_plan_path("delete-nonexistent.md");
    let _ = std::fs::remove_file(&tmp);
    plan::delete_plan(&tmp); // should not panic
}

#[tokio::test]
async fn test_handle_startup_no_plan_returns_false() {
    let tmp = unique_plan_path("startup-nonexistent.md");
    let _ = std::fs::remove_file(&tmp);
    let result = plan::handle_startup(&tmp).await.unwrap();
    assert!(!result);
}

// --- transcript tests ---

#[test]
fn test_save_iteration_creates_file() {
    let session_id = "test-save-iteration";
    let data = loop_test_data_dir();
    let dir = data.path.join("loops").join(session_id);

    // Clean up before test
    let _ = std::fs::remove_dir_all(&dir);

    transcript::save_iteration(
        session_id,
        1,
        "test prompt",
        "test response",
        Some("validation ok"),
        "summary",
    )
    .unwrap();

    let iter_file = dir.join("iter-0001.json");
    assert!(iter_file.exists());

    let content = std::fs::read_to_string(&iter_file).unwrap();
    assert!(content.contains("test prompt"));
    assert!(content.contains("test response"));
    assert!(content.contains("validation ok"));
    assert!(content.contains("summary"));
    assert!(content.contains("\"iteration\": 1"));

    // Clean up
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_save_iteration_without_validation_output() {
    let session_id = "test-save-no-validation";
    let data = loop_test_data_dir();
    let dir = data.path.join("loops").join(session_id);

    let _ = std::fs::remove_dir_all(&dir);

    transcript::save_iteration(session_id, 2, "p", "r", None, "s").unwrap();

    let content = std::fs::read_to_string(dir.join("iter-0002.json")).unwrap();
    assert!(content.contains("\"validation_output\": null"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_build_loop_refreshes_generated_bytecode_policy_before_verification() {
    let loop_script = include_str!("../../scripts/loop.sh");
    let verification_policy = include_str!("../../scripts/loop-verification-policy.sh");

    assert_eq!(
        4,
        loop_script.matches("load_verification_policy").count(),
        "define once, load at startup, and reload before both verification passes",
    );
    assert!(verification_policy.contains("*/__pycache__/*|*.pyc|*.pyo"));
    assert!(
        verification_policy.find("path_is_generated_python_bytecode \"$path\" && return 1")
            < verification_policy.find("case \"$path\" in"),
        "generated bytecode must be rejected before surface relevance",
    );
}

#[test]
fn loop_script_invokes_configured_agent_as_a_quoted_array() {
    let loop_script = include_str!("../../scripts/loop.sh");
    assert!(loop_script.contains("read -r -a AGENT_CMD_ARGS"));
    assert!(loop_script.matches("\"${AGENT_CMD_ARGS[@]}\"").count() >= 3);
    assert!(!loop_script.contains("| $AGENT_CMD"));
}

#[cfg(unix)]
#[test]
fn test_workflow_only_headless_relevance_check_executes_embedded_policy() {
    crate::extras::r#loop::verify_workflow_only_headless_relevance().unwrap();

    let workflow = include_str!("../../.github/workflows/ci.yml");
    assert!(
        workflow.contains("run: >-\n          cargo test --locked sandbox::"),
        "the shared sandbox test filter must remain inside a YAML-safe block scalar",
    );
}

#[cfg(unix)]
#[tokio::test]
async fn interactive_loop_validation_routes_results_cancellation_and_stale_events() {
    use crate::event::{AgentEvent, UserEvent};
    use crate::extras::validation::{self, ValidationStatus};
    use crate::sandbox::{CommandOutputLimit, DEFAULT_COMMAND_LIMITS, Sandbox};
    use crate::ui::event_handler::{handle_agent_event, handle_loop_validation_event};
    use crate::ui::state::{AgentRunState, ChainState, SlashState, UiContext};
    use clap::Parser;
    use std::time::Duration;

    let root = loop_test_data_dir();
    for case in ["nonzero", "flood", "unavailable", "cancel"] {
        let workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&root.path).unwrap());
        // Explicit context avoids loading personal prompts or provider services.
        let mut context = crate::context::ContextFiles {
            workspace_root: workspace.root().to_path_buf(),
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
        };
        let cli = crate::cli::Cli::parse_from(["mini-agent", "--no-session"]);
        let cfg = crate::config::Config::default();
        let mut session = crate::session::Session::new("openrouter", "test", 128_000, "");
        let client = crate::provider::AnyClient::OpenRouter(
            rig::providers::openrouter::Client::new("unused-test-key").unwrap(),
        );
        let sandbox = Sandbox::new(case == "unavailable", "__missing_loop_test_backend__");
        let mut ui = UiContext::new(
            &cli,
            &cfg,
            &mut session,
            &mut context,
            workspace,
            client,
            None,
            None,
            sandbox.clone(),
            None,
        );
        let mut renderer = crate::ui::renderer::Renderer::new().unwrap();
        let slash = SlashState {
            show_reasoning: false,
            reasoning_enabled: false,
            todo_tools_enabled: false,
        };
        let command = match case {
            "nonzero" => "printf caller-out; printf caller-err >&2; exit 7",
            "flood" => "yes loop-output",
            "cancel" => "exec sleep 5",
            _ => "printf must-not-run",
        };
        let mut state = LoopState::new(
            "finish".into(),
            root.path.join("plan.md"),
            Some(1),
            Some(command.into()),
        );
        state.iteration = 1;
        let mut chain = ChainState {
            loop_state: Some(state),
            ..Default::default()
        };
        let mut run = AgentRunState::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        handle_agent_event(
            AgentEvent::Done {
                response: "finished".into(),
                interactions: Vec::new(),
            },
            &mut renderer,
            &mut run,
            &mut ui,
            &slash,
            &mut chain,
            &tx,
        )
        .await
        .unwrap();
        assert!(
            run.is_running && run.validation_active(),
            "{case}: validation must stay interruptible"
        );

        // Use the same sandbox for an unrelated command. Cancel only after both
        // groups are live, so a pre-launch cancellation cannot satisfy this case.
        let release = root.path.join("release-unrelated");
        let unrelated = if case == "cancel" {
            let quoted_release = format!(
                "'{}'",
                release.display().to_string().replace('\'', "'\"'\"'")
            );
            let operation = validation::start(
                &sandbox,
                &format!(
                    "while [ ! -f {quoted_release} ]; do sleep 0.01; done; printf unrelated-survived"
                ),
            );
            let task = tokio::spawn(operation.wait());
            tokio::time::timeout(Duration::from_secs(2), async {
                while sandbox.active_group_count() != 2 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("both commands must launch");
            assert!(run.cancel_validation());
            Some(task)
        } else {
            None
        };
        let UserEvent::LoopValidationDone(event) =
            tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("validator must finish promptly")
                .expect("completion event")
        else {
            panic!("expected loop validation result");
        };
        let expected = match case {
            "nonzero" => ValidationStatus::NonZeroExit { exit_code: Some(7) },
            "flood" => ValidationStatus::OutputLimitExceeded(CommandOutputLimit::Stdout),
            "cancel" => ValidationStatus::Cancelled,
            _ => ValidationStatus::Failed,
        };
        assert_eq!(event.result.status, expected, "{case}");
        assert_eq!(event.response, "finished");
        assert_eq!(event.summary, "finished");
        let diagnostic = event.result.render();
        assert!(event.result.stdout.len() <= DEFAULT_COMMAND_LIMITS.stdout_bytes);
        assert!(event.result.stderr.len() <= DEFAULT_COMMAND_LIMITS.stderr_bytes);
        if case == "nonzero" {
            assert_eq!(event.result.stdout, b"caller-out");
            assert_eq!(event.result.stderr, b"caller-err");
        } else if case == "unavailable" {
            assert!(event.result.stdout.is_empty());
            assert!(diagnostic.contains("requested-but-unavailable"));
        }
        if let Some(unrelated) = unrelated {
            // Start another validation through the real Done handler before the
            // cancelled generation's already queued result is delivered.
            chain.loop_state.as_mut().unwrap().run_cmd = Some("printf replacement".into());
            handle_agent_event(
                AgentEvent::Done {
                    response: "replacement response".into(),
                    interactions: Vec::new(),
                },
                &mut renderer,
                &mut run,
                &mut ui,
                &slash,
                &mut chain,
                &tx,
            )
            .await
            .unwrap();
            assert!(
                !handle_loop_validation_event(event, &mut renderer, &mut run, &mut ui, &mut chain)
                    .await
                    .unwrap()
            );
            assert!(run.validation_active() && run.is_running);
            assert!(chain.loop_state.as_ref().unwrap().last_run_output.is_none());
            let UserEvent::LoopValidationDone(replacement) =
                tokio::time::timeout(Duration::from_secs(3), rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("expected replacement result")
            };
            assert_eq!(
                replacement.result.status,
                ValidationStatus::Success { exit_code: Some(0) }
            );
            assert_eq!(replacement.result.stdout, b"replacement");
            assert!(
                handle_loop_validation_event(
                    replacement,
                    &mut renderer,
                    &mut run,
                    &mut ui,
                    &mut chain
                )
                .await
                .unwrap()
            );
            std::fs::write(&release, "release").unwrap();
            let unrelated = tokio::time::timeout(Duration::from_secs(3), unrelated)
                .await
                .unwrap()
                .unwrap();
            assert!(unrelated.succeeded());
            assert_eq!(unrelated.stdout, b"unrelated-survived");
            assert_eq!(sandbox.active_group_count(), 0);
        } else {
            assert!(
                handle_loop_validation_event(event, &mut renderer, &mut run, &mut ui, &mut chain)
                    .await
                    .unwrap()
            );
            assert_eq!(
                chain
                    .loop_state
                    .as_ref()
                    .unwrap()
                    .last_run_output
                    .as_deref(),
                Some(diagnostic.as_str())
            );
        }
        assert!(!run.is_running && !run.validation_active());
        let state = chain.loop_state.as_ref().unwrap();
        assert!(!state.active);
        assert_eq!(state.iteration, 2);
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                crate::paths::process_paths()
                    .unwrap()
                    .transcripts_dir()
                    .join(&ui.session.id)
                    .join("iter-0001.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            record["validation_output"].as_str(),
            state.last_run_output.as_deref()
        );
        assert_eq!(
            record["response"],
            if case == "cancel" {
                "replacement response"
            } else {
                "finished"
            }
        );
    }
}
