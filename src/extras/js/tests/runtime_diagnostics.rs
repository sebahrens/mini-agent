//! Model-visible runtime diagnostics, queueing and teardown behaviour of the `js` tool
//! (mini-agent-nvnkx.114/.115/.117/.122/.123/.125/.170/.174/.329).

use std::sync::Arc;
use std::time::{Duration, Instant};

use rig::tool::Tool;

use super::{TestTempDir, make_test_tool, make_test_tool_with_permissions};
use crate::extras::js::audit::EffectAudit;
use crate::extras::js::host::AllowConfig;
use crate::extras::js::supervisor::JsWorkerSupervisor;
use crate::extras::js::tool::{JsArgs, JsTool};
use crate::sandbox::Sandbox;
use crate::sandbox::worker::TestWorkerLauncher;

fn shared_parts(
    dir: &TestTempDir,
) -> (
    Arc<JsWorkerSupervisor>,
    crate::extras::js::broker::SharedEffectAudit,
) {
    let root = dir.path().join(format!("audit-{}", uuid::Uuid::new_v4()));
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
    (
        Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            TestWorkerLauncher::internal_worker_process(),
        )),
        Arc::new(std::sync::Mutex::new(audit)),
    )
}

fn tool_with(
    config: AllowConfig,
    supervisor: &Arc<JsWorkerSupervisor>,
    audit: &crate::extras::js::broker::SharedEffectAudit,
) -> JsTool {
    JsTool::new_with_runtime_for_test(
        Sandbox::new(false, "bwrap").with_complete_process_tree_for_test(),
        None,
        None,
        config,
        supervisor.clone(),
        audit.clone(),
    )
}

async fn run(tool: &JsTool, code: &str) -> String {
    tool.call(JsArgs { code: code.into() })
        .await
        .unwrap_or_else(|error| format!("TOOL ERROR: {error}"))
}

#[tokio::test]
async fn an_awaited_promise_that_never_settles_is_not_a_job_limit() {
    let audit_dirs = TestTempDir::new("js-unsettled");
    let tool = make_test_tool(&audit_dirs);
    let out = run(&tool, "await new Promise(() => {}); 1").await;
    assert!(out.contains("never settled"), "{out}");
    assert!(out.contains("no timers or callbacks"), "{out}");
    assert!(!out.contains("job limit"), "{out}");
    // The real job cap keeps its own wording.
    let spin = run(
        &tool,
        "function spin(){ Promise.resolve().then(spin); } spin(); await new Promise(r => r())",
    )
    .await;
    assert!(spin.contains("job limit exceeded"), "{spin}");
}

#[tokio::test]
async fn reference_errors_name_the_identifier_and_suggest_a_catalogued_helper() {
    let audit_dirs = TestTempDir::new("js-identifier");
    let tool = make_test_tool(&audit_dirs);
    let out = run(&tool, "const a = 1;\nconst doc = yaml_load('a: 1');\ndoc").await;
    assert!(out.starts_with("JS ReferenceError at "), "{out}");
    assert!(out.contains("near `yaml_load`"), "{out}");
    assert!(out.contains("did you mean `yaml_parse`?"), "{out}");
    assert!(out.contains("(stage: evaluation; script: model)"), "{out}");

    // A token at the reported position is only trusted when it is a plain identifier.
    let call = run(&tool, "const o = {};\no.run();").await;
    assert!(call.contains("near `o.run`"), "{call}");
    let keyword_start = run(&tool, "const o = {};\nconst n = o.items.length;\nn").await;
    assert!(!keyword_start.contains("near `const`"), "{keyword_start}");
}

#[tokio::test]
async fn the_named_identifier_is_always_the_models_own_source_text() {
    let audit_dirs = TestTempDir::new("js-identifier-source");
    let root = audit_dirs.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("secret.txt"), "SECRET_FILE_CONTENT").unwrap();
    let tool = make_test_tool(&audit_dirs);
    // The thrown value and a string literal built from file data never reach the diagnostic.
    let out = run(
        &tool,
        &format!(
            "const t = read_file({:?});\nthrow new ReferenceError(t);",
            root.join("secret.txt").to_string_lossy()
        ),
    )
    .await;
    assert!(!out.contains("SECRET_FILE_CONTENT"), "{out}");
}

#[tokio::test]
async fn an_uncaught_effect_failure_names_the_effect_code_and_reason() {
    let audit_dirs = TestTempDir::new("js-effect-failure");
    let tool = make_test_tool_with_permissions(
        &audit_dirs,
        Sandbox::new(false, "bwrap"),
        Some(super::restrictive_permission_allowing_js_entrypoint()),
        None,
    );
    let out = run(&tool, "read_file('Cargo.toml')").await;
    assert!(out.starts_with("JS exception at "), "{out}");
    assert!(
        out.contains("; last failed effect: read_file -> denied (permission policy denied it)"),
        "{out}"
    );
    // A script that catches the failure and then throws something unrelated is still rendered
    // with the closed vocabulary only: the thrown text never appears.
    let out = run(
        &tool,
        "try { read_file('Cargo.toml') } catch (e) {}\nthrow new Error('attacker text');",
    )
    .await;
    assert!(
        out.contains("last failed effect: read_file -> denied"),
        "{out}"
    );
    assert!(!out.contains("attacker text"), "{out}");
}

#[tokio::test]
async fn a_missing_file_is_distinguishable_from_a_policy_denial() {
    let audit_dirs = TestTempDir::new("js-effect-not-found");
    let tool = make_test_tool(&audit_dirs);
    let out = run(&tool, "read_file('definitely/not/here.txt')").await;
    assert!(
        out.contains("last failed effect: read_file -> not_found"),
        "{out}"
    );
}

#[tokio::test]
async fn the_description_states_the_file_policy_instead_of_advertising_denied_globals() {
    let dir = TestTempDir::new("js-description-policy");
    let base = dir.path();
    std::fs::create_dir_all(base.join("src")).unwrap();
    let (supervisor, audit) = shared_parts(&dir);
    let tool_for = |read: Option<&[String]>, write: Option<&[String]>| {
        let config = AllowConfig::from_settings(base, None, read, write, false, false);
        tool_with(config, &supervisor, &audit)
    };
    let denied = tool_for(None, None).description();
    assert!(!denied.contains("`read_file(path"), "{denied}");
    assert!(!denied.contains("`write_file(path"), "{denied}");
    assert!(denied.contains("File reads are disabled (no js-read-roots set)"));
    assert!(denied.contains("write_file is disabled (js-write-roots unset)"));

    let roots = vec!["src".to_string()];
    let scoped = tool_for(Some(&roots), Some(&roots)).description();
    assert!(scoped.contains("`read_file(path"), "{scoped}");
    assert!(scoped.contains("File reads are limited to: src (js-read-roots)."));
    assert!(scoped.contains("write_file is limited to: src (js-write-roots)."));
}

#[tokio::test]
async fn the_description_lists_every_catalogued_pack_helper_by_name() {
    let dir = TestTempDir::new("js-description-pack");
    let description = make_test_tool(&dir).description();
    let names: Vec<&str> = crate::extras::js::pack::catalog_names().collect();
    assert!(names.len() > 20, "{names:?}");
    for name in names {
        assert!(description.contains(name), "description must name {name}");
    }
}

#[tokio::test]
async fn queueing_behind_another_call_is_not_charged_to_the_step_deadline() {
    let dir = TestTempDir::new("js-queue");
    let (supervisor, audit) = shared_parts(&dir);
    let config = AllowConfig::unrestricted(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let first = tool_with(config.clone(), &supervisor, &audit).with_step_timeout_secs(1);
    let second = tool_with(config, &supervisor, &audit).with_step_timeout_secs(1);
    let first_run = tokio::spawn(async move { run(&first, "while (true) {}").await });
    // Let the first call take the lease and spin until its 1 s step deadline ends it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let queued_for = Instant::now();
    let second_out = run(&second, "40 + 2").await;
    assert_eq!(
        second_out, "42",
        "the queued call must run with a full budget"
    );
    assert!(
        queued_for.elapsed() >= Duration::from_millis(300),
        "the second call must have waited behind the first"
    );
    let first_out = first_run.await.unwrap();
    assert!(
        first_out.starts_with("JS error: execution timed out"),
        "{first_out}"
    );
}

#[tokio::test]
async fn a_call_that_waits_past_the_queue_cap_reports_that_it_never_ran() {
    let dir = TestTempDir::new("js-queue-cap");
    let (supervisor, audit) = shared_parts(&dir);
    let config = AllowConfig::unrestricted(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let second = tool_with(config, &supervisor, &audit).with_step_timeout_secs(1);
    // Hold the lease for longer than the queue cap (3 x the 1 s step) without a step deadline
    // applying to this holder: it sits in an effect only the 2 s hard bound ends.
    let lease_holder = {
        let cancellation = crate::extras::js::types::PermCancellation::new();
        supervisor
            .acquire_lease(&cancellation, Instant::now() + Duration::from_secs(5))
            .await
            .expect("lease")
    };
    let out = run(&second, "1 + 1").await;
    drop(lease_holder);
    assert!(
        out.starts_with("JS error: waited ") && out.contains("for another js call to finish"),
        "{out}"
    );
    assert!(out.contains("the script did not run"), "{out}");
    assert!(!out.contains("timed out"), "{out}");
}

#[tokio::test]
async fn scratch_delete_and_keys_round_trip_and_a_full_store_names_its_limit() {
    let dir = TestTempDir::new("js-scratch-ops");
    let tool = make_test_tool(&dir);
    assert_eq!(
        run(
            &tool,
            "scratch_put('a', 1); scratch_put('b', {x: 2});\nJSON.stringify([scratch_keys(), scratch_delete('a'), scratch_delete('a'), scratch_keys(), scratch_get('b')])"
        )
        .await,
        r#"[["a","b"],true,false,["b"],{"x":2}]"#
    );
    // Fill to the entry cap: the failure names the entry limit and the remedy, and deleting a
    // key frees a slot again.
    let full = run(
        &tool,
        "for (let i = 0; i < 127; i++) scratch_put('k' + i, i);\ntry { scratch_put('overflow', 1); 'stored' } catch (e) { e.code + ': ' + e.message }",
    )
    .await;
    assert!(full.starts_with("too_large"), "{full}");
    assert!(
        full.contains("128 entries") && full.contains("scratch_delete"),
        "{full}"
    );
    assert_eq!(
        run(
            &tool,
            "scratch_delete('k0'); scratch_put('overflow', 1); scratch_keys().length"
        )
        .await,
        "128"
    );
    let uncaught = run(&tool, "scratch_put('another', 1)").await;
    assert!(
        uncaught.contains(
            "last failed effect: scratch_put -> too_large (scratch store entry limit reached)"
        ),
        "{uncaught}"
    );
}

#[tokio::test]
async fn a_miss_on_a_fresh_scratch_store_says_it_may_have_been_reset() {
    let dir = TestTempDir::new("js-scratch-reset");
    let tool = make_test_tool(&dir);
    let first = run(&tool, "scratch_get('turn:1') === null").await;
    assert!(
        first.starts_with("true\n[note: the scratch store is empty"),
        "{first}"
    );
    assert!(first.contains("--resume"), "{first}");
    let second = run(&tool, "scratch_get('turn:1') === null").await;
    assert_eq!(second, "true", "the note appears once per store");
    run(&tool, "scratch_put('turn:1', 1); 1").await;
    let written = run(&tool, "scratch_get('other') === null").await;
    assert_eq!(written, "true", "a written store is not reported as reset");
}
