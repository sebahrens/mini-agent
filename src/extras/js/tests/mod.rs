#[cfg(feature = "skills")]
mod agent_skill_catalog;
#[cfg(feature = "skills")]
mod auto_admission_end_to_end;
#[cfg(feature = "skills")]
mod capability_manifest_v2;
#[cfg(feature = "skills")]
mod evidence_policy_scheduler;
#[cfg(feature = "skills")]
mod evidence_promotion_policy;
#[cfg(feature = "skills")]
mod phase5_operations_benchmark;
#[cfg(feature = "skills")]
mod propose_skill_host;
#[cfg(feature = "skills")]
mod self_learning_end_to_end;
#[cfg(feature = "skills")]
mod skill_admission_gate;
#[cfg(feature = "skills")]
mod skill_admission_schema;
#[cfg(feature = "skills")]
mod skill_canary_routing;
#[cfg(feature = "skills")]
mod skill_embedder;
#[cfg(feature = "skills")]
mod skill_event_attribution;
#[cfg(feature = "skills")]
mod skill_held_out_evaluator;
#[cfg(feature = "skills")]
mod skill_index;
#[cfg(feature = "skills")]
mod skill_lifecycle_schema;
#[cfg(feature = "skills")]
mod skill_quarantine_policy;
#[cfg(feature = "skills")]
mod skill_realm_isolation;
#[cfg(feature = "skills")]
mod skill_repair_and_rollback;
#[cfg(feature = "skills")]
mod skill_repair_records;
#[cfg(feature = "skills")]
mod skill_retrieval_benchmark;
#[cfg(feature = "skills")]
mod skill_runtime_binding;
#[cfg(feature = "skills")]
mod skill_runtime_prompt;
#[cfg(feature = "skills")]
mod skill_store_identity;
#[cfg(feature = "skills")]
mod skill_store_schema;
#[cfg(feature = "skills")]
mod skill_targeted_feedback;
#[cfg(feature = "skills")]
mod skill_telemetry_retention;
#[cfg(feature = "skills")]
mod skill_verification_semantics;
mod tool_console_output;
mod vendor_integrity;
mod worker_broker;
mod worker_containment;
mod worker_effect_cancellation;
mod worker_effect_services;
mod worker_fault_matrix;
mod worker_protocol;
mod worker_resource_benchmark;
pub(super) mod worker_runtime;

use crate::extras::js::audit::EffectAudit;
use crate::extras::js::host::AllowConfig;
use crate::extras::js::supervisor::JsWorkerSupervisor;
use crate::extras::js::tool::JsTool;
use crate::permission::ask::AskSender;
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{PermissionConfig, PermissionConfigs, SecurityMode};
use crate::sandbox::Sandbox;
use crate::sandbox::worker::TestWorkerLauncher;

struct TestTempDir(std::path::PathBuf);

impl TestTempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mini-agent-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_test_tool() -> JsTool {
    make_test_tool_with_sandbox(Sandbox::new(false, "bwrap"))
}

fn make_test_tool_with_sandbox(sandbox: Sandbox) -> JsTool {
    make_test_tool_with_permissions(sandbox, None, None)
}

fn make_test_tool_with_permissions(
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> JsTool {
    make_test_tool_with_permissions_and_process_tree(
        sandbox.with_complete_process_tree_for_test(),
        permission,
        ask_tx,
    )
}

fn make_test_tool_with_permissions_and_process_tree(
    sandbox: Sandbox,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
) -> JsTool {
    let root =
        std::env::temp_dir().join(format!("mini-agent-js-test-audit-{}", uuid::Uuid::new_v4()));
    let paths = crate::paths::AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        local_data_dir: root.join("local"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        credentials_dir: root.join("credentials"),
        project_dir: None,
    };
    let audit = EffectAudit::open(paths.effect_audit()).expect("test effect audit");
    JsTool::new_with_runtime_for_test(
        sandbox,
        permission,
        ask_tx,
        AllowConfig::unrestricted(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))),
        std::sync::Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            TestWorkerLauncher::internal_worker_process(),
        )),
        std::sync::Arc::new(std::sync::Mutex::new(audit)),
    )
}

fn make_test_tool_in_workspace(
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    permission: PermCheck,
) -> JsTool {
    let root = std::env::temp_dir().join(format!(
        "mini-agent-js-workspace-audit-{}",
        uuid::Uuid::new_v4()
    ));
    let paths = crate::paths::AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        local_data_dir: root.join("local"),
        state_dir: root.join("state"),
        cache_dir: root.join("cache"),
        credentials_dir: root.join("credentials"),
        project_dir: None,
    };
    let audit = EffectAudit::open(paths.effect_audit()).expect("test effect audit");
    JsTool::new_with_runtime_for_test(
        Sandbox::new(false, "bwrap")
            .with_workspace_binding(workspace.clone())
            .with_complete_process_tree_for_test(),
        Some(permission),
        None,
        AllowConfig::unrestricted(workspace.root()).with_workspace_binding(workspace),
        std::sync::Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            TestWorkerLauncher::internal_worker_process(),
        )),
        std::sync::Arc::new(std::sync::Mutex::new(audit)),
    )
}

fn restrictive_permission_allowing_js_entrypoint() -> PermCheck {
    let config = PermissionConfig {
        allow_entries: Some(
            [("js".to_string(), vec!["**".to_string()])]
                .into_iter()
                .collect(),
        ),
        ..PermissionConfig::default()
    };
    std::sync::Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Restrictive,
            std::env::current_dir().ok(),
            Some(vec!["restrictive".to_string()]),
        )
        .expect("valid permission test configuration"),
    ))
}

fn restrictive_permission_denying_js_entrypoint() -> PermCheck {
    let config = PermissionConfig {
        deny_entries: Some(
            [("js".to_string(), vec!["**".to_string()])]
                .into_iter()
                .collect(),
        ),
        ..PermissionConfig::default()
    };
    std::sync::Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Restrictive,
            std::env::current_dir().ok(),
            Some(vec!["restrictive".to_string()]),
        )
        .expect("valid permission test configuration"),
    ))
}

#[tokio::test]
async fn test_return_value() {
    use rig::tool::Tool;
    let tool = make_test_tool();
    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "1 + 1".to_string(),
        })
        .await
        .expect("call failed");
    assert_eq!(result, "2", "expected '2' but got: {result}");
}

#[tokio::test]
async fn test_fetch_global_matches_sandbox_feature() {
    use rig::tool::Tool;
    let tool = make_test_tool();
    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "typeof fetch".to_string(),
        })
        .await
        .expect("call failed");
    assert_eq!(
        result,
        if cfg!(feature = "sandbox") {
            "function"
        } else {
            "undefined"
        }
    );
}

#[tokio::test]
async fn tool_description_prefers_javascript_for_computation() {
    use rig::tool::Tool;
    let description = make_test_tool().description();

    assert!(description.contains("Prefer this tool for computation"));
    assert!(description.contains("instead of invoking Python through a shell"));
    assert!(description.contains("strict script in a fresh runtime"));
    assert!(description.contains("top-level `await` supported"));
    assert!(description.contains("Host globals are synchronous"));
    assert!(description.contains("spawn(program: string, args: string[])"));
}

#[tokio::test]
async fn tool_result_contract_guides_explicit_json_stringification() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let rejected = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "({value: undefined})".to_string(),
        })
        .await
        .expect("worker should return a closed invalid-result diagnostic");
    assert!(rejected.contains("return a string/plain JSON value"));
    assert!(rejected.contains("JSON.stringify(value)"));

    let explicit = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "JSON.stringify({missing: undefined, nan: NaN, date: new Date(0), map: new Map([['key', 1]]), set: new Set([1]), instance: new (class Item { constructor() { this.value = 7; } })()})".to_string(),
        })
        .await
        .expect("explicit JSON stringification should produce a valid string result");
    assert_eq!(
        explicit,
        r#"{"nan":null,"date":"1970-01-01T00:00:00.000Z","map":{},"set":{},"instance":{"value":7}}"#
    );
}

#[tokio::test]
async fn tool_description_only_advertises_spawn_with_process_tree_ownership() {
    use rig::tool::Tool;
    let tool =
        make_test_tool_with_permissions_and_process_tree(Sandbox::new(false, "bwrap"), None, None);

    assert!(
        !tool
            .description()
            .contains("spawn(program: string, args: string[])")
    );
    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "typeof spawn".to_string(),
        })
        .await
        .expect("worker should execute without an unavailable spawn global");
    assert_eq!(result, "undefined");
}

#[tokio::test]
async fn worker_installs_spawn_when_process_tree_ownership_is_available() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "typeof spawn".to_string(),
        })
        .await
        .expect("worker should advertise the serviceable spawn global");
    assert_eq!(result, "function");
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn shipped_tool_omits_unserviceable_proposal_global() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    assert!(!tool.description().contains("propose_skill"));
    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "typeof propose_skill".to_string(),
        })
        .await
        .expect("call failed");
    assert_eq!(result, "undefined");
}

#[cfg(feature = "sandbox")]
#[tokio::test]
async fn test_fetch_options_fail_closed_before_network_io() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    for (code, expected) in [
        (
            "try { fetch('https://example.com', {method: 'DELETE'}); } catch (e) { String(e) }",
            "method must be GET or POST",
        ),
        (
            "try { fetch('https://example.com', {headers: {Host: 'evil'}}); } catch (e) { String(e) }",
            "header 'host' is controlled by the host",
        ),
        (
            "try { fetch('https://example.com', {unknown: true}); } catch (e) { String(e) }",
            "unsupported field 'unknown'",
        ),
        (
            "try { fetch('https://example.com', {method: 'POST', body: 'x'.repeat(262145)}); } catch (e) { String(e) }",
            "request body exceeds the configured limit",
        ),
    ] {
        let result = tool
            .call(crate::extras::js::tool::JsArgs {
                code: code.to_string(),
            })
            .await
            .expect("call failed");
        assert!(
            result.contains(expected),
            "expected {expected:?} in fetch error, got {result:?}"
        );
    }
}

#[tokio::test]
async fn test_read_write_roundtrip() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let path = std::env::temp_dir().join(format!("zs-test-roundtrip-{}.txt", uuid::Uuid::new_v4()));
    let path_str = path.to_string_lossy().to_string();

    let write_code = format!(
        "write_file({path:?}, 'hello from js'); 'ok'",
        path = path_str
    );
    let write_result = tool
        .call(crate::extras::js::tool::JsArgs { code: write_code })
        .await
        .expect("write call failed");
    assert_eq!(write_result, "ok", "write_file returned: {write_result}");

    let read_code = format!("read_file({path:?})", path = path_str);
    let read_result = tool
        .call(crate::extras::js::tool::JsArgs { code: read_code })
        .await
        .expect("read call failed");
    assert_eq!(
        read_result, "hello from js",
        "read_file returned: {read_result}"
    );
}

#[tokio::test]
async fn windows_workspace_authority_js_tool_relative_gold_eiffel() {
    use rig::tool::Tool;

    let temp = TestTempDir::new("js-workspace");
    let root = temp.path().to_path_buf();
    let workspace = std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&root).unwrap());
    let permission = std::sync::Arc::new(std::sync::Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::default(),
            SecurityMode::Standard,
            Some(root.clone()),
            Some(vec!["standard".to_string()]),
        )
        .unwrap(),
    ));
    let tool = make_test_tool_in_workspace(workspace, permission);

    assert_eq!(
        tool.call(crate::extras::js::tool::JsArgs {
            code: "write_file('gold-eiffel.js', 'from js tool'); read_file('gold-eiffel.js')"
                .to_string(),
        })
        .await
        .unwrap(),
        "from js tool"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("gold-eiffel.js")).unwrap(),
        "from js tool"
    );
}

#[tokio::test]
async fn test_spawn_captures_output_and_exit_code() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: r#"JSON.stringify(spawn("sh", ["-c", "printf out; printf err >&2; exit 7"]))"#
                .to_string(),
        })
        .await
        .expect("spawn call failed");

    assert_eq!(
        result,
        r#"{"stdout":"out","stderr":"err","code":7,"timed_out":false,"stdout_truncated":false,"stderr_truncated":false}"#
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_spawn_uses_configured_sandbox_wrapper() {
    use rig::tool::Tool;
    let sandbox = Sandbox::new(false, "bwrap").with_shell("false");
    let tool = make_test_tool_with_sandbox(sandbox);

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: r#"JSON.stringify(spawn("printf", ["must not run"]))"#.to_string(),
        })
        .await
        .expect("spawn call failed");

    assert_eq!(
        result,
        r#"{"stdout":"","stderr":"","code":1,"timed_out":false,"stdout_truncated":false,"stderr_truncated":false}"#
    );
}

#[tokio::test]
async fn test_host_globals_enforce_restrictive_permissions() {
    use rig::tool::Tool;

    let permission = restrictive_permission_allowing_js_entrypoint();
    let tool =
        make_test_tool_with_permissions(Sandbox::new(false, "bwrap"), Some(permission), None);
    let path = std::env::temp_dir().join(format!(
        "zs_js_permission_{}_{}.txt",
        std::process::id(),
        line!()
    ));
    let path_str = path.to_string_lossy();

    let read_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "read_file('Cargo.toml')".to_string(),
        })
        .await
        .expect("read call failed");
    assert!(
        read_result.starts_with("JS exception at ")
            && read_result.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {read_result}"
    );

    let write_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: format!("write_file({path_str:?}, 'forbidden')"),
        })
        .await
        .expect("write call failed");
    assert!(
        write_result.starts_with("JS exception at ")
            && write_result.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {write_result}"
    );
    assert!(
        !path.exists(),
        "denied write_file created {}",
        path.display()
    );

    let spawn_result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: format!("spawn('touch', [{path_str:?}])"),
        })
        .await
        .expect("spawn call failed");
    assert!(
        spawn_result.starts_with("JS exception at ")
            && spawn_result.ends_with("(stage: evaluation; script: model)"),
        "unexpected: {spawn_result}"
    );
    assert!(!path.exists(), "denied spawn created {}", path.display());
}

#[tokio::test]
async fn test_timeout() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "while (true) {}".to_string(),
        })
        .await
        .expect("timeout call failed");

    assert_eq!(result, "JS error: execution timed out (30s limit exceeded)");
}

#[tokio::test]
async fn test_exception_is_a_source_free_closed_error() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "throw new Error('test exception')".to_string(),
        })
        .await
        .expect("exception call failed");

    assert_eq!(
        result,
        "JS exception at 1:10 (stage: evaluation; script: model)"
    );
    assert!(!result.contains("test exception"));
}

#[tokio::test]
async fn test_permission_denied() {
    use rig::tool::Tool;
    let permission = restrictive_permission_denying_js_entrypoint();
    let tool =
        make_test_tool_with_permissions(Sandbox::new(false, "bwrap"), Some(permission), None);

    let error = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "1 + 1".to_string(),
        })
        .await
        .expect_err("denied JavaScript unexpectedly executed");

    assert!(
        error.to_string().contains("Permission denied"),
        "unexpected permission error: {error}"
    );
}

#[tokio::test]
async fn test_oom() {
    use rig::tool::Tool;
    let tool = make_test_tool();

    let result = tool
        .call(crate::extras::js::tool::JsArgs {
            code: "const chunks = []; while (true) { chunks.push(new ArrayBuffer(1024 * 1024)); }"
                .to_string(),
        })
        .await
        .expect("OOM call failed");

    assert!(
        result.starts_with("JS error: out of memory (64 MiB limit exceeded)"),
        "memory limit did not produce the classified OOM response: {result}"
    );
}
