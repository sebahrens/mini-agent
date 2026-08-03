use compact_str::CompactString;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub type AskSender = mpsc::Sender<AskRequest>;
pub type AskReceiver = mpsc::Receiver<AskRequest>;

#[derive(Debug)]
pub struct AskRequest {
    pub tool: CompactString,
    pub input: String,
    /// Optional caller-supplied AllowAlways scope when the operation knows a
    /// safer boundary than the generic UI heuristic.
    pub suggested_pattern: Option<String>,
    /// Additional scopes persisted with AllowAlways. This lets a project-tree
    /// grant cover both the exact root and its descendants without widening to
    /// the parent directory.
    pub additional_allow_patterns: Vec<String>,
    pub reply: oneshot::Sender<UserDecision>,
}

#[derive(Debug, Clone)]
pub enum UserDecision {
    AllowOnce,
    AllowAlways(String),
    Deny,
}
