use std::path::PathBuf;

use crate::cli::Cli;
use crate::config::Config;
use crate::context::ContextFiles;
use crate::extras::r#loop as loop_mod;
use crate::extras::status_signals::StatusSignals;
use crate::provider::AnyAgent;
use crate::sandbox::Sandbox;
use crate::session::Session;

/// Install the goal a `--loop` run is, before the agent is built.
///
/// Order matters: the agent's preamble carries the goal block, and that block
/// is captured when the agent is built. A loop whose goal arrived afterwards
/// ran every iteration without the rules that tell the agent to report — so
/// the early finish the guide documents could never happen and every iteration
/// looked like a stall to the gate.
///
/// Returns `false` when there is nothing to run.
pub(crate) async fn install_loop_goal(
    cli: &Cli,
    cfg: &Config,
    session: &Session,
) -> anyhow::Result<bool> {
    let Some(prompt) = loop_prompt(cli) else {
        anyhow::bail!("No loop prompt. Use --loop-prompt or pass a message.");
    };
    let plan_file = cli
        .loop_plan
        .clone()
        .unwrap_or_else(|| PathBuf::from(loop_mod::DEFAULT_PLAN_FILENAME));

    // Honour the existing resume prompt: an operator who declines a stale plan
    // gets it removed before the first round reads it.
    let _ = loop_mod::plan::handle_startup(&plan_file).await?;

    // `--loop-max 0` has always meant "run nothing", which a goal's minimum of
    // one round cannot express. Answer it here rather than silently running an
    // iteration the operator asked not to have.
    if cli.loop_max == Some(0) {
        eprintln!("[loop] max iterations (0) reached, stopping");
        return Ok(false);
    }

    let preset = crate::extras::goal::preset::loop_goal(
        &prompt,
        &plan_file,
        cli.loop_max,
        cli.loop_run.as_deref(),
        crate::extras::goal::GoalDefaults {
            cfg,
            provider: &session.provider,
            model: &session.model,
        },
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    // A resumed session may already hold an unfinished goal. Replacing it is
    // the same decision `--goal` makes the operator state explicitly, so the
    // same flag governs it here.
    session
        .goal_store
        .set(preset.goal, cli.goal_args.goal_replace)
        .map_err(|error| {
            anyhow::anyhow!("{error} (or pass --goal-replace to start the loop anyway)")
        })?;
    Ok(true)
}

/// The prompt a loop was given, from either spelling.
fn loop_prompt(cli: &Cli) -> Option<String> {
    cli.loop_prompt.clone().or_else(|| {
        let msg = cli.message.join(" ");
        if msg.is_empty() { None } else { Some(msg) }
    })
}

/// Run `--loop` as a goal.
///
/// A loop iteration is a goal round in restart mode: a fresh conversation each
/// time, the plan file re-read into every prompt, and the validator run every
/// round as feedback. Both features share one round engine so they cannot drift
/// on what an iteration is or when one stops.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_headless_loop(
    agent: AnyAgent,
    cli: &Cli,
    cfg: &Config,
    _context: &ContextFiles,
    session: &Session,
    status_signals: Option<StatusSignals>,
    sandbox: &Sandbox,
    client: &crate::provider::AnyClient,
) -> anyhow::Result<()> {
    // `install_loop_goal` ran before the agent was built, and answered the
    // "nothing to run" cases there.
    if !session.goal_store.is_active() {
        return Ok(());
    }
    let prompt = loop_prompt(cli).unwrap_or_default();

    let mut saved_session = session.clone();
    if let Some(ss) = status_signals.as_ref() {
        ss.send_start();
    }
    let outcome = crate::startup::run_goal_rounds_for_loop(
        &agent,
        &mut saved_session,
        cli,
        cfg,
        sandbox,
        client,
        &prompt,
    )
    .await;
    if let Some(ss) = status_signals.as_ref() {
        ss.send_stop();
    }
    outcome
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(feature = "hooks")]
    use super::*;

    /// A resumed session's history reaches the first round. Later rounds are
    /// restart rounds and deliberately start clean, carrying the objective and
    /// a harness-built summary instead; the goal driver owns that and is tested
    /// there.
    #[test]
    fn a_loop_preset_starts_each_iteration_from_a_clean_conversation() {
        let cfg = crate::config::Config::default();
        let preset = crate::extras::goal::preset::loop_goal(
            "keep going",
            std::path::Path::new("LOOP_PLAN.md"),
            Some(3),
            Some("cargo test"),
            crate::extras::goal::GoalDefaults {
                cfg: &cfg,
                provider: "openrouter",
                model: "big",
            },
        )
        .expect("valid preset");
        assert!(matches!(
            preset.goal.continuation,
            crate::extras::goal::ContinuationMode::Restart { .. }
        ));
        assert!(preset.goal.bounds.check_every_round);
        assert_eq!(preset.goal.bounds.max_rounds, 3);
    }

    /// A `Stop` hook still learns which iteration it is watching and whether
    /// more are coming. Folding the loop onto the goal driver moved where
    /// those two facts come from; it must not empty the fields.
    #[cfg(feature = "hooks")]
    #[test]
    fn a_loop_round_still_reports_its_iteration_to_stop_hooks() {
        let session = Session::new("openrouter", "test", 128_000, "");
        let goal = crate::extras::goal::Goal::new("keep going", Vec::new()).expect("valid goal");
        session.goal_store.set(goal, false).expect("no prior goal");

        let info = crate::startup::loop_round_info(&session, true).expect("a loop reports");
        assert_eq!(info.iteration, 1, "the round about to run is iteration one");
        assert!(info.active);

        // Three rounds in, and then parked.
        session.goal_store.with_mut(|goal| {
            goal.progress.rounds = 3;
            goal.set_status(crate::extras::goal::GoalStatus::BudgetLimited, None);
        });
        let info = crate::startup::loop_round_info(&session, true).expect("a loop reports");
        assert_eq!(info.iteration, 4);
        assert!(!info.active, "a parked goal is not still looping");

        assert!(
            crate::startup::loop_round_info(&session, false).is_none(),
            "a plain -p run is not a loop and says so"
        );
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
            let root = std::env::temp_dir()
                .join(format!("mini-agent-loop-progress-{}", uuid::Uuid::new_v4()));
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
            // The same two steps production takes: the goal is installed
            // before the agent would be built, then the rounds run.
            assert!(
                install_loop_goal(&cli, &Config::default(), &session)
                    .await
                    .expect("the loop goal installs")
            );
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
                    &crate::provider::AnyClient::OpenRouter(
                        rig::providers::openrouter::Client::new("unused-test-key").unwrap(),
                    ),
                ),
            )
            .await
            .expect("headless loop must terminate after provider failure");
            server.abort();
            let _ = server.await;
            // The goal engine retries a failed round before giving up, so the
            // propagated error is from the last attempt rather than the first.
            // What must not change is that the failure propagates at all and
            // that the effects and usage from before it are durable.
            let error = result.expect_err("provider failure must propagate");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains("loop provider failure")
                    || rendered.contains("error sending request"),
                "{rendered}"
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
            let returned = crate::extras::goal::driver::persist_round(
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
            let result = crate::extras::goal::driver::persist_round(
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
