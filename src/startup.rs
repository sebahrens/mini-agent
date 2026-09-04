use compact_str::CompactString;

use crate::agent::tools;
use crate::cli::Cli;
use crate::config::{self, Config};
use crate::context::{self, ContextFiles};
use crate::extras::status_signals::StatusSignals;
use crate::paths::AppPaths;
use crate::permission::ask::{AskReceiver, AskSender};
use crate::permission::checker::PermCheck;
use crate::provider::{self, AnyClient};
use crate::sandbox::{DEFAULT_COMMAND_LIMITS, Sandbox, SandboxPolicy};
use crate::session::{self, MessageRole, Session};

#[cfg(feature = "advisor")]
use crate::session::SessionMessage;

// ── Helper functions ─────────────────────────────────────────────────────

/// Regenerate embedded prompts/themes, printing the actual outcome instead
/// of claiming success on failure. Returns true when regeneration succeeded.
fn regen_resource(regen: fn() -> anyhow::Result<()>, what: &str, suffix: &str) -> bool {
    match regen() {
        Ok(()) => {
            eprintln!("{} regenerated{}.", what, suffix);
            true
        }
        Err(e) => {
            eprintln!(
                "warning: failed to regenerate {}: {}",
                what.to_lowercase(),
                e
            );
            false
        }
    }
}

/// Validate the complete configured policy before any provider, model, tool,
/// transport, or UI construction. All execution modes share `Startup::init`,
/// including ACP, headless print, loop, and interactive startup.
fn validate_startup_permission_policy(cli: &Cli, cfg: &Config) -> anyhow::Result<()> {
    let authority = crate::permission::resolve_execution_authority(
        cli,
        cfg,
        crate::sandbox::SandboxPolicy::Disabled,
        "disabled",
    )?;
    crate::permission::build_noninteractive_permission(cfg, authority)?;
    Ok(())
}
fn unavailable_sandbox_must_fail(cli: &Cli, cfg: &Config, is_windows: bool) -> bool {
    cli.resolve_sandbox(cfg) && (is_windows || cli.sandbox_explicitly_requested(cfg))
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderIdentity {
    provider: CompactString,
    model: CompactString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeProviderDecision {
    Restore(ProviderIdentity),
    Override {
        saved: ProviderIdentity,
        target: ProviderIdentity,
    },
}

impl ResumeProviderDecision {
    fn target(&self) -> &ProviderIdentity {
        match self {
            Self::Restore(identity) => identity,
            Self::Override { target, .. } => target,
        }
    }

    fn override_identities(&self) -> Option<(&ProviderIdentity, &ProviderIdentity)> {
        match self {
            Self::Restore(_) => None,
            Self::Override { saved, target } => Some((saved, target)),
        }
    }
}

fn resolve_resume_provider_decision(
    cli: &Cli,
    cfg: &Config,
    session: &Session,
) -> anyhow::Result<ResumeProviderDecision> {
    let saved = ProviderIdentity {
        provider: session.provider.clone(),
        model: session.model.clone(),
    };
    let target_provider = cli
        .resume_provider
        .as_deref()
        .map(CompactString::new)
        .unwrap_or_else(|| saved.provider.clone());
    let target_model = if let Some(model) = cli.resume_model.as_deref() {
        CompactString::new(model)
    } else if target_provider != saved.provider {
        let (model, _) = provider::default_model_for_provider(&target_provider, cfg)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "resume provider/profile '{}' has no configured default model; pass --resume-model explicitly",
                    target_provider
                )
            })?;
        CompactString::new(model)
    } else {
        saved.model.clone()
    };
    let target = ProviderIdentity {
        provider: target_provider,
        model: target_model,
    };

    if target == saved {
        Ok(ResumeProviderDecision::Restore(saved))
    } else {
        Ok(ResumeProviderDecision::Override { saved, target })
    }
}

fn apply_resume_provider_decision(
    session: &mut Session,
    decision: &ResumeProviderDecision,
    cfg: &Config,
) {
    let Some((saved, target)) = decision.override_identities() else {
        return;
    };

    session.record_provider_override(
        &target.provider,
        &target.model,
        saved.provider != target.provider,
    );
    session.input_token_cost = 0.0;
    session.output_token_cost = 0.0;
    let qm = config::quick_models_map(cfg);
    if let Some(model_cfg) = qm
        .values()
        .find(|model_cfg| model_cfg.provider == target.provider && model_cfg.model == target.model)
    {
        session.input_token_cost = model_cfg.input_token_cost;
        session.output_token_cost = model_cfg.output_token_cost;
    } else if let Some((input_cost, output_cost)) =
        Config::catalog_input_output_cost(&target.provider, &target.model)
    {
        session.input_token_cost = input_cost;
        session.output_token_cost = output_cost;
    }
    session.update_context_window(cfg.resolve_context_window(&target.provider, &target.model, &qm));
}

#[cfg(any(test, all(target_os = "windows", feature = "js")))]
fn run_startup_probes_concurrently<General, Worker, WorkerStatus>(
    general: General,
    worker: Worker,
) -> anyhow::Result<(anyhow::Result<()>, WorkerStatus)>
where
    General: FnOnce() -> anyhow::Result<()> + Send,
    Worker: FnOnce() -> WorkerStatus + Send,
    WorkerStatus: Send,
{
    std::thread::scope(|scope| -> anyhow::Result<_> {
        let general = std::thread::Builder::new()
            .name("windows-general-preflight".into())
            .spawn_scoped(scope, general)
            .map_err(|_| anyhow::anyhow!("failed to start general containment preflight"))?;
        let worker = match std::thread::Builder::new()
            .name("windows-js-worker-preflight".into())
            .spawn_scoped(scope, worker)
        {
            Ok(worker) => worker,
            Err(_) => {
                let _ = general.join();
                anyhow::bail!("failed to start JavaScript-worker containment preflight");
            }
        };
        let general = general.join();
        let worker = worker.join();
        let general =
            general.map_err(|_| anyhow::anyhow!("general containment preflight worker failed"))?;
        let worker = worker
            .map_err(|_| anyhow::anyhow!("JavaScript-worker containment preflight failed"))?;
        Ok((general, worker))
    })
}

pub(crate) fn verify_resume_provider_safety() -> anyhow::Result<()> {
    let saved = Session::new("anthropic", "claude-saved", 200_000, "");
    let changed_defaults = Cli {
        provider: Some("openai".to_string()),
        model: Some("gpt-current-default".to_string()),
        ..Cli::default()
    };
    let cfg = Config::default();
    let restored = resolve_resume_provider_decision(&changed_defaults, &cfg, &saved)?;
    anyhow::ensure!(
        restored.target().provider == "anthropic" && restored.target().model == "claude-saved",
        "changed defaults did not restore the saved provider identity"
    );

    let explicit_override = Cli {
        resume_provider: Some("openai".to_string()),
        resume_model: Some("gpt-explicit".to_string()),
        ..Cli::default()
    };
    let overridden = resolve_resume_provider_decision(&explicit_override, &cfg, &saved)?;
    let mut audited = saved;
    apply_resume_provider_decision(&mut audited, &overridden, &cfg);
    anyhow::ensure!(
        audited.provider == "openai"
            && audited.model == "gpt-explicit"
            && audited.provider_override_audit.len() == 1
            && audited.provider_override_audit[0].context_disclosure_acknowledged,
        "explicit cross-provider resume was not applied and audited consistently"
    );
    Ok(())
}

/// Apply the `[prompt_to_model]` mapping at startup before the TUI is
/// available. Updates `provider`, `model`, and `session` fields so the
/// initial agent is built with the correct model.
fn apply_startup_prompt_model(
    prompt_name: &str,
    cfg: &Config,
    provider: &mut CompactString,
    model: &mut CompactString,
    session: &mut Session,
) {
    let qm_name = match cfg.resolve_prompt_model(prompt_name) {
        Some(name) => name,
        None => return,
    };
    let qm = config::quick_models_map(cfg);
    let Some(qmc) = qm.get(qm_name) else {
        return;
    };
    *provider = qmc.provider.clone();
    *model = qmc.model.clone();
    session.model = qmc.model.clone();
    session.provider = qmc.provider.clone();
    session.input_token_cost = qmc.input_token_cost;
    session.output_token_cost = qmc.output_token_cost;
    session.update_context_window(cfg.resolve_context_window(
        &session.provider,
        &session.model,
        &qm,
    ));
}

/// Connect configured MCP servers for a headless (`-p`/`--loop`) run. Unlike
/// the TUI (`ui::ensure_mcp_manager`), headless has no alt-screen to protect,
/// so connection failures are printed to stderr instead of staying silent
/// until surfaced by the renderer.
#[cfg(feature = "mcp")]
pub(crate) async fn connect_headless_mcp(
    cfg: &Config,
    workspace: &std::sync::Arc<crate::paths::WorkspaceBinding>,
) -> Option<crate::extras::mcp::McpClientManager> {
    let servers = cfg.mcp_servers.as_ref()?;
    if servers.is_empty() {
        return None;
    }
    let manager =
        crate::extras::mcp::McpClientManager::connect_all_in_binding(servers, workspace).await;
    for notice in &manager.notices {
        eprintln!("{}", notice);
    }
    Some(manager)
}

// ── Startup state ────────────────────────────────────────────────────────

pub(crate) struct Startup {
    pub cli: Cli,
    pub cfg: Config,
    // Startup-owned source of truth shared by persistent artifact owners.
    #[allow(dead_code)]
    pub paths: AppPaths,
    pub workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    pub is_first_startup: bool,
    pub context: ContextFiles,
    pub provider: CompactString,
    pub model: CompactString,
    pub session: Session,
    pub client: AnyClient,
    pub is_interactive: bool,
    pub version_changed: bool,
    // Set by init_features:
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub ask_rx: Option<AskReceiver>,
    pub sandbox: Sandbox,
    shell_search_path: Option<std::ffi::OsString>,
    pub status_signals: Option<StatusSignals>,
    openrouter_pricing_refresh: Option<OpenRouterPricingRefresh>,
    #[cfg(feature = "advisor")]
    pub handoff_rx: Option<crate::extras::advisor::HandoffReceiver>,
    // Set by resolve_prompts:
    pub arch_msg: Option<String>,
    pub session_resumed: bool,
    pub resume_override_pending: bool,
}

type OpenRouterPricingMap = std::collections::HashMap<String, provider::OpenRouterModelInfo>;
const OPENROUTER_PRICING_ABORT_JOIN_GRACE: std::time::Duration =
    std::time::Duration::from_millis(100);
static ACTIVE_OPENROUTER_PRICING_REAPERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadlessCompactionPlan {
    cut_idx: usize,
    tokens_before: u64,
    input_token_budget: u64,
    response_token_budget: u64,
}

// `pending_tokens`: estimated tokens in the pending request payload (user
// prompt + media) to include in the compaction decision.
fn headless_compaction_plan(
    session: &Session,
    cfg: &Config,
    pending_tokens: u64,
    #[cfg(feature = "memory")] memory: Option<&str>,
) -> Option<HeadlessCompactionPlan> {
    if !cfg.resolve_compact_enabled() {
        return None;
    }
    let quick_models = config::quick_models_map(cfg);
    let configured_reserve =
        cfg.resolve_reserve_tokens(&session.model, &quick_models, session.context_window);
    #[cfg(feature = "memory")]
    let reserve = crate::extras::memory::effective_reserve(configured_reserve, memory);
    #[cfg(not(feature = "memory"))]
    let reserve = configured_reserve;
    if !session.needs_compaction_with_pending(reserve, pending_tokens) {
        return None;
    }

    let keep_recent = cfg.resolve_keep_recent_tokens(session.context_window);
    let cut_idx = Session::select_compaction_cut(&session.messages, keep_recent);
    if cut_idx == 0 {
        return None;
    }
    let tokens_before = session.messages[..cut_idx]
        .iter()
        .map(|message| message.estimated_tokens)
        .sum();
    Some(HeadlessCompactionPlan {
        cut_idx,
        tokens_before,
        input_token_budget: session.context_window.saturating_sub(reserve),
        response_token_budget: reserve,
    })
}

async fn compact_headless_session_if_needed(
    session: &mut Session,
    client: &AnyClient,
    cfg: &Config,
    context: &ContextFiles,
    pending_tokens: u64,
) -> anyhow::Result<bool> {
    #[cfg(not(feature = "memory"))]
    let _ = context;
    let compacted = compact_headless_session_with(
        session,
        cfg,
        pending_tokens,
        #[cfg(feature = "memory")]
        context.memory.as_deref(),
        |model, messages, previous_summary, input_token_budget, response_token_budget| async move {
            client
                .compress_messages(
                    &model,
                    &messages,
                    previous_summary.as_deref(),
                    None,
                    input_token_budget,
                    response_token_budget,
                )
                .await
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("headless auto-compaction failed: {error}"))?;

    #[cfg(feature = "memory")]
    if let Some((summary, cut_idx)) = &compacted {
        crate::extras::memory::flush_compaction_summary(
            &crate::extras::memory::Mem::open(),
            summary,
            Some(*cut_idx),
        );
    }
    Ok(compacted.is_some())
}

async fn compact_headless_session_with<S, F>(
    session: &mut Session,
    cfg: &Config,
    pending_tokens: u64,
    #[cfg(feature = "memory")] memory: Option<&str>,
    summarize: S,
) -> anyhow::Result<Option<(String, usize)>>
where
    S: FnOnce(String, Vec<crate::session::SessionMessage>, Option<String>, u64, u64) -> F,
    F: std::future::Future<Output = anyhow::Result<(String, usize)>>,
{
    let Some(plan) = headless_compaction_plan(
        session,
        cfg,
        pending_tokens,
        #[cfg(feature = "memory")]
        memory,
    ) else {
        return Ok(None);
    };

    eprintln!("auto-compacting headless session...");
    let model = session.model.to_string();
    let messages = session.messages[..plan.cut_idx].to_vec();
    let previous_summary = session
        .compactions
        .last()
        .map(|compaction| compaction.summary.to_string());
    let (summary, messages_included) = summarize(
        model,
        messages,
        previous_summary,
        plan.input_token_budget,
        plan.response_token_budget,
    )
    .await?;
    // `messages_included` is the length of the oldest prefix of the cut slice
    // whose content the summarizer saw (`cut_idx` with full coverage). Drain
    // exactly that prefix so unsummarized history is never discarded.
    let first_kept_index = Session::compaction_drain_len(plan.cut_idx, messages_included)?;
    let tokens_before = if first_kept_index == plan.cut_idx {
        plan.tokens_before
    } else {
        session.messages[..first_kept_index]
            .iter()
            .map(|message| message.estimated_tokens)
            .sum()
    };
    session.compress(summary.clone(), first_kept_index, tokens_before);
    Ok(Some((summary, first_kept_index)))
}

fn reap_aborted_openrouter_pricing_refresh(
    handle: tokio::task::JoinHandle<anyhow::Result<OpenRouterPricingMap>>,
) {
    ACTIVE_OPENROUTER_PRICING_REAPERS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    tokio::spawn(async move {
        struct ReaperPermit;
        impl Drop for ReaperPermit {
            fn drop(&mut self) {
                ACTIVE_OPENROUTER_PRICING_REAPERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        let _permit = ReaperPermit;
        let _ = handle.await;
    });
}

struct OpenRouterPricingRefresh {
    handle: Option<tokio::task::JoinHandle<anyhow::Result<OpenRouterPricingMap>>>,
    model: CompactString,
    need_pricing: bool,
    need_context: bool,
    initial_context_window: u64,
}

impl OpenRouterPricingRefresh {
    fn start<Refresh>(
        model: CompactString,
        need_pricing: bool,
        need_context: bool,
        initial_context_window: u64,
        refresh: Refresh,
    ) -> Self
    where
        Refresh:
            std::future::Future<Output = anyhow::Result<OpenRouterPricingMap>> + Send + 'static,
    {
        Self {
            handle: Some(tokio::spawn(refresh)),
            model,
            need_pricing,
            need_context,
            initial_context_window,
        }
    }

    async fn finish_without_wait(&mut self) -> Option<anyhow::Result<OpenRouterPricingMap>> {
        let mut handle = self.handle.take()?;
        if !handle.is_finished() {
            handle.abort();
            if tokio::time::timeout(OPENROUTER_PRICING_ABORT_JOIN_GRACE, &mut handle)
                .await
                .is_err()
            {
                reap_aborted_openrouter_pricing_refresh(handle);
            }
            return None;
        }
        handle.await.ok()
    }

    #[cfg(test)]
    fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }
}

impl Drop for OpenRouterPricingRefresh {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn apply_openrouter_pricing_refresh_result(
    session: &mut Session,
    refresh: &OpenRouterPricingRefresh,
    result: Option<anyhow::Result<OpenRouterPricingMap>>,
) {
    let Some(Ok(infos)) = result else {
        return;
    };
    if session.provider != "openrouter" || session.model != refresh.model {
        return;
    }
    let Some(info) = infos.get(session.model.as_str()) else {
        return;
    };
    if refresh.need_pricing && session.input_token_cost == 0.0 && session.output_token_cost == 0.0 {
        session.input_token_cost = info.input_cost;
        session.output_token_cost = info.output_cost;
    }
    if refresh.need_context
        && session.context_window == refresh.initial_context_window
        && let Some(context_window) = info.context_length
    {
        session.update_context_window(context_window);
    }
}

fn needs_openrouter_context_refresh(cfg: &Config, model: &str) -> bool {
    cfg.context_window.is_none()
        && cfg.quick_models.as_ref().is_none_or(|quick_models| {
            quick_models
                .values()
                .all(|quick| quick.model.as_str() != model || quick.context_window.is_none())
        })
        && Config::catalog_context_window("openrouter", model).is_none()
}

impl Startup {
    /// Phase 1: context load, provider/model resolution, session
    /// creation/resolution, client creation.
    pub(crate) async fn init(
        cli: Cli,
        cfg: Config,
        paths: AppPaths,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
        is_first_startup: bool,
        version_changed: bool,
        is_interactive: bool,
    ) -> anyhow::Result<Self> {
        validate_startup_permission_policy(&cli, &cfg)?;

        // Load context first so prompts/themes are available early.
        let no_context_files = cli.resolve_no_context_files(&cfg);
        let context =
            context::load(no_context_files).for_workspace_binding(no_context_files, &workspace);

        let mut provider = cli.resolve_provider(&cfg);
        let mut model = cli.resolve_model(&cfg);

        // --quick-model overrides provider + model
        if let Some(qm) = cli.resolve_quick_model(&cfg) {
            provider = qm.provider.clone();
            model = qm.model.clone();
        }

        let name = cli.name.as_deref().unwrap_or("");
        let qm_map = config::quick_models_map(&cfg);
        let mut session = Session::new(
            &provider,
            &model,
            cfg.resolve_context_window(&provider, &model, &qm_map),
            name,
        );

        // Resolve input/output token costs from quick models or defaults
        if let Some(qm) = cli.resolve_quick_model(&cfg) {
            session.input_token_cost = qm.input_token_cost;
            session.output_token_cost = qm.output_token_cost;
        } else if let Some(qm) = qm_map
            .iter()
            .find(|(_, v)| v.model.as_str() == model && v.provider.as_str() == provider)
            .map(|(_, v)| v)
        {
            session.input_token_cost = qm.input_token_cost;
            session.output_token_cost = qm.output_token_cost;
        } else if let Some((input_cost, output_cost)) =
            Config::catalog_input_output_cost(&provider, &model)
        {
            session.input_token_cost = input_cost;
            session.output_token_cost = output_cost;
        }

        let mut session_resumed = false;

        if cli.continue_session
            && cli.session.is_none()
            && let Ok(sessions) = session::storage::find_recent_sessions(1)
            && let Some(s) = sessions.into_iter().next()
        {
            session = s;
            session_resumed = true;
        }

        if let Some(session_id) = &cli.session {
            let sessions = session::storage::find_sessions_by_prefix(session_id)?;
            if sessions.is_empty() {
                // try exact name match as fallback
                if let Some(s) = session::storage::find_session_by_name(session_id)? {
                    session = s;
                    session_resumed = true;
                } else {
                    anyhow::bail!("no session matching '{}'", session_id);
                }
            } else if sessions.len() == 1 {
                session = sessions.into_iter().next().unwrap();
                session_resumed = true;
            } else {
                eprintln!("multiple sessions match '{}':", session_id);
                for s in &sessions {
                    let preview = s
                        .messages
                        .last()
                        .map(|m| {
                            let truncated: String = m.content.chars().take(40).collect();
                            truncated
                        })
                        .unwrap_or_default();
                    let time = crate::ui::events::format_time(&s.updated_at);
                    let name_part = if s.name.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", s.name)
                    };
                    eprintln!(
                        "  {}  {}  {}msgs  {}  {}{}",
                        crate::print::short_session_id(&s.id),
                        time,
                        s.messages.len(),
                        s.model,
                        preview,
                        name_part
                    );
                }
                anyhow::bail!("be more specific with the session ID prefix");
            }
        }

        let resume_decision = if session_resumed {
            Some(resolve_resume_provider_decision(&cli, &cfg, &session)?)
        } else {
            if cli.resume_provider.is_some() || cli.resume_model.is_some() {
                anyhow::bail!(
                    "--resume-provider/--resume-model require --continue or --session with an existing saved session"
                );
            }
            None
        };

        if let Some(decision) = &resume_decision {
            let target = decision.target();
            provider = target.provider.clone();
            model = target.model.clone();
            if let Some((saved, target)) = decision.override_identities() {
                if cli.no_session {
                    anyhow::bail!(
                        "an explicit resume provider/model override cannot be combined with --no-session because its audit record must be persisted"
                    );
                }
                if saved.provider != target.provider {
                    eprintln!(
                        "PRIVACY WARNING: resuming with provider/profile '{}' will send saved conversation and source context previously associated with '{}' to that provider; this explicit override will be recorded in session metadata.",
                        target.provider, saved.provider
                    );
                }
                apply_resume_provider_decision(&mut session, decision, &cfg);
            }
        }

        // A resumed session persisted its context_window when first saved, which can
        // be stale if the model's catalog entry has changed since (e.g. a model that
        // grew from 128k to 1M). Re-derive it from the catalog for the session's own
        // model, unless the user pinned `context_window` in config (then that wins).
        if cfg.context_window.is_none()
            && let Some(cw) =
                Config::catalog_context_window(session.provider.as_str(), session.model.as_str())
        {
            session.update_context_window(cw);
        }

        // The invocation's captured workspace is authoritative until an
        // explicit, validated rebind replaces it. Saved pathname strings must
        // not split tools across a different root during startup.
        session.working_dir = CompactString::new(workspace.root().to_string_lossy());

        let client = provider::create_client(
            &provider,
            cli.api_key.as_deref(),
            &cfg.custom_providers_map(),
            cfg.api_keys.as_ref(),
        )?;

        let resume_override_pending = resume_decision
            .as_ref()
            .is_some_and(|decision| decision.override_identities().is_some());

        // Rebuilds of this logical session share one process-local read
        // history, while a new/resumed process session starts from the active
        // configuration rather than any serialized state.
        session.initialize_read_tracker(cfg.deny_repeated_reads.unwrap_or(true));

        Ok(Self {
            cli,
            cfg,
            paths,
            workspace,
            is_first_startup,
            context,
            provider,
            model,
            session,
            client,
            is_interactive,
            version_changed,
            permission: None,
            ask_tx: None,
            ask_rx: None,
            sandbox: Sandbox::new(false, "bwrap"),
            shell_search_path: std::env::var_os("PATH"),
            status_signals: None,
            openrouter_pricing_refresh: None,
            #[cfg(feature = "advisor")]
            handoff_rx: None,
            arch_msg: None,
            session_resumed,
            resume_override_pending,
        })
    }

    /// Validate the common process sandbox contract before entering any
    /// execution surface, including ACP which intentionally skips feature
    /// initialization. Windows is always fail-closed while sandboxing is on;
    /// other platforms retain the legacy warning only for an entirely
    /// inherited default.
    pub(crate) fn validate_sandbox_availability(&self) -> anyhow::Result<()> {
        if !self.cli.general_sandbox_is_eligible(&self.cfg) {
            return Ok(());
        }
        let backend = self.cli.resolve_sandbox_backend(&self.cfg);
        let sandbox = Sandbox::new(self.cli.resolve_sandbox(&self.cfg), &backend)
            .with_windows_appcontainer_roots(
                self.cli.resolve_windows_appcontainer_read_roots(&self.cfg),
                self.cli.resolve_windows_appcontainer_write_roots(&self.cfg),
            );
        if sandbox.policy() == SandboxPolicy::RequiredButUnavailable
            && unavailable_sandbox_must_fail(&self.cli, &self.cfg, cfg!(target_os = "windows"))
        {
            anyhow::bail!(
                "sandbox backend '{backend}' is unavailable or has no successful production preflight — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly)"
            );
        }
        Ok(())
    }

    /// Resolve the startup containment gates after the workspace has been captured. On Windows,
    /// the regular AppContainer and LPAC worker preflights are independent. Starting both before
    /// joining avoids adding their cold-start latencies while their separate process-local caches,
    /// process-creation lock, and fail-closed consumers retain their existing contracts.
    pub(crate) fn preflight_startup_capabilities(&self) -> anyhow::Result<()> {
        #[cfg(all(target_os = "windows", feature = "js"))]
        if self.cli.tool_is_eligible(&self.cfg, "js") {
            let (general, _worker_status) = run_startup_probes_concurrently(
                || self.validate_sandbox_availability(),
                crate::sandbox::worker::containment_status,
            )?;
            // Worker unavailability is consumed later as an unavailable JS tool; the cached status
            // is never upgraded or bypassed. General sandbox failure retains its startup error.
            return general;
        }

        self.validate_sandbox_availability()
    }

    /// Phase 2: subagents, sandbox, tools config,
    /// permission checker, advisor.
    pub(crate) async fn init_features(&mut self) -> anyhow::Result<()> {
        #[cfg(feature = "subagents")]
        {
            let task_max_turns = self.cfg.task_max_turns.unwrap_or(20);
            let qm = config::quick_models_map(&self.cfg);

            // Resolve subagent model: subagent_model config > subagent_provider + model > main model
            let (sub_provider, mut sub_model) = if let Some(sa_model) = &self.cfg.subagent_model {
                if let Some(q) = qm.get(sa_model.as_str()) {
                    (q.provider.clone(), q.model.clone())
                } else {
                    let prov = self
                        .cfg
                        .subagent_provider
                        .clone()
                        .unwrap_or_else(|| self.provider.clone());
                    (prov, sa_model.clone())
                }
            } else if let Some(sa_prov) = &self.cfg.subagent_provider {
                (sa_prov.clone(), self.model.clone())
            } else {
                (self.provider.clone(), self.model.clone())
            };

            let sub_client = if sub_provider.as_str() == self.provider.as_str() {
                self.client.clone()
            } else {
                match crate::provider::create_client(
                    &sub_provider,
                    self.cli.api_key.as_deref(),
                    &self.cfg.custom_providers_map(),
                    self.cfg.api_keys.as_ref(),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            "Could not initialize subagent provider '{}' ({}); \
                             falling back to main provider '{}'. \
                             Set `subagent_provider`/`subagent_model` in config, or the \
                             provider's API key, to silence this.",
                            sub_provider,
                            e,
                            self.provider
                        );
                        sub_model = self.model.clone();
                        self.client.clone()
                    }
                }
            };

            crate::extras::subagents::init(
                sub_client,
                sub_model.to_string(),
                task_max_turns,
                self.cfg.clone(),
            );
        }

        // Sandbox, tools config, status signals, permission checker
        let (authority, sandbox) =
            crate::permission::resolve_configured_execution_authority(&self.cli, &self.cfg)?;
        self.sandbox = crate::permission::bind_configured_shell(
            &self.cli,
            &self.cfg,
            authority,
            &self.workspace,
            self.shell_search_path.as_deref(),
            sandbox,
        )
        .with_workspace_binding(self.workspace.clone());
        let edit_system = self.cli.resolve_edit_system(&self.cfg);
        tools::set_edit_system(edit_system);

        #[cfg(feature = "status-signals")]
        {
            self.status_signals = self.cli.status_socket.clone().map(StatusSignals::new);
        }

        let (permission, ask_tx, ask_rx) = crate::permission::build_interactive_permission_at(
            &self.cfg,
            authority,
            Some(self.workspace.root().to_path_buf()),
        )?;
        self.permission = permission;
        self.ask_tx = ask_tx;
        self.ask_rx = ask_rx;

        // Advisor setup
        #[cfg(feature = "advisor")]
        {
            let enabled = self.cli.resolve_advisor_enabled(&self.cfg);
            let human_handoff = self.cli.resolve_advisor_human_handoff(&self.cfg);
            let advisor_model_name = self.cli.resolve_advisor_model(&self.cfg);
            let max_uses = self.cli.resolve_advisor_max_uses(&self.cfg);
            let kilobytes_limit = self.cli.resolve_advisor_kilobytes_limit(&self.cfg);

            let qm = config::quick_models_map(&self.cfg);
            let (advisor_provider, advisor_model) =
                if let Some(q) = qm.get(advisor_model_name.as_str()) {
                    (q.provider.to_string(), q.model.to_string())
                } else {
                    (self.provider.to_string(), advisor_model_name)
                };

            let advisor_client = if advisor_provider == self.provider.as_str() {
                Some(self.client.clone())
            } else {
                match crate::provider::create_client(
                    &advisor_provider,
                    self.cli.api_key.as_deref(),
                    &self.cfg.custom_providers_map(),
                    self.cfg.api_keys.as_ref(),
                ) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(
                            "Could not create advisor client for provider '{}' ({}); \
                             advisor disabled. Set `advisor.model` and API key in config.",
                            advisor_provider,
                            e
                        );
                        None
                    }
                }
            };

            let (handoff_tx, handoff_rx) = if human_handoff && self.is_interactive {
                let (tx, rx) = tokio::sync::mpsc::channel(8);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };

            let config = crate::extras::advisor::AdvisorToolConfig {
                client: advisor_client,
                advisor_model,
                human_handoff,
                max_uses,
                handoff_tx,
                enabled,
                kilobytes_limit,
            };
            crate::extras::advisor::init_config(config);

            self.handoff_rx = handoff_rx;
        }

        Ok(())
    }

    pub(crate) fn start_openrouter_pricing_refresh(&mut self) {
        if self.provider != "openrouter" {
            return;
        }
        let need_pricing =
            self.session.input_token_cost == 0.0 && self.session.output_token_cost == 0.0;
        let need_context = needs_openrouter_context_refresh(&self.cfg, self.model.as_str());
        if !need_pricing && !need_context {
            return;
        }

        let api_key = self.cli.api_key.clone();
        let custom_providers = self.cfg.custom_providers_map();
        let config_api_keys = self.cfg.api_keys.clone();
        self.openrouter_pricing_refresh = Some(OpenRouterPricingRefresh::start(
            self.model.clone(),
            need_pricing,
            need_context,
            self.session.context_window,
            async move {
                provider::fetch_openrouter_pricing(
                    api_key.as_deref(),
                    &custom_providers,
                    config_api_keys.as_ref(),
                )
                .await
            },
        ));
    }

    pub(crate) async fn finish_openrouter_pricing_refresh(&mut self) {
        let Some(mut refresh) = self.openrouter_pricing_refresh.take() else {
            return;
        };
        let result = refresh.finish_without_wait().await;
        apply_openrouter_pricing_refresh_result(&mut self.session, &refresh, result);
    }

    /// Phase 3: version-change prompts, MCP recommendations, ARCHITECTURE.md,
    /// default prompt resolution, --load-prompt override, permission mode from
    /// prompt directive.
    pub(crate) async fn resolve_prompts(&mut self) -> anyhow::Result<()> {
        // Version-change prompts
        if self.version_changed && self.is_interactive && !self.is_first_startup {
            let prompts_dir = context::prompts::global_dir();
            let themes_dir = context::themes::global_dir();
            let mut regenerated = false;

            match self.cfg.resolve_auto_update_prompts() {
                Some(true) => {
                    regenerated |= regen_resource(context::prompts::regen, "Prompts", "");
                }
                Some(false) => { /* skip: user explicitly denied */ }
                None => {
                    if !prompts_dir.exists() {
                        regenerated |=
                            regen_resource(context::prompts::regen, "Prompts", " (first launch)");
                    } else {
                        let mut input = String::new();
                        eprint!("Regenerate prompts? [y/N] ");
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                        std::io::stdin().read_line(&mut input)?;
                        if matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
                            regenerated |= regen_resource(context::prompts::regen, "Prompts", "");
                        }
                    }
                }
            }

            match self.cfg.resolve_auto_update_themes() {
                Some(true) => {
                    regenerated |= regen_resource(context::themes::regen, "Themes", "");
                }
                Some(false) => { /* skip: user explicitly denied */ }
                None => {
                    if !themes_dir.exists() {
                        regenerated |=
                            regen_resource(context::themes::regen, "Themes", " (first launch)");
                    } else {
                        let mut input = String::new();
                        eprint!("Regenerate themes? [y/N] ");
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                        std::io::stdin().read_line(&mut input)?;
                        if matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
                            regenerated |= regen_resource(context::themes::regen, "Themes", "");
                        }
                    }
                }
            }

            if regenerated {
                self.context = context::load(self.cli.resolve_no_context_files(&self.cfg))
                    .for_workspace_binding(
                        self.cli.resolve_no_context_files(&self.cfg),
                        &self.workspace,
                    );
            }
        }

        // Recommended MCP prompts on first startup
        #[cfg(feature = "mcp")]
        if self.is_first_startup && self.is_interactive {
            let prompted =
                self.cfg.enable_context7_mcp.is_none() || self.cfg.enable_grepapp_mcp.is_none();
            if prompted {
                let config_before_prompts = self.cfg.clone();
                if self.cfg.enable_context7_mcp.is_none() {
                    let mut input = String::new();
                    eprint!("Enable Context7 MCP (documentation and code context lookup)? [y/N] ");
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    std::io::stdin().read_line(&mut input)?;
                    let enable = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                    self.cfg.enable_context7_mcp = Some(enable);
                    if enable {
                        eprintln!("Context7 MCP enabled.");
                    }
                }
                if self.cfg.enable_grepapp_mcp.is_none() {
                    let mut input = String::new();
                    eprint!(
                        "Enable Grep.app MCP (semantic code search across repositories)? [y/N] "
                    );
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    std::io::stdin().read_line(&mut input)?;
                    let enable = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                    self.cfg.enable_grepapp_mcp = Some(enable);
                    if enable {
                        eprintln!("Grep.app MCP enabled.");
                    }
                }
                config::inject_mcp_defaults(&mut self.cfg);
                if let Err(e) = config::save_config_changes(&config_before_prompts, &self.cfg) {
                    tracing::warn!("Failed to save config with MCP choices: {e}");
                }
            }
        }

        // `SessionStart` fires here once `session_resumed` is known.
        #[cfg(feature = "hooks")]
        {
            let source = if self.session_resumed {
                "resume"
            } else {
                "startup"
            };
            crate::extras::hooks::dispatch_session_start(source).await;
        }

        // ARCHITECTURE.md prompt
        #[cfg(feature = "archmd")]
        let arch_created = if !self.cli.resolve_no_context_files(&self.cfg) {
            let workspace = self.workspace.root();
            if workspace.exists() {
                crate::extras::archmd::ask_and_create(workspace).unwrap_or_else(|e| {
                    tracing::warn!("Architecture.md prompt failed: {e}");
                    false
                })
            } else {
                false
            }
        } else {
            false
        };

        // Reload context after potential ARCHITECTURE.md creation
        #[cfg(feature = "archmd")]
        if arch_created {
            self.context.architecture =
                crate::context::load_architecture_from(self.workspace.root());
        }

        // Default prompt resolution (after prompts may have been regenerated)
        {
            let default_prompt = self.cfg.default_prompt.as_deref().unwrap_or("code");
            if let Some(content) = self.context.prompts.get(default_prompt) {
                let (mode_directive, clean_content) = crate::permission::parse_prompt_mode(content);
                let mut prompt_text = if mode_directive.is_some() {
                    clean_content.to_string()
                } else {
                    content.clone()
                };

                let caps: &[&str] = &[
                    #[cfg(feature = "memory")]
                    "- **Memory**: persistent memory across sessions (memory_read, memory_write, memory_search)",
                    #[cfg(feature = "subagents")]
                    "- **Subagents**: delegate specific multi-step investigations to parallel subagents via the `task` tool",
                ];

                if !caps.is_empty() {
                    prompt_text.push_str("\n\n## Available Capabilities\n\n");
                    prompt_text.push_str(&caps.join("\n"));
                    prompt_text.push('\n');
                }

                self.context.current_prompt = Some(prompt_text);
                self.context.current_prompt_name = Some(default_prompt.to_string());

                if !self.session_resumed {
                    apply_startup_prompt_model(
                        default_prompt,
                        &self.cfg,
                        &mut self.provider,
                        &mut self.model,
                        &mut self.session,
                    );
                }
            }
        }

        // --load-prompt overrides the default prompt
        if let Some(ref name) = self.cli.load_prompt {
            if let Some(content) = self.context.prompts.get(name) {
                let (mode_directive, clean_content) = crate::permission::parse_prompt_mode(content);
                let mut prompt_text = if mode_directive.is_some() {
                    clean_content.to_string()
                } else {
                    content.clone()
                };

                let caps: &[&str] = &[
                    #[cfg(feature = "memory")]
                    "- **Memory**: persistent memory across sessions (memory_read, memory_write, memory_search)",
                    #[cfg(feature = "subagents")]
                    "- **Subagents**: delegate specific multi-step investigations to parallel subagents via the `task` tool",
                ];

                if !caps.is_empty() {
                    prompt_text.push_str("\n\n## Available Capabilities\n\n");
                    prompt_text.push_str(&caps.join("\n"));
                    prompt_text.push('\n');
                }

                self.context.current_prompt = Some(prompt_text);
                self.context.current_prompt_name = Some(name.clone());

                if !self.session_resumed {
                    apply_startup_prompt_model(
                        name,
                        &self.cfg,
                        &mut self.provider,
                        &mut self.model,
                        &mut self.session,
                    );
                }
            } else {
                let mut sorted: Vec<&String> = self.context.prompts.keys().collect();
                sorted.sort();
                eprintln!("error: unknown prompt '{}'", name);
                eprintln!("available prompts:");
                for p in &sorted {
                    eprintln!("  {}", p);
                }
                anyhow::bail!("unknown prompt '{}'", name);
            }
        }

        // Rebuild client if the provider changed due to prompt-to-model mapping
        if self.client.provider_name() != self.provider.as_str() {
            match provider::create_client(
                &self.provider,
                self.cli.api_key.as_deref(),
                &self.cfg.custom_providers_map(),
                self.cfg.api_keys.as_ref(),
            ) {
                Ok(new_client) => {
                    self.client = new_client;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to rebuild client for prompt-mapped provider '{}': {}",
                        self.provider,
                        e
                    );
                }
            }
        }

        // Apply mode from prompt %%mode= directive (if any).
        if let Some(perm) = &self.permission {
            let allowlist: Vec<(String, String)> = self
                .session
                .permission_allowlist
                .iter()
                .map(|e| (e.tool.to_string(), e.pattern.to_string()))
                .collect();
            let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
            guard.load_session_allowlist(&allowlist);
            // Project-sourced prompts only carry a directive when the project
            // config is trusted (context::prompts), and the checker refuses
            // any directive that would raise the mode above the user's
            // CLI/config selection, so this can only narrow authority.
            if let Some(name) = &self.context.current_prompt_name
                && let Some(mode) =
                    crate::permission::resolve_startup_prompt_mode(&self.context.prompts, name)
                && !guard.set_prompt_mode(mode)
            {
                tracing::info!(
                    "startup prompt '{name}' requested mode {mode}; keeping user-selected {}",
                    guard.mode()
                );
            }
        }

        // Build the auto-trigger message for ARCHITECTURE.md creation
        #[cfg(feature = "archmd")]
        {
            self.arch_msg = if arch_created {
                Some(
                    "I've just created an empty ARCHITECTURE.md template at the project root. \
                    Explore the codebase thoroughly using the `task` tool (delegating parallel exploration to subagents) \
                    and fill ARCHITECTURE.md with a high-level architecture document covering:\n\
                    - Directory layout and module responsibilities\n\
                    - Key types, traits, and their relationships\n\
                    - Control flow (how requests/events flow through the system)\n\
                    - Data flow (how data is transformed from input to output)\n\
                    - Design decisions and rationale\n\
                    - External dependencies and how they are used\n\
                    - Entry points for different execution modes\n\n\
                    Keep the document under ~300 lines of code total. Keep entries concise and reference specific source files."
                        .to_string(),
                )
            } else {
                None
            };
        }

        Ok(())
    }

    /// Phase 4: mode dispatch — print, loop, or interactive.
    pub(crate) async fn dispatch(mut self) -> anyhow::Result<()> {
        #[cfg(feature = "hooks")]
        crate::extras::hooks::set_active_workspace(self.workspace.root());
        if self.resume_override_pending {
            // All fallible startup validation has completed. Persist the
            // identity/audit update atomically before any agent can receive
            // saved history.
            session::storage::save_session(&self.session)?;
            self.resume_override_pending = false;
        }
        if self.cli.print {
            self.dispatch_print().await
        } else {
            #[cfg(feature = "loop")]
            if self.cli.loop_mode {
                return self.dispatch_loop().await;
            }

            self.dispatch_interactive().await
        }
    }

    async fn dispatch_print(mut self) -> anyhow::Result<()> {
        let msg = self.cli.message.join(" ");
        if msg.starts_with('!') {
            if msg
                .strip_prefix('!')
                .is_some_and(|command| !command.trim().is_empty())
            {
                let run = self
                    .sandbox
                    .clone()
                    .run_explicit_shell(&msg, DEFAULT_COMMAND_LIMITS, None)
                    .await?;
                let result = run.rendered_output();
                println!("{}", result);
                if !self.cli.no_session {
                    let mut session = self.session;
                    session.add_message(MessageRole::User, &msg);
                    session.add_message(MessageRole::Assistant, &result);
                    session::storage::save_session(&session)?;
                    if let Err(e) = session::chat_history::append_entry(
                        &session::chat_history::ChatHistoryEntry {
                            content: msg,
                            timestamp: session.updated_at.clone(),
                        },
                    ) {
                        eprintln!("warning: failed to append chat history entry: {}", e);
                    }
                }
            } else {
                eprintln!("error: empty command after '!'");
            }
        } else {
            let pending_tokens = Session::estimate_tokens(&msg);
            // Preflight: reject locally when the payload cannot fit even after
            // a full compaction. This avoids a provider call that would fail or
            // silently truncate context.
            let quick_models = config::quick_models_map(&self.cfg);
            let reserve = self.cfg.resolve_reserve_tokens(
                &self.session.model,
                &quick_models,
                self.session.context_window,
            );
            if self
                .session
                .is_irreducible_with_pending(reserve, pending_tokens)
            {
                anyhow::bail!(
                    "message is too large to fit in the context window \
                     (estimated {pending_tokens} tokens; overhead {oh} tokens; \
                     window {cw} tokens; reserve {reserve} tokens): \
                     reduce the message size or clear the session with /clear",
                    oh = self.session.overhead_tokens,
                    cw = self.session.context_window,
                );
            }
            compact_headless_session_if_needed(
                &mut self.session,
                &self.client,
                &self.cfg,
                &self.context,
                pending_tokens,
            )
            .await?;
            let temperature = config::resolve_temperature(&self.cli, &self.cfg, &self.model);
            let extra_body = config::resolve_extra_body(&self.cfg, &self.model);
            let completion_model = self.client.completion_model(self.model.to_string());
            let read_tracker = self.session.read_tracker.clone();
            #[cfg(feature = "mcp")]
            let mcp_manager = if !self.cli.mcp_is_eligible(&self.cfg) {
                None
            } else {
                connect_headless_mcp(&self.cfg, &self.workspace).await
            };
            let agent = provider::build_agent_in_workspace(
                completion_model,
                &self.cli,
                &self.cfg,
                &self.context,
                self.workspace.clone(),
                self.permission,
                // Non-interactive dispatch never keeps the ask channel: with
                // no one draining it, an `Ask` verdict must fail closed as a
                // denial rather than block forever. See `handle_ask_inner`.
                None,
                self.sandbox.clone(),
                read_tracker,
                true,
                temperature,
                extra_body,
                #[cfg(feature = "skills")]
                std::sync::Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
                #[cfg(feature = "mcp")]
                mcp_manager.as_ref(),
            )
            .await;
            #[cfg(feature = "advisor")]
            {
                let mut msgs = self.session.messages.clone();
                msgs.push(SessionMessage {
                    role: MessageRole::User,
                    content: CompactString::new(&msg),
                    estimated_tokens: Session::estimate_tokens(&msg),
                    tool_call_id: None,
                });
                crate::extras::advisor::set_session_messages(msgs);
            }
            if let Some(ss) = self.status_signals.as_ref() {
                ss.send_start();
            }
            let history = crate::agent::runner::convert_history(&self.session);
            let response_result = agent
                .run_print(
                    &msg,
                    self.cli.pure_stdout,
                    &self.cfg.retry,
                    history,
                    #[cfg(feature = "hooks")]
                    None,
                )
                .await;
            if let Some(ss) = self.status_signals.as_ref() {
                ss.send_stop();
            }
            let (response, usage, interactions) = response_result?;
            if !self.cli.no_session {
                let mut session = self.session;
                // Prompt, then tool calls/results in provider order, then the
                // assistant message: the same record order the interactive UI
                // writes, so `--continue` replays the turn in sequence.
                crate::print::persist_headless_turn(&mut session, &msg, &response, &interactions);
                let anthropic_native = self.cfg.is_anthropic_native(&session.provider);
                session.charge_usage_delta(usage.into(), anthropic_native);
                session::storage::save_session(&session)?;
                let _ =
                    session::chat_history::append_entry(&session::chat_history::ChatHistoryEntry {
                        content: msg,
                        timestamp: session.updated_at.clone(),
                    });
            }
        }

        #[cfg(feature = "hooks")]
        crate::extras::hooks::dispatch_session_end("exit").await;

        Ok(())
    }

    #[cfg(feature = "loop")]
    async fn dispatch_loop(self) -> anyhow::Result<()> {
        let model_completion = self.client.completion_model(self.model.to_string());
        let temperature = config::resolve_temperature(&self.cli, &self.cfg, &self.model);
        let extra_body = config::resolve_extra_body(&self.cfg, &self.model);
        let read_tracker = self.session.read_tracker.clone();
        #[cfg(feature = "mcp")]
        let mcp_manager = if !self.cli.mcp_is_eligible(&self.cfg) {
            None
        } else {
            connect_headless_mcp(&self.cfg, &self.workspace).await
        };
        let agent = provider::build_agent_in_workspace(
            model_completion,
            &self.cli,
            &self.cfg,
            &self.context,
            self.workspace.clone(),
            self.permission,
            // Non-interactive dispatch never keeps the ask channel; see the
            // matching note in `dispatch_print`.
            None,
            self.sandbox.clone(),
            read_tracker,
            true,
            temperature,
            extra_body,
            #[cfg(feature = "skills")]
            std::sync::Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager.as_ref(),
        )
        .await;
        let result = crate::extras::r#loop::headless::run_headless_loop(
            agent,
            &self.cli,
            &self.cfg,
            &self.context,
            self.status_signals,
            &self.sandbox,
        )
        .await;
        #[cfg(feature = "hooks")]
        crate::extras::hooks::dispatch_session_end("exit").await;
        result
    }

    async fn dispatch_interactive(self) -> anyhow::Result<()> {
        let Startup {
            cli,
            cfg,
            mut session,
            mut context,
            workspace,
            client,
            permission,
            ask_tx,
            ask_rx,
            sandbox,
            status_signals,
            arch_msg,
            #[cfg(feature = "advisor")]
            handoff_rx,
            ..
        } = self;

        let auto_trigger_msg = select_interactive_auto_trigger(&cli, arch_msg);

        crate::ui::run_interactive(
            crate::ui::state::UiContext::new(
                &cli,
                &cfg,
                &mut session,
                &mut context,
                workspace,
                client,
                permission,
                ask_tx,
                sandbox,
                status_signals,
            ),
            None,
            ask_rx,
            auto_trigger_msg,
            #[cfg(feature = "advisor")]
            handoff_rx,
        )
        .await?;

        #[cfg(feature = "hooks")]
        crate::extras::hooks::dispatch_session_end("exit").await;

        Ok(())
    }
}

fn interactive_initial_message(cli: &Cli) -> Option<String> {
    if cli.print {
        return None;
    }
    let message = cli.message.join(" ");
    (!message.trim().is_empty()).then_some(message)
}

fn select_interactive_auto_trigger(cli: &Cli, fallback: Option<String>) -> Option<String> {
    interactive_initial_message(cli).or(fallback)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "multithread")]
    use super::ACTIVE_OPENROUTER_PRICING_REAPERS;
    use super::{
        OpenRouterPricingRefresh, ResumeProviderDecision, apply_openrouter_pricing_refresh_result,
        apply_resume_provider_decision, compact_headless_session_with, headless_compaction_plan,
        interactive_initial_message, needs_openrouter_context_refresh,
        resolve_resume_provider_decision, run_startup_probes_concurrently,
        select_interactive_auto_trigger, unavailable_sandbox_must_fail,
        validate_startup_permission_policy,
    };
    use crate::cli::Cli;

    #[test]
    fn headless_print_plans_compaction_for_an_over_budget_resumed_session() {
        let mut session = crate::session::Session::new("openai", "model", 100, "");
        session.overhead_tokens = 90;
        session.add_message(crate::session::MessageRole::User, &"a".repeat(40));
        session.add_message(crate::session::MessageRole::Assistant, &"b".repeat(40));
        let cfg = crate::config::Config {
            compact_enabled: Some(true),
            reserve_tokens: Some(20),
            keep_recent_tokens: Some(5),
            ..crate::config::Config::default()
        };

        let plan = headless_compaction_plan(
            &session,
            &cfg,
            0,
            #[cfg(feature = "memory")]
            None,
        )
        .expect("enabled over-budget headless history must compact before dispatch");

        assert_eq!(plan.cut_idx, 1);
        assert_eq!(
            plan.tokens_before,
            crate::session::Session::estimate_tokens(&"a".repeat(40))
        );
        assert_eq!(plan.input_token_budget, 80);
        assert_eq!(plan.response_token_budget, 20);
    }

    #[test]
    fn headless_compaction_plan_includes_pending_prompt_tokens() {
        // Session well within budget on its own, but pending prompt pushes it over.
        // Two messages are needed so select_compaction_cut can return a non-zero cut.
        let mut session = crate::session::Session::new("openai", "model", 100, "");
        session.overhead_tokens = 10;
        // Older message (to be cut) + recent message (to be kept).
        session.add_message(crate::session::MessageRole::User, &"a".repeat(40));
        session.add_message(crate::session::MessageRole::Assistant, &"b".repeat(10));
        let cfg = crate::config::Config {
            compact_enabled: Some(true),
            reserve_tokens: Some(20),
            // keep_recent_tokens must be <= the assistant message's estimated tokens
            // so that select_compaction_cut finds a non-zero cut index (the older
            // user message gets summarized, the recent assistant message is kept).
            keep_recent_tokens: Some(3),
            ..crate::config::Config::default()
        };
        // Without pending tokens: overhead(10)+msgs ~25 tokens, well under budget=80.
        assert!(
            headless_compaction_plan(
                &session,
                &cfg,
                0,
                #[cfg(feature = "memory")]
                None,
            )
            .is_none(),
            "session alone must not trigger compaction"
        );
        // With pending=60 the total ~25+60=85 > 80; plan must be produced.
        assert!(
            headless_compaction_plan(
                &session,
                &cfg,
                60,
                #[cfg(feature = "memory")]
                None,
            )
            .is_some(),
            "pending prompt must trigger compaction"
        );
    }

    #[test]
    fn headless_print_does_not_plan_compaction_when_disabled() {
        let mut session = crate::session::Session::new("openai", "model", 100, "");
        session.overhead_tokens = 100;
        session.add_message(crate::session::MessageRole::User, &"a".repeat(80));
        assert!(
            headless_compaction_plan(
                &session,
                &crate::config::Config::default(),
                0,
                #[cfg(feature = "memory")]
                None,
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn headless_print_compacts_resumed_history_before_provider_dispatch() {
        let mut session = crate::session::Session::new("openai", "model", 100, "");
        session.overhead_tokens = 90;
        session.add_message(crate::session::MessageRole::User, &"a".repeat(40));
        session.add_message(crate::session::MessageRole::Assistant, &"b".repeat(40));
        let cfg = crate::config::Config {
            compact_enabled: Some(true),
            reserve_tokens: Some(20),
            keep_recent_tokens: Some(5),
            ..crate::config::Config::default()
        };

        let result = compact_headless_session_with(
            &mut session,
            &cfg,
            0,
            #[cfg(feature = "memory")]
            None,
            |model, messages, previous_summary, input_budget, response_budget| async move {
                assert_eq!(model, "model");
                assert_eq!(messages.len(), 1);
                assert!(previous_summary.is_none());
                assert_eq!(input_budget, 80);
                assert_eq!(response_budget, 20);
                Ok(("HEADLESS_SUMMARY".to_string(), 1usize))
            },
        )
        .await
        .unwrap();

        assert_eq!(result, Some(("HEADLESS_SUMMARY".to_string(), 1)));
        assert_eq!(session.compactions.len(), 1);
        assert_eq!(session.messages[0].content, "HEADLESS_SUMMARY");
        assert_eq!(session.messages.len(), 2);
    }
    use crate::config::Config;
    use crate::sandbox::{Sandbox, SandboxPolicy};
    use crate::session::Session;

    struct LocalPricingServer {
        url: String,
        started: std::sync::mpsc::Receiver<()>,
        closed: std::sync::mpsc::Receiver<bool>,
        thread: std::thread::JoinHandle<()>,
    }

    fn spawn_local_pricing_server(response: Option<(u16, &'static str)>) -> LocalPricingServer {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started) = std::sync::mpsc::channel();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "pricing client closed before sending headers");
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            started_tx.send(()).unwrap();
            if let Some((status, body)) = response {
                let reason = if status == 200 { "OK" } else { "Failure" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                return;
            }

            let closed_cleanly = loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break false;
                    }
                    Err(_) => break true,
                }
            };
            closed_tx.send(closed_cleanly).unwrap();
        });
        LocalPricingServer {
            url: format!("http://{address}/models"),
            started,
            closed,
            thread,
        }
    }

    async fn recv_channel<T>(receiver: &std::sync::mpsc::Receiver<T>, label: &str) -> T {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match receiver.try_recv() {
                Ok(value) => return value,
                Err(std::sync::mpsc::TryRecvError::Empty)
                    if std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => panic!("{label}: {error}"),
            }
        }
    }

    async fn wait_for_pricing_refresh(refresh: &OpenRouterPricingRefresh) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !refresh.is_finished() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(refresh.is_finished(), "pricing refresh did not finish");
    }

    fn local_pricing_refresh(url: String) -> OpenRouterPricingRefresh {
        OpenRouterPricingRefresh::start("test/model".into(), true, true, 128_000, async move {
            crate::provider::fetch_openrouter_pricing_from_url(
                None,
                &std::collections::HashMap::new(),
                None,
                &url,
            )
            .await
        })
    }

    #[tokio::test]
    async fn pending_pricing_refresh_never_blocks_readiness_and_allows_relaunch() {
        let server = spawn_local_pricing_server(None);
        let mut refresh = local_pricing_refresh(server.url.clone());
        recv_channel(
            &server.started,
            "pricing request did not reach delayed server",
        )
        .await;

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            refresh.finish_without_wait(),
        )
        .await
        .expect("readiness waited for the delayed pricing response");
        assert!(result.is_none(), "pending refresh returned a result");
        assert!(
            recv_channel(&server.closed, "cancelled pricing connection stayed open").await,
            "cancelled pricing connection was not closed"
        );
        server.thread.join().unwrap();

        const BODY: &str = r#"{"data":[{"id":"test/model","pricing":{"prompt":"0.000001","completion":"0.000002"}}]}"#;
        let replacement_server = spawn_local_pricing_server(Some((200, BODY)));
        let mut replacement = local_pricing_refresh(replacement_server.url.clone());
        recv_channel(
            &replacement_server.started,
            "replacement pricing request did not start",
        )
        .await;
        wait_for_pricing_refresh(&replacement).await;
        assert!(matches!(
            replacement.finish_without_wait().await,
            Some(Ok(_))
        ));
        replacement_server.thread.join().unwrap();
    }

    #[cfg(feature = "multithread")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_non_cooperative_pricing_refresh_has_a_bounded_join() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let mut refresh =
            OpenRouterPricingRefresh::start("test/model".into(), true, true, 128_000, async move {
                let _ = started_tx.send(());
                std::thread::sleep(std::time::Duration::from_millis(500));
                let _ = finished_tx.send(());
                Ok(std::collections::HashMap::new())
            });
        started_rx.await.unwrap();

        let started = std::time::Instant::now();
        assert!(refresh.finish_without_wait().await.is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(250),
            "aborted pricing join exceeded its readiness budget"
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), finished_rx)
            .await
            .expect("aborted pricing work did not finish under its reaper")
            .expect("aborted pricing work dropped its completion signal");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while ACTIVE_OPENROUTER_PRICING_REAPERS.load(std::sync::atomic::Ordering::Acquire) != 0
            && std::time::Instant::now() < deadline
        {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            ACTIVE_OPENROUTER_PRICING_REAPERS.load(std::sync::atomic::Ordering::Acquire),
            0,
            "aborted pricing reaper remained active"
        );
    }

    #[tokio::test]
    async fn completed_pricing_refresh_updates_missing_session_metadata() {
        const BODY: &str = r#"{"data":[{"id":"test/model","pricing":{"prompt":"0.000001","completion":"0.000002"},"context_length":64000}]}"#;
        let server = spawn_local_pricing_server(Some((200, BODY)));
        let mut refresh = local_pricing_refresh(server.url.clone());
        recv_channel(
            &server.started,
            "pricing request did not reach success server",
        )
        .await;
        wait_for_pricing_refresh(&refresh).await;
        let result = refresh.finish_without_wait().await;
        let mut session = Session::new("openrouter", "test/model", 128_000, "");

        apply_openrouter_pricing_refresh_result(&mut session, &refresh, result);

        assert_eq!(session.input_token_cost, 1.0);
        assert_eq!(session.output_token_cost, 2.0);
        assert_eq!(session.context_window, 64_000);
        server.thread.join().unwrap();
    }

    #[test]
    fn failed_pricing_refresh_preserves_existing_session_metadata() {
        let refresh = OpenRouterPricingRefresh {
            handle: None,
            model: "test/model".into(),
            need_pricing: true,
            need_context: true,
            initial_context_window: 128_000,
        };
        let mut session = Session::new("openrouter", "test/model", 128_000, "");
        session.input_token_cost = 7.0;
        session.output_token_cost = 9.0;

        apply_openrouter_pricing_refresh_result(
            &mut session,
            &refresh,
            Some(Err(anyhow::anyhow!("closed pricing failure"))),
        );

        assert_eq!(session.input_token_cost, 7.0);
        assert_eq!(session.output_token_cost, 9.0);
        assert_eq!(session.context_window, 128_000);
    }

    #[test]
    fn quick_model_context_prevents_live_context_refresh() {
        let cfg = Config {
            quick_models: Some(std::collections::HashMap::from([(
                "test".to_string(),
                crate::config::QuickModelConfig {
                    provider: "openrouter".into(),
                    model: "uncatalogued/model".into(),
                    input_token_cost: 0.0,
                    output_token_cost: 0.0,
                    reserve_tokens: None,
                    temperature: None,
                    extra_body: None,
                    context_window: Some(64_000),
                },
            )])),
            ..Config::default()
        };

        assert!(!needs_openrouter_context_refresh(
            &cfg,
            "uncatalogued/model"
        ));
        assert!(needs_openrouter_context_refresh(&cfg, "other/model"));
    }

    #[tokio::test]
    async fn custom_provider_timeout_still_controls_live_pricing_request() {
        let server = spawn_local_pricing_server(None);
        let url = server.url.clone();
        let mut custom = std::collections::HashMap::new();
        custom.insert(
            "openrouter".to_string(),
            crate::config::CustomProviderConfig {
                provider_type: "openrouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key_env: None,
                danger_accept_invalid_certs: None,
                api_style: None,
                headers: std::collections::HashMap::new(),
                timeout_secs: Some(1),
                model: None,
            },
        );
        let mut refresh =
            OpenRouterPricingRefresh::start("test/model".into(), true, true, 128_000, async move {
                crate::provider::fetch_openrouter_pricing_from_url(None, &custom, None, &url).await
            });
        recv_channel(
            &server.started,
            "custom-timeout pricing request did not start",
        )
        .await;

        wait_for_pricing_refresh(&refresh).await;
        assert!(matches!(refresh.finish_without_wait().await, Some(Err(_))));
        assert!(
            recv_channel(&server.closed, "timed-out pricing connection stayed open").await,
            "timed-out pricing connection was not closed"
        );
        server.thread.join().unwrap();
    }

    #[test]
    fn startup_pricing_refresh_is_owned_across_prompt_resolution() {
        let startup = include_str!("startup.rs");
        assert!(startup.contains("impl Drop for OpenRouterPricingRefresh"));
        assert!(startup.contains("handle.abort();"));

        let main = include_str!("main.rs");
        let start = main
            .find("startup.start_openrouter_pricing_refresh();")
            .expect("pricing refresh start missing");
        let features = main
            .find("startup.init_features().await?;")
            .expect("feature initialization missing");
        let prompts = main
            .find("let prompts = startup.resolve_prompts().await;")
            .expect("prompt result was not retained for pricing cleanup");
        let cleanup = main
            .find("startup.finish_openrouter_pricing_refresh().await;")
            .expect("pricing refresh cleanup missing");
        let propagate = main
            .find("prompts?;")
            .expect("prompt error propagation missing");
        let dispatch = main
            .find("startup.dispatch().await")
            .expect("startup dispatch missing");
        assert!(
            start < features
                && features < prompts
                && prompts < cleanup
                && cleanup < propagate
                && propagate < dispatch
        );
    }

    #[test]
    fn startup_probe_join_starts_both_before_waiting_for_the_slower_probe() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_general_tx, release_general_rx) = std::sync::mpsc::channel();
        let (release_worker_tx, release_worker_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();

        let join = std::thread::spawn(move || {
            let general_started_tx = started_tx.clone();
            let probes = run_startup_probes_concurrently(
                move || {
                    general_started_tx.send("general").unwrap();
                    release_general_rx.recv().unwrap();
                    anyhow::bail!("closed general failure")
                },
                move || {
                    started_tx.send("worker").unwrap();
                    release_worker_rx.recv().unwrap();
                    "closed worker status"
                },
            );
            finished_tx.send(probes).unwrap();
        });

        let deadline = std::time::Duration::from_secs(1);
        let first = started_rx
            .recv_timeout(deadline)
            .expect("first startup probe did not start");
        let second = started_rx
            .recv_timeout(deadline)
            .expect("second startup probe was serialized behind the first");
        assert_ne!(first, second);

        release_general_tx.send(()).unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "join returned before the slower worker probe completed"
        );
        release_worker_tx.send(()).unwrap();
        let (general, worker) = finished_rx
            .recv_timeout(deadline)
            .expect("startup probes did not join after the slower probe completed")
            .expect("startup probe orchestration failed");
        assert_eq!(general.unwrap_err().to_string(), "closed general failure");
        assert_eq!(worker, "closed worker status");
        join.join().unwrap();
    }

    #[test]
    fn positional_interactive_input_becomes_one_auto_trigger_message() {
        let cli = Cli {
            message: vec!["review".into(), "this change".into()],
            ..Cli::default()
        };

        assert_eq!(
            interactive_initial_message(&cli),
            Some("review this change".to_string())
        );
    }

    #[test]
    fn empty_interactive_startup_remains_idle() {
        assert_eq!(interactive_initial_message(&Cli::default()), None);
        let whitespace = Cli {
            message: vec!["   ".into()],
            ..Cli::default()
        };
        assert_eq!(interactive_initial_message(&whitespace), None);
    }

    #[test]
    fn print_mode_keeps_positional_input_out_of_the_tui_auto_trigger() {
        let cli = Cli {
            print: true,
            message: vec!["headless prompt".into()],
            ..Cli::default()
        };

        assert_eq!(interactive_initial_message(&cli), None);
        assert_eq!(cli.message.join(" "), "headless prompt");
    }

    #[test]
    fn positional_input_precedes_the_existing_interactive_fallback() {
        let cli = Cli {
            message: vec!["user prompt".into()],
            ..Cli::default()
        };

        assert_eq!(
            select_interactive_auto_trigger(&cli, Some("fallback".into())),
            Some("user prompt".into())
        );
    }

    #[test]
    fn empty_interactive_startup_preserves_the_existing_fallback() {
        assert_eq!(
            select_interactive_auto_trigger(&Cli::default(), Some("fallback".into())),
            Some("fallback".into())
        );
    }

    #[test]
    fn every_execution_mode_rejects_invalid_permissions_before_startup() {
        let invalid = Config {
            permission_regex: Some(serde_json::json!({
                "read": {"[unterminated": "allow"}
            })),
            ..Config::default()
        };
        let modes = [
            Cli::default(),
            Cli {
                print: true,
                ..Cli::default()
            },
            Cli {
                #[cfg(feature = "loop")]
                loop_mode: true,
                ..Cli::default()
            },
            Cli {
                #[cfg(feature = "acp")]
                acp_enabled: true,
                ..Cli::default()
            },
        ];

        for cli in modes {
            let error = validate_startup_permission_policy(&cli, &invalid)
                .unwrap_err()
                .to_string();
            assert!(error.contains("permission-regex"), "{error}");
            assert!(error.contains("read"), "{error}");
            assert!(error.contains("[unterminated"), "{error}");
        }

        for cli in [
            Cli {
                no_tools: true,
                ..Cli::default()
            },
            Cli {
                dangerously_skip_permissions: true,
                ..Cli::default()
            },
        ] {
            let error = validate_startup_permission_policy(&cli, &invalid)
                .unwrap_err()
                .to_string();
            assert!(error.contains("permission-regex"), "{error}");
            assert!(error.contains("read"), "{error}");
            assert!(error.contains("[unterminated"), "{error}");
        }
    }

    /// Pins the two inputs that decide whether a missing backend bails or
    /// degrades. An unknown backend is never "available" on any platform, so
    /// this reproduces Windows (no backend at all) and bwrap-less Linux
    /// without depending on the host.
    #[test]
    fn missing_backend_bails_only_when_the_sandbox_was_explicitly_requested() {
        let unavailable = Sandbox::new(true, "definitely-not-a-real-backend");
        assert_eq!(unavailable.policy(), SandboxPolicy::RequiredButUnavailable);

        // Inheriting the default: attempt to sandbox, but degrade rather than
        // refuse to start, since some platforms have no backend to offer.
        let cli = Cli::default();
        let cfg = Config::default();
        assert!(cli.resolve_sandbox(&cfg));
        assert!(!cli.sandbox_explicitly_requested(&cfg));

        // Asking for it explicitly keeps the fail-closed guarantee.
        let explicit = Cli {
            sandbox: true,
            ..Cli::default()
        };
        assert!(explicit.sandbox_explicitly_requested(&cfg));

        let configured = Config {
            sandbox: Some(true),
            ..Config::default()
        };
        assert!(Cli::default().sandbox_explicitly_requested(&configured));

        // Refusing it outranks everything and never bails.
        let refused = Cli {
            no_sandbox: true,
            ..Cli::default()
        };
        assert!(!refused.resolve_sandbox(&configured));
        assert!(!refused.sandbox_explicitly_requested(&configured));

        // Windows never silently drops an enabled sandbox, including when an
        // unknown or stale backend name was selected.
        assert!(unavailable_sandbox_must_fail(&cli, &cfg, true));
        assert!(!unavailable_sandbox_must_fail(&refused, &configured, true));

        let selected = Cli {
            sandbox_backend: Some("definitely-not-a-real-backend".into()),
            ..Cli::default()
        };
        assert!(unavailable_sandbox_must_fail(&selected, &cfg, false));
    }

    #[test]
    fn session_resume_provider_identity_restores_saved_identity_under_changed_defaults() {
        let session = Session::new("anthropic", "claude-saved", 200_000, "");
        let cli = Cli {
            provider: Some("openai".to_string()),
            model: Some("gpt-current-default".to_string()),
            ..Cli::default()
        };

        let decision =
            resolve_resume_provider_decision(&cli, &Config::default(), &session).unwrap();

        assert_eq!(
            decision,
            ResumeProviderDecision::Restore(super::ProviderIdentity {
                provider: "anthropic".into(),
                model: "claude-saved".into(),
            })
        );
    }

    #[test]
    fn session_resume_provider_override_is_explicit_and_auditable() {
        let mut session = Session::new("anthropic", "claude-saved", 200_000, "");
        let cli = Cli {
            resume_provider: Some("openai".to_string()),
            resume_model: Some("gpt-explicit".to_string()),
            ..Cli::default()
        };
        let cfg = Config::default();

        let decision = resolve_resume_provider_decision(&cli, &cfg, &session).unwrap();
        apply_resume_provider_decision(&mut session, &decision, &cfg);

        assert_eq!(session.provider, "openai");
        assert_eq!(session.model, "gpt-explicit");
        assert_eq!(session.provider_override_audit.len(), 1);
        let audit = &session.provider_override_audit[0];
        assert_eq!(audit.from_provider, "anthropic");
        assert_eq!(audit.from_model, "claude-saved");
        assert_eq!(audit.to_provider, "openai");
        assert_eq!(audit.to_model, "gpt-explicit");
        assert!(audit.context_disclosure_acknowledged);
    }

    #[test]
    fn session_resume_same_provider_model_override_is_explicit_without_cross_provider_ack() {
        let mut session = Session::new("anthropic", "claude-old", 200_000, "");
        let cli = Cli {
            resume_model: Some("claude-new".to_string()),
            ..Cli::default()
        };
        let cfg = Config::default();

        let decision = resolve_resume_provider_decision(&cli, &cfg, &session).unwrap();
        apply_resume_provider_decision(&mut session, &decision, &cfg);

        assert_eq!(session.provider, "anthropic");
        assert_eq!(session.model, "claude-new");
        assert_eq!(session.provider_override_audit.len(), 1);
        assert!(
            !session.provider_override_audit[0].context_disclosure_acknowledged,
            "same-provider model changes must not be recorded as cross-provider disclosure"
        );
    }

    #[test]
    fn session_resume_provider_override_resolution_does_not_mutate_on_restore_or_error() {
        let session = Session::new("anthropic", "claude-saved", 200_000, "");
        let original_provider = session.provider.clone();
        let original_model = session.model.clone();
        let cli = Cli {
            resume_provider: Some("missing-profile".to_string()),
            ..Cli::default()
        };

        assert!(resolve_resume_provider_decision(&cli, &Config::default(), &session).is_err());
        assert_eq!(session.provider, original_provider);
        assert_eq!(session.model, original_model);
        assert!(session.provider_override_audit.is_empty());
    }

    #[test]
    fn session_resume_provider_identity_legacy_metadata_deserializes_without_audit() {
        let session = Session::new("anthropic", "claude-saved", 200_000, "");
        let mut legacy_json = serde_json::to_value(&session).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("provider_override_audit");

        let restored: Session = serde_json::from_value(legacy_json).unwrap();

        assert_eq!(restored.provider, "anthropic");
        assert_eq!(restored.model, "claude-saved");
        assert!(restored.provider_override_audit.is_empty());
    }
}
