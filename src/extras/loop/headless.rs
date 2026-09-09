use std::future::Future;
use std::io;
use std::path::PathBuf;

use uuid::Uuid;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::extras::r#loop as loop_mod;
use crate::extras::status_signals::StatusSignals;
use crate::provider::AnyAgent;
use crate::sandbox::Sandbox;
use crate::session::Session;

#[cfg(any(feature = "hooks", all(test, unix)))]
fn hook_loop_active(iteration: u32, max_iterations: Option<u32>) -> bool {
    max_iterations.is_none_or(|max| iteration < max)
}

async fn await_validation_or_interrupt<F>(
    operation: loop_mod::validation::ValidationOperation,
    interrupt: F,
) -> io::Result<(loop_mod::validation::ValidationResult, bool)>
where
    F: Future<Output = io::Result<()>>,
{
    let cancellation = operation.cancellation();
    let wait = operation.wait();
    tokio::pin!(wait);
    tokio::pin!(interrupt);

    tokio::select! {
        result = &mut wait => Ok((result, false)),
        signal = &mut interrupt => {
            cancellation.cancel();
            // The scoped worker reports only after the validator group is
            // terminated and its direct child is reaped.
            let result = wait.await;
            signal?;
            Ok((result, true))
        }
    }
}

pub(crate) async fn run_headless_loop(
    agent: AnyAgent,
    cli: &Cli,
    cfg: &Config,
    _context: &ContextFiles,
    session: &Session,
    status_signals: Option<StatusSignals>,
    sandbox: &Sandbox,
) -> anyhow::Result<()> {
    let prompt = cli
        .loop_prompt
        .clone()
        .or_else(|| {
            let msg = cli.message.join(" ");
            if msg.is_empty() { None } else { Some(msg) }
        })
        .ok_or_else(|| anyhow::anyhow!("No loop prompt. Use --loop-prompt or pass a message."))?;

    let plan_file = cli
        .loop_plan
        .clone()
        .unwrap_or_else(|| PathBuf::from(loop_mod::DEFAULT_PLAN_FILENAME));
    let max_iterations = cli.loop_max;
    let run_cmd = cli.loop_run.clone();
    let session_id = Uuid::new_v4().to_string();
    // Keep the loop's summary-based prompt strategy, but retain every completed
    // turn in the durable session for --continue, including failed iterations.
    let mut saved_session = session.clone();

    let use_existing = loop_mod::plan::handle_startup(&plan_file).await?;
    if !use_existing {
        // No plan exists — agent will generate one on first iteration
    }

    let mut state = loop_mod::LoopState::new(prompt, plan_file, max_iterations, run_cmd);

    loop {
        state.iteration += 1;

        if state.should_stop() {
            eprintln!(
                "[loop] max iterations ({}) reached, stopping",
                state.max_iterations.unwrap_or(0)
            );
            break;
        }

        let iteration_prompt = state.build_prompt();

        eprintln!("=== {} ===", state.iteration_label());
        eprintln!();

        if let Some(ss) = status_signals.as_ref() {
            ss.send_start();
        }
        let turn = agent
            .run_print(
                &iteration_prompt,
                cli.pure_stdout,
                true,
                &cfg.retry,
                iteration_history(session, cfg),
                #[cfg(feature = "hooks")]
                Some(crate::extras::hooks::LoopInfo {
                    iteration: state.iteration,
                    active: hook_loop_active(state.iteration, state.max_iterations),
                }),
            )
            .await;
        if let Some(ss) = status_signals.as_ref() {
            ss.send_stop();
        }
        let response = settle_iteration(
            &mut saved_session,
            &iteration_prompt,
            turn,
            cfg,
            cli.no_session,
        )?;

        let summary: String = response
            .chars()
            .take(loop_mod::SUMMARY_TRUNCATION_CHARS)
            .collect();
        state.last_summary = Some(summary.clone());

        let validation_output = if let Some(cmd) = &state.run_cmd {
            eprintln!(
                "--- Validation: {} ---",
                loop_mod::validation::display_command(cmd)
            );
            let operation = loop_mod::validation::start(sandbox, cmd);
            let (result, interrupted) =
                await_validation_or_interrupt(operation, tokio::signal::ctrl_c()).await?;
            let diagnostic = result.render();
            eprintln!("{}", diagnostic);
            if interrupted {
                eprintln!("[loop] interrupted during validation");
                return Ok(());
            }
            Some(diagnostic)
        } else {
            None
        };
        state.last_run_output = validation_output.clone();

        if let Err(e) = loop_mod::transcript::save_iteration(
            &session_id,
            state.iteration,
            &iteration_prompt,
            &response,
            validation_output.as_deref(),
            &summary,
        ) {
            eprintln!("[loop] warning: failed to save transcript: {}", e);
        }

        eprintln!("--- iteration {} complete, looping ---\n", state.iteration);
    }

    Ok(())
}

fn settle_iteration(
    session: &mut Session,
    prompt: &str,
    turn: crate::agent::runner::HeadlessTurn,
    cfg: &Config,
    no_session: bool,
) -> anyhow::Result<String> {
    let crate::agent::runner::HeadlessTurn {
        response,
        usage,
        interactions,
        failure,
    } = turn;
    let persistence = if no_session {
        Ok(())
    } else {
        crate::print::persist_headless_turn(session, prompt, &response, &interactions);
        session.charge_usage_delta(usage.into(), cfg.is_anthropic_native(&session.provider));
        crate::session::storage::save_session(session)
    };
    if let Some(failure) = failure {
        return Err(match persistence {
            Ok(()) => failure,
            Err(error) => failure.context(format!(
                "the partial loop iteration could not be persisted either: {error}"
            )),
        });
    }
    persistence?;
    Ok(response)
}

fn iteration_history(
    session: &Session,
    cfg: &Config,
) -> std::sync::Arc<[rig::completion::Message]> {
    crate::agent::runner::convert_history_shared_with_tool_result_retention(
        session,
        cfg.resolve_keep_recent_tool_results(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::extras::r#loop::validation::ValidationStatus;

    #[test]
    fn resumed_session_history_is_forwarded_to_each_loop_iteration() {
        let mut session = Session::new("provider", "model", 128_000, "");
        session.add_message(crate::session::MessageRole::User, "prior turn");
        assert_eq!(iteration_history(&session, &Config::default()).len(), 1);
    }

    #[test]
    fn final_bounded_iteration_is_not_reported_as_looping() {
        assert!(hook_loop_active(1, Some(2)));
        assert!(!hook_loop_active(2, Some(2)));
        assert!(hook_loop_active(99, None));
    }

    #[tokio::test]
    async fn headless_sigint_path_cancels_and_awaits_scoped_validation() {
        let operation = loop_mod::validation::start(
            &Sandbox::new(false, "bwrap"),
            "trap '' TERM; while :; do :; done",
        );
        let started = Instant::now();
        let (result, interrupted) = await_validation_or_interrupt(operation, async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        })
        .await
        .unwrap();

        assert!(interrupted);
        assert_eq!(result.status, ValidationStatus::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::agent::runner::HeadlessTurn;
    use crate::session::{MessageRole, storage};
    use clap::Parser;
    use rig::client::CompletionClient;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct StateRoot {
        path: PathBuf,
        _environment: crate::tests::ScopedProcessEnv,
    }

    impl StateRoot {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("mini-agent-loop-progress-{}", Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let environment = crate::tests::ScopedProcessEnv::set(&[
                ("ZS_DATA_DIR", Some(root.clone().into_os_string())),
                ("ZS_STATE_DIR", Some(root.clone().into_os_string())),
                ("ZS_CONFIG_DIR", Some(root.clone().into_os_string())),
            ]);
            Self {
                path: root,
                _environment: environment,
            }
        }
    }

    impl Drop for StateRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn failed_loop_iteration_saves_completed_effect_and_usage_unless_disabled() {
        #[cfg(feature = "hooks")]
        let _hooks = crate::tests::fake_model::dispatcher_guard::acquire();
        for no_session in [false, true] {
            let root = StateRoot::new();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let call = serde_json::json!({
                "id": "loop-turn", "object": "chat.completion.chunk", "created": 0, "model": "test",
                "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                    "index": 0, "id": "loop-write", "type": "function", "function": {
                        "name": "write", "arguments": "{\"path\":\"effect.txt\",\"content\":\"written\\n\"}"
                    }
                }]}, "finish_reason": null}]
            });
            let finish = serde_json::json!({
                "id": "loop-turn", "object": "chat.completion.chunk", "created": 0, "model": "test",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
            });
            let stream_body = format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
            let error_body =
                r#"{"error":{"message":"loop provider failure","type":"invalid_request_error"}}"#
                    .to_string();
            let server = tokio::spawn(async move {
                for (status, content_type, body) in [
                    ("200 OK", "text/event-stream", stream_body),
                    ("400 Bad Request", "application/json", error_body),
                ] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let byte = socket.read_u8().await.unwrap();
                        request.push(byte);
                        assert!(request.len() < 64 * 1024, "request headers must be bounded");
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let headers = String::from_utf8(request).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    assert!(length < 1024 * 1024);
                    socket.read_exact(&mut vec![0; length]).await.unwrap();
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(reply.as_bytes()).await.unwrap();
                }
            });
            let client = rig::providers::openrouter::Client::builder()
                .api_key("test-key")
                .base_url(format!("http://{address}"))
                .http_client(reqwest::Client::builder().no_proxy().build().unwrap())
                .build()
                .unwrap();
            let agent = client
                .agent("test")
                .tool(
                    crate::agent::tools::WriteTool::new(None, None, None)
                        .with_workspace(&root.path),
                )
                .default_max_turns(4)
                .build();
            let agent = AnyAgent::without_skills(crate::provider::AnyAgentInner::OpenRouter(agent));
            let mut cli = Cli::parse_from(["mini-agent"]);
            cli.loop_prompt = Some("write the file".into());
            cli.loop_plan = Some(root.path.join("plan.md"));
            cli.loop_max = Some(1);
            cli.no_session = no_session;
            let mut session = Session::new("openrouter", "test", 128_000, "");
            session.add_message(MessageRole::User, "prior turn");
            session.total_input_tokens = 7;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                run_headless_loop(
                    agent,
                    &cli,
                    &Config::default(),
                    &crate::context::load(true),
                    &session,
                    None,
                    &Sandbox::new(false, "bwrap"),
                ),
            )
            .await
            .expect("headless loop must terminate after provider failure");
            server.abort();
            let _ = server.await;
            let error = result.expect_err("provider failure must propagate");
            assert!(
                format!("{error:#}").contains("loop provider failure"),
                "{error:#}"
            );
            assert_eq!(
                std::fs::read_to_string(root.path.join("effect.txt")).unwrap(),
                "written\n"
            );
            let saved = storage::load_session_exact(&session.id).unwrap();
            if no_session {
                assert!(saved.is_none(), "--no-session must suppress persistence");
                continue;
            }
            let saved = saved.expect("failed iteration must leave a resumable session");
            assert_eq!(saved.messages[0].content, "prior turn");
            assert_eq!(saved.total_input_tokens, 107);
            assert_eq!(saved.total_output_tokens, 20);
            for role in [MessageRole::ToolCall, MessageRole::ToolResult] {
                assert!(
                    saved.messages.iter().any(|message| message.role == role
                        && message.tool_call_id.as_deref() == Some("loop-write")),
                    "missing persisted {role:?}: {:?}",
                    saved.messages
                );
            }
            let result = saved
                .messages
                .iter()
                .find(|message| message.role == MessageRole::ToolResult)
                .unwrap();
            assert!(
                result.content.contains("Written 8 bytes to"),
                "{}",
                result.content
            );
            assert!(result.content.contains("effect.txt"), "{}", result.content);
        }
    }

    #[test]
    fn successful_loop_iterations_accumulate_once_in_the_saved_session() {
        let _root = StateRoot::new();
        let mut session = Session::new("openrouter", "test", 128_000, "");
        for (prompt, response) in [
            ("first prompt", "first reply"),
            ("next prompt", "next reply"),
        ] {
            let returned = settle_iteration(
                &mut session,
                prompt,
                HeadlessTurn {
                    response: response.into(),
                    usage: rig::completion::Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                        total_tokens: 5,
                        ..Default::default()
                    },
                    interactions: Vec::new(),
                    failure: None,
                },
                &Config::default(),
                false,
            )
            .unwrap();
            assert_eq!(returned, response);
        }
        let saved = storage::load_session_exact(&session.id).unwrap().unwrap();
        assert_eq!(saved.total_input_tokens, 6);
        assert_eq!(saved.total_output_tokens, 4);
        assert_eq!(
            saved
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["first prompt", "first reply", "next prompt", "next reply"]
        );
    }

    #[test]
    fn loop_persistence_failures_are_reported_without_hiding_the_turn_failure() {
        let root = StateRoot::new();
        std::fs::write(root.path.join("sessions"), "blocks session directory").unwrap();
        for failure in [None, Some(anyhow::anyhow!("original provider failure"))] {
            let had_failure = failure.is_some();
            let mut session = Session::new("openrouter", "test", 128_000, "");
            let result = settle_iteration(
                &mut session,
                "prompt",
                HeadlessTurn {
                    response: "partial response".into(),
                    usage: Default::default(),
                    interactions: Vec::new(),
                    failure,
                },
                &Config::default(),
                false,
            );
            let error = format!(
                "{:#}",
                result.expect_err("persistence errors cannot become success")
            );
            if had_failure {
                assert!(error.contains("original provider failure"), "{error}");
                assert!(error.contains("could not be persisted either"), "{error}");
            }
        }
    }
}
