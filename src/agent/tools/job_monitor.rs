//! Output-pattern monitor for background shell jobs (mini-agent-1cg19).
//!
//! `shell` with `background = true` and `monitor = "<regex>"` starts a watcher
//! that scans the job's new output while it runs. Matching lines queue a
//! bounded notice on the session's job notice queue, which is delivered like a
//! completion notice: appended to the next tool result, injected into the next
//! model request, or used to wake an idle TUI session.
//!
//! Limits (documented in `docs/agent/TOOL_RUNTIME.md`): one monitor per job,
//! at most [`MONITOR_MAX_NOTICES`] notices per job, at least
//! [`MonitorLimits::min_interval`] between notices (matches in between are
//! counted and reported together), one first line of at most 300 characters
//! per notice, and the full output stays in the job log.

use std::time::Duration;

use crate::sandbox::Sandbox;
use crate::sandbox::job_notices::{JobNotice, MONITOR_MAX_NOTICES, OutputMatch};

/// Bytes of new output examined per scan step.
const SCAN_BYTES: usize = 256 * 1024;
/// Scan steps per tick, so one very chatty job cannot monopolise the task.
const MAX_STEPS_PER_TICK: usize = 16;
const MAX_LINE_CHARS: usize = 300;

#[derive(Clone, Copy, Debug)]
pub(crate) struct MonitorLimits {
    pub poll: Duration,
    pub min_interval: Duration,
}

impl Default for MonitorLimits {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(500),
            min_interval: Duration::from_secs(5),
        }
    }
}

/// Matches found in one scan step.
#[derive(Default)]
struct StepResult {
    matches: u32,
    first_line: Option<String>,
    /// Offset the next step starts from.
    next: u64,
}

/// Scan one window of new output. Only complete lines are consumed (a partial
/// last line is re-read next time) unless `finished`, when everything is.
fn scan_step(
    regex: &regex::Regex,
    window: &crate::sandbox::OutputWindow,
    pty: bool,
    finished: bool,
) -> StepResult {
    let mut result = StepResult {
        next: window.next,
        ..StepResult::default()
    };
    // A split window: examine the head and resume right after it. Bytes that
    // were neither retained nor logged cannot be read and are passed over.
    let split = window.skipped > 0 && !window.head.is_empty();
    let mut pieces: Vec<(&[u8], u64, bool)> = Vec::new();
    if split {
        let end = window.start + window.head.len() as u64;
        pieces.push((&window.head, end, true));
        result.next = end;
    } else {
        if !window.head.is_empty() {
            let end = window.start + window.head.len() as u64;
            pieces.push((&window.head, end, window.tail.is_empty()));
        }
        if !window.tail.is_empty() {
            pieces.push((&window.tail, window.next, true));
        }
    }
    for (bytes, end, is_last) in pieces {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        // Hold back an unfinished last line so a match split across reads is
        // seen whole; a very long line is consumed rather than held forever.
        if is_last && !finished && !text.ends_with('\n') {
            if let Some(at) = text.rfind('\n') {
                result.next = end - (bytes.len() - at - 1) as u64;
                text.truncate(at + 1);
            } else if bytes.len() < 8192 {
                result.next = end - bytes.len() as u64;
                text.clear();
            }
        }
        let text = if pty {
            crate::agent::tools::bash::normalize_terminal_output(&text)
        } else {
            text
        };
        for line in text.lines() {
            if regex.is_match(line) {
                result.matches += 1;
                if result.first_line.is_none() {
                    result.first_line = Some(line.chars().take(MAX_LINE_CHARS).collect());
                }
            }
        }
    }
    result
}

/// Watch `id` until it ends, queueing rate-limited notices. Returns when the
/// job is terminal (after flushing pending matches), the record is gone, the
/// notice budget is spent, or notifications are disabled.
pub(crate) async fn run_monitor(
    sandbox: Sandbox,
    id: String,
    regex: regex::Regex,
    limits: MonitorLimits,
) {
    let mut cursor = 0u64;
    let mut pending_matches = 0u32;
    let mut pending_line: Option<String> = None;
    let mut sent = 0u32;
    let mut last_sent: Option<tokio::time::Instant> = None;
    loop {
        tokio::time::sleep(limits.poll).await;
        let queue = sandbox.job_notice_queue().clone();
        if !queue.enabled() {
            return;
        }
        let Ok(snapshot) = sandbox.background_job_snapshot(&id) else {
            return;
        };
        let finished = snapshot.status.is_terminal();
        for _ in 0..MAX_STEPS_PER_TICK {
            let Ok(window) = sandbox.peek_background_job_window(&id, cursor, SCAN_BYTES) else {
                return;
            };
            if window.next <= cursor {
                break;
            }
            let step = scan_step(&regex, &window, snapshot.pty, finished);
            pending_matches = pending_matches.saturating_add(step.matches);
            if pending_line.is_none() {
                pending_line = step.first_line;
            }
            if step.next <= cursor {
                break;
            }
            cursor = step.next;
        }
        let due = last_sent.is_none_or(|at| at.elapsed() >= limits.min_interval);
        let flush = pending_matches > 0
            && (due || finished)
            && !(finished && snapshot.status == crate::sandbox::BackgroundJobStatus::Cancelled);
        if flush {
            sent += 1;
            queue.push(JobNotice {
                id: id.clone(),
                command: snapshot.command.clone(),
                status: snapshot.status,
                exit_code: snapshot.exit_code,
                signal: snapshot.signal,
                pty: snapshot.pty,
                log_path: snapshot.transcript.log.as_ref().map(|log| log.path.clone()),
                tail: Vec::new(),
                earlier_bytes: 0,
                output_match: Some(OutputMatch {
                    regex: regex.as_str().chars().take(MAX_LINE_CHARS).collect(),
                    line: pending_line.take().unwrap_or_default(),
                    matches: pending_matches,
                    monitor_stopped: sent >= MONITOR_MAX_NOTICES,
                }),
            });
            pending_matches = 0;
            last_sent = Some(tokio::time::Instant::now());
            if sent >= MONITOR_MAX_NOTICES {
                return;
            }
        }
        if finished {
            return;
        }
    }
}

/// Start the monitor as a detached task. It holds a clone of the sandbox
/// handle only until the job ends.
pub(crate) fn spawn_monitor(sandbox: &Sandbox, id: &str, regex: regex::Regex) {
    let sandbox = sandbox.clone();
    let id = id.to_string();
    tokio::spawn(run_monitor(sandbox, id, regex, MonitorLimits::default()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::OutputWindow;
    use rig::tool::Tool;

    fn window(start: u64, text: &str) -> OutputWindow {
        OutputWindow {
            start,
            head: text.as_bytes().to_vec(),
            skipped: 0,
            tail: Vec::new(),
            next: start + text.len() as u64,
        }
    }

    #[test]
    fn scan_counts_matching_lines_and_holds_back_an_unfinished_line() {
        let regex = regex::Regex::new("ERROR").unwrap();
        let step = scan_step(
            &regex,
            &window(10, "ok\nERROR a\nERROR b\nERR"),
            false,
            false,
        );
        assert_eq!(step.matches, 2);
        assert_eq!(step.first_line.as_deref(), Some("ERROR a"));
        assert_eq!(
            step.next,
            10 + 19,
            "resumes at the start of the partial line"
        );
        let done = scan_step(&regex, &window(0, "x\nERROR"), false, true);
        assert_eq!(done.matches, 1, "a finished job's last line counts");
        assert_eq!(done.next, 7);
    }

    #[test]
    fn scan_of_a_split_window_reads_the_head_then_resumes_after_it() {
        let regex = regex::Regex::new("hit").unwrap();
        let split = OutputWindow {
            start: 0,
            head: b"a\nhit\n".to_vec(),
            skipped: 100,
            tail: b"hit\n".to_vec(),
            next: 110,
        };
        let step = scan_step(&regex, &split, false, false);
        assert_eq!(step.matches, 1, "the tail is re-read from the next window");
        assert_eq!(step.next, 6);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_queues_a_rate_limited_notice_while_the_job_keeps_running() {
        use crate::agent::tools::bash::{BashTool, JobStatusTool};
        use crate::agent::tools::{BashArgs, JobAction, JobStatusArgs};
        let dir = crate::tests::support::temp::TempDir::new("job-monitor");
        let shell = BashTool::new(None, None, Sandbox::new(false, "bwrap"), None)
            .with_output_log_dir(Some(dir.path().join("logs")));
        let started = shell
            .call(BashArgs {
                command: "echo boot; sleep 0.3; for i in 1 2 3; do echo \"ERROR number $i\"; done; sleep 30"
                    .to_string(),
                background: true,
                monitor: Some("ERROR".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            started.contains("Monitor: output lines matching /ERROR/"),
            "{started}"
        );
        let queue = shell.sandbox.job_notice_queue().clone();
        tokio::time::timeout(Duration::from_secs(10), queue.wait_pending())
            .await
            .expect("a monitor notice arrives while the job runs");
        let notices = queue.take();
        assert_eq!(notices.len(), 1, "matches in one burst are coalesced");
        let hit = notices[0].output_match.as_ref().unwrap();
        assert_eq!((hit.matches, hit.line.as_str()), (3, "ERROR number 1"));
        assert_eq!(notices[0].status.as_str(), "running");
        assert!(notices[0].log_path.is_some());
        // Rendering is the model-facing, fenced form.
        assert!(crate::sandbox::job_notices::render(&notices[0]).contains("```"));
        let id = notices[0].id.clone();
        let status = JobStatusTool::new(shell.sandbox.clone(), None);
        drop(
            status
                .call(JobStatusArgs {
                    id: Some(id),
                    action: JobAction::Stop,
                    ..Default::default()
                })
                .await,
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_is_background_only_validated_and_off_when_notifications_are_off() {
        use crate::agent::tools::BashArgs;
        use crate::agent::tools::bash::BashTool;
        let shell = BashTool::new(None, None, Sandbox::new(false, "bwrap"), None);
        for (background, monitor) in [(false, "x"), (true, "("), (true, "")] {
            let error = shell
                .call(BashArgs {
                    command: "echo hi".into(),
                    background,
                    monitor: Some(monitor.into()),
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("monitor"), "{error}");
        }
        assert!(
            shell.sandbox.background_job_summaries().is_empty(),
            "nothing started"
        );
        shell.sandbox.job_notice_queue().set_enabled(false);
        let started = shell
            .call(BashArgs {
                command: "sleep 5".into(),
                background: true,
                monitor: Some("x".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(started.contains("Monitor not started"), "{started}");
        for job in shell.sandbox.background_job_summaries() {
            let _ = shell.sandbox.stop_background_job(&job.id).await;
        }
    }

    #[test]
    fn long_matched_lines_are_truncated() {
        let regex = regex::Regex::new("x").unwrap();
        let step = scan_step(
            &regex,
            &window(0, &format!("{}\n", "x".repeat(1000))),
            false,
            false,
        );
        assert_eq!(step.first_line.unwrap().chars().count(), MAX_LINE_CHARS);
    }
}
