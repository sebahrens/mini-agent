use std::time::Duration;

pub const STEP_TIMEOUT: Duration = Duration::from_secs(30);
pub const MEMORY_LIMIT: usize = 64 * 1024 * 1024; // 64 MiB
pub const STACK_LIMIT: usize = 512 * 1024; // 512 KiB JS stack
pub const THREAD_STACK: usize = 8 * 1024 * 1024; // 8 MiB OS thread stack

#[derive(Debug)]
pub struct JsRequest {
    pub code: String,
    pub reply: tokio::sync::oneshot::Sender<JsResponse>,
}

#[derive(Debug)]
pub struct JsResponse {
    pub outcome: JsOutcome,
}

#[derive(Debug)]
pub enum JsOutcome {
    Value(String),
    Void,
    Error(String),
    Timeout,
    OomKilled,
}

/// Sent from the JS thread to tokio to request permission for a host call.
/// The JS thread blocks on `reply_rx.recv()` while tokio resolves the check.
#[derive(Debug)]
pub struct PermRequest {
    pub tool: String,
    pub key: String,
    pub reply: std::sync::mpsc::Sender<PermResponse>,
}

#[derive(Debug)]
pub enum PermResponse {
    Allowed,
    Denied(String),
}

/// Returned to JS by `spawn(cmd, args)`.
/// Visible in JS as `{ stdout: string, stderr: string, code: number }`.
#[derive(Debug, serde::Serialize)]
pub struct SpawnResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}
