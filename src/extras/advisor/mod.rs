use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rig::streaming::StreamingChat;
use rig::tool::Tool;
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::agent::tools::ToolError;
use crate::provider::{AnyClient, AnyModel, OpenAiModel};
use crate::retry::{self, RetryConfig};
use crate::session::{MessageRole, SessionMessage};

const ADVISOR_SYSTEM_PROMPT: &str = "\
You are an expert advisor called by a coding assistant for strategic guidance. \
The assistant is driving a real coding session with file read/write/edit, \
bash, grep, and other tools at its disposal.

Below is the full conversation so far, followed by the assistant's question. \
Your role:
- Review the conversation to understand what has happened
- Provide a clear plan, approach, or course correction
- Focus on architecture, design decisions, edge cases, and risk
- Keep guidance concise: aim for 150-300 words unless the question demands more
- Do NOT produce user-facing output or call any tools yourself

The assistant will continue the task after receiving your advice. \
Give it the strategic direction it needs to proceed correctly.";

pub struct HandoffRequest {
    pub question: String,
    pub reply: oneshot::Sender<String>,
}

pub type HandoffSender = tokio::sync::mpsc::Sender<HandoffRequest>;
pub type HandoffReceiver = tokio::sync::mpsc::Receiver<HandoffRequest>;

#[derive(Clone)]
pub struct AdvisorToolConfig {
    pub client: Option<AnyClient>,
    pub advisor_provider: String,
    pub advisor_model: String,
    pub human_handoff: bool,
    pub max_uses: Option<usize>,
    pub handoff_tx: Option<HandoffSender>,
    pub enabled: bool,
    pub kilobytes_limit: u32,
}

impl AdvisorToolConfig {
    pub(crate) fn refresh_client(&mut self, provider: &str, client: AnyClient) {
        if provider == self.advisor_provider {
            self.client = Some(client);
        }
    }

    pub(crate) fn select_model(
        &mut self,
        name: &str,
        main_provider: &str,
        main_client: &AnyClient,
        cfg: &crate::config::Config,
        api_key: Option<&str>,
    ) -> anyhow::Result<()> {
        let (provider, model) = resolve_model(name, main_provider, cfg);
        let client = model_client(&provider, main_provider, main_client, cfg, api_key)?;
        self.advisor_provider = provider;
        self.advisor_model = model;
        self.client = Some(client);
        Ok(())
    }

    /// Keep the interactive receiver available even when handoff starts disabled,
    /// so a later mode change can use the same UI-owned channel.
    pub(crate) fn prepare_handoff_channel(
        &mut self,
        is_interactive: bool,
    ) -> Option<HandoffReceiver> {
        if is_interactive {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            self.handoff_tx = Some(tx);
            Some(rx)
        } else {
            self.handoff_tx = None;
            None
        }
    }
}

pub(crate) fn resolve_model(
    name: &str,
    main_provider: &str,
    cfg: &crate::config::Config,
) -> (String, String) {
    match crate::config::quick_models_map(cfg).get(name) {
        Some(model) => (model.provider.to_string(), model.model.to_string()),
        None => (main_provider.to_owned(), name.to_owned()),
    }
}

pub(crate) fn model_client(
    provider: &str,
    main_provider: &str,
    main_client: &AnyClient,
    cfg: &crate::config::Config,
    api_key: Option<&str>,
) -> anyhow::Result<AnyClient> {
    if provider == main_provider {
        Ok(main_client.clone())
    } else {
        crate::provider::create_client(
            provider,
            api_key,
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )
    }
}

static CONFIG: Mutex<Option<AdvisorToolConfig>> = Mutex::new(None);

#[derive(Debug, thiserror::Error)]
#[error("advisor config not initialized")]
pub struct ConfigNotInitialized;

pub fn init_config(cfg: AdvisorToolConfig) {
    tracing::debug!(
        "advisor init: model={}, enabled={}, max_uses={:?}, human_handoff={}",
        cfg.advisor_model,
        cfg.enabled,
        cfg.max_uses,
        cfg.human_handoff,
    );
    *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(cfg);
}

pub fn with_config<F, R>(f: F) -> Result<R, ConfigNotInitialized>
where
    F: FnOnce(&AdvisorToolConfig) -> R,
{
    let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let cfg = guard.as_ref().ok_or(ConfigNotInitialized)?;
    Ok(f(cfg))
}

pub fn update_client(provider: &str, client: AnyClient) {
    let mut guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref mut cfg) = *guard {
        cfg.refresh_client(provider, client);
    }
}

fn current_messages() -> Vec<SessionMessage> {
    crate::agent::runner::with_advisor_messages(|messages| messages.clone()).unwrap_or_default()
}

/// Observe the accepted run history and bounded tool outputs, after result-rewriting hooks.
pub(crate) struct AdvisorContextHook;

impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for AdvisorContextHook {
    async fn on_event(
        &self,
        _ctx: &rig::agent::HookContext,
        event: rig::agent::StepEvent<'_, M>,
    ) -> rig::agent::Flow {
        use rig::agent::StepEvent;
        crate::agent::runner::with_advisor_messages(|messages| match event {
            StepEvent::CompletionCall {
                prompt, history, ..
            } => {
                // Replace rather than append: retries and continuations carry their own history.
                messages.clear();
                for message in history.iter().chain(std::iter::once(prompt)) {
                    append_message(messages, message);
                }
            }
            StepEvent::ModelTurnFinished { content, .. } => append_assistant(messages, content),
            StepEvent::ToolResult { result, .. } => {
                append_context(messages, MessageRole::ToolResult, result.to_owned());
            }
            _ => {}
        });
        rig::agent::Flow::cont()
    }

    fn observes(&self, kind: rig::agent::StepEventKind) -> bool {
        use rig::agent::StepEventKind;
        matches!(
            kind,
            StepEventKind::CompletionCall
                | StepEventKind::ModelTurnFinished
                | StepEventKind::ToolResult
        )
    }
}

fn append_context(messages: &mut Vec<SessionMessage>, role: MessageRole, content: String) {
    messages.push(SessionMessage {
        role,
        content: content.into(),
        estimated_tokens: 0,
        tool_call_id: None,
        tool: None,
    });
}

fn append_message(messages: &mut Vec<SessionMessage>, message: &rig::completion::Message) {
    use rig::completion::Message;
    use rig::message::{ToolResultContent, UserContent};
    match message {
        Message::System { content } => {
            append_context(messages, MessageRole::System, content.clone())
        }
        Message::Assistant { content, .. } => append_assistant(messages, content),
        Message::User { content } => {
            for item in content.iter() {
                match item {
                    UserContent::Text(text) => {
                        append_context(messages, MessageRole::User, text.text.clone())
                    }
                    UserContent::ToolResult(result) => {
                        for item in result.content.iter() {
                            let text = match item {
                                ToolResultContent::Text(text) => text.text.as_str(),
                                ToolResultContent::Image(_) => "[image]",
                            };
                            append_context(messages, MessageRole::ToolResult, text.to_owned());
                        }
                    }
                    UserContent::Image(_) => {
                        append_context(messages, MessageRole::User, "[image]".into())
                    }
                    UserContent::Audio(_) => {
                        append_context(messages, MessageRole::User, "[audio]".into())
                    }
                    UserContent::Video(_) => {
                        append_context(messages, MessageRole::User, "[video]".into())
                    }
                    UserContent::Document(_) => {
                        append_context(messages, MessageRole::User, "[document]".into())
                    }
                }
            }
        }
    }
}

fn append_assistant(
    messages: &mut Vec<SessionMessage>,
    content: &rig::OneOrMany<rig::message::AssistantContent>,
) {
    use rig::message::AssistantContent;
    for item in content.iter() {
        match item {
            AssistantContent::Text(text) => {
                append_context(messages, MessageRole::Assistant, text.text.clone())
            }
            AssistantContent::ToolCall(call) => append_context(
                messages,
                MessageRole::ToolCall,
                format!(
                    "{} {}: {}",
                    call.id, call.function.name, call.function.arguments
                ),
            ),
            AssistantContent::Image(_) => {
                append_context(messages, MessageRole::Assistant, "[image]".into())
            }
            // Provider reasoning/signatures are not part of the advisor's plain-text transcript.
            AssistantContent::Reasoning(_) => {}
        }
    }
}

#[derive(Deserialize)]
pub struct AdvisorArgs {
    pub question: String,
}

pub struct AdvisorTool {
    uses: AtomicUsize,
}

impl AdvisorTool {
    pub fn new() -> Self {
        Self {
            uses: AtomicUsize::new(0),
        }
    }

    fn reserve_use(&self, max_uses: Option<usize>) -> Result<(), ToolError> {
        // Cached agents share tools across requests; the request owns its budget.
        // Direct calls outside a runner retain the tool's local allowance.
        let scoped = crate::agent::runner::current_advisor_usage();
        let uses = scoped.as_deref().unwrap_or(&self.uses);
        if let Some(max) = max_uses {
            uses.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                (used < max).then(|| used + 1)
            })
            .map_err(|_| ToolError::Msg("Advisor call limit reached for this request".into()))?;
        } else {
            uses.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

impl Tool for AdvisorTool {
    const NAME: &'static str = "advisor";
    type Error = ToolError;
    type Args = AdvisorArgs;
    type Output = String;

    fn description(&self) -> String {
        let human_handoff = CONFIG
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|c| c.human_handoff))
            .unwrap_or(false);

        if human_handoff {
            "Consult the user for strategic guidance. \
Call this before substantive work, before writing, before committing to an \
interpretation, when stuck, or when considering a change of approach. \
The user sees your full conversation so far. \
Describe your question clearly — include relevant context, what you're \
trying to do, what you've tried, and what you need guidance on."
                .to_string()
        } else {
            "Consult an expert advisor model for strategic guidance. \
The advisor receives your full conversation transcript automatically. \
Call this before substantive work, before writing, before committing to an \
interpretation, when stuck, or when considering a change of approach. \
Describe your question clearly — the advisor already sees the full \
conversation, so focus your question on the specific decision you need help with."
                .to_string()
        }
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "Your question for the advisor. The advisor \
            already sees the full conversation transcript. Focus on the specific decision, \
            approach, or problem you need guidance on."
                }
            },
            "required": ["question"]
        })
    }

    async fn call(&self, args: AdvisorArgs) -> Result<String, ToolError> {
        if args.question.is_empty() {
            return Err(ToolError::Msg("advisor: question must not be empty".into()));
        }

        tracing::debug!("advisor call: question_len={}", args.question.len());

        let cfg = with_config(|c| c.clone()).map_err(|e| ToolError::Msg(e.to_string()))?;

        self.reserve_use(cfg.max_uses)?;

        if cfg.human_handoff {
            let Some(ref tx) = cfg.handoff_tx else {
                return Err(ToolError::Msg(
                    "Human handoff unavailable (non-interactive mode)".into(),
                ));
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send(HandoffRequest {
                question: args.question,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ToolError::Msg("Handoff channel closed".into()))?;

            match reply_rx.await {
                Ok(response) => {
                    if response.is_empty() {
                        Ok("[User provided no response]".to_string())
                    } else {
                        Ok(response)
                    }
                }
                Err(_) => Err(ToolError::Msg("Handoff cancelled".into())),
            }
        } else {
            let Some(ref client) = cfg.client else {
                return Err(ToolError::Msg("Advisor model not configured".into()));
            };

            let model = client.completion_model(cfg.advisor_model.clone());
            let messages = current_messages();
            run_advisor_completion(model, &args.question, &messages, cfg.kilobytes_limit)
                .await
                .map_err(|e| ToolError::Msg(format!("Advisor call failed: {e}")))
        }
    }
}

async fn run_advisor_completion(
    model: AnyModel,
    question: &str,
    messages: &[SessionMessage],
    kilobytes_limit: u32,
) -> anyhow::Result<String> {
    let conversation = format_conversation(messages, kilobytes_limit);
    let prompt = format!(
        "## Conversation\n\n{}\n\n## Assistant's question\n\n{}",
        conversation, question
    );

    match model {
        AnyModel::OpenRouter(m, _) => advisor_call(m, prompt).await,
        AnyModel::OpenAI(m) => match m {
            OpenAiModel::Responses(m) => advisor_call(m, prompt).await,
            OpenAiModel::Completions(m) => advisor_call(m, prompt).await,
        },
        AnyModel::Anthropic(m) => advisor_call(m, prompt).await,
        AnyModel::Gemini(m) => advisor_call(m, prompt).await,
        AnyModel::Ollama(m) => advisor_call(m, prompt).await,
    }
}

pub(crate) fn format_conversation(msgs: &[SessionMessage], kilobytes_limit: u32) -> String {
    let per_side = (kilobytes_limit as usize * 1024) / 2;

    fn format_line(msg: &SessionMessage) -> String {
        let role = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
            MessageRole::ToolCall => "ToolCall",
            MessageRole::ToolResult => "ToolResult",
            MessageRole::SubagentToolCall => "SubagentToolCall",
        };
        format!("[{role}]: {}", msg.content)
    }

    // Collect head (oldest messages)
    let mut head_end = 0usize;
    let mut head_chars = 0usize;
    for (i, msg) in msgs.iter().enumerate() {
        let line = format_line(msg);
        let needed = if head_chars > 0 {
            line.len() + 2
        } else {
            line.len()
        };
        if head_chars + needed > per_side {
            break;
        }
        head_chars += needed;
        head_end = i + 1;
    }

    // Collect tail (newest messages)
    let mut tail_start = msgs.len();
    let mut tail_chars = 0usize;
    for (i, msg) in msgs.iter().enumerate().rev() {
        let line = format_line(msg);
        let needed = if tail_chars > 0 {
            line.len() + 2
        } else {
            line.len()
        };
        if tail_chars + needed > per_side {
            break;
        }
        tail_chars += needed;
        tail_start = i;
    }

    if msgs.is_empty() {
        return String::new();
    }

    let mut result = String::new();

    // Head
    for (i, msg) in msgs.iter().enumerate().take(head_end) {
        if i > 0 {
            result.push_str("\n\n");
        }
        result.push_str(&format_line(msg));
    }

    // Omission marker if there is a gap
    if head_end < tail_start {
        result.push_str("\n\n[... conversation omitted ...]\n\n");
    } else if head_end > 0 && head_end < msgs.len() {
        result.push_str("\n\n");
    }

    // Tail (only messages not already in head)
    let tail_begin = head_end.max(tail_start);
    for (i, msg) in msgs.iter().enumerate().skip(tail_begin) {
        if i > tail_begin {
            result.push_str("\n\n");
        }
        result.push_str(&format_line(msg));
    }

    result
}

async fn advisor_call<M>(model: M, prompt: String) -> anyhow::Result<String>
where
    M: rig::completion::CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    let mut preamble = ADVISOR_SYSTEM_PROMPT.to_string();
    if let Some(s) = crate::session::storage::load_suffix() {
        preamble.push_str("\n\n---\n\n");
        preamble.push_str(&s);
    }

    let agent = rig::agent::AgentBuilder::new(model)
        .preamble(&preamble)
        .build();

    use futures::StreamExt;
    let _history: Vec<rig::completion::Message> = vec![];
    let agent_ref = &agent;
    let mut stream = retry::retry_stream_chat(&RetryConfig::default(), move || {
        let p = prompt.clone();
        let h: Vec<rig::completion::Message> = vec![];
        async move { agent_ref.stream_chat(p, h).max_turns(1).await }
    })
    .await
    .map_err(|e| anyhow::anyhow!("Advisor call failed: {e}"))?;

    let mut response = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(rig::agent::MultiTurnStreamItem::FinalResponse(res)) => {
                response = res.output.to_string();
                break;
            }
            Err(e) => return Err(anyhow::anyhow!("Advisor call failed: {e}")),
            _ => {}
        }
    }

    if response.is_empty() {
        Ok("[Advisor returned empty response]".to_string())
    } else {
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone)]
    struct ContextProbe {
        snapshots: Arc<Mutex<Vec<(String, String)>>>,
        barrier: Option<Arc<tokio::sync::Barrier>>,
    }

    impl Tool for ContextProbe {
        const NAME: &'static str = "context_probe";
        type Error = ToolError;
        type Args = AdvisorArgs;
        type Output = String;

        fn description(&self) -> String {
            "Capture the advisor transcript".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]})
        }

        async fn call(&self, args: AdvisorArgs) -> Result<String, ToolError> {
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            self.snapshots
                .lock()
                .unwrap()
                .push((args.question, format_conversation(&current_messages(), 256)));
            Ok("context captured".into())
        }
    }

    struct EvidenceTool;

    impl Tool for EvidenceTool {
        const NAME: &'static str = "evidence";
        type Error = ToolError;
        type Args = AdvisorArgs;
        type Output = String;

        fn description(&self) -> String {
            "Produce evidence for the advisor".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"question": {"type": "string"}}, "required": ["question"]})
        }

        async fn call(&self, _: AdvisorArgs) -> Result<String, ToolError> {
            Ok("raw output removed by result hook".into())
        }
    }

    struct RewriteEvidence;

    impl<M: rig::completion::CompletionModel> rig::agent::AgentHook<M> for RewriteEvidence {
        async fn on_event(
            &self,
            _: &rig::agent::HookContext,
            event: rig::agent::StepEvent<'_, M>,
        ) -> rig::agent::Flow {
            if let rig::agent::StepEvent::ToolResult {
                tool_name: EvidenceTool::NAME,
                ..
            } = event
            {
                rig::agent::Flow::rewrite_result("bounded visible evidence")
            } else {
                rig::agent::Flow::cont()
            }
        }
    }

    #[test]
    fn advisor_context_represents_media_without_copying_payloads_or_reasoning() {
        use rig::OneOrMany;
        use rig::completion::Message;
        use rig::message::{
            AssistantContent, Audio, Document, DocumentSourceKind, Image, Reasoning,
            ToolResultContent, UserContent, Video,
        };
        let data = DocumentSourceKind::Base64("private-payload".into());
        let image = Image {
            data: data.clone(),
            ..Default::default()
        };
        let history = [
            Message::User {
                content: OneOrMany::many([
                    UserContent::Image(image.clone()),
                    UserContent::Audio(Audio {
                        data: data.clone(),
                        ..Default::default()
                    }),
                    UserContent::Video(Video {
                        data: data.clone(),
                        ..Default::default()
                    }),
                    UserContent::Document(Document {
                        data,
                        ..Default::default()
                    }),
                ])
                .unwrap(),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::many([
                    AssistantContent::Image(image.clone()),
                    AssistantContent::Reasoning(Reasoning::new_with_signature(
                        "private reasoning",
                        Some("private signature".into()),
                    )),
                ])
                .unwrap(),
            },
            Message::User {
                content: OneOrMany::one(UserContent::tool_result(
                    "media-result",
                    OneOrMany::one(ToolResultContent::Image(image)),
                )),
            },
        ];
        let mut messages = Vec::new();
        for message in history {
            append_message(&mut messages, &message);
        }
        assert_eq!(
            format_conversation(&messages, 256),
            "[User]: [image]\n\n[User]: [audio]\n\n[User]: [video]\n\n[User]: [document]\n\n[Assistant]: [image]\n\n[ToolResult]: [image]"
        );
    }

    #[tokio::test]
    async fn advisor_context_tracks_live_results_and_refreshes_without_duplicates() {
        use futures::StreamExt;
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![
                MockStreamEvent::text("checking current evidence"),
                MockStreamEvent::tool_call(
                    "evidence-id",
                    EvidenceTool::NAME,
                    serde_json::json!({"question": "read"}),
                ),
                MockStreamEvent::tool_call(
                    "probe-first",
                    ContextProbe::NAME,
                    serde_json::json!({"question": "first"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::tool_call(
                    "probe-second",
                    ContextProbe::NAME,
                    serde_json::json!({"question": "second"}),
                ),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = rig::agent::AgentBuilder::new(model)
            .tool(EvidenceTool)
            .tool(ContextProbe {
                snapshots: Arc::clone(&snapshots),
                barrier: None,
            })
            .add_hook(RewriteEvidence)
            .add_hook(AdvisorContextHook)
            .build();
        let scope = crate::agent::runner::AgentWorkScope::new();
        scope
            .run(async {
                let mut stream = agent
                    .stream_chat(
                        "active user request",
                        vec![
                            rig::completion::Message::system("prior system context"),
                            rig::completion::Message::user("prior user request"),
                            rig::completion::Message::assistant("prior assistant response"),
                        ],
                    )
                    .max_turns(4)
                    .tool_concurrency(1)
                    .await;
                while let Some(item) = stream.next().await {
                    item.unwrap();
                }
            })
            .await;
        let snapshots = snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2);
        for (label, transcript) in snapshots.iter() {
            for required in [
                "[System]: prior system context",
                "[User]: prior user request",
                "[Assistant]: prior assistant response",
                "[User]: active user request",
                "[Assistant]: checking current evidence",
                "[ToolCall]: evidence-id evidence:",
                "[ToolResult]: bounded visible evidence",
            ] {
                assert!(
                    transcript.contains(required),
                    "{label} is missing {required}: {transcript}"
                );
                assert_eq!(
                    transcript.matches(required).count(),
                    1,
                    "duplicate {required}: {transcript}"
                );
            }
            assert!(!transcript.contains("raw output removed"));
        }
        assert!(snapshots[1].1.contains("[ToolResult]: context captured"));
    }

    #[tokio::test]
    async fn advisor_context_is_isolated_between_concurrent_requests() {
        use futures::StreamExt;
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let run = |label: &'static str| {
            let probe = ContextProbe {
                snapshots: Arc::clone(&snapshots),
                barrier: Some(Arc::clone(&barrier)),
            };
            async move {
                let model = MockCompletionModel::from_stream_turns(vec![
                    vec![
                        MockStreamEvent::tool_call(
                            "probe",
                            ContextProbe::NAME,
                            serde_json::json!({"question": label}),
                        ),
                        MockStreamEvent::final_response_with_default_usage(),
                    ],
                    vec![
                        MockStreamEvent::text("done"),
                        MockStreamEvent::final_response_with_default_usage(),
                    ],
                ]);
                let agent = rig::agent::AgentBuilder::new(model)
                    .tool(probe)
                    .add_hook(AdvisorContextHook)
                    .build();
                crate::agent::runner::AgentWorkScope::new()
                    .run(async {
                        let mut stream = agent
                            .stream_chat(label, Vec::<rig::completion::Message>::new())
                            .max_turns(3)
                            .await;
                        while let Some(item) = stream.next().await {
                            item.unwrap();
                        }
                    })
                    .await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(run("request-alpha"), run("request-beta"));
        })
        .await
        .expect("both requests reach the probe");
        {
            let snapshots = snapshots.lock().unwrap();
            assert_eq!(snapshots.len(), 2);
            for (label, transcript) in snapshots.iter() {
                assert!(transcript.contains(&format!("[User]: {label}")));
                let other = if label == "request-alpha" {
                    "request-beta"
                } else {
                    "request-alpha"
                };
                assert!(
                    !transcript.contains(other),
                    "request context leaked: {transcript}"
                );
            }
        }
        crate::agent::runner::AgentWorkScope::new()
            .run(async {
                assert!(
                    current_messages().is_empty(),
                    "a new request starts without previous context"
                );
            })
            .await;
        assert!(
            current_messages().is_empty(),
            "unscoped calls cannot read another request"
        );
    }

    fn routing_config() -> crate::config::Config {
        toml::from_str(
            r#"
            [custom_providers.main_gateway]
            provider_type = "openai"
            base_url = "https://main.invalid/v1"
            api_style = "completions"
            [custom_providers.advisor_gateway]
            provider_type = "openai"
            base_url = "https://advisor.invalid/v1"
            api_style = "completions"
            [quick_models.consult]
            provider = "advisor_gateway"
            model = "advisor-model"
            [quick_models.local]
            provider = "main_gateway"
            model = "local-model"
            [quick_models.broken]
            provider = "missing-provider"
            model = "unavailable-model"
        "#,
        )
        .unwrap()
    }

    fn routing_advisor() -> AdvisorToolConfig {
        AdvisorToolConfig {
            client: None,
            advisor_provider: String::new(),
            advisor_model: String::new(),
            human_handoff: false,
            max_uses: Some(3),
            handoff_tx: None,
            enabled: true,
            kilobytes_limit: 256,
        }
    }

    fn client_url(client: &AnyClient) -> &str {
        match client {
            AnyClient::OpenAI(crate::provider::OpenAiClient::Completions(client)) => {
                client.base_url()
            }
            _ => panic!("expected an OpenAI completions gateway"),
        }
    }

    #[test]
    fn advisor_model_selection_resolves_routes_and_preserves_selection_on_error() {
        let config = routing_config();
        let main = crate::provider::create_client(
            "main_gateway",
            Some("test-key"),
            &config.custom_providers_map(),
            None,
        )
        .unwrap();
        let mut advisor = routing_advisor();
        for (name, provider, model, url) in [
            (
                "consult",
                "advisor_gateway",
                "advisor-model",
                "https://advisor.invalid/v1",
            ),
            (
                "bare-model",
                "main_gateway",
                "bare-model",
                "https://main.invalid/v1",
            ),
            (
                "local",
                "main_gateway",
                "local-model",
                "https://main.invalid/v1",
            ),
        ] {
            advisor
                .select_model(name, "main_gateway", &main, &config, Some("test-key"))
                .unwrap();
            assert_eq!(advisor.advisor_provider, provider);
            assert_eq!(advisor.advisor_model, model);
            assert_eq!(client_url(advisor.client.as_ref().unwrap()), url);
            assert!(
                advisor
                    .select_model("broken", "main_gateway", &main, &config, Some("test-key"))
                    .is_err()
            );
            assert_eq!(advisor.advisor_provider, provider);
            assert_eq!(advisor.advisor_model, model);
            assert_eq!(client_url(advisor.client.as_ref().unwrap()), url);
        }
    }

    #[test]
    fn advisor_refresh_distinguishes_custom_providers_using_the_same_protocol() {
        let mut config = routing_config();
        let main = crate::provider::create_client(
            "main_gateway",
            Some("test-key"),
            &config.custom_providers_map(),
            None,
        )
        .unwrap();
        let mut advisor = routing_advisor();
        advisor
            .select_model("consult", "main_gateway", &main, &config, Some("test-key"))
            .unwrap();
        advisor.refresh_client("main_gateway", main);
        assert_eq!(
            client_url(advisor.client.as_ref().unwrap()),
            "https://advisor.invalid/v1"
        );
        config
            .custom_providers
            .as_mut()
            .unwrap()
            .get_mut("advisor_gateway")
            .unwrap()
            .base_url = "https://rotated.invalid/v1".into();
        let refreshed = crate::provider::create_client(
            "advisor_gateway",
            Some("rotated-key"),
            &config.custom_providers_map(),
            None,
        )
        .unwrap();
        advisor.refresh_client("advisor_gateway", refreshed);
        assert_eq!(
            client_url(advisor.client.as_ref().unwrap()),
            "https://rotated.invalid/v1"
        );
        assert_eq!(advisor.advisor_provider, "advisor_gateway");
        assert_eq!(advisor.advisor_model, "advisor-model");
    }

    #[tokio::test]
    async fn cached_advisor_tool_has_a_fresh_allowance_for_each_request() {
        use crate::agent::runner::AgentWorkScope;
        let tool = AdvisorTool::new();
        // Unscoped calls remain bounded, but must not consume a later request.
        tool.reserve_use(Some(1)).unwrap();
        assert!(tool.reserve_use(Some(1)).is_err());
        for _ in 0..2 {
            let request = AgentWorkScope::new();
            request
                .run(async {
                    tool.reserve_use(Some(1)).unwrap();
                    assert!(tool.reserve_use(Some(1)).is_err());
                })
                .await;
        }
    }

    #[tokio::test]
    async fn advisor_allowances_are_shared_with_children_and_isolated_between_requests() {
        use crate::agent::runner::{AgentWorkScope, spawn_async_scoped};
        let tool = std::sync::Arc::new(AdvisorTool::new());
        let first = AgentWorkScope::new();
        let second = AgentWorkScope::new();
        let run = || async {
            let mut calls = Vec::new();
            for _ in 0..3 {
                let tool = std::sync::Arc::clone(&tool);
                calls.push(spawn_async_scoped(async move {
                    tool.reserve_use(Some(2)).is_ok()
                }));
            }
            let mut accepted = 0;
            for call in calls {
                accepted += usize::from(call.await.unwrap());
            }
            assert_eq!(accepted, 2);
            assert!(tool.reserve_use(Some(2)).is_err());
            assert!(AdvisorTool::new().reserve_use(Some(2)).is_err());
        };
        tokio::join!(first.run(run()), second.run(run()));
        first.wait_idle().await;
        second.wait_idle().await;

        // Re-entering the same scope (as retries/continuations do) keeps its usage.
        first
            .run(async {
                assert!(tool.reserve_use(Some(2)).is_err());
            })
            .await;
        let unlimited = AgentWorkScope::new();
        unlimited
            .run(async {
                for _ in 0..4 {
                    tool.reserve_use(None).unwrap();
                }
                assert!(tool.reserve_use(Some(4)).is_err());
                tool.reserve_use(Some(5)).unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn interactive_handoff_can_be_enabled_after_model_only_startup() {
        let initial = AdvisorToolConfig {
            client: None,
            advisor_provider: "provider".into(),
            advisor_model: "model".into(),
            human_handoff: false,
            max_uses: Some(3),
            handoff_tx: None,
            enabled: false,
            kilobytes_limit: 256,
        };
        let mut config = initial.clone();
        let mut ui_rx = config
            .prepare_handoff_channel(true)
            .expect("interactive receiver");
        assert!(!config.enabled && !config.human_handoff);

        // Runtime settings clone and replace this config. Repeated mode changes
        // must retain the channel whose receiver was handed to the UI at startup.
        for response in ["first answer", "second answer"] {
            let mut updated = config.clone();
            updated.enabled = true;
            updated.human_handoff = true;
            config = updated;
            let (reply_tx, reply_rx) = oneshot::channel();
            config
                .handoff_tx
                .as_ref()
                .expect("runtime handoff sender")
                .send(HandoffRequest {
                    question: "continue?".into(),
                    reply: reply_tx,
                })
                .await
                .unwrap();
            let request = ui_rx
                .try_recv()
                .expect("request reaches original UI receiver");
            assert_eq!(request.question, "continue?");
            request.reply.send(response.into()).unwrap();
            assert_eq!(reply_rx.await.unwrap(), response);
            config.human_handoff = false;
        }

        // Headless startup never exposes a UI channel, even for handoff mode.
        for handoff in [false, true] {
            let mut headless = initial.clone();
            headless.human_handoff = handoff;
            assert!(headless.prepare_handoff_channel(false).is_none());
            assert!(headless.handoff_tx.is_none());
        }
    }

    #[test]
    fn with_config_without_init_returns_error() {
        let previous = CONFIG.lock().unwrap_or_else(|e| e.into_inner()).take();

        let result = with_config(|_| ());

        *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = previous;
        assert!(matches!(result, Err(ConfigNotInitialized)));
    }
}
