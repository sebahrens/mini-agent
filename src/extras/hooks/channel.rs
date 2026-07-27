use super::subprocess::HookOutput;

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
    /// Any other exit code: non-blocking error, the action proceeds.
    Error {
        exit_code: Option<i32>,
        stderr: String,
    },
    /// The hook exceeded its timeout and was killed.
    TimedOut,
}

pub(crate) fn interpret_hook_output(output: &HookOutput) -> ChannelResult {
    if output.timed_out {
        return ChannelResult::TimedOut;
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
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
        other => ChannelResult::Error {
            exit_code: other,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
    }
}
