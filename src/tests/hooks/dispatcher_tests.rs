use std::collections::HashMap;

use crate::extras::hooks::dispatcher::HookDispatcher;
use crate::extras::hooks::envelope::EventFields;
use crate::extras::hooks::settings::{HookGroup, HookHandler, HooksConfig};
use crate::extras::hooks::{Decision, HookCtx, Verdict};

fn handler(command: &str) -> HookHandler {
    HookHandler {
        kind: "command".to_string(),
        command: Some("sh".to_string()),
        args: Some(vec!["-c".to_string(), command.to_string()]),
        timeout: Some(5),
        is_async: false,
        condition: None,
        once: false,
        trust: crate::extras::hooks::settings::HookTrust::Trusted,
        env: Default::default(),
    }
}

fn handler_with_condition(command: &str, condition: &str) -> HookHandler {
    HookHandler {
        condition: Some(condition.to_string()),
        ..handler(command)
    }
}

fn handler_once(command: &str) -> HookHandler {
    HookHandler {
        once: true,
        ..handler(command)
    }
}

fn handler_once_with_env(command: &str, key: &str, value: &str) -> HookHandler {
    let mut configured = handler_once(command);
    configured.env.insert(key.to_string(), value.to_string());
    configured
}

fn async_handler(command: &str) -> HookHandler {
    HookHandler {
        is_async: true,
        ..handler(command)
    }
}

fn ctx() -> HookCtx {
    HookCtx {
        session_id: "sess-1".into(),
        session_path: "/tmp/sess.json".into(),
        cwd: super::TEST_WORKING_DIR.into(),
        permission_mode: "default".into(),
    }
}

fn config_with(event: &str, matcher: Option<&str>, handlers: Vec<HookHandler>) -> HooksConfig {
    let mut config: HooksConfig = HashMap::new();
    config.insert(
        event.to_string(),
        vec![HookGroup {
            matcher: matcher.map(str::to_string),
            hooks: handlers,
        }],
    );
    config
}

#[test]
fn invalid_regex_matcher_fails_at_load_time() {
    let config = config_with("PreToolUse", Some("(unclosed"), vec![handler("true")]);
    assert!(HookDispatcher::from_config(&config).is_err());
}

#[test]
fn wildcard_matcher_matches_every_tool() {
    let config = config_with("PreToolUse", None, vec![handler("true")]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    assert!(!dispatcher.handlers_for("PreToolUse", "bash").is_empty());
    assert!(
        !dispatcher
            .handlers_for("PreToolUse", "anything_else")
            .is_empty()
    );
}

#[test]
fn name_list_matcher_matches_after_normalization() {
    // "Edit|Write" is CC-style names; the model calls zerostack's "write" tool.
    let config = config_with("PreToolUse", Some("Edit|Write"), vec![handler("true")]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    assert!(!dispatcher.handlers_for("PreToolUse", "write").is_empty());
    assert!(dispatcher.handlers_for("PreToolUse", "bash").is_empty());
}

#[test]
fn is_empty_true_when_no_events_configured() {
    let dispatcher = HookDispatcher::from_config(&HashMap::new()).unwrap();
    assert!(dispatcher.is_empty());
}

#[test]
fn is_empty_false_when_a_handler_is_configured() {
    let config = config_with("PreToolUse", None, vec![handler("true")]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    assert!(!dispatcher.is_empty());
}

#[test]
fn summary_is_empty_when_no_events_configured() {
    let dispatcher = HookDispatcher::from_config(&HashMap::new()).unwrap();
    assert!(dispatcher.summary().is_empty());
}

#[test]
fn summary_lists_events_with_handler_counts_sorted_by_event_name() {
    let mut config: HooksConfig = HashMap::new();
    config.insert(
        "Stop".to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: vec![handler("true")],
        }],
    );
    config.insert(
        "PreToolUse".to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: vec![handler("true"), handler("false")],
        }],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    assert_eq!(
        dispatcher.summary(),
        vec![("PreToolUse".to_string(), 2), ("Stop".to_string(), 1),]
    );
}

#[test]
fn identical_commands_are_deduplicated() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![handler("echo dup"), handler("echo dup")],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    assert_eq!(dispatcher.handlers_for("PreToolUse", "bash").len(), 1);
}

#[test]
fn same_executable_with_distinct_arguments_is_not_deduplicated() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![handler("echo first"), handler("echo second")],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    assert_eq!(dispatcher.handlers_for("PreToolUse", "bash").len(), 2);
}

#[tokio::test]
async fn dispatch_returns_continue_without_running_anything_when_no_handler_matches() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-dispatch-nomatch-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let cmd = format!("touch {}", marker.display());
    let config = config_with("PreToolUse", Some("Bash"), vec![handler(&cmd)]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();

    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "write", serde_json::json!({}))
        .await;

    assert_eq!(decision.verdict, Verdict::Defer);
    assert!(!marker.exists());
}

#[tokio::test]
async fn dispatch_pre_tool_use_defers_when_hook_exits_zero_with_no_decision() {
    let config = config_with("PreToolUse", None, vec![handler("true")]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({"command": "ls"}))
        .await;
    assert_eq!(decision.verdict, Verdict::Defer);
    assert!(decision.updated_input.is_none());
}

#[tokio::test]
async fn dispatch_pre_tool_use_a_lone_allow_verdict_merges_as_allow() {
    // Regression: Verdict's declared/derived Ord is Allow < Defer < Ask <
    // Deny (least to most severe), so a merge seeded from a hardcoded
    // Defer sentinel would never let a lone Allow verdict win (Allow is not
    // > Defer) and would silently report Defer instead.
    let config = config_with(
        "PreToolUse",
        None,
        vec![handler(r#"echo '{"permissionDecision":"allow"}'"#)],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(decision.verdict, Verdict::Allow);
}

#[tokio::test]
async fn dispatch_pre_tool_use_denies_on_exit_code_two() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![handler("echo 'no way' 1>&2; exit 2")],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(decision.verdict, Verdict::Deny);
    assert_eq!(decision.reason.as_deref().map(str::trim), Some("no way"));
}

#[tokio::test]
async fn dispatch_pre_tool_use_merges_most_severe_verdict() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![
            handler(r#"echo '{"permissionDecision":"allow"}'"#),
            handler("exit 2"),
        ],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(decision.verdict, Verdict::Deny);
}

#[tokio::test]
async fn dispatch_pre_tool_use_folds_updated_input_in_declared_order() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![
            handler(r#"echo '{"updatedInput":{"command":"first"}}'"#),
            handler(r#"echo '{"updatedInput":{"command":"second"}}'"#),
        ],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({"command": "orig"}))
        .await;
    assert_eq!(
        decision.updated_input,
        Some(serde_json::json!({"command": "second"}))
    );
}

#[tokio::test]
async fn dispatch_generic_returns_continue_when_no_handler_matches() {
    let dispatcher = HookDispatcher::from_config(&HashMap::new()).unwrap();
    let decision = dispatcher
        .dispatch(
            "Stop",
            None,
            &ctx(),
            EventFields::Stop {
                stop_hook_active: false,
                loop_iteration: None,
                loop_active: None,
            },
        )
        .await;
    assert_eq!(decision, Decision::Continue);
}

#[tokio::test]
async fn dispatch_generic_blocks_on_decision_block_json() {
    let config = config_with(
        "Stop",
        None,
        vec![handler(
            r#"echo '{"decision":"block","reason":"tests still failing"}'"#,
        )],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch(
            "Stop",
            None,
            &ctx(),
            EventFields::Stop {
                stop_hook_active: false,
                loop_iteration: None,
                loop_active: None,
            },
        )
        .await;
    assert_eq!(
        decision,
        Decision::Block {
            reason: "tests still failing".to_string()
        }
    );
}

#[tokio::test]
async fn dispatch_waits_for_async_handlers_but_ignores_their_decisions() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-async-complete-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let command = format!(
        "sleep 0.1; printf complete > {}; echo '{{\"decision\":\"block\"}}'",
        marker.display()
    );
    let config = config_with("Stop", None, vec![async_handler(&command)]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();

    let decision = dispatcher
        .dispatch(
            "Stop",
            None,
            &ctx(),
            EventFields::Stop {
                stop_hook_active: false,
                loop_iteration: None,
                loop_active: None,
            },
        )
        .await;

    assert_eq!(decision, Decision::Continue);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap_or_default(),
        "complete"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_dispatch_terminates_async_hook_descendants() {
    let pid_file = std::env::temp_dir().join(format!(
        "zerostack-hooks-async-cancel-descendant-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&pid_file);
    let command = format!(
        "sh -c 'echo $$ > \"{}\"; while :; do sleep 1; done' & wait",
        pid_file.display()
    );
    let config = config_with("Stop", None, vec![async_handler(&command)]);
    let dispatcher = std::sync::Arc::new(HookDispatcher::from_config(&config).unwrap());
    let dispatch = tokio::spawn({
        let dispatcher = std::sync::Arc::clone(&dispatcher);
        async move {
            dispatcher
                .dispatch(
                    "Stop",
                    None,
                    &ctx(),
                    EventFields::Stop {
                        stop_hook_active: false,
                        loop_iteration: None,
                        loop_active: None,
                    },
                )
                .await
        }
    });

    let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !pid_file.exists() && tokio::time::Instant::now() < ready_deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let descendant_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("async descendant should start before dispatch cancellation")
        .trim()
        .parse()
        .unwrap();
    dispatch.abort();
    let _ = dispatch.await;

    let cleanup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while process_is_alive(descendant_pid) && tokio::time::Instant::now() < cleanup_deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!process_is_alive(descendant_pid));
    let _ = std::fs::remove_file(pid_file);
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test]
async fn dispatch_post_tool_use_failure_runs_but_cannot_change_outcome() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-posttooluse-failure-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let cmd = format!("touch {}", marker.display());
    let config = config_with("PostToolUseFailure", None, vec![handler(&cmd)]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();

    dispatcher
        .dispatch_post_tool_use_failure(&ctx(), "bash", serde_json::json!({}), "boom")
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(marker.exists());
}

#[tokio::test]
async fn if_condition_true_runs_the_handler() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![handler_with_condition(
            r#"echo '{"permissionDecision":"deny"}'"#,
            "true",
        )],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(decision.verdict, Verdict::Deny);
}

#[tokio::test]
async fn if_condition_false_skips_the_handler() {
    let config = config_with(
        "PreToolUse",
        None,
        vec![handler_with_condition(
            r#"echo '{"permissionDecision":"deny"}'"#,
            "false",
        )],
    );
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(decision.verdict, Verdict::Defer);
}

#[tokio::test]
async fn if_condition_broken_command_fails_closed_and_runs_anyway() {
    // A condition that hangs past its timeout counts as "cannot be
    // evaluated" per the fail-closed requirement: the handler still runs.
    let handler = HookHandler {
        timeout: Some(1),
        ..handler_with_condition(r#"echo '{"permissionDecision":"deny"}'"#, "sleep 30")
    };
    let config = config_with("PreToolUse", None, vec![handler]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();
    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(decision.verdict, Verdict::Deny);
}

#[tokio::test]
async fn condition_and_handler_share_explicit_environment_and_project_cwd() {
    let mut configured = handler_with_condition(
        r#"test "$HOOK_POLICY" = expected && test "$PWD" = "$ZEROSTACK_PROJECT_DIR" && echo '{"permissionDecision":"deny"}'"#,
        r#"test "$HOOK_POLICY" = expected && test "$PWD" = "$ZEROSTACK_PROJECT_DIR""#,
    );
    configured
        .env
        .insert("HOOK_POLICY".to_string(), "expected".to_string());
    let config = config_with("PreToolUse", None, vec![configured]);
    let dispatcher = HookDispatcher::from_config_with_backend(&config, "unused").unwrap();

    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;

    assert_eq!(decision.verdict, Verdict::Deny);
}

#[tokio::test]
async fn unavailable_required_sandbox_denies_condition_and_handler_before_launch() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-policy-denied-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let mut configured = handler_with_condition(
        &format!("touch {}", marker.display()),
        &format!("touch {}", marker.display()),
    );
    configured.trust = crate::extras::hooks::settings::HookTrust::Sandboxed;
    let config = config_with("PreToolUse", None, vec![configured]);
    let dispatcher =
        HookDispatcher::from_config_with_backend(&config, "__mini_agent_missing_hook_sandbox__")
            .unwrap();

    let decision = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;

    assert_eq!(decision.verdict, Verdict::Deny);
    assert!(!marker.exists());
}

#[tokio::test]
async fn once_policy_denial_is_retried_instead_of_consumed() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-once-policy-denied-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let mut configured = handler_once(&format!("touch {}", marker.display()));
    configured.trust = crate::extras::hooks::settings::HookTrust::Sandboxed;
    let config = config_with("PreToolUse", None, vec![configured]);
    let dispatcher =
        HookDispatcher::from_config_with_backend(&config, "__mini_agent_missing_hook_sandbox__")
            .unwrap();

    for _ in 0..2 {
        let decision = dispatcher
            .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
            .await;
        assert_eq!(decision.verdict, Verdict::Deny);
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn false_condition_does_not_consume_once_binding() {
    let condition_marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-once-condition-{}",
        std::process::id()
    ));
    let output_marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-once-condition-output-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&condition_marker);
    let _ = std::fs::remove_file(&output_marker);
    let mut configured = handler_with_condition(
        &format!("printf x >> {}", output_marker.display()),
        &format!("test -f {}", condition_marker.display()),
    );
    configured.once = true;
    let config = config_with("PreToolUse", None, vec![configured]);
    let dispatcher = HookDispatcher::from_config_with_backend(&config, "unused").unwrap();

    let first = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(first.verdict, Verdict::Defer);
    std::fs::write(&condition_marker, b"ready").unwrap();
    for _ in 0..2 {
        let _ = dispatcher
            .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
            .await;
    }

    assert_eq!(std::fs::read_to_string(&output_marker).unwrap(), "x");
    let _ = std::fs::remove_file(condition_marker);
    let _ = std::fs::remove_file(output_marker);
}

#[tokio::test]
async fn immutable_policy_root_prevents_cwd_retargeting() {
    let base =
        std::env::temp_dir().join(format!("zerostack-hooks-bound-root-{}", std::process::id()));
    let project_a = base.join("a");
    let project_b = base.join("b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();
    let marker_a = project_a.join("ran");
    let marker_b = project_b.join("ran");
    let command = r#"test "$PWD" = "$ZEROSTACK_PROJECT_DIR" && touch ran"#;
    let config = config_with("PreToolUse", None, vec![handler(command)]);
    let dispatcher =
        HookDispatcher::from_config_with_backend_and_root(&config, "unused", &project_a).unwrap();
    let mut changed_ctx = ctx();
    changed_ctx.cwd = project_b.to_string_lossy().into_owned();

    let _ = dispatcher
        .dispatch_pre_tool_use(&changed_ctx, "bash", serde_json::json!({}))
        .await;

    assert!(marker_a.exists());
    assert!(!marker_b.exists());
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test]
async fn failed_execution_root_rebind_is_sticky_until_an_explicit_valid_rebind() {
    let base = std::env::temp_dir().join(format!(
        "zerostack-hooks-sticky-invalid-root-{}",
        uuid::Uuid::new_v4()
    ));
    let initial = base.join("initial");
    let missing = base.join("missing");
    std::fs::create_dir_all(&initial).unwrap();
    let config = config_with("PreToolUse", None, vec![handler("touch ran")]);
    let dispatcher =
        HookDispatcher::from_config_with_backend_and_root(&config, "unused", &initial).unwrap();

    assert!(dispatcher.rebind_execution_root(&missing).is_err());
    std::fs::create_dir_all(&missing).unwrap();
    let denied = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(denied.verdict, Verdict::Deny);
    assert!(
        !missing.join("ran").exists(),
        "creating the failed path later must not revive stale execution authority"
    );

    dispatcher.rebind_execution_root(&missing).unwrap();
    let allowed = dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    assert_eq!(allowed.verdict, Verdict::Defer);
    assert!(missing.join("ran").exists());
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test]
async fn rebind_during_condition_invalidates_handler_launch_lease() {
    let base = std::env::temp_dir().join(format!(
        "zerostack-hooks-concurrent-rebind-{}",
        uuid::Uuid::new_v4()
    ));
    let initial = base.join("initial");
    let selected = base.join("selected");
    let ready = base.join("condition-ready");
    let release = base.join("condition-release");
    std::fs::create_dir_all(&initial).unwrap();
    std::fs::create_dir_all(&selected).unwrap();
    let mut configured = handler_with_condition(
        "touch ran",
        "touch \"$READY\"; while [ ! -f \"$RELEASE\" ]; do sleep 0.01; done",
    );
    configured
        .env
        .insert("READY".to_string(), ready.to_string_lossy().into_owned());
    configured.env.insert(
        "RELEASE".to_string(),
        release.to_string_lossy().into_owned(),
    );
    let config = config_with("PreToolUse", None, vec![configured]);
    let dispatcher = std::sync::Arc::new(
        HookDispatcher::from_config_with_backend_and_root(&config, "unused", &initial).unwrap(),
    );
    let running = {
        let dispatcher = std::sync::Arc::clone(&dispatcher);
        tokio::spawn(async move {
            dispatcher
                .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
                .await
        })
    };

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !ready.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition child must reach the launch barrier");
    dispatcher.rebind_execution_root(&selected).unwrap();
    std::fs::write(&release, b"go").unwrap();

    let decision = running.await.unwrap();
    assert_eq!(decision.verdict, Verdict::Deny);
    assert!(
        !selected.join("ran").exists(),
        "a handler from the old dispatch must not launch in the rebound workspace"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[tokio::test]
async fn directory_identity_replacement_after_condition_denies_handler_launch() {
    let base = std::env::temp_dir().join(format!(
        "zerostack-hooks-root-replacement-{}",
        uuid::Uuid::new_v4()
    ));
    let selected = base.join("selected");
    let original = base.join("original");
    let ready = base.join("condition-ready");
    let release = base.join("condition-release");
    std::fs::create_dir_all(&selected).unwrap();
    let mut configured = handler_with_condition(
        "touch ran",
        "touch \"$READY\"; while [ ! -f \"$RELEASE\" ]; do sleep 0.01; done",
    );
    configured
        .env
        .insert("READY".to_string(), ready.to_string_lossy().into_owned());
    configured.env.insert(
        "RELEASE".to_string(),
        release.to_string_lossy().into_owned(),
    );
    let config = config_with("PreToolUse", None, vec![configured]);
    let dispatcher = std::sync::Arc::new(
        HookDispatcher::from_config_with_backend_and_root(&config, "unused", &selected).unwrap(),
    );
    let running = {
        let dispatcher = std::sync::Arc::clone(&dispatcher);
        tokio::spawn(async move {
            dispatcher
                .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
                .await
        })
    };

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !ready.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition child must reach the launch barrier");
    std::fs::rename(&selected, &original).unwrap();
    std::fs::create_dir(&selected).unwrap();
    std::fs::write(&release, b"go").unwrap();

    let decision = running.await.unwrap();
    assert_eq!(decision.verdict, Verdict::Deny);
    assert!(
        !selected.join("ran").exists(),
        "replacement directory identity must not inherit hook execution authority"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[tokio::test]
async fn once_identity_keeps_distinct_argv_and_environment_bindings_independent() {
    let marker = std::env::temp_dir().join(format!(
        "zerostack-hooks-once-policy-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let command = format!("printf \"$VALUE\" >> {}", marker.display());
    let config = config_with(
        "Stop",
        None,
        vec![
            handler_once_with_env(&command, "VALUE", "first"),
            handler_once_with_env(&command, "VALUE", "second"),
        ],
    );
    let dispatcher = HookDispatcher::from_config_with_backend(&config, "unused").unwrap();

    for _ in 0..2 {
        let _ = dispatcher
            .dispatch(
                "Stop",
                None,
                &ctx(),
                EventFields::Stop {
                    stop_hook_active: false,
                    loop_iteration: None,
                    loop_active: None,
                },
            )
            .await;
    }

    let contents = std::fs::read_to_string(&marker).unwrap();
    assert!(contents == "firstsecond" || contents == "secondfirst");
    let _ = std::fs::remove_file(marker);
}

#[tokio::test]
async fn once_handler_runs_on_first_dispatch_and_is_skipped_on_second() {
    let marker = std::env::temp_dir().join(format!("zerostack-hooks-once-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let cmd = format!("printf x >> {}", marker.display());
    let config = config_with("PreToolUse", None, vec![handler_once(&cmd)]);
    let dispatcher = HookDispatcher::from_config(&config).unwrap();

    dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;
    dispatcher
        .dispatch_pre_tool_use(&ctx(), "bash", serde_json::json!({}))
        .await;

    let contents = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(contents, "x", "handler with once:true must not run twice");
}
