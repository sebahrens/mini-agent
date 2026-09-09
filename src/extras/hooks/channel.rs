use super::subprocess::{HookOutput, HookStatus};

const BLOCK_REASON_MAX_BYTES: usize = 8 * 1024;

fn safe_reason(bytes: &[u8]) -> String {
    let (mut reason, truncated) =
        crate::extras::validation::sanitize_bytes(bytes, BLOCK_REASON_MAX_BYTES);
    if truncated {
        reason.push_str("…[hook reason truncated]");
    }
    reason
}

/// Normalized result of interpreting one hook's raw process output via the
/// exit-code and stdout-JSON channels, per the hook-dispatch spec's
/// "exit-code and stdout-JSON contract" requirement.
#[derive(Debug)]
pub(crate) enum ChannelResult {
    /// Exit 0: no objection. `json` is `Some` only when stdout parsed as
    /// valid JSON; invalid or empty stdout is silently ignored.
    NoObjection { json: Option<serde_json::Value> },
    /// Exit 2: block the action. `stderr` is fed back as the block reason.
    Block { stderr: String },
    /// Any other exit code: the dispatcher applies the event's failure policy.
    /// Error stderr is deliberately not copied into decision or audit payloads.
    Error { exit_code: Option<i32> },
    /// The hook exceeded its timeout and was killed.
    TimedOut,
    /// The hook exceeded a hard output cap and was killed. Its bounded output
    /// prefix must not be interpreted as a complete hook response.
    OutputLimitExceeded,
    /// The configured trust/containment policy denied launch before a child
    /// was created.
    PolicyDenied { reason: String },
}

pub(crate) fn interpret_hook_output(output: &HookOutput) -> ChannelResult {
    match output.status {
        HookStatus::TimedOut => return ChannelResult::TimedOut,
        HookStatus::OutputLimitExceeded(_) => return ChannelResult::OutputLimitExceeded,
        HookStatus::PolicyDenied => {
            return ChannelResult::PolicyDenied {
                reason: safe_reason(&output.stderr),
            };
        }
        HookStatus::Completed | HookStatus::Failed => {}
    }
    match output.exit_code {
        Some(0) => {
            let json = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
            ChannelResult::NoObjection { json }
        }
        Some(2) => {
            if serde_json::from_slice::<serde_json::Value>(&output.stdout).is_ok() {
                tracing::warn!(
                    "hooks: hook exited 2 (block) and also printed JSON on stdout; the JSON is ignored"
                );
            }
            ChannelResult::Block {
                stderr: safe_reason(&output.stderr),
            }
        }
        other => ChannelResult::Error { exit_code: other },
    }
}
