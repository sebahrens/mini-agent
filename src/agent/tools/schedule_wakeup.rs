//! The `schedule_wakeup` tool: how a self-paced `/cron` entry chooses its next
//! fire.
//!
//! The TUI registers this tool on the main agent's tool server only for the
//! duration of a turn started by a dynamic `/cron` entry and removes it when
//! that turn ends, so ordinary turns, subagents (their own tool servers),
//! side questions, headless runs and ACP never see it. The call is the only
//! channel: response text is never parsed for a marker, because a tool result
//! or fetched page could coax the model into printing one.
//!
//! The tool only records the request in a [`WakeupSlot`] shared with the TUI;
//! the scheduler applies it after the turn ends. The delay is clamped to
//! [`WAKEUP_MIN_SECS`]..=[`WAKEUP_MAX_SECS`]. A turn that ends without a call
//! retires its entry.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rig::tool::Tool;
use serde::Deserialize;

use super::ToolError;

/// Shortest wakeup delay the model may request; shorter requests are raised.
pub(crate) const WAKEUP_MIN_SECS: u64 = 60;
/// Longest wakeup delay the model may request; longer requests are lowered.
pub(crate) const WAKEUP_MAX_SECS: u64 = 60 * 60;
/// Bound on the stored reason (characters), which the TUI echoes.
pub(crate) const WAKEUP_MAX_REASON_CHARS: usize = 200;

/// One accepted `schedule_wakeup` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wakeup {
    /// The clamped delay until the next fire.
    pub delay: Duration,
    /// The model reported that nothing changed since the previous fire.
    pub noop: bool,
    /// Single-line, bounded reason.
    pub reason: String,
}

/// Where the tool leaves the latest request for the TUI scheduler. The last
/// call of a turn wins.
#[derive(Debug, Clone, Default)]
pub(crate) struct WakeupSlot(Arc<Mutex<Option<Wakeup>>>);

impl WakeupSlot {
    fn set(&self, wakeup: Wakeup) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(wakeup);
    }

    /// Take the recorded request, leaving the slot empty.
    pub(crate) fn take(&self) -> Option<Wakeup> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Clamp a requested delay in seconds to the accepted range.
pub(crate) fn clamp_delay_secs(requested: i64) -> Duration {
    let secs = u64::try_from(requested).unwrap_or(0);
    Duration::from_secs(secs.clamp(WAKEUP_MIN_SECS, WAKEUP_MAX_SECS))
}

fn single_line_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(WAKEUP_MAX_REASON_CHARS)
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScheduleWakeupArgs {
    #[serde(deserialize_with = "crate::agent::tools::lenient::delay_secs_i64")]
    pub delay_secs: i64,
    pub reason: String,
    #[serde(deserialize_with = "crate::agent::tools::lenient::noop_flag")]
    pub noop: bool,
}

/// The slot of the self-paced turn that is currently running, if any. The tool
/// stays registered for as long as a self-paced entry exists, so the request's
/// tool list (part of the cached prompt prefix) does not change per turn; a
/// call while no cron turn is armed fails instead.
#[derive(Debug, Clone, Default)]
pub(crate) struct WakeupCell(Arc<Mutex<Option<WakeupSlot>>>);

impl WakeupCell {
    /// Arm (or, with `None`, disarm) the cell for the turn about to run.
    pub(crate) fn set(&self, slot: Option<WakeupSlot>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = slot;
    }

    fn current(&self) -> Option<WakeupSlot> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The `schedule_wakeup` tool, registered while a self-paced `/cron` entry exists.
pub(crate) struct ScheduleWakeup {
    cell: WakeupCell,
}

impl ScheduleWakeup {
    pub(crate) fn new(cell: WakeupCell) -> Self {
        Self { cell }
    }
}

impl Tool for ScheduleWakeup {
    const NAME: &'static str = "schedule_wakeup";

    type Error = ToolError;
    type Args = ScheduleWakeupArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "Only for turns started by a self-paced /cron schedule (a call in any other turn \
             fails). Call this once before ending such a turn to choose when the schedule fires next. delay_secs is clamped \
             to {WAKEUP_MIN_SECS}..{WAKEUP_MAX_SECS}. Set noop to true when nothing changed \
             since the previous check. If you do not call it, the schedule stops."
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "delay_secs": {
                    "type": "integer",
                    "description": format!(
                        "Seconds until the next fire; clamped to {WAKEUP_MIN_SECS}..{WAKEUP_MAX_SECS}."
                    )
                },
                "reason": {
                    "type": "string",
                    "description": "One short line on why this delay was chosen."
                },
                "noop": {
                    "type": "boolean",
                    "description": "True when this check found nothing new."
                }
            },
            "required": ["delay_secs", "reason", "noop"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: ScheduleWakeupArgs) -> Result<String, ToolError> {
        let Some(slot) = self.cell.current() else {
            return Err(ToolError::Msg(
                "schedule_wakeup is only valid in a turn started by a self-paced /cron schedule"
                    .into(),
            ));
        };
        let delay = clamp_delay_secs(args.delay_secs);
        let wakeup = Wakeup {
            delay,
            noop: args.noop,
            reason: single_line_reason(&args.reason),
        };
        slot.set(wakeup);
        let requested = u64::try_from(args.delay_secs).ok();
        let clamped = if requested == Some(delay.as_secs()) {
            String::new()
        } else {
            format!(" (clamped from {}s)", args.delay_secs)
        };
        Ok(format!(
            "Next wakeup scheduled in {}s{clamped}. End the turn now.",
            delay.as_secs()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::tool::ToolDyn;

    #[test]
    fn delay_is_clamped_to_one_minute_through_one_hour() {
        assert_eq!(clamp_delay_secs(5), Duration::from_secs(60));
        assert_eq!(clamp_delay_secs(-30), Duration::from_secs(60));
        assert_eq!(clamp_delay_secs(0), Duration::from_secs(60));
        assert_eq!(clamp_delay_secs(60), Duration::from_secs(60));
        assert_eq!(clamp_delay_secs(900), Duration::from_secs(900));
        assert_eq!(clamp_delay_secs(3600), Duration::from_secs(3600));
        assert_eq!(clamp_delay_secs(99_999), Duration::from_secs(3600));
        assert_eq!(clamp_delay_secs(i64::MAX), Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn a_call_outside_an_armed_cron_turn_is_refused() {
        let cell = WakeupCell::default();
        let tool = ScheduleWakeup::new(cell.clone());
        let error = ToolDyn::call(
            &tool,
            r#"{"delay_secs":60,"reason":"x","noop":false}"#.into(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("only valid in a turn started by"), "{error}");
        let slot = WakeupSlot::default();
        cell.set(Some(slot.clone()));
        ToolDyn::call(
            &tool,
            r#"{"delay_secs":60,"reason":"x","noop":false}"#.into(),
        )
        .await
        .unwrap();
        assert!(slot.take().is_some());
        cell.set(None);
        assert!(
            ToolDyn::call(
                &tool,
                r#"{"delay_secs":60,"reason":"x","noop":false}"#.into()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn wire_call_records_the_clamped_request_in_the_slot() {
        let slot = WakeupSlot::default();
        let cell = WakeupCell::default();
        cell.set(Some(slot.clone()));
        let tool = ScheduleWakeup::new(cell);
        let output = ToolDyn::call(
            &tool,
            r#"{"delay_secs":5,"reason":"build\nstill running","noop":false}"#.into(),
        )
        .await
        .unwrap();
        assert!(output.contains("in 60s (clamped from 5s)"), "{output}");
        assert_eq!(
            slot.take(),
            Some(Wakeup {
                delay: Duration::from_secs(60),
                noop: false,
                reason: "build still running".into(),
            })
        );
        assert_eq!(slot.take(), None, "take empties the slot");

        ToolDyn::call(
            &tool,
            r#"{"delay_secs":99999,"reason":"quiet","noop":true}"#.into(),
        )
        .await
        .unwrap();
        // The last call of a turn wins.
        ToolDyn::call(
            &tool,
            r#"{"delay_secs":600,"reason":"later","noop":true}"#.into(),
        )
        .await
        .unwrap();
        let last = slot.take().unwrap();
        assert_eq!(last.delay, Duration::from_secs(600));
        assert!(last.noop);
    }

    #[tokio::test]
    async fn wire_call_clamps_the_upper_bound_and_refuses_malformed_arguments() {
        let slot = WakeupSlot::default();
        let cell = WakeupCell::default();
        cell.set(Some(slot.clone()));
        let tool = ScheduleWakeup::new(cell);
        let output = ToolDyn::call(
            &tool,
            r#"{"delay_secs":99999,"reason":"idle","noop":true}"#.into(),
        )
        .await
        .unwrap();
        assert!(
            output.contains("in 3600s (clamped from 99999s)"),
            "{output}"
        );
        assert_eq!(slot.take().unwrap().delay, Duration::from_secs(3600));

        for bad in [
            r#"{"delay_secs":"5m","reason":"x","noop":false}"#,
            r#"{"delay_secs":1.5,"reason":"x","noop":false}"#,
            r#"{"reason":"x","noop":false}"#,
            r#"{"delay_secs":60,"reason":"x","noop":false,"extra":1}"#,
        ] {
            assert!(ToolDyn::call(&tool, bad.into()).await.is_err(), "{bad}");
        }
        assert_eq!(slot.take(), None, "a refused call records nothing");
    }

    #[test]
    fn reason_is_one_bounded_line() {
        let long = "word ".repeat(200);
        let reason = single_line_reason(&long);
        assert_eq!(reason.chars().count(), WAKEUP_MAX_REASON_CHARS);
        assert!(!reason.contains('\n'));
    }
}
