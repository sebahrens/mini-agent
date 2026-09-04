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
        let response = match agent
            .run_print(
                &iteration_prompt,
                cli.pure_stdout,
                &cfg.retry,
                iteration_history(session),
                #[cfg(feature = "hooks")]
                Some(crate::extras::hooks::LoopInfo {
                    iteration: state.iteration,
                    active: state.active,
                }),
            )
            .await
        {
            Ok((r, _usage, _interactions)) => {
                if let Some(ss) = status_signals.as_ref() {
                    ss.send_stop();
                }
                r
            }
            Err(e) => {
                if let Some(ss) = status_signals.as_ref() {
                    ss.send_stop();
                }
                eprintln!("[loop] error in iteration {}: {}", state.iteration, e);
                break;
            }
        };

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

fn iteration_history(session: &Session) -> Vec<rig::completion::Message> {
    crate::agent::runner::convert_history(session)
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
        assert_eq!(iteration_history(&session).len(), 1);
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
