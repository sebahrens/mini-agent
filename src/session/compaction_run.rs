//! The one compaction driver (mini-agent-sskdu.240).
//!
//! The TUI, headless and ACP front ends decide *whether* and *where* to cut
//! (the gate), how to present progress, and how usage is billed. Everything
//! between "a cut was chosen" and "the history is compacted" lives here, so a
//! fix lands once: background pause, summarizer input, the `PreCompact`
//! announcement, summarising, charging, the drain/commit with the summarizer's
//! byte budget and file-window restore, and the `PostCompact` /
//! `SessionStart(compact)` hooks and daily-memory copy.
//!
//! The summarizer itself (text request, cache fork, compaction model) is
//! supplied by the front end through [`SummaryRequest`].

use std::sync::atomic::AtomicBool;

use super::{Session, SessionMessage};
use crate::event::UsageDelta;
use crate::provider::CompactionOutput;

/// Resolves where the kept history starts from the summarizer's covered end.
pub(crate) type SnapFn = dyn Fn(&Session, usize) -> Option<usize> + Sync;

/// One compaction of `messages[..cut_idx]`.
pub(crate) struct Plan<'a> {
    /// Exclusive end of the prefix to summarize.
    pub cut_idx: usize,
    pub input_token_budget: u64,
    pub response_token_budget: u64,
    /// Tokens of recent `read` windows re-attached after the commit.
    pub restore_read_tokens: u64,
    /// `auto` versus manual: the hook matcher.
    pub auto: bool,
    /// Operator instructions for the `PreCompact` payload.
    pub instructions: Option<&'a str>,
    /// When `Some`, the driver builds the cache-fork conversation prefix after
    /// pausing background work (see [`SummaryRequest::fork_history`]); the
    /// value is the tool-result retention the fork replays with.
    pub fork_keep_recent_tool_results: Option<usize>,
    /// `PreCompact` fires once per compaction even when the front end runs
    /// several rounds; share one flag across them.
    pub pre_compact_sent: &'a AtomicBool,
    /// Resolves the first kept index when the summarizer covered only a prefix
    /// of the cut (`consumed` is the covered end). `None` drains exactly the
    /// covered prefix; `Some` lets a front end require a legal cut boundary,
    /// and `None` from it fails the round with [`RunError::NoLegalCut`].
    pub snap: Option<&'a SnapFn>,
}

/// What the summarizer is asked to do.
pub(crate) struct SummaryRequest {
    pub model: String,
    pub messages: Vec<SessionMessage>,
    pub previous_summary: Option<String>,
    pub input_token_budget: u64,
    pub response_token_budget: u64,
    /// The converted conversation prefix of a cache-reusing fork, present only
    /// when the plan asked for one and a fork is safe for this prefix.
    pub fork_history: Option<Vec<rig::completion::Message>>,
}

/// Front-end behaviour that is not part of the compaction itself.
pub(crate) trait Observer {
    /// Charge the summarizer's usage; returns what was charged (normalized
    /// for the session's provider) so the caller can credit a goal round.
    fn charge(&mut self, session: &mut Session, usage: UsageDelta) -> UsageDelta;
    /// The summary and drain length, before the session is mutated. Front ends
    /// copy the summary to daily memory here (immediately, or staged until the
    /// turn is saved).
    fn memory_summary(&mut self, _summary: &str, _first_kept_index: usize) {}
}

/// An [`Observer`] that charges nothing (the caller bills the returned usage)
/// and hands the summary to one closure before the session is mutated.
pub(crate) struct StageObserver<F>(Option<F>);

impl<F: FnOnce(&str, usize)> StageObserver<F> {
    pub(crate) fn new(stage: F) -> Self {
        Self(Some(stage))
    }
}

impl<F: FnOnce(&str, usize)> Observer for StageObserver<F> {
    fn charge(&mut self, _session: &mut Session, usage: UsageDelta) -> UsageDelta {
        usage
    }
    fn memory_summary(&mut self, summary: &str, first_kept_index: usize) {
        if let Some(stage) = self.0.take() {
            stage(summary, first_kept_index);
        }
    }
}

/// A committed compaction.
#[derive(Debug)]
pub(crate) struct Compacted {
    pub first_kept_index: usize,
    /// Estimated tokens the replaced prefix held.
    pub tokens_before: u64,
    /// The usage [`Observer::charge`] applied.
    pub usage: UsageDelta,
}

/// Why a round did not commit. The history is unchanged in every case.
pub(crate) enum RunError {
    /// Background work could not be paused (the pool is closing).
    #[cfg_attr(not(feature = "subagents"), allow(dead_code))]
    Pause(anyhow::Error),
    /// The summarizer failed.
    Summary(anyhow::Error),
    /// The summarizer covered no messages.
    NothingSummarized,
    /// The summarizer's coverage ended before any legal cut boundary.
    NoLegalCut,
}

impl RunError {
    /// A short reason a front end can show.
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::Pause(error) | Self::Summary(error) => format!("{error:#}"),
            Self::NothingSummarized => "the summarizer covered no messages".to_owned(),
            Self::NoLegalCut => "the summary did not cover a whole turn".to_owned(),
        }
    }
}

impl From<RunError> for anyhow::Error {
    fn from(error: RunError) -> Self {
        match error {
            RunError::Pause(error) | RunError::Summary(error) => error,
            RunError::NothingSummarized => {
                anyhow::anyhow!("compaction summarized no messages; nothing to drain")
            }
            RunError::NoLegalCut => anyhow::anyhow!("{}", RunError::NoLegalCut.reason()),
        }
    }
}

/// Dispatch `PreCompact` unless this compaction already did.
#[cfg(feature = "hooks")]
pub(crate) async fn dispatch_pre_compact_once(
    sent: &AtomicBool,
    auto: bool,
    instructions: Option<&str>,
) {
    if !sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
        crate::startup::heap_future(|| {
            crate::extras::hooks::dispatch_pre_compact(auto, instructions)
        })
        .await;
    }
}

/// Runs one compaction round: see the module documentation.
pub(crate) async fn run<O, S, F>(
    session: &mut Session,
    plan: Plan<'_>,
    observer: &mut O,
    summarize: S,
) -> Result<Compacted, RunError>
where
    O: Observer,
    S: FnOnce(SummaryRequest) -> F,
    F: std::future::Future<Output = anyhow::Result<CompactionOutput>>,
{
    #[cfg(feature = "subagents")]
    let _background_pause = match &session.background_pool {
        Some(pool) => Some(pool.pause_for_history().await.map_err(RunError::Pause)?),
        None => None,
    };
    #[cfg(feature = "hooks")]
    dispatch_pre_compact_once(plan.pre_compact_sent, plan.auto, plan.instructions).await;
    #[cfg(not(feature = "hooks"))]
    let _ = (plan.pre_compact_sent, plan.instructions, plan.auto);

    let cut_idx = plan.cut_idx;
    let (skipped, messages) = session.compaction_input(cut_idx);
    let fork_history = plan.fork_keep_recent_tool_results.and_then(|keep| {
        crate::agent::runner::cache_fork_history(session, cut_idx, keep, plan.input_token_budget)
    });
    let output = summarize(SummaryRequest {
        model: session.model.to_string(),
        messages,
        previous_summary: session.previous_summary_prose(),
        input_token_budget: plan.input_token_budget,
        response_token_budget: plan.response_token_budget,
        fork_history,
    })
    .await
    .map_err(RunError::Summary)?;
    let CompactionOutput {
        summary,
        summary_byte_budget,
        messages_included,
        usage,
    } = output;
    let drain = Session::compaction_drain_len(cut_idx - skipped, messages_included)
        .map_err(|_| RunError::NothingSummarized)?;
    // A round that covered something spent its tokens whatever happens next, so
    // it is charged here. A failed or empty round is charged by the caller from
    // its own usage observer, which saw every request.
    let usage = observer.charge(session, usage);
    let mut first_kept_index = skipped + drain;
    if let Some(snap) = plan.snap {
        first_kept_index = snap(session, first_kept_index).ok_or(RunError::NoLegalCut)?;
    }
    observer.memory_summary(&summary, first_kept_index);
    let tokens_before = session.commit_compaction_prefix(
        summary,
        first_kept_index,
        summary_byte_budget,
        plan.restore_read_tokens,
    );

    #[cfg(feature = "hooks")]
    {
        let summary = session.compacted_context().0.map(str::to_owned);
        crate::startup::heap_future(|| {
            crate::extras::hooks::dispatch_post_compact(plan.auto, summary.as_deref())
        })
        .await;
        let notices =
            crate::startup::heap_future(|| crate::extras::hooks::dispatch_session_start("compact"))
                .await;
        crate::extras::hooks::append_context_session(session, &notices.into_notices());
    }
    Ok(Compacted {
        first_kept_index,
        tokens_before,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MessageRole;

    fn session() -> Session {
        let mut s = Session::new("openai", "model", 100_000, "");
        s.add_message(MessageRole::User, "u0");
        s.add_tool_call_with_id("a", "read", &serde_json::json!({"path": "a"}));
        s.add_tool_call_with_id("b", "read", &serde_json::json!({"path": "b"}));
        s.add_tool_result_with_id("a", "read", "ra");
        s.add_tool_result_with_id("b", "read", "rb");
        s.add_message(MessageRole::User, "u5");
        s.add_tool_call_with_id("c", "read", &serde_json::json!({"path": "c"}));
        s.add_tool_result_with_id("c", "read", "rc");
        s.add_message(MessageRole::User, "u8");
        s.add_message(MessageRole::Assistant, "a9");
        s
    }

    fn user_start(session: &Session, consumed: usize) -> Option<usize> {
        session
            .messages
            .iter()
            .enumerate()
            .take(consumed + 1)
            .skip(1)
            .filter(|(_, m)| m.role == MessageRole::User)
            .map(|(i, _)| i)
            .max()
    }

    struct Quiet;
    impl Observer for Quiet {
        fn charge(&mut self, _: &mut Session, usage: UsageDelta) -> UsageDelta {
            usage
        }
    }

    /// The cut invariants every gate shares: exactly the covered prefix is
    /// drained (parallel tool batches included), a zero-coverage summary is
    /// refused, and a gate that needs a turn boundary fails when the covered
    /// prefix ends before one. The history is untouched on every failure.
    #[tokio::test]
    async fn drain_follows_coverage_for_every_gate() {
        type Snap = fn(&Session, usize) -> Option<usize>;
        #[allow(clippy::type_complexity)]
        let cases: [(&str, usize, usize, Option<Snap>, Result<usize, &str>); 6] = [
            ("full coverage", 8, 8, None, Ok(8)),
            (
                "covered prefix ends inside a parallel batch",
                8,
                3,
                None,
                Ok(3),
            ),
            ("zero coverage", 8, 0, None, Err("covered no messages")),
            ("turn gate, full coverage", 8, 8, Some(user_start), Ok(8)),
            (
                "turn gate, coverage ends in turn one",
                8,
                4,
                Some(user_start),
                Err("whole turn"),
            ),
            (
                "turn gate, coverage reaches turn two",
                8,
                7,
                Some(user_start),
                Ok(5),
            ),
        ];
        for (name, cut_idx, included, snap, expected) in cases {
            let mut s = session();
            let before = s.messages.len();
            let sent = AtomicBool::new(false);
            let snap_ref = snap.as_ref().map(|f| f as &SnapFn);
            let result = run(
                &mut s,
                Plan {
                    cut_idx,
                    input_token_budget: 1000,
                    response_token_budget: 100,
                    restore_read_tokens: 0,
                    auto: true,
                    instructions: None,
                    fork_keep_recent_tool_results: None,
                    pre_compact_sent: &sent,
                    snap: snap_ref,
                },
                &mut Quiet,
                |_| async move {
                    Ok(CompactionOutput {
                        summary: "S".into(),
                        summary_byte_budget: 16 * 1024,
                        messages_included: included,
                        usage: UsageDelta::default(),
                    })
                },
            )
            .await;
            match (result, expected) {
                (Ok(done), Ok(first_kept)) => {
                    assert_eq!(done.first_kept_index, first_kept, "{name}");
                    assert_eq!(s.compactions.len(), 1, "{name}");
                    assert_eq!(s.messages.len(), before - first_kept + 1, "{name}");
                }
                (Err(error), Err(reason)) => {
                    assert!(
                        error.reason().contains(reason),
                        "{name}: {}",
                        error.reason()
                    );
                    assert_eq!(s.messages.len(), before, "{name}: history unchanged");
                    assert!(s.compactions.is_empty(), "{name}");
                }
                (other, expected) => panic!("{name}: got ok={} want {expected:?}", other.is_ok()),
            }
        }
    }
}
