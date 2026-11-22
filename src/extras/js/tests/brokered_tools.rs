//! Brokered `call_tool` (mini-agent-nvnkx.116) and the batch effects `fetch_all` / `spawn_all`
//! (mini-agent-nvnkx.119) through a real contained worker.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rig::tool::Tool;

use super::TestTempDir;
use crate::extras::js::audit::EffectAudit;
use crate::extras::js::host::AllowConfig;
use crate::extras::js::supervisor::JsWorkerSupervisor;
use crate::extras::js::tool::{JsArgs, JsTool};
use crate::extras::js::tool_bridge::{
    ToolAccess, ToolBridge, ToolInvokeFailure, ToolInvokeFuture, ToolInvoker,
};
use crate::sandbox::Sandbox;
use crate::sandbox::worker::TestWorkerLauncher;

/// Deterministic stand-in for the agent's tool server.
#[derive(Default)]
struct FakeTools {
    calls: Mutex<Vec<(String, String)>>,
}

impl ToolInvoker for FakeTools {
    fn classify(&self, name: &str) -> ToolAccess {
        match name {
            "grep" | "failing" | "huge" => ToolAccess::Allowed,
            "edit" | "shell" => ToolAccess::NotReadOnly,
            _ => ToolAccess::Unknown,
        }
    }

    fn invoke<'a>(&'a self, name: &'a str, arguments_json: &'a str) -> ToolInvokeFuture<'a> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments_json.to_string()));
        Box::pin(async move {
            match name {
                "failing" => Err(ToolInvokeFailure::Failed("boom: bad pattern".into())),
                "huge" => Ok("x".repeat(2 * 1024 * 1024)),
                _ => Ok(format!("{name} saw {arguments_json}")),
            }
        })
    }
}

fn tool_with_bridge(
    dir: &TestTempDir,
    bridge: Option<ToolBridge>,
) -> (JsTool, Arc<Mutex<EffectAudit>>) {
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
    let audit = Arc::new(Mutex::new(
        EffectAudit::open(paths.effect_audit()).expect("test effect audit"),
    ));
    let mut tool = JsTool::new_with_runtime_for_test(
        Sandbox::new(false, "bwrap").with_complete_process_tree_for_test(),
        None,
        None,
        AllowConfig::unrestricted(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))),
        Arc::new(JsWorkerSupervisor::with_launcher_for_test(
            TestWorkerLauncher::internal_worker_process(),
        )),
        audit.clone(),
    );
    if let Some(bridge) = bridge {
        tool = tool.with_tool_bridge(bridge);
    }
    (tool, audit)
}

async fn run(tool: &JsTool, code: &str) -> String {
    tool.call(JsArgs { code: code.into() })
        .await
        .unwrap_or_else(|error| format!("TOOL ERROR: {error}"))
}

fn bound_bridge(fake: &Arc<FakeTools>) -> ToolBridge {
    let bridge = ToolBridge::default();
    bridge.bind(fake.clone()).unwrap();
    bridge
}

#[tokio::test]
async fn call_tool_runs_read_only_tools_in_a_loop_and_returns_only_the_aggregate() {
    let dir = TestTempDir::new("js-call-tool");
    let fake = Arc::new(FakeTools::default());
    let (tool, audit) = tool_with_bridge(&dir, Some(bound_bridge(&fake)));
    let out = run(
        &tool,
        "const a = call_tool('grep', {pattern: 'alpha'});\nconst b = call_tool('grep', {pattern: 'beta', path: 'src'});\nresult({texts: [a.text, b.text], ok: a.ok && b.ok, truncated: a.truncated})",
    )
    .await;
    // Key order of the re-serialized aggregate (and of the argument JSON the
    // fake echoes) depends on whether serde_json's `preserve_order` feature is
    // unified into this feature row, so compare parsed values.
    let parsed: serde_json::Value = serde_json::from_str(&out).expect(&out);
    let texts: Vec<serde_json::Value> = parsed["texts"]
        .as_array()
        .expect(&out)
        .iter()
        .map(|text| {
            let text = text.as_str().expect(&out);
            let args = text.strip_prefix("grep saw ").expect(text);
            serde_json::from_str(args).expect(text)
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            serde_json::json!({"pattern": "alpha"}),
            serde_json::json!({"pattern": "beta", "path": "src"}),
        ]
    );
    assert_eq!(parsed["ok"], serde_json::json!(true), "{out}");
    assert_eq!(parsed["truncated"], serde_json::json!(false), "{out}");
    assert_eq!(parsed.as_object().unwrap().len(), 3, "{out}");
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    // Each call is one audited effect that names the tool only by a keyed tag.
    let audit = audit.lock().unwrap();
    let call_tool_records: Vec<_> = audit
        .records()
        .iter()
        .filter(|record| record.capability == "call_tool")
        .collect();
    assert_eq!(call_tool_records.len(), 4, "intent + completion per call");
    let serialized = format!("{call_tool_records:?}");
    assert!(
        !serialized.contains("grep") && !serialized.contains("alpha"),
        "{serialized}"
    );
}

#[tokio::test]
async fn call_tool_reports_a_tool_error_as_data_and_refuses_unknown_and_mutating_tools() {
    let dir = TestTempDir::new("js-call-tool-denials");
    let fake = Arc::new(FakeTools::default());
    let (tool, _audit) = tool_with_bridge(&dir, Some(bound_bridge(&fake)));
    assert_eq!(
        run(
            &tool,
            "const r = call_tool('failing', {}); JSON.stringify([r.ok, r.text])"
        )
        .await,
        r#"[false,"boom: bad pattern"]"#
    );
    assert_eq!(
        run(
            &tool,
            "try { call_tool('nope', {}); 'ran' } catch (e) { e.code }"
        )
        .await,
        "not_found"
    );
    for name in ["edit", "shell"] {
        assert_eq!(
            run(
                &tool,
                &format!("try {{ call_tool('{name}', {{}}); 'ran' }} catch (e) {{ e.code }}")
            )
            .await,
            "denied",
            "{name} can change state and must not be callable"
        );
    }
    assert_eq!(
        run(
            &tool,
            "try { call_tool('grep', [1]); 'ran' } catch (e) { e.constructor.name }"
        )
        .await,
        "TypeError"
    );
    let calls = fake.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "only the failing read-only tool was invoked: {calls:?}"
    );
}

#[tokio::test]
async fn call_tool_cuts_an_oversized_result_and_flags_it() {
    let dir = TestTempDir::new("js-call-tool-size");
    let fake = Arc::new(FakeTools::default());
    let (tool, _audit) = tool_with_bridge(&dir, Some(bound_bridge(&fake)));
    assert_eq!(
        run(
            &tool,
            "const r = call_tool('huge'); JSON.stringify([r.ok, r.truncated, r.text.length])"
        )
        .await,
        "[true,true,1048576]"
    );
}

#[tokio::test]
async fn call_tool_is_absent_without_a_bridge_and_fails_closed_before_one_is_bound() {
    let dir = TestTempDir::new("js-call-tool-absent");
    let (plain, _audit) = tool_with_bridge(&dir, None);
    assert_eq!(run(&plain, "typeof call_tool").await, "undefined");
    assert!(!plain.description().contains("call_tool"));

    let (unbound, _audit) = tool_with_bridge(&dir, Some(ToolBridge::default()));
    assert_eq!(run(&unbound, "typeof call_tool").await, "function");
    assert_eq!(
        run(
            &unbound,
            "try { call_tool('grep', {}); 'ran' } catch (e) { e.code }"
        )
        .await,
        "backend_failure"
    );
    assert!(unbound.description().contains("`call_tool(name: string"));
}

#[tokio::test]
async fn call_tool_goes_through_the_real_tool_server() {
    use rig::tool::server::ToolServer;

    #[derive(Clone)]
    struct Probe(&'static str);

    #[derive(serde::Deserialize)]
    struct ProbeArgs {
        pattern: String,
    }

    impl Tool for Probe {
        const NAME: &'static str = "grep";
        type Error = crate::agent::tools::ToolError;
        type Args = ProbeArgs;
        type Output = String;

        fn description(&self) -> String {
            "probe".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn call(&self, args: ProbeArgs) -> Result<String, Self::Error> {
            Ok(format!("{}:{}", self.0, args.pattern))
        }
    }

    let handle = ToolServer::new().run();
    handle.add_tool(Probe("server")).await.unwrap();
    let bridge = ToolBridge::default();
    bridge
        .bind(Arc::new(
            crate::extras::js::tool_bridge::ServerToolInvoker::new(
                handle.downgrade(),
                ["grep".to_string(), "edit".to_string()],
                ["tracker_search".to_string()],
            ),
        ))
        .unwrap();
    let dir = TestTempDir::new("js-call-tool-server");
    let (tool, _audit) = tool_with_bridge(&dir, Some(bridge));
    assert_eq!(
        run(&tool, "call_tool('grep', {pattern: 'needle'}).text").await,
        "server:needle"
    );
    assert_eq!(
        run(
            &tool,
            "try { call_tool('edit', {}); 'ran' } catch (e) { e.code }"
        )
        .await,
        "denied",
        "a registered tool outside the read-only set stays unreachable"
    );
    drop(handle);
    assert_eq!(
        run(
            &tool,
            "try { call_tool('grep', {pattern: 'x'}); 'ran' } catch (e) { e.code }"
        )
        .await,
        "backend_failure",
        "a retired tool server fails closed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_all_runs_independent_commands_together_and_audits_each_one() {
    let dir = TestTempDir::new("js-spawn-all");
    let (tool, audit) = tool_with_bridge(&dir, None);
    let started = Instant::now();
    let out = run(
        &tool,
        "const rs = spawn_all([0, 1, 2, 3].map(i => ({program: 'sh', args: ['-c', 'sleep 0.6; echo item' + i]})));\nJSON.stringify(rs.map(r => r.stdout.trim()))",
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(out, r#"["item0","item1","item2","item3"]"#);
    assert!(
        elapsed < Duration::from_millis(2_000),
        "4 x 0.6 s commands must overlap: {elapsed:?}"
    );
    let audit = audit.lock().unwrap();
    let spawn_records = audit
        .records()
        .iter()
        .filter(|record| record.capability == "spawn")
        .count();
    assert_eq!(
        spawn_records, 8,
        "one intent and one completion per command"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_bad_spawn_all_item_is_an_item_error_and_the_batch_continues() {
    let dir = TestTempDir::new("js-spawn-all-error");
    let (tool, _audit) = tool_with_bridge(&dir, None);
    let out = run(
        &tool,
        "const rs = spawn_all([\n {program: 'sh', args: ['-c', 'echo one']},\n {program: 'definitely-not-a-real-program-xyz', args: []},\n {program: 'sh', args: ['-c', 'echo three']},\n]);\nJSON.stringify(rs.map(r => r.error ? 'error:' + r.error : r.stdout.trim()))",
    )
    .await;
    assert!(
        out.starts_with(r#"["one","error:"#) && out.ends_with(r#""three"]"#),
        "{out}"
    );
    let invalid = run(&tool, "try { spawn_all([]); 'ran' } catch (e) { e.code }").await;
    assert_eq!(invalid, "invalid_target");
}

#[cfg(feature = "sandbox")]
#[tokio::test]
async fn fetch_all_validates_each_item_before_any_network_use() {
    let dir = TestTempDir::new("js-fetch-all");
    let (tool, _audit) = tool_with_bridge(&dir, None);
    assert_eq!(
        run(&tool, "typeof fetch_all").await,
        "function",
        "fetch_all exists exactly when fetch does"
    );
    assert_eq!(
        run(
            &tool,
            "try { fetch_all([{url: ''}]); 'ran' } catch (e) { e.code }"
        )
        .await,
        "invalid_target"
    );
    assert_ne!(
        run(&tool, "try { fetch_all([{url: 'https://a.example', headers: 1}]); 'ran' } catch (e) { e.name }").await,
        "ran",
        "a malformed item is rejected before any request"
    );
}
