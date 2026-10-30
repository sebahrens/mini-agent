//! Scheduling controls use a local completed snapshot; no test may upgrade the
//! process-wide platform attestation or replace its production full preflight.
use super::*;
use crate::agent::runner::AgentWorkScope;
use crate::sandbox::worker::{
    StartupWorkerProbe, WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus,
};
use std::sync::{Condvar, Mutex, OnceLock};

struct Release(Arc<(Mutex<bool>, Condvar)>);
impl Release {
    fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}

async fn build(
    wire: &Wire,
    root: &std::path::Path,
    snapshot: Option<WorkerContainmentStatus>,
) -> AnyAgent {
    build_with_context(
        wire,
        root,
        snapshot,
        "openrouter",
        NativeJsContext::default(),
    )
    .await
}

async fn build_with_context(
    wire: &Wire,
    root: &std::path::Path,
    snapshot: Option<WorkerContainmentStatus>,
    protocol: &str,
    native_js_context: NativeJsContext,
) -> AnyAgent {
    let cli = Cli {
        tools: vec!["js".into()],
        no_context_files: true,
        ..Default::default()
    };
    let cfg = Config::default();
    let context = crate::context::load_for_workspace(true, Some(root));
    build_agent_with_probe_snapshot(
        wire.client_for(protocol)
            .completion_model("startup-fixture"),
        &cli,
        &cfg,
        &context,
        Arc::new(crate::paths::WorkspaceBinding::capture(root).unwrap()),
        None,
        None,
        Sandbox::new(false, "none"),
        crate::agent::tools::ReadTracker::new(true),
        crate::agent::tools::TodoStore::default(),
        #[cfg(feature = "goal")]
        crate::extras::goal::GoalStore::default(),
        "startup-probe-wire",
        None,
        native_js_context,
        false,
        None,
        None,
        crate::extras::js::session::JsSessionStateOwner::default(),
        #[cfg(feature = "skills")]
        Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
        #[cfg(feature = "mcp")]
        None,
        snapshot,
    )
    .await
}

fn reply() -> String {
    let chunk = |delta: Value, finish: Value| {
        format!(
            "data: {}\n\n",
            json!({"id":"startup","object":"chat.completion.chunk","created":0,"model":"startup-fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
        )
    };
    chunk(json!({"role":"assistant","content":"ready"}), Value::Null)
        + &chunk(json!({}), json!("stop"))
        + "data: [DONE]\n\n"
}

#[tokio::test(flavor = "current_thread")]
async fn startup5_first_http_overlaps_probe_and_completed_snapshot_changes_only_next_bundle() {
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("startup5-provider-races");
    let names = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
        "MINI_AGENT_HOME",
    ];
    let values: Vec<_> = names
        .iter()
        .map(|name| {
            let path = root.path().join(name);
            crate::fs::ensure_private_directory(&path).unwrap();
            (*name, Some(path.into_os_string()))
        })
        .collect();
    let _environment = crate::tests::ScopedProcessEnv::set(&values);
    let wire = Wire::at_path(vec![(true, reply()); 3], "/chat/completions");
    let completed = Arc::new(OnceLock::new());
    let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
    let held = release.0.clone();
    let slot = completed.clone();
    let scope = AgentWorkScope::new();
    let (started, entered) = tokio::sync::oneshot::channel();
    let probe = StartupWorkerProbe::start_for_test(scope.clone(), move || {
        let _ = started.send(());
        let mut ready = held.0.lock().unwrap();
        while !*ready {
            ready = held.1.wait(ready).unwrap();
        }
        let status = WorkerContainmentStatus::Available {
            backend: WorkerBackend::for_current_platform(),
            assurance: WorkerContainmentAssurance::Enforced,
        };
        slot.set(status.clone()).unwrap();
        status
    })
    .await;
    entered.await.unwrap();
    let pending = build(&wire, root.path(), completed.get().cloned()).await;
    assert!(pending.js_probe_pending);
    #[cfg(feature = "skills")]
    assert!(
        pending.skills.is_none(),
        "unfinished verification cannot initialize learned services"
    );
    let turn = tokio::time::timeout(
        Duration::from_secs(2),
        pending.run_print(
            "first literal",
            true,
            false,
            &RetryConfig::default(),
            Vec::<Message>::new(),
            #[cfg(feature = "hooks")]
            None,
        ),
    )
    .await
    .expect("first request waited for held worker verification");
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    assert_eq!(turn.response, "ready");
    assert!(completed.get().is_none());
    assert_eq!(
        scope.active_children(),
        1,
        "first HTTP completed before probe retirement"
    );
    release.release();
    probe.retire().await.unwrap();
    assert_eq!(scope.active_children(), 0);
    let rebuilt = build(&wire, root.path(), completed.get().cloned()).await;
    assert!(!rebuilt.js_probe_pending);
    for (agent, prompt) in [
        (&rebuilt, "next settled turn"),
        (&pending, "old bundle remains immutable"),
    ] {
        let turn = agent
            .run_print(
                prompt,
                true,
                false,
                &RetryConfig::default(),
                Vec::<Message>::new(),
                #[cfg(feature = "hooks")]
                None,
            )
            .await;
        assert!(turn.failure.is_none(), "{:?}", turn.failure);
    }
    let requests = wire.finish();
    assert_eq!(requests.len(), 3);
    let js = |body: &Value| {
        body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "js"))
    };
    assert!(!js(&requests[0]), "held probe must not advertise JS");
    assert!(js(&requests[1]), "probe-first next build must advertise JS");
    assert!(
        !js(&requests[2]),
        "a late result cannot mutate an existing tool bundle"
    );
}

/// Exercise accepted signed content, canonical bounded save/reload, unrelated
/// agent rebuild and a fresh native session. The local proof snapshot is test
/// data only; the test never invokes JS or upgrades the platform attestation.
#[tokio::test(flavor = "current_thread")]
async fn startup5_native_losing_inventory_survives_rebuild_and_reload_without_prefix_change() {
    use crate::session::native_replay::{ReplayBoundary, RetiredNativeOutcome};
    #[cfg(feature = "hooks")]
    let _dispatcher = crate::tests::fake_model::dispatcher_guard::acquire();
    let root = crate::tests::OwnedTestDirectory::new("startup5-native-pin");
    let names = [
        "MINI_AGENT_CONFIG_DIR",
        "MINI_AGENT_DATA_DIR",
        "MINI_AGENT_LOCAL_DATA_DIR",
        "MINI_AGENT_STATE_DIR",
        "MINI_AGENT_CACHE_DIR",
        "MINI_AGENT_CREDENTIALS_DIR",
        "MINI_AGENT_HOME",
    ];
    let values: Vec<_> = names
        .iter()
        .map(|name| {
            let path = root.path().join(name);
            crate::fs::ensure_private_directory(&path).unwrap();
            (*name, Some(path.into_os_string()))
        })
        .collect();
    let _environment = crate::tests::ScopedProcessEnv::set(&values);
    let signed = || start() + &startup_thinking() + &text(1, "ready") + &finish();
    let wire = Wire::new(vec![
        (true, signed()),
        (true, start() + &text(0, "resumed") + &finish()),
        (true, start() + &text(0, "fresh") + &finish()),
    ]);
    let available = || {
        Some(WorkerContainmentStatus::Available {
            backend: WorkerBackend::for_current_platform(),
            assurance: WorkerContainmentAssurance::Enforced,
        })
    };
    let mut session = crate::session::Session::new("anthropic", "startup-fixture", 32_000, "");
    let first = build_with_context(
        &wire,
        root.path(),
        None,
        "anthropic",
        session.native_js_context(),
    )
    .await;
    assert!(first.js_probe_pending);
    #[cfg(feature = "skills")]
    assert!(first.skills.is_none());
    let (first, binding) = first
        .bind_native_turn(
            &session,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            RecordedToolIds::Canonical,
        )
        .unwrap();
    let turn = first
        .run_print(
            "first literal",
            true,
            false,
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    let prices = crate::print::HeadlessPricing::from_session(&session, &Config::default());
    binding
        .commit_canonical_into_with_outcome(
            &mut session,
            "first literal",
            &turn.response,
            &turn.interactions,
            turn.usage.into(),
            prices,
            RetiredNativeOutcome::Completed,
        )
        .unwrap();
    assert!(session.native_js_context().omitted);
    let json = serde_json::to_string(&session).unwrap();
    assert!(json.len() <= crate::session::storage::MAX_SESSION_BYTES);
    let mut restored = crate::session::storage::decode_session_json(&json).unwrap();
    assert!(restored.native_js_context().omitted);
    let resumed = build_with_context(
        &wire,
        root.path(),
        available(),
        "anthropic",
        restored.native_js_context(),
    )
    .await;
    assert!(!resumed.js_probe_pending);
    #[cfg(feature = "skills")]
    assert!(
        resumed.skills.is_none(),
        "verification cannot add learned/proposal services to this signed prefix"
    );
    let (resumed, binding) = resumed
        .bind_native_turn(
            &restored,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            RecordedToolIds::Canonical,
        )
        .unwrap();
    let turn = resumed
        .run_print(
            "second literal",
            true,
            false,
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(turn.failure.is_none(), "{:?}", turn.failure);
    binding
        .commit_canonical_into_with_outcome(
            &mut restored,
            "second literal",
            &turn.response,
            &turn.interactions,
            turn.usage.into(),
            prices,
            RetiredNativeOutcome::Completed,
        )
        .unwrap();
    assert_eq!(restored.total_input_tokens, 20);
    assert_eq!(restored.total_output_tokens, 4);
    // Imported data may restrict capability, but clearing that restriction must
    // not authorize a changed actual tools/system prefix against signed blocks.
    let mut tampered: Value = serde_json::from_str(&json).unwrap();
    tampered["native_history"]["host"]["startup_js_omitted"] = json!(false);
    let tampered = crate::session::storage::decode_session_json(&tampered.to_string()).unwrap();
    let changed = build_with_context(
        &wire,
        root.path(),
        available(),
        "anthropic",
        tampered.native_js_context(),
    )
    .await;
    let (changed, binding) = changed
        .bind_native_turn(
            &tampered,
            Vec::<Message>::new().into(),
            ReplayBoundary::CompletedTurn,
            RecordedToolIds::Canonical,
        )
        .unwrap();
    let refused = changed
        .run_print(
            "changed imported pin",
            true,
            false,
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            binding.history(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(
        refused.failure.is_some(),
        "a serialized restriction is never replay authority"
    );
    let fresh = build_with_context(
        &wire,
        root.path(),
        available(),
        "anthropic",
        NativeJsContext::default(),
    )
    .await;
    let fresh_turn = fresh
        .run_print(
            "fresh literal",
            true,
            false,
            &RetryConfig {
                max_attempts: 1,
                ..Default::default()
            },
            Vec::<Message>::new(),
            #[cfg(feature = "hooks")]
            None,
        )
        .await;
    assert!(fresh_turn.failure.is_none(), "{:?}", fresh_turn.failure);
    let requests = wire.finish();
    assert_eq!(
        requests.len(),
        3,
        "changed imported pin must refuse before HTTP"
    );
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert_eq!(requests[0]["system"], requests[1]["system"]);
    let js = |body: &Value| {
        body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "js"))
    };
    assert!(!js(&requests[0]));
    assert!(!js(&requests[1]));
    assert!(
        js(&requests[2]),
        "a new session may adopt completed verification"
    );
    assert_eq!(
        requests[1]["messages"]
            .to_string()
            .matches("startup5-original-signature")
            .count(),
        1
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("first literal")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("second literal")
    );
}

fn startup_thinking() -> String {
    event(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    ) + &event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"startup5-original-signature"}}),
    ) + &event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    )
}
