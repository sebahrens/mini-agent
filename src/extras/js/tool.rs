use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{ToolError, check_perm};
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

use crate::extras::js::types::{JsOutcome, JsRequest};

pub struct JsTool {
    tx: std::sync::mpsc::Sender<JsRequest>,
    permission: Option<PermCheck>,
    ask_tx: Option<AskSender>,
}

impl JsTool {
    pub fn new(
        tx: std::sync::mpsc::Sender<JsRequest>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            tx,
            permission,
            ask_tx,
        }
    }
}

impl Tool for JsTool {
    const NAME: &'static str = "js";
    type Error = ToolError;
    type Args = JsArgs;
    type Output = String;

    fn description(&self) -> String {
        "Execute JavaScript code. Available globals: read_file(path), write_file(path, content), \
         spawn(cmd, args), console.log(...). Returns the last expression value as a string. \
         Errors include the stack trace for self-correction."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "JavaScript code to execute" }
            },
            "required": ["code"]
        })
    }

    async fn call(&self, args: JsArgs) -> Result<String, ToolError> {
        if let Some(msg) = check_perm(&self.permission, &self.ask_tx, "js", &args.code).await? {
            return Ok(format!("JS permission coaching: {msg}"));
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(JsRequest {
                code: args.code,
                reply: reply_tx,
            })
            .map_err(|_| ToolError::Msg("JS engine thread disconnected".into()))?;
        let response = reply_rx
            .await
            .map_err(|_| ToolError::Msg("JS engine reply channel closed".into()))?;
        match response.outcome {
            JsOutcome::Value(v) => Ok(v),
            JsOutcome::Void => Ok(String::new()),
            JsOutcome::Error(e) => Ok(format!("JS error:\n{e}")),
            JsOutcome::Timeout => Ok("JS error: execution timed out (30s limit exceeded)".into()),
            JsOutcome::OomKilled => Ok("JS error: out of memory (64 MiB limit exceeded)".into()),
        }
    }
}

#[derive(Deserialize)]
pub struct JsArgs {
    pub code: String,
}
