//! Connection-owned, exact-RPC elicitation. No permission decision or
//! main-question claim can be produced by an MCP server callback.
use super::forms::RawForms;
use crate::permission::ask::{AskSender, QuestionRequest, UserRequest};
use crate::question::{QuestionAnswer, QuestionTurn};
use rmcp::model::{ClientInfo, ElicitRequestParams, ElicitResult, ElicitationAction};
use rmcp::service::{RequestContext, RoleClient};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

#[derive(Clone)]
pub(crate) enum FrontendOwner {
    Main(String),
    Operator {
        session: compact_str::CompactString,
        workspace: Arc<crate::paths::WorkspaceBinding>,
        generation: u64,
    },
}
pub(crate) struct Request {
    pub question: QuestionRequest,
    pub owner: FrontendOwner,
    pub valid: Arc<dyn Fn() -> bool + Send + Sync>,
}
#[derive(Clone)]
pub(crate) struct AskOrigin {
    pub owner: FrontendOwner,
    pub valid: Arc<dyn Fn() -> bool + Send + Sync>,
}
pub(crate) struct PermissionRequest {
    pub request: crate::permission::ask::AskRequest,
    pub origin: AskOrigin,
}
impl std::fmt::Debug for PermissionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpPermissionRequest")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpElicitationRequest")
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Default)]
pub struct Handler {
    pub forms: RawForms,
    leases: Arc<Mutex<Vec<Weak<LeaseInner>>>>,
    /// Set by `notifications/tools/list_changed`; consumed by the
    /// connection's `ToolListCache` at the next tool collection.
    pub(crate) tools_changed: Arc<AtomicBool>,
    /// In-flight calls whose `notifications/progress` feed the live preview.
    pub(crate) progress: super::progress::ProgressRegistry,
    /// Workspace root advertised through `roots/list`. Only set for stdio
    /// servers, which already run in that workspace; remote (HTTP) servers
    /// are not told local paths and get no roots capability.
    pub(crate) root: Option<Arc<std::path::Path>>,
}

/// Longest URL accepted from a URL-mode elicitation.
const MAX_ELICITATION_URL_BYTES: usize = 1536;
/// Longest server message shown with a URL-mode elicitation.
const MAX_ELICITATION_MESSAGE_CHARS: usize = 300;

/// Bounded single-line rendering of untrusted server text.
fn one_line(text: &str, max_chars: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The question shown for a URL-mode elicitation. The URL is displayed for
/// the user to open themselves; mini-agent never opens or fetches it. Only an
/// absolute HTTPS URL (or loopback HTTP) without embedded credentials passes.
pub(crate) fn url_elicitation_questions(
    server: &str,
    message: &str,
    url: &str,
) -> Result<crate::question::QuestionBatch, &'static str> {
    use crate::question::{Question, QuestionBatch, QuestionOption};
    if url.len() > MAX_ELICITATION_URL_BYTES {
        return Err("elicitation URL too long");
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| "elicitation URL invalid")?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.as_str().chars().any(char::is_control)
    {
        return Err("elicitation URL must be https without credentials");
    }
    let batch = QuestionBatch(vec![Question {
        question: format!(
            "MCP {}: {} — open this link in your browser to continue (mini-agent does not open it): {}",
            one_line(server, 64),
            one_line(message, MAX_ELICITATION_MESSAGE_CHARS),
            parsed.as_str()
        ),
        options: vec![
            QuestionOption {
                label: "Done".into(),
                description: "I opened the link and completed the step".into(),
            },
            QuestionOption {
                label: "Decline".into(),
                description: "Do not continue with this request".into(),
            },
        ],
        allow_free_text: false,
    }]);
    batch.validate()?;
    Ok(batch)
}

enum Elicitation {
    Form(super::forms::Form),
    Url(crate::question::QuestionBatch),
}
struct LeaseInner {
    active: AtomicBool,
    cancelled: tokio::sync::Notify,
    valid: Arc<dyn Fn() -> bool + Send + Sync>,
    ask: Option<AskSender>,
    owner: Option<FrontendOwner>,
    cancellation: Option<crate::agent::runner::AgentWorkCancellation>,
    run: Option<crate::spend::run::Run>,
    question: Option<QuestionTurn>,
    server: String,
    asking: AtomicBool,
}
pub(crate) struct Lease(Arc<LeaseInner>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
        self.0.cancelled.notify_waiters();
    }
}
impl LeaseInner {
    fn current(&self) -> bool {
        self.active.load(Ordering::Acquire)
            && self
                .cancellation
                .as_ref()
                .is_none_or(|cancel| !cancel.is_cancelled())
            && (self.valid)()
            && self
                .question
                .as_ref()
                .is_none_or(|turn| turn.required().is_none())
    }
    async fn retired(&self) {
        loop {
            let changed = self.cancelled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.active.load(Ordering::Acquire) {
                return;
            }
            tokio::select! { _=&mut changed=>{}, _=async {if let Some(cancel)=&self.cancellation {cancel.cancelled().await;} else {std::future::pending::<()>().await;}}=>return }
        }
    }
}
impl Handler {
    /// A handler that advertises `root` through `roots/list`.
    pub(crate) fn with_root(root: &std::path::Path) -> Self {
        Self {
            root: Some(Arc::from(root)),
            ..Self::default()
        }
    }

    pub(crate) fn enter(
        &self,
        server: String,
        ask: Option<AskSender>,
        valid: Arc<dyn Fn() -> bool + Send + Sync>,
        owner: Option<FrontendOwner>,
    ) -> Lease {
        let question = QuestionTurn::current();
        let owner = owner.or_else(|| question.as_ref().map(|turn| FrontendOwner::Main(turn.id())));
        let inner = Arc::new(LeaseInner {
            active: AtomicBool::new(true),
            cancelled: Default::default(),
            valid,
            ask,
            owner,
            cancellation: crate::agent::runner::current_work_cancellation(),
            run: crate::spend::run::Run::current(),
            question,
            server,
            asking: AtomicBool::new(false),
        });
        let mut leases = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        // Only prune dropped leases here: `current()` revalidates the MCP
        // source, and running it for every other live call on each entry made
        // concurrent calls quadratic. `unique()` still filters by `current()`
        // before any elicitation can be routed.
        leases.retain(|lease| lease.strong_count() > 0);
        leases.push(Arc::downgrade(&inner));
        Lease(inner)
    }
    fn unique(&self) -> Option<Arc<LeaseInner>> {
        let mut leases = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        leases.retain(|lease| lease.upgrade().is_some_and(|lease| lease.current()));
        // The protocol does not reliably identify the originating client RPC.
        // Never guess a last tool/turn when two requests share this connection.
        if leases.len() != 1 {
            return None;
        }
        leases[0].upgrade()
    }
}
fn response(action: ElicitationAction, content: Option<serde_json::Value>) -> ElicitResult {
    let mut response = ElicitResult::new(action);
    response.content = content;
    response
}
impl rmcp::ClientHandler for Handler {
    #[allow(deprecated)]
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.capabilities.elicitation = Some(
            rmcp::model::ElicitationCapability::new()
                .with_form(rmcp::model::FormElicitationCapability::default())
                .with_url(rmcp::model::UrlElicitationCapability::new()),
        );
        // Sampling is deliberately not advertised: a server-initiated model
        // request would bypass the operator's turn, budget and permission
        // flow. rmcp answers sampling/createMessage with method-not-found.
        if self.root.is_some() {
            info.capabilities.roots = Some(rmcp::model::RootsCapabilities::default());
        }
        info
    }
    // Roots are deprecated by SEP-2577 but still part of the negotiated
    // protocol that filesystem servers use today.
    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::ErrorData> {
        let mut result = rmcp::model::ListRootsResult::default();
        if let Some(root) = &self.root
            && let Ok(uri) = reqwest::Url::from_directory_path(root)
        {
            let mut entry = rmcp::model::Root::new(uri.to_string());
            entry.name = root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            result.roots.push(entry);
        }
        Ok(result)
    }
    async fn on_tool_list_changed(&self, _context: rmcp::service::NotificationContext<RoleClient>) {
        tracing::debug!("MCP server announced tools/list_changed");
        self.tools_changed.store(true, Ordering::Release);
    }
    async fn on_progress(
        &self,
        params: rmcp::model::ProgressNotificationParam,
        _context: rmcp::service::NotificationContext<RoleClient>,
    ) {
        self.progress.deliver(&params);
    }
    async fn create_elicitation(
        &self,
        params: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        let url = match &params {
            ElicitRequestParams::FormElicitationParams { .. } => None,
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
                Some((message.clone(), url.clone()))
            }
            #[allow(unreachable_patterns)]
            _ => return Ok(response(ElicitationAction::Decline, None)),
        };
        // A form's raw schema is consumed first, as before, so a declined
        // callback cannot leave it for a later request to claim.
        let form = match url {
            None => match self.forms.take(&context.id) {
                Some(form) => Some(form),
                None => return Ok(response(ElicitationAction::Decline, None)),
            },
            Some(_) => None,
        };
        let Some(lease) = self.unique() else {
            return Ok(response(ElicitationAction::Decline, None));
        };
        let elicitation = match (form, url) {
            (Some(form), _) => Elicitation::Form(form),
            (None, Some((message, url))) => {
                match url_elicitation_questions(&lease.server, &message, &url) {
                    Ok(questions) => Elicitation::Url(questions),
                    Err(_) => return Ok(response(ElicitationAction::Decline, None)),
                }
            }
            (None, None) => return Ok(response(ElicitationAction::Decline, None)),
        };
        let Some(ask) = lease.ask.clone() else {
            return Ok(response(ElicitationAction::Decline, None));
        };
        let Some(owner) = lease.owner.clone() else {
            return Ok(response(ElicitationAction::Decline, None));
        };
        let Some(cancellation) = lease.cancellation.clone() else {
            return Ok(response(ElicitationAction::Decline, None));
        };
        cancellation.run_callback(crate::spend::run::scope(lease.run.clone(), async {
        if lease.asking.swap(true,Ordering::AcqRel){return Ok(response(ElicitationAction::Decline,None));}
        struct Asking(Arc<LeaseInner>);
        impl Drop for Asking {fn drop(&mut self){self.0.asking.store(false,Ordering::Release);}}
        let _asking=Asking(lease.clone());
        let questions=match &elicitation {
            Elicitation::Form(form)=>match form.questions(&lease.server){Ok(questions)=>questions,Err(_)=>return Ok(response(ElicitationAction::Decline,None))},
            Elicitation::Url(questions)=>questions.clone(),
        };
        let (reply,receiver)=tokio::sync::oneshot::channel();
        let proof=lease.clone();
        let request=Request {question:QuestionRequest{id:uuid::Uuid::new_v4().to_string(),questions,#[cfg(feature="acp")]tool_call_id:None,reply},owner,valid:Arc::new(move ||proof.current())};
        let answer=tokio::select! {biased;
            _=context.ct.cancelled()=>return Ok(response(ElicitationAction::Cancel,None)),
            _=lease.retired()=>return Ok(response(ElicitationAction::Cancel,None)),
            result=async {ask.send(UserRequest::McpElicitation(request)).await.map_err(|_|())?;receiver.await.map_err(|_|())}=>result,
        };
        if !lease.current(){return Ok(response(ElicitationAction::Cancel,None));}
        let content=match (answer, &elicitation) {
            (Ok(QuestionAnswer::Answered(answers)), Elicitation::Form(form))=>match form.answer(&answers){Ok(value)=>Some(value),Err(_)=>return Ok(response(ElicitationAction::Decline,None))},
            // URL mode carries no content: "Done" accepts, anything else declines.
            (Ok(QuestionAnswer::Answered(answers)), Elicitation::Url(_))=>match answers.as_slice() {
                [crate::question::Answer::Option(0)]=>None,
                _=>return Ok(response(ElicitationAction::Decline,None)),
            },
            _=>return Ok(response(ElicitationAction::Cancel,None)),
        };
        // The answer can unblock the already authorized remote tool. Recheck
        // its captured financial admission before returning any submitted data.
        if let Some(run)=&lease.run {
            let allowed=tokio::select! {biased;_=context.ct.cancelled()=>false,_=lease.retired()=>false,result=run.admit_effect_ready()=>result.is_ok()};
            if !allowed{return Ok(response(ElicitationAction::Cancel,None));}
        }
        if !lease.current(){return Ok(response(ElicitationAction::Cancel,None));}
        Ok(response(ElicitationAction::Accept,content))
        })).await
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use rmcp::ClientHandler as _;

    #[test]
    fn remote_handlers_get_no_roots_and_no_handler_offers_sampling() {
        let remote = Handler::default().get_info();
        assert!(remote.capabilities.roots.is_none());
        assert!(remote.capabilities.sampling.is_none());
        let elicitation = remote.capabilities.elicitation.unwrap();
        assert!(elicitation.form.is_some() && elicitation.url.is_some());
        let local = Handler::with_root(std::path::Path::new("/tmp/ws")).get_info();
        assert!(local.capabilities.roots.is_some());
        assert!(local.capabilities.sampling.is_none());
    }

    #[test]
    fn url_elicitation_shows_only_safe_links_and_never_opens_them() {
        let batch =
            url_elicitation_questions("auth", "Sign in\nnow", "https://auth.example/device?c=1")
                .unwrap();
        let question = &batch.0[0];
        assert!(
            question
                .question
                .contains("https://auth.example/device?c=1")
        );
        assert!(question.question.contains("Sign in now"));
        assert!(question.question.contains("does not open it"));
        assert_eq!(question.options[0].label, "Done");
        assert!(!question.allow_free_text);
        assert!(url_elicitation_questions("a", "m", "http://localhost:8080/cb").is_ok());
        for url in [
            "http://auth.example/",
            "https://user:pw@auth.example/",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(url_elicitation_questions("a", "m", url).is_err(), "{url}");
        }
        let long = format!("https://auth.example/{}", "a".repeat(2000));
        assert!(url_elicitation_questions("a", "m", &long).is_err());
    }
}
