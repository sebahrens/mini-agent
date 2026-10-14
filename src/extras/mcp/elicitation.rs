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
        leases.retain(|lease| lease.upgrade().is_some_and(|lease| lease.current()));
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
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.capabilities.elicitation = Some(
            rmcp::model::ElicitationCapability::new()
                .with_form(rmcp::model::FormElicitationCapability::default()),
        );
        info
    }
    async fn create_elicitation(
        &self,
        params: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        if !matches!(params, ElicitRequestParams::FormElicitationParams { .. }) {
            return Ok(response(ElicitationAction::Decline, None));
        }
        let Some(form) = self.forms.take(&context.id) else {
            return Ok(response(ElicitationAction::Decline, None));
        };
        let Some(lease) = self.unique() else {
            return Ok(response(ElicitationAction::Decline, None));
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
        let questions=match form.questions(&lease.server){Ok(questions)=>questions,Err(_)=>return Ok(response(ElicitationAction::Decline,None))};
        let (reply,receiver)=tokio::sync::oneshot::channel();
        let proof=lease.clone();
        let request=Request {question:QuestionRequest{id:uuid::Uuid::new_v4().to_string(),questions,#[cfg(feature="acp")]tool_call_id:None,reply},owner,valid:Arc::new(move ||proof.current())};
        let answer=tokio::select! {biased;
            _=context.ct.cancelled()=>return Ok(response(ElicitationAction::Cancel,None)),
            _=lease.retired()=>return Ok(response(ElicitationAction::Cancel,None)),
            result=async {ask.send(UserRequest::McpElicitation(request)).await.map_err(|_|())?;receiver.await.map_err(|_|())}=>result,
        };
        if !lease.current(){return Ok(response(ElicitationAction::Cancel,None));}
        let content=match answer {
            Ok(QuestionAnswer::Answered(answers))=>match form.answer(&answers){Ok(value)=>value,Err(_)=>return Ok(response(ElicitationAction::Decline,None))},
            _=>return Ok(response(ElicitationAction::Cancel,None)),
        };
        // The answer can unblock the already authorized remote tool. Recheck
        // its captured financial admission before returning any submitted data.
        if let Some(run)=&lease.run {
            let allowed=tokio::select! {biased;_=context.ct.cancelled()=>false,_=lease.retired()=>false,result=run.admit_effect_ready()=>result.is_ok()};
            if !allowed{return Ok(response(ElicitationAction::Cancel,None));}
        }
        if !lease.current(){return Ok(response(ElicitationAction::Cancel,None));}
        Ok(response(ElicitationAction::Accept,Some(content)))
        })).await
    }
}
