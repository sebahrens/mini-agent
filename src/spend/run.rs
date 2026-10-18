//! Explicit per-run accounting profile and effect admission. The profile is
//! copied into SDK extensions; task-local scope is never assumed to cross spawn.
use super::{Account, Ledger, PriceSnapshot, Receipt, RequestKind};
use crate::{config::Config, event::UsageDelta, session::Session};
use rig::agent::{AgentHook, Flow, HookContext, StepEvent};
use rig::completion::CompletionModel;
use rig::tool::ToolCallExtensions;
use std::sync::Arc;

pub(crate) const BUDGET_STOP: &str = "USD budget prevents further model requests or effects";

#[derive(Debug, thiserror::Error)]
#[error("USD budget prevents further model requests or effects")]
pub(crate) struct BudgetStopped;

#[derive(Clone)]
pub(crate) struct Run {
    pub(crate) account: Account,
    ledger: Option<Arc<Ledger>>,
    session_id: String,
    provider: String,
    model: String,
    pub(crate) prices: PriceSnapshot,
    kind: RequestKind,
    setup_error: Option<Arc<str>>,
    reasoning_exclusive: bool,
    cache_ttl: crate::config::PromptCacheTtl,
    cap_required: bool,
    refused: Arc<std::sync::atomic::AtomicBool>,
    sdk_request: Arc<std::sync::Mutex<std::sync::Weak<SdkRequest>>>,
    accepted_usage: Arc<std::sync::Mutex<UsageDelta>>,
    #[cfg(feature = "subagents")]
    background_observation: Option<crate::session::background_observation::JobObservation>,
    #[cfg(feature = "subagents")]
    background_pool: std::sync::Weak<crate::extras::subagents::background::Pool>,
}

tokio::task_local! { static CURRENT: Option<Run>; }

impl Run {
    #[cfg(test)]
    pub(crate) fn fixture(
        account: Account,
        ledger: Arc<Ledger>,
        session_id: &str,
        prices: PriceSnapshot,
    ) -> Self {
        let cap_required = account.snapshot().cap.is_some();
        Self {
            account,
            ledger: Some(ledger),
            session_id: session_id.into(),
            provider: "openai".into(),
            model: "fixture".into(),
            prices,
            kind: RequestKind::Turn,
            setup_error: None,
            reasoning_exclusive: false,
            cache_ttl: Default::default(),
            cap_required,
            refused: Default::default(),
            sdk_request: Default::default(),
            accepted_usage: Default::default(),
            #[cfg(feature = "subagents")]
            background_observation: None,
            #[cfg(feature = "subagents")]
            background_pool: Default::default(),
        }
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn background_pool(
        &self,
    ) -> Option<Arc<crate::extras::subagents::background::Pool>> {
        self.background_pool.upgrade()
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn background_ask_origin(
        &self,
    ) -> Option<crate::extras::subagents::background::AskOrigin> {
        self.background_pool()?
            .ask_origin(self.background_observation.clone()?)
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn background_notices(
        &self,
    ) -> Option<crate::extras::subagents::background::NoticeLease> {
        if !self.kind.is_main_turn() || self.background_observation.is_some() {
            return None;
        }
        self.background_pool()?.pending_notices()
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn belongs_to_account(&self, session: &str, account: &Account) -> bool {
        self.session_id == session && self.account.same_owner(account)
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn with_background_pool(
        mut self,
        pool: &Arc<crate::extras::subagents::background::Pool>,
    ) -> Self {
        self.background_pool = Arc::downgrade(pool);
        self
    }

    #[cfg(feature = "subagents")]
    pub(crate) fn with_background_observation(
        mut self,
        observation: crate::session::background_observation::JobObservation,
    ) -> Self {
        self.background_observation = Some(observation);
        self
    }

    pub(crate) fn for_session(session: &mut Session, cfg: &Config, kind: RequestKind) -> Self {
        let initialization_error = session
            .initialize_spend_account(cfg.max_budget_usd)
            .err()
            .map(|_| Arc::<str>::from("invalid session cost accounting baseline"));
        let ledger =
            crate::paths::process_paths().map(|paths| Arc::new(Ledger::new(&paths.state_dir)));
        let (ledger, setup_error) = match ledger {
            Ok(ledger) => {
                let ledger = session.spend_account.runtime_ledger(ledger);
                (Some(ledger), None)
            }
            Err(_) => {
                session.spend_account.tracking_failed();
                // No path is opened after this setup error, even in uncapped mode.
                (None, Some(Arc::from("usage ledger storage unavailable")))
            }
        };
        #[cfg(feature = "subagents")]
        let background_pool = crate::extras::subagents::background::Pool::for_session(
            session,
            cfg.resolve_task_max_concurrency(),
        );
        #[cfg(feature = "subagents")]
        let setup_error = setup_error.or_else(|| {
            background_pool
                .as_ref()
                .err()
                .map(|_| Arc::<str>::from("background session state is invalid"))
        });
        let mut run = Self {
            account: session.spend_account.clone(),
            ledger,
            session_id: session.id.to_string(),
            provider: session.provider.to_string(),
            model: session.model.to_string(),
            prices: PriceSnapshot::from_session(session, cfg),
            kind,
            setup_error: initialization_error.or(setup_error),
            cap_required: cfg.max_budget_usd.is_some(),
            refused: Default::default(),
            sdk_request: Default::default(),
            accepted_usage: Default::default(),
            #[cfg(feature = "subagents")]
            background_observation: None,
            #[cfg(feature = "subagents")]
            background_pool: background_pool
                .as_ref()
                .map(Arc::downgrade)
                .unwrap_or_default(),
            cache_ttl: cfg.cache_ttl.unwrap_or_default(),
            reasoning_exclusive: cfg.reasoning_tokens_are_exclusive_of_output(&session.provider),
        };
        run.reject_model_override(cfg);
        run
    }
    /// ACP stores only process-local history. Its logical session still owns
    /// the same financial account and private ledger as the other frontends.
    #[cfg(feature = "acp")]
    pub(crate) fn for_new_session(
        session_id: &str,
        provider: &str,
        model: &str,
        cfg: &Config,
        selected: Option<&str>,
    ) -> anyhow::Result<Self> {
        let account = Account::new(session_id, cfg.max_budget_usd, 0.0)?;
        let (ledger, setup_error) = match crate::paths::process_paths() {
            Ok(paths) => {
                let ledger = account.runtime_ledger(Arc::new(Ledger::new(&paths.state_dir)));
                (Some(ledger), None)
            }
            Err(_) => {
                account.tracking_failed();
                (None, Some(Arc::from("usage ledger storage unavailable")))
            }
        };
        let run = Self {
            account,
            ledger,
            session_id: session_id.into(),
            provider: provider.into(),
            model: model.into(),
            prices: PriceSnapshot::capture(None, Default::default(), false),
            kind: RequestKind::Turn,
            setup_error,
            cap_required: cfg.max_budget_usd.is_some(),
            refused: Default::default(),
            sdk_request: Default::default(),
            accepted_usage: Default::default(),
            #[cfg(feature = "subagents")]
            background_observation: None,
            #[cfg(feature = "subagents")]
            background_pool: Default::default(),
            cache_ttl: cfg.cache_ttl.unwrap_or_default(),
            reasoning_exclusive: cfg.reasoning_tokens_are_exclusive_of_output(provider),
        };
        Ok(run.for_model(provider, model, cfg, selected, None, RequestKind::Turn))
    }

    pub(crate) fn with_captured_profile(
        &self,
        provider: &str,
        model: &str,
        prices: PriceSnapshot,
        cfg: &Config,
        kind: RequestKind,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            prices: prices.with_cache_policy(cfg, model),
            kind,
            cache_ttl: cfg.cache_ttl.unwrap_or_default(),
            reasoning_exclusive: cfg.reasoning_tokens_are_exclusive_of_output(provider),
            refused: Default::default(),
            sdk_request: Default::default(),
            accepted_usage: Default::default(),
            ..self.clone()
        }
    }

    /// Distinct child invocations share only the financial account/storage and
    /// captured request parameters. Refusal and SDK correlation belong to one
    /// invocation, not to the whole batch/model profile.
    #[cfg(feature = "subagents")]
    pub(crate) fn for_invocation(&self) -> Self {
        Self {
            refused: Default::default(),
            sdk_request: Default::default(),
            accepted_usage: Default::default(),
            ..self.clone()
        }
    }

    #[cfg(feature = "goal")]
    pub(crate) fn for_judge(
        &self,
        judge: &crate::extras::goal::ResolvedJudge,
        cfg: &Config,
        source: Option<&crate::provider_listing::ListingKey>,
    ) -> Self {
        if judge.same_as_session {
            self.with_captured_profile(
                &judge.provider,
                &judge.model,
                self.prices,
                cfg,
                RequestKind::Judge,
            )
        } else {
            self.for_model(
                &judge.provider,
                &judge.model,
                cfg,
                Some(&judge.label),
                source,
                RequestKind::Judge,
            )
        }
    }

    #[cfg(any(
        feature = "goal",
        feature = "subagents",
        feature = "advisor",
        feature = "acp"
    ))]
    pub(crate) fn for_model(
        &self,
        provider: &str,
        model: &str,
        cfg: &Config,
        selected: Option<&str>,
        source: Option<&crate::provider_listing::ListingKey>,
        kind: RequestKind,
    ) -> Self {
        let quick = crate::config::quick_models_map(cfg);
        let prices = selected
            .and_then(|alias| quick.get(alias))
            .filter(|entry| entry.provider == provider && entry.model == model)
            .and_then(|entry| entry.resolved_prices())
            .or_else(|| crate::ui::slash::resolve_model_pricing(provider, model, cfg, source));
        let cache = crate::pricing::cache_prices_for_model(
            provider,
            model,
            cfg,
            selected,
            prices.map_or(0.0, |prices| prices.0),
        );
        let mut run = Self {
            provider: provider.into(),
            model: model.into(),
            prices: PriceSnapshot::capture(prices, cache, cfg.is_anthropic_native(provider))
                .with_cache_policy(cfg, model),
            kind,
            cache_ttl: cfg.cache_ttl.unwrap_or_default(),
            reasoning_exclusive: cfg.reasoning_tokens_are_exclusive_of_output(provider),
            refused: Default::default(),
            sdk_request: Default::default(),
            accepted_usage: Default::default(),
            ..self.clone()
        };
        run.reject_model_override(cfg);
        run
    }
    fn reject_model_override(&mut self, cfg: &Config) {
        if !matches!(
            self.kind,
            RequestKind::Turn
                | RequestKind::Scheduled
                | RequestKind::Btw
                | RequestKind::Subagent
                | RequestKind::Advisor
        ) {
            return;
        }
        let extra = crate::config::resolve_extra_body(cfg, &self.model);
        if extra
            .as_ref()
            .and_then(|extra| extra.get("model"))
            .is_some_and(|model| model.as_str() != Some(self.model.as_str()))
        {
            self.prices =
                PriceSnapshot::capture(None, Default::default(), self.prices.anthropic_native);
        }
    }
    /// The accounting kind every request of this run records.
    pub(crate) fn kind(&self) -> RequestKind {
        self.kind
    }
    pub(crate) fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok().flatten()
    }
    pub(crate) fn admit(&self) -> anyhow::Result<()> {
        if self.cap_required && self.setup_error.is_some() {
            self.refused
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(BudgetStopped.into());
        }
        self.account.admit(self.prices).map_err(|_| {
            self.refused
                .store(true, std::sync::atomic::Ordering::Release);
            anyhow::Error::from(BudgetStopped)
        })
    }
    pub(crate) fn admit_effect(&self) -> anyhow::Result<()> {
        if self.cap_required && (self.setup_error.is_some() || !self.account.ledger_ready()) {
            self.refused
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(BudgetStopped.into());
        }
        self.account.admit_effect().map_err(|_| {
            self.refused
                .store(true, std::sync::atomic::Ordering::Release);
            anyhow::Error::from(BudgetStopped)
        })
    }
    pub(crate) async fn admit_effect_ready(&self) -> anyhow::Result<()> {
        // Another admitted sibling can finish while this writer is settling.
        // Retry bounded persistence batches rather than borrowing its receipt
        // or misclassifying a temporary pending write as exhausted dollars.
        for _ in 0..8 {
            let snapshot = self.account.snapshot();
            if self.cap_required
                && (snapshot.exhausted() || snapshot.tracking_error || snapshot.unpriced_tokens > 0)
            {
                return self.admit_effect();
            }
            self.flush().await;
            if !self.account.has_pending() {
                return self.admit_effect();
            }
        }
        self.refused
            .store(true, std::sync::atomic::Ordering::Release);
        Err(BudgetStopped.into())
    }
    /// Charge before disk/output/cancellation awaits. Tracking failure never
    /// erases observed usage; the next capped admission fails closed.
    pub(crate) fn observe(&self, usage: UsageDelta) {
        self.observe_receipt(&self.receipt(usage));
    }
    fn receipt(&self, mut usage: UsageDelta) -> Receipt {
        if self.reasoning_exclusive {
            usage.output_tokens = usage.output_tokens.saturating_add(usage.reasoning_tokens);
        }
        let receipt = Receipt::new(
            &self.session_id,
            &self.provider,
            &self.model,
            self.kind,
            usage,
            self.prices,
        );
        #[cfg(feature = "subagents")]
        let receipt = receipt.with_background_observation(self.background_observation.clone());
        receipt
    }
    fn observe_receipt(&self, receipt: &Receipt) {
        let accounted = self.account.observe(receipt);
        if accounted.is_err() {
            self.account.tracking_failed();
        }
    }
    /// File scans, serialization locks and fsync run only in a scoped blocking
    /// child. The account retains its charge and pending row before this await.
    pub(crate) async fn flush(&self) {
        if !self.account.has_pending() {
            return;
        }
        let Some(ledger) = self.ledger.clone() else {
            self.account.tracking_failed();
            return;
        };
        let account = self.account.clone();
        let result = crate::agent::runner::spawn_blocking_scoped(move || {
            let result = ledger.flush(&account);
            if result.is_err() {
                account.tracking_failed();
            }
            result
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            self.account.tracking_failed();
            tracing::warn!("cost tracking incomplete: observed usage remains in memory");
        }
    }
    pub(crate) async fn prepare(&self) -> anyhow::Result<()> {
        if !self.account.ledger_ready() {
            let ledger = self.ledger.clone();
            let account = self.account.clone();
            let session_id = self.session_id.clone();
            let cap = self.account.snapshot().cap;
            let result = crate::agent::runner::spawn_blocking_scoped(move || {
                let result = ledger
                    .ok_or_else(|| anyhow::anyhow!("usage ledger unavailable"))
                    .and_then(|ledger| ledger.summary_for_session(&session_id))
                    .and_then(|summary| account.reconcile(cap, &summary.session));
                if result.is_err() {
                    account.tracking_failed();
                }
                result
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                self.account.tracking_failed();
            }
        }
        self.flush().await;
        self.admit()
    }
    /// Frontend preparation can precede its normal actor installation. Retain
    /// an explicit I/O owner there too, without starting context/model work.
    #[cfg(feature = "git-worktree")]
    pub(crate) async fn prepare_owned(&self) -> anyhow::Result<()> {
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        let result = scope.run(self.prepare()).await;
        cleanup.settle().await;
        result
    }
    /// Accepted SDK reports can precede the SDK's public completion item. Keep
    /// their exact per-invocation total for cancellation settlement as well.
    pub(crate) fn accepted_usage(&self) -> UsageDelta {
        *self
            .accepted_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn begin_sdk_request(&self) -> anyhow::Result<Arc<SdkRequest>> {
        let mut active = self.sdk_request.lock().unwrap_or_else(|e| e.into_inner());
        // A profile is one sequential SDK invocation. Concurrent auxiliary
        // invocations own distinct profiles sharing only the financial account.
        // Refuse ambiguous same-profile overlap instead of correlating arrival order.
        if active.upgrade().is_some_and(|request| {
            !request
                .guard
                .finished
                .load(std::sync::atomic::Ordering::Acquire)
        }) {
            self.refused
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(BudgetStopped.into());
        }
        let request = Arc::new(SdkRequest {
            guard: StreamGuard::new(Some(self.clone())),
            run: self.clone(),
            receipt: Default::default(),
        });
        *active = Arc::downgrade(&request);
        Ok(request)
    }
    /// Capture before the HTTP await; the adapter never looks up a different
    /// request after its response arrives. Non-SDK auxiliary/listing requests
    /// have no registered request and keep their existing owned raw observers.
    pub(crate) fn sdk_stream_guard(&self) -> Option<Arc<StreamGuard>> {
        self.sdk_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
            .map(|request| request.guard.clone())
    }
    fn observe_projection(&self, usage: UsageDelta) {
        let request = self
            .sdk_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade();
        let remaining = request
            .as_ref()
            .map_or(Some(usage), |request| request.consume_projection(usage));
        if let Some(usage) = remaining {
            self.observe(usage);
        }
    }
    pub(crate) fn was_refused(&self) -> bool {
        self.refused.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn extensions(&self) -> ToolCallExtensions {
        let mut extensions = ToolCallExtensions::default();
        extensions.insert(self.clone());
        extensions
    }
}

pub(crate) fn scope_sync<T>(run: Option<Run>, operation: impl FnOnce() -> T) -> T {
    CURRENT.sync_scope(run, operation)
}

pub(crate) async fn scope<F: std::future::Future>(run: Option<Run>, future: F) -> F::Output {
    CURRENT.scope(run, future).await
}
pub(crate) async fn admit_current_effect() -> anyhow::Result<()> {
    match Run::current() {
        Some(run) => run.admit_effect_ready().await,
        None => Ok(()),
    }
}
pub(crate) fn observe_current(usage: UsageDelta) {
    if let Some(run) = Run::current() {
        run.observe_projection(usage);
    }
}
pub(crate) fn current_extensions() -> ToolCallExtensions {
    Run::current().map_or_else(ToolCallExtensions::default, |run| run.extensions())
}

#[derive(Clone)]
pub(crate) struct AdmissionHook(pub(crate) Option<Run>);
impl<M: CompletionModel> AgentHook<M> for AdmissionHook {
    async fn on_event(&self, ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        let Some(run) = &self.0 else {
            return Flow::Continue;
        };
        match event {
            // Rig may fail invalid/unknown tools before draining the actual
            // provider Final. No usage can be invented from an unread chunk.
            // Preserve its existing recovery/failure flow, but a capped account
            // cannot admit another request/effect with incomplete billing state.
            StepEvent::InvalidToolCall(_) => {
                run.account.tracking_failed();
                Flow::Continue
            }
            StepEvent::CompletionCall { .. } => {
                if run.prepare().await.is_err() {
                    return Flow::Terminate {
                        reason: BUDGET_STOP.into(),
                    };
                }
                match run.begin_sdk_request() {
                    Ok(request) => {
                        ctx.scratchpad().insert(request);
                        Flow::Continue
                    }
                    Err(_) => Flow::Terminate {
                        reason: BUDGET_STOP.into(),
                    },
                }
            }
            StepEvent::ModelTurnFinished { usage, .. } => {
                if let Some(request) = ctx.scratchpad().get::<Arc<SdkRequest>>() {
                    request.accept(usage.into());
                    run.flush().await;
                }
                Flow::Continue
            }
            // A skipped sibling retains truthful completed results and exact IDs.
            StepEvent::ToolCall { .. } => {
                if run.admit_effect_ready().await.is_err() {
                    Flow::Skip {
                        reason: BUDGET_STOP.into(),
                    }
                } else {
                    Flow::Continue
                }
            }
            _ => Flow::Continue,
        }
    }
}

/// The SDK scratchpad owns this exact request through semantic retirement;
/// HTTP body teardown never settles it. A canonical accepted turn is charged
/// before tools/output awaits, then later stream projections consume raw usage
/// credits from this request only. A replacement turn cannot borrow old credits.
struct SdkRequest {
    guard: Arc<StreamGuard>,
    run: Run,
    receipt: std::sync::Mutex<Option<(Receipt, UsageDelta)>>,
}
impl SdkRequest {
    fn accept(&self, usage: UsageDelta) {
        let mut receipt = self.receipt.lock().unwrap_or_else(|e| e.into_inner());
        if receipt.is_some() {
            return;
        }
        let observed = self.run.receipt(usage);
        self.run.observe_receipt(&observed);
        let mut total = self
            .run
            .accepted_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *total = total.saturating_add(usage);
        *receipt = Some((observed, usage));
        self.guard.finished();
    }
    fn consume_projection(&self, usage: UsageDelta) -> Option<UsageDelta> {
        let mut receipt = self.receipt.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, credit)) = receipt.as_mut() else {
            // A steering hook can suppress ModelTurnFinished while the SDK
            // still yields a canonical CompletionCall/terminal projection.
            // That owned observation settles the same request honestly.
            drop(receipt);
            self.accept(usage);
            return None;
        };
        let remaining = usage.nonnegative_difference(*credit);
        *credit = credit.nonnegative_difference(usage);
        remaining.has_values().then_some(remaining)
    }
}

/// Parent-held usage survives parser failures, cancellation and rolling
/// compaction. Each SDK request reconciles its own aggregate before recording.
#[derive(Clone, Default)]
pub(crate) struct UsageObserver {
    usage: Arc<std::sync::Mutex<UsageDelta>>,
    run: Option<Run>,
}
impl UsageObserver {
    pub(crate) fn cache_ttl(&self) -> crate::config::PromptCacheTtl {
        self.run
            .as_ref()
            .map(|run| run.cache_ttl)
            .unwrap_or_default()
    }
    pub(crate) fn with_run(run: Run) -> Self {
        Self {
            usage: Default::default(),
            run: Some(run),
        }
    }
    pub(crate) fn record(&self, usage: UsageDelta) {
        let mut observed = self.usage.lock().unwrap_or_else(|error| error.into_inner());
        *observed = observed.saturating_add(usage);
        drop(observed);
        if let Some(run) = &self.run {
            run.observe_projection(usage);
        }
    }
    #[cfg(any(feature = "goal", test))]
    pub(crate) fn total(&self) -> UsageDelta {
        *self.usage.lock().unwrap_or_else(|error| error.into_inner())
    }
    pub(crate) fn take(&self) -> UsageDelta {
        std::mem::take(&mut *self.usage.lock().unwrap_or_else(|error| error.into_inner()))
    }
    pub(crate) async fn admit(&self) -> anyhow::Result<()> {
        match &self.run {
            Some(run) => run.prepare().await,
            None => Ok(()),
        }
    }
    pub(crate) async fn flush(&self) {
        if let Some(run) = &self.run {
            run.flush().await;
        }
    }
    pub(crate) fn stream_guard(&self) -> Arc<StreamGuard> {
        StreamGuard::new(self.run.clone())
    }
    #[cfg(feature = "advisor")]
    pub(crate) fn hook(&self) -> AdmissionHook {
        AdmissionHook(self.run.clone())
    }
}

/// An admitted successful HTTP stream that is dropped or fails before its
/// canonical Final has incomplete billing state. It contributes no invented
/// usage, but future capped admissions must not assume that request was free.
pub(crate) struct StreamGuard {
    run: Option<Run>,
    opened: std::sync::atomic::AtomicBool,
    finished: std::sync::atomic::AtomicBool,
}
impl StreamGuard {
    pub(crate) fn new(run: Option<Run>) -> Arc<Self> {
        Arc::new(Self {
            run,
            opened: Default::default(),
            finished: Default::default(),
        })
    }
    pub(crate) fn opened(&self) {
        self.opened
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn finished(&self) {
        self.finished
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        if self.opened.load(std::sync::atomic::Ordering::Acquire)
            && !self.finished.load(std::sync::atomic::Ordering::Acquire)
            && let Some(run) = &self.run
        {
            run.account.tracking_failed();
        }
    }
}
