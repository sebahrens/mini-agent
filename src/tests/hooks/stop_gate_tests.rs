use std::collections::HashMap;

use crate::extras::hooks::dispatcher::HookDispatcher;
use crate::extras::hooks::settings::{HookGroup, HookHandler, HooksConfig};
use crate::extras::hooks::{HookCtx, StopGate, gate_stop};

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

fn dispatcher_with(handlers: Vec<HookHandler>) -> HookDispatcher {
    let mut config: HooksConfig = HashMap::new();
    config.insert(
        "Stop".to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: handlers,
        }],
    );
    HookDispatcher::from_config(&config).unwrap()
}

fn ctx() -> HookCtx {
    HookCtx {
        session_id: "sess".into(),
        session_path: "".into(),
        cwd: super::TEST_WORKING_DIR.into(),
        permission_mode: "standard".into(),
    }
}

#[tokio::test]
async fn no_hooks_configured_releases() {
    let dispatcher = HookDispatcher::from_config(&HashMap::new()).unwrap();
    let gate = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    assert!(matches!(gate, StopGate::Release));
}

#[tokio::test]
async fn exit_zero_no_decision_releases() {
    let dispatcher = dispatcher_with(vec![handler("true")]);
    let gate = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    assert!(matches!(gate, StopGate::Release));
}

#[tokio::test]
async fn decision_block_forces_continuation_with_reason() {
    let dispatcher = dispatcher_with(vec![handler(
        r#"echo '{"decision":"block","reason":"tests still failing"}'"#,
    )]);
    let gate = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    match gate {
        StopGate::Continue { reason } => assert_eq!(reason, "tests still failing"),
        StopGate::Release => panic!("expected Continue"),
    }
}

#[tokio::test]
async fn exit_two_also_forces_continuation() {
    let dispatcher = dispatcher_with(vec![handler("echo 'not done yet' 1>&2; exit 2")]);
    let gate = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    match gate {
        StopGate::Continue { reason } => assert_eq!(reason.trim(), "not done yet"),
        StopGate::Release => panic!("expected Continue"),
    }
}

#[tokio::test]
async fn failing_stop_hook_releases_but_leaves_a_notice() {
    let dispatcher = dispatcher_with(vec![handler("exit 41")]);
    let gate = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    assert!(matches!(gate, StopGate::Release));
    let notices = crate::extras::hooks::take_hook_problem_notices();
    assert!(
        notices
            .iter()
            .any(|notice| notice == "hook 'sh' (Stop) exited 41; ignored (exit 2 blocks)"),
        "{notices:?}"
    );
    // The same problem is reported once per process.
    let _ = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    assert!(crate::extras::hooks::take_hook_problem_notices().is_empty());
}

#[tokio::test]
async fn nvnkx334_unlaunchable_stop_hook_releases_and_reports() {
    use crate::extras::hooks::pins::HookContentPins;
    let root = std::env::temp_dir().canonicalize().unwrap();
    let configured = handler("true");
    let mut config: HooksConfig = HashMap::new();
    config.insert(
        "Stop".to_string(),
        vec![HookGroup {
            matcher: None,
            hooks: vec![configured.clone()],
        }],
    );
    let pins = HashMap::from([(configured, HookContentPins::rejected("x"))]);
    let dispatcher =
        HookDispatcher::from_config_with_backend_root_and_pins(&config, "unused", &root, pins)
            .unwrap();
    let gate = gate_stop(&dispatcher, &ctx(), false, None, None, None).await;
    assert!(matches!(gate, StopGate::Release));
    let notices = crate::extras::hooks::take_hook_problem_notices();
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice.contains("hook 'sh' (Stop) could not launch"))
            .count(),
        1,
        "{notices:?}"
    );
}
