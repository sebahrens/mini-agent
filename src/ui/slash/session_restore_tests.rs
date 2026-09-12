//! Exercise session activation through the actual frontend, builder and tool loop.

use super::*;
use clap::Parser;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct FixtureRoot(PathBuf);

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Two real provider requests per turn: dispatch tools, then observe their
// results before completing. Joining the server and caller under one deadline
// leaves no detached accept/read task when either side fails.
async fn run_probe(
    agent: &AnyAgent,
    listener: &tokio::net::TcpListener,
    marker: &str,
    js: bool,
) -> Vec<Value> {
    let server = async {
        let mut requests = Vec::new();
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (header_len, body_len) = loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0, "provider request closed before its headers");
                bytes.extend_from_slice(&chunk[..count]);
                assert!(bytes.len() < 1024 * 1024);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                    let len = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    assert!(len < 1024 * 1024);
                    break (end + 4, len);
                }
            };
            while bytes.len() < header_len + body_len {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0, "provider request closed before its body");
                bytes.extend_from_slice(&chunk[..count]);
            }
            requests.push(
                serde_json::from_slice::<Value>(&bytes[header_len..header_len + body_len]).unwrap(),
            );
            let mut calls = vec![
                ("read", json!({"path":"probe.txt", "offset":0, "limit":300})),
                (
                    "todo_write",
                    json!({"todos":[{"content":marker, "status":"pending", "priority":"high"}]}),
                ),
            ];
            #[cfg(unix)]
            calls.push(("shell", json!({"command":"pwd"})));
            if js {
                calls.push(("js", json!({"code":format!("const prior = scratch_get('owner'); scratch_put('last_probe', {marker:?}); prior")})));
            }
            let delta = if turn == 0 {
                let calls: Vec<_> = calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, args))| {
                        json!({
                            "index":index, "id":format!("probe-{name}"), "type":"function",
                            "function":{"name":name, "arguments":args.to_string()}
                        })
                    })
                    .collect();
                json!({"role":"assistant", "tool_calls":calls})
            } else {
                json!({"role":"assistant", "content":"probe complete"})
            };
            let chunk = json!({"id":"probe-turn", "object":"chat.completion.chunk", "created":0,
                "model":"fixture", "choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            let finish = json!({"id":"probe-turn", "object":"chat.completion.chunk", "created":0,
                "model":"fixture", "choices":[{"index":0,"delta":{},"finish_reason":if turn == 0 {"tool_calls"} else {"stop"}}],
                "usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}});
            let body = format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    };
    let retry = crate::retry::RetryConfig {
        max_attempts: 1,
        ..Default::default()
    };
    let caller = agent.run_print(
        "exercise session tools",
        true,
        false,
        &retry,
        Vec::<rig::completion::Message>::new(),
        #[cfg(feature = "hooks")]
        None,
    );
    let (requests, result) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(server, caller)
    })
    .await
    .expect("session probe must settle");
    assert!(result.failure.is_none(), "{:?}", result.failure);
    assert_eq!(result.response, "probe complete");
    requests
}

fn tool_result<'a>(requests: &'a [Value], name: &str) -> &'a str {
    requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| {
            message["role"] == "tool" && message["tool_call_id"] == format!("probe-{name}")
        })
        .unwrap_or_else(|| panic!("missing {name} result: {}", requests[1]))["content"]
        .as_str()
        .unwrap()
}

fn assert_spill(session: &mut Session, requests: &[Value]) {
    let text = tool_result(requests, "read");
    assert!(text.contains("[tool output truncated:"), "{text}");
    session.add_tool_result_with_id("probe-read", "read", text);
    let Some(crate::session::PersistedToolMessage::Result {
        artifact_path: Some(path),
        ..
    }) = &session.messages.last().unwrap().tool
    else {
        panic!(
            "session {} must receive its agent's live spill",
            session.name
        );
    };
    assert_eq!(
        std::path::Path::new(path.as_str()).parent().unwrap(),
        crate::session::storage::tool_output_dir(&session.id)
    );
    let full = std::fs::read_to_string(path.as_str()).unwrap();
    assert!(full.contains("ACTIVE_WORKSPACE_PAYLOAD"));
    assert!(full.chars().count() > crate::session::TOOL_RESULT_SAVE_THRESHOLD);
}

#[tokio::test]
async fn replacement_binds_incoming_owners_and_preserves_active_authority_on_failure() {
    for provider_changed in [false, true] {
        exercise_replacement(provider_changed).await;
    }
}

async fn exercise_replacement(provider_changed: bool) {
    let root = FixtureRoot(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "mini-agent-session-restore-{}",
        uuid::Uuid::new_v4()
    )));
    std::fs::create_dir_all(root.0.join("active")).unwrap();
    std::fs::create_dir(root.0.join("foreign")).unwrap();
    let _environment = crate::tests::ScopedProcessEnv::set(&[
        (
            "ZS_CONFIG_DIR",
            Some(root.0.join("config").into_os_string()),
        ),
        ("ZS_DATA_DIR", Some(root.0.join("data").into_os_string())),
        ("ZS_STATE_DIR", Some(root.0.join("state").into_os_string())),
        ("ZS_CACHE_DIR", Some(root.0.join("cache").into_os_string())),
    ]);
    let workspace =
        Arc::new(crate::paths::WorkspaceBinding::capture(&root.0.join("active")).unwrap());
    std::fs::write(
        workspace.root().join("probe.txt"),
        "ACTIVE_WORKSPACE_PAYLOAD abcdefghijklmnopqrstuvwxyz 0123456789\n".repeat(300),
    )
    .unwrap();
    std::fs::write(root.0.join("foreign/probe.txt"), "WRONG_WORKSPACE").unwrap();
    let cwd_before = std::env::current_dir().unwrap();
    let js = {
        #[cfg(feature = "js")]
        {
            let containment = crate::sandbox::worker::containment_status();
            eprintln!("session restore JS coverage: {containment:?}");
            matches!(
                containment,
                crate::sandbox::worker::WorkerContainmentStatus::Available { .. }
            )
        }
        #[cfg(not(feature = "js"))]
        {
            false
        }
    };
    let mut selected_tools = "read,todo_write".to_string();
    #[cfg(unix)]
    selected_tools.push_str(",shell");
    if js {
        selected_tools.push_str(",js");
    }
    let cli = Cli::parse_from([
        "mini-agent",
        "--no-session",
        "--api-key",
        "unused-test-key",
        "--no-sandbox",
        "--tools",
        &selected_tools,
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let cfg: Config = serde_json::from_value(json!({"custom_providers": {
        "replacement-fixture": {"provider_type":"openai", "base_url":endpoint, "api_style":"completions"}
    }})).unwrap();
    let mut context = ContextFiles {
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
    let mut client = AnyClient::OpenRouter(
        rig::providers::openrouter::Client::builder()
            .api_key("unused-test-key")
            .base_url(format!("http://{}", listener.local_addr().unwrap()))
            .build()
            .unwrap(),
    );
    let mut session = Session::new("openrouter", "outgoing-model", 128_000, "outgoing");
    let outgoing_id = session.id.clone();
    let outgoing_todos = session.todos.clone();
    #[cfg(feature = "js")]
    let outgoing_scratch = session.js_session_state.for_workspace(workspace.root());
    #[cfg(feature = "js")]
    outgoing_scratch
        .put("owner".into(), "\"outgoing\"".into())
        .unwrap();
    let sandbox = Sandbox::new(false, "none").with_workspace_binding(workspace.clone());
    #[cfg(unix)]
    let sandbox = sandbox.with_resolved_shell(Some(
        crate::sandbox::ShellCapability::resolve("/bin/sh", workspace.root(), None).unwrap(),
    ));
    let mut agent = None;
    let mut renderer = Renderer::new().unwrap();
    let mut input = InputEditor::new();
    let mut terminal_guard = TerminalGuard::detached_for_test();
    let invalidated = std::sync::atomic::AtomicBool::new(false);
    let mut show_reasoning = false;
    let mut reasoning_enabled = false;
    let mut is_running = false;
    let mut todo_tools_enabled = true;
    #[cfg(feature = "skills")]
    let skill_services = Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new());
    let mut ctx = SlashCtx {
        prebuild_invalidated: &invalidated,
        agent: &mut agent,
        client: &mut client,
        renderer: &mut renderer,
        session: &mut session,
        cli: &cli,
        cfg: &cfg,
        context: &mut context,
        workspace: &workspace,
        show_reasoning: &mut show_reasoning,
        reasoning_enabled: &mut reasoning_enabled,
        is_running: &mut is_running,
        input: &mut input,
        permission: &None,
        ask_tx: &None,
        todo_tools_enabled: &mut todo_tools_enabled,
        sandbox: &sandbox,
        terminal_guard: &mut terminal_guard,
        #[cfg(feature = "skills")]
        skill_services: &skill_services,
        #[cfg(feature = "mcp")]
        mcp_manager: None,
    };
    ctx.rebuild_agent().await;
    let before = run_probe(ctx.agent.as_ref().unwrap(), &listener, "before", js).await;
    assert_eq!(before[0]["model"], "outgoing-model");
    assert_spill(ctx.session, &before);

    let incoming_provider = if provider_changed {
        "replacement-fixture"
    } else {
        "openrouter"
    };
    let mut incoming = Session::new(incoming_provider, "incoming-model", 256_000, "incoming");
    incoming.working_dir = root.0.join("foreign").to_string_lossy().as_ref().into();
    let incoming_id = incoming.id.clone();
    #[cfg(feature = "js")]
    let incoming_scratch = incoming.js_session_state.for_workspace(workspace.root());
    #[cfg(feature = "js")]
    incoming_scratch
        .put("owner".into(), "\"incoming\"".into())
        .unwrap();
    ctx.replace_session(incoming).await.unwrap();
    let after = run_probe(ctx.agent.as_ref().unwrap(), &listener, "after", js).await;
    assert_eq!(after[0]["model"], "incoming-model");
    assert_eq!(ctx.session.id, incoming_id);
    assert_ne!(ctx.session.id, outgoing_id);
    assert_eq!(
        ctx.session.working_dir.as_str(),
        workspace.root().to_str().unwrap()
    );
    assert_eq!(ctx.context.workspace_root, workspace.root());
    assert_eq!(
        ctx.sandbox.workspace_root_for_test(),
        Some(workspace.root())
    );
    assert_eq!(std::env::current_dir().unwrap(), cwd_before);
    #[cfg(unix)]
    {
        assert_eq!(
            ctx.sandbox.shell_capability().unwrap().executable(),
            sandbox.shell_capability().unwrap().executable()
        );
        assert!(tool_result(&after, "shell").contains(workspace.root().to_str().unwrap()));
    }
    assert_eq!(outgoing_todos.snapshot()[0].content, "before");
    assert_eq!(ctx.session.todos.snapshot()[0].content, "after");
    #[cfg(feature = "js")]
    if js {
        assert!(
            tool_result(&after, "js").contains("incoming"),
            "{}",
            tool_result(&after, "js")
        );
        assert_eq!(
            outgoing_scratch.get("last_probe").unwrap().as_deref(),
            Some("\"before\"")
        );
        assert_eq!(
            incoming_scratch.get("last_probe").unwrap().as_deref(),
            Some("\"after\"")
        );
    }
    assert_spill(ctx.session, &after);

    let saved = serde_json::to_value(&*ctx.session).unwrap();
    let mut invalid = Session::new(
        "nonexistent-session-restore-provider",
        "invalid-model",
        1,
        "invalid",
    );
    invalid.working_dir = root.0.join("foreign").to_string_lossy().as_ref().into();
    assert!(ctx.replace_session(invalid).await.is_err());
    assert_eq!(serde_json::to_value(&*ctx.session).unwrap(), saved);
    assert_eq!(ctx.session.provider, incoming_provider);
    assert_eq!(matches!(ctx.client, AnyClient::OpenAI(_)), provider_changed);
    let recovered = run_probe(ctx.agent.as_ref().unwrap(), &listener, "recovered", js).await;
    assert_eq!(recovered[0]["model"], "incoming-model");
    assert!(
        tool_result(&recovered, "read").contains("read blocked:"),
        "failed activation must retain the active read tracker"
    );
    assert_eq!(ctx.session.todos.snapshot()[0].content, "recovered");
    assert_eq!(outgoing_todos.snapshot()[0].content, "before");
    #[cfg(unix)]
    assert!(tool_result(&recovered, "shell").contains(workspace.root().to_str().unwrap()));
    #[cfg(feature = "js")]
    if js {
        assert_eq!(
            incoming_scratch.get("last_probe").unwrap().as_deref(),
            Some("\"recovered\"")
        );
        assert_eq!(
            outgoing_scratch.get("last_probe").unwrap().as_deref(),
            Some("\"before\"")
        );
    }
    assert_eq!(
        ctx.session.working_dir.as_str(),
        workspace.root().to_str().unwrap()
    );
    assert_eq!(std::env::current_dir().unwrap(), cwd_before);
}
