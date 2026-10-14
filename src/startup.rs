use compact_str::CompactString;

use crate::agent::tools;
use crate::cli::{Cli, OutputFormat};
use crate::config::{self, Config};
use crate::context::{self, ContextFiles};
use crate::extras::status_signals::StatusSignals;
use crate::paths::AppPaths;
use crate::permission::ask::{AskReceiver, AskSender};
use crate::permission::checker::PermCheck;
use crate::provider::{self, AnyClient};
use crate::sandbox::{DEFAULT_COMMAND_LIMITS, Sandbox, SandboxPolicy};
use crate::session::{self, Session};

// ── Helper functions ─────────────────────────────────────────────────────

/// Invalid headless input is a local usage error, before provider/session setup.
#[derive(Debug, thiserror::Error)]
#[error("no prompt given")]
pub(crate) struct HeadlessUsageError;

async fn validate_headless_prompt(cli: &Cli, workspace: &std::path::Path) -> anyhow::Result<()> {
    if !cli.print || cli.message.iter().any(|part| !part.trim().is_empty()) {
        return Ok(());
    }
    crate::print::emit_headless_preflight_failure(cli, workspace, "no prompt given").await?;
    Err(HeadlessUsageError.into())
}

fn resolve_startup_model_pricing(
    cli: &Cli,
    cfg: &Config,
    provider: &str,
    model: &str,
    openrouter_model: Option<&str>,
) -> Option<(f64, f64)> {
    if let Some(quick) = selected_startup_quick_model(cli, cfg, provider, model, openrouter_model) {
        return quick.resolved_prices();
    }
    crate::ui::slash::resolve_model_pricing(provider, model, cfg, None)
}

fn selected_startup_quick_model(
    cli: &Cli,
    cfg: &Config,
    provider: &str,
    model: &str,
    openrouter_model: Option<&str>,
) -> Option<config::QuickModelConfig> {
    if let Some(quick) = cli.resolve_quick_model(cfg) {
        return Some(quick.clone());
    }
    // A selected config alias carries operator pricing, just like /models or
    // /provider. A CLI/raw environment override selects raw-id pricing even
    // when that id happens to equal the alias's underlying model.
    let raw_env_override =
        provider == "openrouter" && openrouter_model.is_some_and(|model| !model.is_empty());
    if cli.model.is_none() && !raw_env_override {
        let quick_models = config::quick_models_map(cfg);
        if let Some(quick) = cfg.model.as_deref().and_then(|name| quick_models.get(name))
            && quick.provider == provider
            && quick.model == model
        {
            return Some(quick.clone());
        }
    }
    None
}

pub(crate) fn diagnostic_startup_pricing(
    cli: &Cli,
    cfg: &Config,
    provider: &str,
    model: &str,
) -> (Option<(f64, f64)>, &'static str) {
    if let Some(quick) = selected_startup_quick_model(
        cli,
        cfg,
        provider,
        model,
        std::env::var("OPENROUTER_MODEL").ok().as_deref(),
    ) {
        return (
            quick.resolved_prices(),
            if cli.quick_model.is_some() {
                "selected quick-model configuration"
            } else {
                "selected config alias"
            },
        );
    }
    crate::ui::slash::providers::diagnostic_model_pricing(provider, model, cfg, None)
}

fn resolve_startup_cache_prices(
    cli: &Cli,
    cfg: &Config,
    provider: &str,
    model: &str,
    openrouter_model: Option<&str>,
    input_price: f64,
) -> crate::pricing::CachePrices {
    let raw_env_override =
        provider == "openrouter" && openrouter_model.is_some_and(|model| !model.is_empty());
    let selected = cli.quick_model.as_deref().or_else(|| {
        (cli.model.is_none() && !raw_env_override)
            .then_some(cfg.model.as_deref())
            .flatten()
    });
    crate::pricing::cache_prices_for_model(provider, model, cfg, selected, input_price)
}

#[cfg(test)]
mod startup_cache_pricing_tests {
    use super::*;

    #[test]
    fn startup_cache_pricing_preserves_selected_alias_and_raw_override_precedence() {
        use clap::Parser;
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider":"anthropic", "model":"operator", "quick_models": {
                "operator":{"provider":"anthropic", "model":"claude-opus-5-5", "input_token_cost":8.0, "output_token_cost":40.0, "cache_read_token_cost":0.6}
            }
        })).unwrap();
        let plain = Cli::parse_from(["mini-agent"]);
        let selected =
            resolve_startup_cache_prices(&plain, &cfg, "anthropic", "claude-opus-5-5", None, 8.0);
        assert_eq!(selected.read, Some(0.6));
        assert_eq!(selected.write, Some(10.0));
        let raw = Cli::parse_from(["mini-agent", "--model", "claude-opus-5-5"]);
        assert_eq!(
            resolve_startup_cache_prices(&raw, &cfg, "anthropic", "claude-opus-5-5", None, 4.0)
                .read,
            Some(0.2)
        );
        let explicit = Cli::parse_from(["mini-agent", "--quick-model", "operator"]);
        assert_eq!(
            resolve_startup_cache_prices(
                &explicit,
                &cfg,
                "anthropic",
                "claude-opus-5-5",
                None,
                8.0
            )
            .read,
            Some(0.6)
        );
        // An environment/raw selection on OpenRouter cannot borrow an alias's
        // explicit cache rate, even if the raw id equals its underlying model.
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider":"openrouter", "model":"operator", "quick_models": {
                "operator":{"provider":"openrouter", "model":"vendor/model", "input_token_cost":8.0, "cache_read_token_cost":2.0}
            }
        })).unwrap();
        assert_eq!(
            resolve_startup_cache_prices(
                &plain,
                &cfg,
                "openrouter",
                "vendor/model",
                Some("other/model"),
                4.0
            )
            .read,
            Some(1.0)
        );
    }
}

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
    crate::permission::build_noninteractive_permission_with_cli(cli, cfg, authority)?;
    Ok(())
}

fn require_recent_session(
    result: anyhow::Result<Vec<Session>>,
    workspace: &std::path::Path,
) -> anyhow::Result<Session> {
    let sessions =
        result.map_err(|error| anyhow::anyhow!("failed to continue session: {error}"))?;
    sessions
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot continue: no saved sessions for this workspace ({}); use --session or /sessions", workspace.display()))
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
    let qm = config::quick_models_map(cfg);
    let prices =
        crate::ui::slash::resolve_model_pricing(&target.provider, &target.model, cfg, None);
    session.set_input_output_prices(prices);
    let input = session.input_token_cost;
    session.set_cache_prices(crate::pricing::cache_prices_for_model(
        &target.provider,
        &target.model,
        cfg,
        None,
        input,
    ));
    session.update_context_window(cfg.resolve_context_window(&target.provider, &target.model, &qm));
}

/// Capture only immutable startup policy. Runtime publication/cleanup owners
/// remain on the calling thread and need not implement Sync.
fn configured_sandbox_validation_probe<'a>(
    cli: &'a Cli,
    cfg: &'a Config,
) -> impl FnOnce() -> anyhow::Result<()> + Send + 'a {
    move || {
        if !cli.general_sandbox_is_eligible(cfg) {
            return Ok(());
        }
        let backend = cli.resolve_sandbox_backend(cfg);
        let sandbox = Sandbox::new(cli.resolve_sandbox(cfg), &backend)
            .with_windows_appcontainer_roots(
                cli.resolve_windows_appcontainer_read_roots(cfg),
                cli.resolve_windows_appcontainer_write_roots(cfg),
            );
        if sandbox.policy() == SandboxPolicy::RequiredButUnavailable
            && unavailable_sandbox_must_fail(cli, cfg, cfg!(target_os = "windows"))
        {
            let reason = sandbox
                .unavailable_diagnostic()
                .map(|diagnostic| format!("\n{diagnostic}"))
                .unwrap_or_default();
            anyhow::bail!(
                "sandbox backend '{backend}' is unavailable or has no successful production preflight — refusing to start with unsandboxed execution (use --no-sandbox to disable sandboxing explicitly){reason}"
            );
        }
        Ok(())
    }
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
    let input_price = qmc.resolved_prices().map(|prices| prices.0).unwrap_or(0.0);
    session.set_input_output_prices(qmc.resolved_prices());
    session.set_cache_prices(crate::pricing::cache_prices_for_model(
        &qmc.provider,
        &qmc.model,
        cfg,
        Some(qm_name),
        input_price,
    ));
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
    let mut manager =
        crate::extras::mcp::McpClientManager::connect_config_in_binding(cfg, workspace).await;
    if !crate::agent::runner::current_work_scope_is_cancelled() {
        report_headless_mcp_notices(Some(&mut manager), |notice| eprintln!("{notice}"));
    }
    Some(manager)
}

/// Drain both connection and later catalog notices to the headless diagnostic sink.
#[cfg(feature = "mcp")]
pub(crate) fn report_headless_mcp_notices(
    manager: Option<&mut crate::extras::mcp::McpClientManager>,
    mut report: impl FnMut(&str),
) {
    if let Some(manager) = manager {
        for notice in manager.take_notices() {
            report(&crate::ui::events::sanitize_output(&notice));
        }
    }
}

// ── Startup state ────────────────────────────────────────────────────────

/// Build a future and move it to the heap inside this short, non-async frame.
///
/// `Box::pin(child()).await` in an async fn still evaluates `child()` inside
/// the caller's poll function, and an unoptimized build reserves that value's
/// full size there, once per temporary, for as long as the caller is polled.
/// Startup stacks several such frames on the main thread, which on Windows has
/// a 1 MiB default stack; `startup_commands_run_with_the_windows_default_stack_budget`
/// runs the binary under that budget. Building the child here charges the
/// caller's frame one pointer instead.
#[inline(never)]
pub(crate) fn heap_future<F: std::future::Future>(
    make: impl FnOnce() -> F,
) -> std::pin::Pin<Box<F>> {
    Box::pin(make())
}

pub(crate) struct Startup {
    pub cli: Cli,
    pub cfg: Config,
    // Startup-owned source of truth shared by persistent artifact owners.
    pub paths: AppPaths,
    pub workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
    pub is_first_startup: bool,
    pub context: ContextFiles,
    pub provider: CompactString,
    pub model: CompactString,
    pub session: Session,
    pub client: AnyClient,
    native_startup: Option<crate::session::native_publication::NativeStartupPublication>,
    native_ticket: Option<crate::session::native_publication::NativePublicationTicket>,
    native_headless: Option<NativeHeadlessOwner>,
    #[cfg(feature = "hooks")]
    native_session_end_done: bool,
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
    #[cfg(feature = "hooks")]
    session_start_task: Option<tokio::task::JoinHandle<()>>,
}

type OpenRouterPricingMap = std::collections::HashMap<String, provider::OpenRouterModelInfo>;
const OPENROUTER_PRICING_ABORT_JOIN_GRACE: std::time::Duration =
    std::time::Duration::from_millis(100);
#[cfg(test)]
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
    if !session.needs_compaction_with_pending_after_tool_result_pruning(
        reserve,
        pending_tokens,
        cfg.resolve_keep_recent_tool_results(),
    ) {
        return None;
    }

    let keep_recent = cfg.resolve_keep_recent_tokens(session.context_window);
    let projected =
        session.context_messages_with_pruned_tool_results(cfg.resolve_keep_recent_tool_results());
    let cut_idx = Session::select_compaction_cut(&projected, keep_recent);
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

#[derive(Debug)]
struct HeadlessCompactionUsage(crate::event::UsageDelta);
impl std::fmt::Display for HeadlessCompactionUsage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("headless auto-compaction failed")
    }
}

/// Preparation may have completed paid compaction requests before refusing
/// another request. Settle that observed accounting without creating a model
/// prompt turn or connecting its tools/MCP servers.
async fn settle_headless_preparation_failure(
    session: &Session,
    cfg: &Config,
    cli: &Cli,
    workspace: &crate::paths::WorkspaceBinding,
    failure: anyhow::Error,
) -> anyhow::Error {
    let persistence = if cli.no_session {
        Ok(())
    } else {
        session::storage::save_session(session)
    };
    if cli.output_format() != OutputFormat::Text {
        let usage = failure
            .downcast_ref::<HeadlessCompactionUsage>()
            .map_or(Default::default(), |observed| observed.0);
        let reason = if failure
            .downcast_ref::<crate::spend::run::BudgetStopped>()
            .is_some()
        {
            crate::print::HeadlessStopReason::Budget
        } else {
            crate::print::HeadlessStopReason::Failed
        };
        let rendered = crate::print::render_headless_json(
            workspace.root(),
            &failure.to_string(),
            &[],
            usage.into(),
            Vec::new(),
            crate::print::HeadlessPricing::from_session(session, cfg),
            reason,
            #[cfg(feature = "goal")]
            None,
        );
        let emitted = match rendered {
            Ok(rendered) => {
                crate::print::emit_headless_result(
                    &rendered,
                    cli.output_format(),
                    Some((&session.id, &session.name)),
                    !cli.no_session && persistence.is_ok(),
                )
                .await
            }
            Err(error) => Err(error.into()),
        };
        if let Err(error) = emitted {
            return failure.context(format!("preparation accounting output failed: {error}"));
        }
    }
    match persistence {
        Ok(()) => failure,
        Err(error) => failure.context(format!(
            "preparation accounting could not be saved: {error}"
        )),
    }
}

async fn compact_headless_session_if_needed(
    session: &mut Session,
    client: &AnyClient,
    cfg: &Config,
    context: &ContextFiles,
    pending_tokens: u64,
    instructions: Option<&str>,
) -> anyhow::Result<crate::event::UsageDelta> {
    #[cfg(not(feature = "memory"))]
    let _ = context;
    let spend =
        crate::spend::run::Run::for_session(session, cfg, crate::spend::RequestKind::Compaction);
    let observed = crate::spend::run::UsageObserver::with_run(spend.clone());
    let request_observer = observed.clone();
    let compacted = compact_headless_session_with(
        session,
        cfg,
        pending_tokens,
        #[cfg(feature = "memory")]
        context.memory.as_deref(),
        |model, messages, previous_summary, input_token_budget, response_token_budget| async move {
            client
                .compress_messages_observed(
                    &model,
                    &messages,
                    previous_summary.as_deref(),
                    instructions,
                    input_token_budget,
                    response_token_budget,
                    &cfg.retry,
                    request_observer,
                )
                .await
        },
    )
    .await;
    let compacted = match compacted {
        Ok(compacted) => compacted,
        Err(error) => {
            let usage = cfg.normalize_usage(&session.provider, observed.take());
            session.charge_usage_delta(usage, cfg.is_anthropic_native(&session.provider));
            let failure = if spend.was_refused() {
                crate::spend::run::BudgetStopped.into()
            } else {
                error
            };
            return Err(failure.context(HeadlessCompactionUsage(usage)));
        }
    };

    #[cfg(feature = "memory")]
    if let Some(compacted) = &compacted {
        crate::extras::memory::flush_compaction_summary(
            &crate::extras::memory::Mem::open(),
            &compacted.summary,
            Some(compacted.first_kept_index),
        );
    }
    Ok(compacted
        .map(|compacted| compacted.usage)
        .unwrap_or_default())
}

/// A completed headless compaction: the summary, where the kept history now
/// starts, and the summarizer usage already charged to the session
/// (normalized for the session's provider).
#[derive(Debug)]
struct HeadlessCompaction {
    // Read by the memory flush and by tests.
    #[cfg_attr(not(feature = "memory"), allow(dead_code))]
    summary: String,
    #[cfg_attr(not(feature = "memory"), allow(dead_code))]
    first_kept_index: usize,
    usage: crate::event::UsageDelta,
}

async fn compact_headless_session_with<S, F>(
    session: &mut Session,
    cfg: &Config,
    pending_tokens: u64,
    #[cfg(feature = "memory")] memory: Option<&str>,
    summarize: S,
) -> anyhow::Result<Option<HeadlessCompaction>>
where
    S: FnOnce(String, Vec<crate::session::SessionMessage>, Option<String>, u64, u64) -> F,
    F: std::future::Future<Output = anyhow::Result<provider::CompactionOutput>>,
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
    let (skipped, messages) =
        session.compaction_input(plan.cut_idx, cfg.resolve_keep_recent_tool_results());
    let previous_summary = session
        .compactions
        .last()
        .map(|compaction| compaction.summary.to_string());
    let provider::CompactionOutput {
        summary,
        summary_byte_budget,
        messages_included,
        usage,
    } = summarize(
        model,
        messages,
        previous_summary,
        plan.input_token_budget,
        plan.response_token_budget,
    )
    .await?;
    // Every rolling summarizer request ran on the session's model, so it is
    // charged at the session's prices like any other completion
    // (mini-agent-i6q98).
    let usage = cfg.normalize_usage(&session.provider, usage);
    session.charge_usage_delta(usage, cfg.is_anthropic_native(&session.provider));
    // `messages_included` is the length of the oldest prefix of the cut slice
    // whose content the summarizer saw (`cut_idx` with full coverage). Drain
    // exactly that prefix so unsummarized history is never discarded.
    let first_kept_index =
        skipped + Session::compaction_drain_len(plan.cut_idx - skipped, messages_included)?;
    let tokens_before = if first_kept_index == plan.cut_idx {
        plan.tokens_before
    } else {
        session.messages[..first_kept_index]
            .iter()
            .map(|message| message.estimated_tokens)
            .sum()
    };
    session.compress_with_budget(
        summary.clone(),
        first_kept_index,
        tokens_before,
        summary_byte_budget,
    );
    Ok(Some(HeadlessCompaction {
        summary,
        first_kept_index,
        usage,
    }))
}

fn reap_aborted_openrouter_pricing_refresh(
    handle: tokio::task::JoinHandle<anyhow::Result<OpenRouterPricingMap>>,
) {
    #[cfg(test)]
    struct ReaperPermit;
    #[cfg(test)]
    impl Drop for ReaperPermit {
        fn drop(&mut self) {
            ACTIVE_OPENROUTER_PRICING_REAPERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    #[cfg(test)]
    let permit = {
        ACTIVE_OPENROUTER_PRICING_REAPERS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ReaperPermit
    };
    tokio::spawn(async move {
        #[cfg(test)]
        let _permit = permit;
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
    if refresh.need_pricing && session.known_input_output_prices().is_none() {
        session.set_input_output_prices(info.known_prices());
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
        mut cli: Cli,
        mut cfg: Config,
        paths: AppPaths,
        workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
        is_first_startup: bool,
        version_changed: bool,
        is_interactive: bool,
    ) -> anyhow::Result<Self> {
        crate::output_schema::prepare(&mut cli)?;
        validate_headless_prompt(&cli, workspace.root()).await?;
        use std::io::IsTerminal as _;
        let read_stdin = cli.print
            && !cli.is_loop_mode()
            && !cli.message.join(" ").trim_start().starts_with('!')
            && !std::io::stdin().is_terminal();
        #[cfg(feature = "acp")]
        let read_stdin = read_stdin && !cli.acp_enabled;
        if read_stdin {
            let stdin = match crate::print::read_headless_stdin(cli.quiet).await {
                Ok(stdin) => stdin,
                Err(error) => {
                    crate::print::emit_headless_preflight_failure(
                        &cli,
                        workspace.root(),
                        &error.to_string(),
                    )
                    .await?;
                    return Err(error);
                }
            };
            cli.message = vec![crate::print::append_stdin_reference(
                &cli.message.join(" "),
                &stdin,
            )];
        }

        let max_budget_usd = cli.resolve_max_budget_usd(&cfg)?;
        cfg.max_budget_usd = max_budget_usd;
        validate_startup_permission_policy(&cli, &cfg)?;

        // Load context first so prompts/themes are available early.
        let no_context_files = cli.resolve_no_context_files(&cfg);
        #[cfg_attr(not(feature = "memory"), allow(unused_mut))]
        let mut context =
            context::load(no_context_files).for_workspace_binding(no_context_files, &workspace);

        // --quick-model overrides provider + model
        let (mut provider, mut model) = cli.resolve_provider_and_model(&cfg);

        let name = cli.name.as_deref().unwrap_or("");
        let qm_map = config::quick_models_map(&cfg);
        let mut session = Session::new(
            &provider,
            &model,
            cfg.resolve_context_window(&provider, &model, &qm_map),
            name,
        );

        // Selected aliases retain their configured prices; raw ids share
        // the same live/catalog/quick fallback as `/model`.
        let costs = resolve_startup_model_pricing(
            &cli,
            &cfg,
            &provider,
            &model,
            std::env::var("OPENROUTER_MODEL").ok().as_deref(),
        );
        session.set_input_output_prices(costs);
        session.set_cache_prices(resolve_startup_cache_prices(
            &cli,
            &cfg,
            &provider,
            &model,
            std::env::var("OPENROUTER_MODEL").ok().as_deref(),
            session.input_token_cost,
        ));

        let mut session_resumed = false;

        if let Some(selection) = cli.resume_selection.take() {
            anyhow::ensure!(
                std::sync::Arc::ptr_eq(&selection.workspace, &workspace),
                "resume workspace binding changed"
            );
            session = session::catalogue::selected(&selection.row, &workspace, selection.all)?;
            session_resumed = true;
        }

        if cli.continue_session && cli.session.is_none() {
            session = require_recent_session(
                session::storage::find_recent_sessions_for_workspace(1, workspace.root()),
                workspace.root(),
            )?;
            session_resumed = true;
        }

        if let Some(session_id) = &cli.session {
            let candidates = session::storage::find_sessions_by_prefix(session_id)?;
            match session::storage::select_session(session_id, candidates) {
                session::storage::SessionSelection::One(selected) => {
                    session = *selected;
                    session_resumed = true;
                }
                session::storage::SessionSelection::NoMatch => {
                    anyhow::bail!("no session matching '{}'", session_id);
                }
                session::storage::SessionSelection::Ambiguous(sessions) => {
                    eprintln!("multiple sessions match '{}':", session_id);
                    for s in &sessions {
                        eprintln!("{}", crate::print::session_list_line(s));
                    }
                    anyhow::bail!("be more specific with the session ID prefix");
                }
            }
        }

        #[cfg(feature = "goal")]
        if cli.loaded_output_schema.is_some() && session.goal_store.is_live() {
            let error = anyhow::anyhow!("--output-schema cannot resume an active goal");
            crate::print::emit_headless_preflight_failure(
                &cli,
                workspace.root(),
                &error.to_string(),
            )
            .await?;
            return Err(error);
        }

        let fork_source = cli.fork_session.then(|| session.id.clone());
        if session_resumed {
            session = session::catalogue::for_workspace(session, &workspace)?;
        }

        if let Some(original) = fork_source {
            anyhow::ensure!(session_resumed, "--fork-session requires a saved session");
            // A cross-workspace selection has already created its fresh fork.
            if session.id == original {
                session = session::lock::fork_at_turn(&session, None)?;
            }
            eprintln!("notice: forked session {original} into {}", session.id);
        }

        // Another live process that already owns this session keeps writing
        // it; continuing in a fork keeps both histories (mini-agent-e42za).
        // An ephemeral run never saves, so it needs no ownership.
        if session_resumed && !cli.no_session {
            let resumed = session::lock::claim_or_fork(session)?;
            session = resumed.session;
            if let Some(notice) = resumed.notice {
                eprintln!("notice: {notice}");
            }
        }

        if session_resumed {
            let prices = crate::pricing::cache_prices_for_model(
                &session.provider,
                &session.model,
                &cfg,
                None,
                session.input_token_cost,
            );
            session.cache_read_token_cost = session.cache_read_token_cost.or(prices.read);
            session.cache_write_token_cost = session.cache_write_token_cost.or(prices.write);
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

        session.initialize_spend_account(max_budget_usd)?;
        let reconciliation_scope = crate::agent::runner::AgentWorkScope::new();
        let reconciliation_cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(
            std::sync::Arc::clone(&reconciliation_scope),
        );
        let account = session.spend_account.clone();
        let session_id = session.id.to_string();
        let ledger = account.runtime_ledger(std::sync::Arc::new(crate::spend::Ledger::new(
            &paths.state_dir,
        )));
        if let Err(error) = reconciliation_scope
            .run(async move {
                crate::agent::runner::spawn_blocking_scoped(move || {
                    let result = ledger
                        .summary_for_session(&session_id)
                        .and_then(|summary| account.reconcile(max_budget_usd, &summary.session));
                    if result.is_err() {
                        account.tracking_failed();
                    }
                    if max_budget_usd.is_none() {
                        Ok(())
                    } else {
                        result
                    }
                })
                .await
            })
            .await
            .unwrap_or_else(|error| Err(error.into()))
        {
            if max_budget_usd.is_none() {
                return Err(error);
            }
            let error = anyhow::Error::from(crate::spend::run::BudgetStopped)
                .context("cost cap cannot safely initialize private usage tracking");
            if cli.print {
                crate::print::emit_headless_budget_failure(
                    &cli,
                    workspace.root(),
                    &error.to_string(),
                )
                .await?;
            }
            return Err(error);
        }

        reconciliation_cleanup.settle().await;

        // A resumed session persisted its context_window when first saved, which can
        // be stale if the model's catalog entry has changed since (e.g. a model that
        // grew from 128k to 1M) or the user has since pinned one. Re-derive it for
        // the session's own model in the same order a new session uses — pinned
        // `context_window`, then the quick model's, then the catalog's — and keep
        // the saved value only when none of them knows the model (mini-agent-ycu8b).
        if let Some(cw) =
            cfg.known_context_window(session.provider.as_str(), session.model.as_str(), &qm_map)
        {
            session.update_context_window(cw);
        }

        // A resumed session replays its compaction summary as a recap; the
        // memory block must not carry the daily log's copy of it as well
        // (mini-agent-vx0yg).
        #[cfg(feature = "memory")]
        if let Some(reference) = session.active_compaction_ref() {
            context.memory =
                crate::extras::memory::Mem::open().context_block_excluding(Some(reference));
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

        // Capture strict native startup ownership before capability probes,
        // SessionStart hooks or agent/MCP prebuild can execute. ACP selects its
        // own canonical editor Session later and establishes its barrier there.
        #[cfg(feature = "acp")]
        let editor_session = cli.acp_enabled;
        #[cfg(not(feature = "acp"))]
        let editor_session = false;
        let adopted_native_frontend = cli.print && !cli.is_loop_mode();
        anyhow::ensure!(
            editor_session
                || adopted_native_frontend
                || !crate::session::native_publication::is_native_bearing(&session),
            "this saved native context requires the headless print frontend; interactive and automatic-round replay are not available yet"
        );
        let mut native_startup = if !editor_session
            && adopted_native_frontend
            && crate::session::native_publication::startup_requires_publication(
                &session,
                matches!(&client, AnyClient::Anthropic(_)),
                cli.no_session,
            )? {
            Some(
                crate::session::native_publication::NativeStartupPublication::begin(&mut session)
                    .await?,
            )
        } else {
            None
        };

        #[cfg(feature = "goal")]
        apply_goal_flags(&cli, &cfg, &session)?;

        // Rename only the selected (possibly forked) session after setup has
        // succeeded. Ephemeral invocations keep the rename in memory only.
        if session_resumed {
            if let Some(name) = cli.name.as_deref() {
                session.name = CompactString::new(name);
            }
            if !cli.no_session
                && native_startup.is_none()
                && (cli.name.is_some() || cli.fork_session)
            {
                session::storage::save_session(&session)?;
            }
        }
        // Goal/name changes belong to this exact startup actor value. They
        // remain InFlight; a metadata mutation supplies no completed model.
        if let Some(owner) = &mut native_startup {
            owner.save_progress(&mut session).await?;
        }

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
            native_startup,
            native_ticket: None,
            native_headless: None,
            #[cfg(feature = "hooks")]
            native_session_end_done: false,
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
            #[cfg(feature = "hooks")]
            session_start_task: None,
        })
    }

    /// Validate the common process sandbox contract before entering any
    /// execution surface, including ACP which intentionally skips feature
    /// initialization. Windows is always fail-closed while sandboxing is on;
    /// other platforms retain the legacy warning only for an entirely
    /// inherited default.
    pub(crate) fn validate_sandbox_availability(&self) -> anyhow::Result<()> {
        configured_sandbox_validation_probe(&self.cli, &self.cfg)()
    }

    /// Resolve the startup containment gates after the workspace has been captured. On Windows,
    /// the regular AppContainer and LPAC worker preflights are independent. Starting both before
    /// joining avoids adding their cold-start latencies while their separate process-local caches,
    /// process-creation lock, and fail-closed consumers retain their existing contracts.
    pub(crate) fn preflight_startup_capabilities(&self) -> anyhow::Result<()> {
        #[cfg(all(target_os = "windows", feature = "js"))]
        if self.cli.tool_is_eligible(&self.cfg, "js") {
            let (general, _worker_status) = run_startup_probes_concurrently(
                configured_sandbox_validation_probe(&self.cli, &self.cfg),
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
    #[cfg(test)]
    pub(crate) fn fail_native_final_before_replace(&mut self) {
        self.native_startup
            .as_mut()
            .unwrap()
            .fail_final_before_replace();
    }

    pub(crate) async fn init_features(&mut self) -> anyhow::Result<()> {
        #[cfg(feature = "subagents")]
        crate::extras::subagents::install(crate::extras::subagents::resolve_config(
            &self.cfg,
            self.cli.api_key.as_deref(),
            &self.provider,
            &self.model,
            &self.client,
        ));

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

        let (permission, ask_tx, ask_rx) =
            crate::permission::build_interactive_permission_with_cli_at(
                &self.cli,
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

            let (advisor_provider, advisor_model) = crate::extras::advisor::resolve_model(
                &advisor_model_name,
                &self.provider,
                &self.cfg,
            );
            let advisor_client = match crate::extras::advisor::model_client(
                &advisor_provider,
                &self.provider,
                &self.client,
                &self.cfg,
                self.cli.api_key.as_deref(),
            ) {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::warn!(
                        "Could not create advisor client for provider '{}' ({}); advisor model unavailable. Set `advisor.model` and API key in config.",
                        advisor_provider,
                        error
                    );
                    None
                }
            };

            let mut config = crate::extras::advisor::AdvisorToolConfig {
                client: advisor_client,
                advisor_provider,
                advisor_model,
                human_handoff,
                max_uses,
                handoff_tx: None,
                enabled,
                kilobytes_limit,
                request_config: std::sync::Arc::new(self.cfg.clone()),
            };
            self.handoff_rx = config.prepare_handoff_channel(self.is_interactive);
            crate::extras::advisor::init_config(config);
        }

        Ok(())
    }

    pub(crate) fn start_openrouter_pricing_refresh(&mut self) {
        if self.provider != "openrouter" {
            return;
        }
        let need_pricing = self.session.known_input_output_prices().is_none();
        let need_context = needs_openrouter_context_refresh(&self.cfg, self.model.as_str());
        if !need_pricing && !need_context {
            return;
        }

        let client = self.client.clone();
        let provider = self.provider.to_string();
        let is_custom = self.cfg.custom_providers_map().contains_key(&provider);
        self.openrouter_pricing_refresh = Some(OpenRouterPricingRefresh::start(
            self.model.clone(),
            need_pricing,
            need_context,
            self.session.context_window,
            async move {
                let models = crate::ui::slash::providers::fetch_models_cached(
                    &provider, is_custom, &client, true,
                )
                .await?;
                Ok(models
                    .iter()
                    .map(|m| {
                        (
                            m.id.clone(),
                            provider::OpenRouterModelInfo {
                                input_cost: m.input_price.unwrap_or(0.0),
                                output_cost: m.output_price.unwrap_or(0.0),
                                context_length: m.context_length.map(u64::from),
                                prices_known: Some(m.input_price.zip(m.output_price).is_some()),
                            },
                        )
                    })
                    .collect())
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

        // `SessionStart` begins once `session_resumed` is known. Interactive
        // startup joins it only after the independent agent/MCP prebuild has
        // started, so slow hooks do not serialize those cold-start paths.
        #[cfg(feature = "hooks")]
        {
            crate::extras::hooks::set_active_session(&self.session.id);
            let source = if self.session_resumed {
                "resume"
            } else {
                "startup"
            }
            .to_string();
            let hook = heap_future(|| async move {
                crate::extras::hooks::dispatch_session_start(&source).await;
            });
            self.session_start_task = Some(match &self.native_startup {
                Some(owner) => owner.spawn_hook(hook).await,
                None => tokio::spawn(hook),
            });
        }

        // ARCHITECTURE.md prompt. It reads a line from stdin, so like the
        // version prompts it is only offered when a person is at the terminal:
        // a `-p`, `--goal` or `--loop` run would otherwise stop on it, consume
        // piped input meant for the run, or record the directory as asked
        // without anyone having answered.
        #[cfg(feature = "archmd")]
        let arch_created = if self.is_interactive && !self.cli.resolve_no_context_files(&self.cfg) {
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
            let default_prompt = self
                .cfg
                .default_prompt
                .as_deref()
                .unwrap_or("code")
                .to_string();
            if self.context.activate_prompt(&default_prompt).is_some() {
                let mut prompt_text = self.context.current_prompt.take().unwrap_or_default();

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

                if !self.session_resumed {
                    apply_startup_prompt_model(
                        &default_prompt,
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
            if self.context.activate_prompt(name).is_some() {
                let mut prompt_text = self.context.current_prompt.take().unwrap_or_default();

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
            if let Some(name) = &self.context.current_prompt_name {
                guard.set_prompt_grant_offer(crate::context::prompts::prompt_grant_offer(
                    &self.context.prompts,
                    name,
                    &self.paths,
                ));
            }
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

    async fn join_native_startup(&mut self) -> anyhow::Result<()> {
        if let Some(mut owner) = self.native_startup.take() {
            owner.save_progress(&mut self.session).await?;
            self.native_ticket = Some(owner.into_model_ticket(&self.session).await?);
        }
        Ok(())
    }

    /// Phase 4: mode dispatch — print, loop, or interactive.
    /// Each mode future is built on the heap by [`heap_future`], so neither its
    /// state nor its construction is reserved in this frame, which stays on the
    /// stack for the whole run.
    pub(crate) async fn dispatch(mut self: Box<Self>) -> anyhow::Result<()> {
        #[cfg(feature = "hooks")]
        crate::extras::hooks::set_active_workspace(self.workspace.root());
        if self.resume_override_pending {
            // All fallible startup validation has completed. Persist the
            // identity/audit update atomically before any agent can receive
            // saved history.
            if let Some(owner) = &mut self.native_startup {
                owner.save_progress(&mut self.session).await?;
            } else {
                session::storage::save_session(&self.session)?;
            }
            self.resume_override_pending = false;
        }
        if self.cli.print {
            #[cfg(feature = "hooks")]
            if let Some(task) = self.session_start_task.take() {
                let joined = task.await;
                if let Some(owner) = &self.native_startup {
                    owner.hook_retired(joined)?;
                }
            }
            self.join_native_startup().await?;
            let result = heap_future(|| self.dispatch_print()).await;
            // Every headless exit ends the session: a failed turn, a
            // persistence failure and a goal's own exit code alike
            // (mini-agent-rnpjm).
            #[cfg(feature = "hooks")]
            if !self.native_session_end_done {
                crate::extras::hooks::dispatch_session_end("exit").await;
            }
            result
        } else {
            #[cfg(feature = "loop")]
            if self.cli.loop_mode {
                #[cfg(feature = "hooks")]
                if let Some(task) = self.session_start_task.take() {
                    let joined = task.await;
                    if let Some(owner) = &self.native_startup {
                        owner.hook_retired(joined)?;
                    }
                }
                self.join_native_startup().await?;
                return heap_future(|| self.dispatch_loop()).await;
            }

            heap_future(|| self.dispatch_interactive()).await
        }
    }

    async fn dispatch_print(&mut self) -> anyhow::Result<()> {
        let cli = self.cli.clone();
        let root = self.workspace.root().to_path_buf();
        let identity = (self.session.id.to_string(), self.session.name.to_string());
        let result = heap_future(|| self.dispatch_print_inner()).await;
        if let Err(error) = &result
            && cli.output_format() == OutputFormat::StreamJson
            && !crate::print::headless_result_emitted()
        {
            crate::print::emit_headless_failure(
                &cli,
                &root,
                &error.to_string(),
                Some((&identity.0, &identity.1)),
            )
            .await?;
        }
        result
    }

    async fn dispatch_print_inner(&mut self) -> anyhow::Result<()> {
        let mut msg = self.cli.message.join(" ").trim().to_string();
        let schema = self.cli.loaded_output_schema.clone();
        if let Some(schema) = &schema {
            msg.push_str("\n\n");
            msg.push_str(&schema.instructions());
        }
        let output = crate::print::HeadlessOutputPolicy::from_cli(&self.cli);
        let json_output = output.format != OutputFormat::Text;
        let identity = (self.session.id.to_string(), self.session.name.to_string());
        output.emit_event(serde_json::json!({"type":"system", "session_id":identity.0, "session_name":identity.1, "ephemeral":self.cli.no_session, "provider":self.session.provider, "model":self.session.model})).await?;
        let change_baseline = if json_output {
            crate::print::capture_workspace_change_baseline(self.workspace.root()).await
        } else {
            None
        };
        if msg.starts_with('!') {
            if msg
                .strip_prefix('!')
                .is_some_and(|command| !command.trim().is_empty())
            {
                let run = crate::print::run_headless_command(
                    self.sandbox
                        .clone()
                        .run_explicit_shell(&msg, DEFAULT_COMMAND_LIMITS, None),
                )
                .await?;
                let result = run.rendered_output();
                let json_context = if json_output {
                    let files_changed = crate::print::files_changed_since(
                        self.workspace.root(),
                        change_baseline.as_ref(),
                        &[],
                    )
                    .await;
                    Some((
                        files_changed,
                        crate::print::HeadlessPricing::from_session(&self.session, &self.cfg),
                    ))
                } else {
                    None
                };
                let mut persistence_failure = None;
                if !self.cli.no_session {
                    let session = &mut self.session;
                    session.add_shell_interaction(&msg, &result);
                    if let Err(error) = session::storage::save_session(session) {
                        persistence_failure = Some(error);
                    } else if let Err(e) = session::chat_history::append_entry(
                        &session::chat_history::ChatHistoryEntry {
                            content: msg,
                            timestamp: session.updated_at.clone(),
                        },
                    ) {
                        eprintln!("warning: failed to append chat history entry: {}", e);
                    }
                }
                if let Some((files_changed, pricing)) = json_context {
                    let stop_reason = if run.succeeded() && persistence_failure.is_none() {
                        crate::print::HeadlessStopReason::Completed
                    } else {
                        crate::print::HeadlessStopReason::Failed
                    };
                    crate::print::emit_headless_result(
                        &crate::print::render_headless_json(
                            self.workspace.root(),
                            &result,
                            &[],
                            rig::completion::Usage::default(),
                            files_changed,
                            pricing,
                            stop_reason,
                            #[cfg(feature = "goal")]
                            None,
                        )?,
                        output.format,
                        Some((&identity.0, &identity.1)),
                        !self.cli.no_session && persistence_failure.is_none(),
                    )
                    .await?;
                }
                if !json_output {
                    crate::print::headless_stdout_async(format!("{result}\n")).await?;
                }
                if !run.succeeded() {
                    let failure = anyhow::anyhow!("explicit shell command failed");
                    return Err(match persistence_failure {
                        Some(error) => failure.context(format!(
                            "the command result could not be persisted either: {error}"
                        )),
                        None => failure,
                    });
                }
                if let Some(error) = persistence_failure {
                    return Err(error);
                }
            } else {
                anyhow::bail!("empty command after '!'");
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
            // Already charged to the session; kept to report in the JSON
            // usage and cost and to count toward a goal's first round.
            let admission = crate::spend::run::Run::for_session(
                &mut self.session,
                &self.cfg,
                crate::spend::RequestKind::Turn,
            )
            .admit();
            let prepared = match admission {
                Ok(()) => {
                    compact_headless_session_if_needed(
                        &mut self.session,
                        &self.client,
                        &self.cfg,
                        &self.context,
                        pending_tokens,
                        self.cli.compact_instructions.as_deref(),
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let compaction_usage = match prepared {
                Ok(usage) => usage,
                Err(error) => {
                    return Err(heap_future(|| {
                        settle_headless_preparation_failure(
                            &self.session,
                            &self.cfg,
                            &self.cli,
                            &self.workspace,
                            error,
                        )
                    })
                    .await);
                }
            };
            let temperature = config::resolve_temperature(&self.cli, &self.cfg, &self.model);
            let extra_body = provider::merge_extra_body(
                config::resolve_extra_body(&self.cfg, &self.model),
                schema.as_ref().and_then(|schema| {
                    schema.native_params(&self.session.provider, &self.model, &self.cfg)
                }),
            );
            let completion_model = self.client.completion_model_with_cache_ttl(
                self.model.to_string(),
                self.cfg.cache_ttl.unwrap_or_default(),
            );
            let read_tracker = self.session.read_tracker.clone();
            let todo_store = self.session.todos.clone();
            #[cfg(feature = "goal")]
            let goal_store = self.session.goal_store.clone();
            #[cfg(feature = "skills")]
            let skill_service_owner_for_goal =
                std::sync::Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new());
            #[cfg(feature = "mcp")]
            let mut mcp_manager = if !self.cli.mcp_is_eligible(&self.cfg) {
                None
            } else {
                connect_headless_mcp(&self.cfg, &self.workspace).await
            };
            // Boxed: building an agent pulls in every provider's setup, and this
            // future is inlined into the startup frame otherwise.
            let agent = heap_future(|| {
                provider::build_agent_in_workspace(
                    completion_model,
                    &self.cli,
                    &self.cfg,
                    &self.context,
                    self.workspace.clone(),
                    self.permission.clone(),
                    // Non-interactive dispatch never keeps the ask channel: with
                    // no one draining it, an `Ask` verdict must fail closed as a
                    // denial rather than block forever. See `handle_ask_inner`.
                    None,
                    self.sandbox.clone(),
                    read_tracker,
                    todo_store,
                    #[cfg(feature = "goal")]
                    goal_store,
                    &self.session.id,
                    Some(self.session.tool_result_spills.clone()),
                    true,
                    temperature,
                    extra_body,
                    #[cfg(feature = "js")]
                    self.session.js_session_state.clone(),
                    #[cfg(feature = "skills")]
                    skill_service_owner_for_goal.clone(),
                    #[cfg(feature = "mcp")]
                    mcp_manager.as_ref(),
                )
            })
            .await;
            #[cfg(feature = "mcp")]
            report_headless_mcp_notices(mcp_manager.as_mut(), |notice| eprintln!("{notice}"));
            if let Some(ss) = self.status_signals.as_ref() {
                ss.send_start();
            }
            let history = crate::agent::runner::convert_history_shared_with_tool_result_retention(
                &self.session,
                self.cfg.resolve_keep_recent_tool_results(),
            );
            // Goal verdicts are skill evidence. The recorder lives behind the
            // provider, so the driver installs it rather than building one.
            #[cfg(all(feature = "goal", feature = "skills"))]
            crate::extras::goal::driver::set_outcome_recorder(
                provider::goal_outcome_recorder(
                    &self.cli,
                    &self.cfg,
                    &skill_service_owner_for_goal,
                    &self.workspace,
                )
                .await,
            );
            // Construct the child in heap_future's short frame; Box::pin alone
            // still reserves its temporary in this parent poll frame. The
            // regression consumer uses the unchanged 1 MiB default stack.
            let response_result = heap_future(|| {
                run_headless_goal_rounds(
                    &agent,
                    &mut self.session,
                    &self.cli,
                    &self.cfg,
                    &self.sandbox,
                    &self.client,
                    &msg,
                    history,
                    json_output,
                    compaction_usage,
                    &mut self.native_ticket,
                    &mut self.native_headless,
                    #[cfg(feature = "hooks")]
                    false,
                )
            })
            .await;
            let mut response_result = response_result;
            // Schema HTTP runs before the measured large final persistence
            // frame is polled; effects, accounting and repair order are unchanged.
            let schema_outcome = heap_future(|| {
                self.prepare_print_schema(&schema, &mut response_result, output, compaction_usage)
            })
            .await;
            heap_future(|| {
                self.finish_print_model(
                    msg,
                    schema,
                    output,
                    identity,
                    change_baseline,
                    compaction_usage,
                    response_result,
                    schema_outcome,
                )
            })
            .await?;
        }

        Ok(())
    }

    async fn prepare_print_schema(
        &mut self,
        schema: &Option<std::sync::Arc<crate::output_schema::OutputSchema>>,
        response_result: &mut HeadlessRounds,
        output: crate::print::HeadlessOutputPolicy,
        compaction_usage: crate::event::UsageDelta,
    ) -> crate::output_schema::SchemaOutcome {
        if let Some(schema) = schema
            && self.native_headless.is_some()
        {
            // The primary native actor is already canonically committed.
            // A tool-free schema repair changes its signed prefix and needs
            // a separate explicitly owned context, not a generic replay.
            match schema.validate(&response_result.turn.response) {
                Ok(value) => crate::output_schema::SchemaOutcome {
                    result_json: Some(value),
                    exhausted: false,
                },
                Err(_) => {
                    if response_result.turn.failure.is_none() {
                        response_result.turn.failure = Some(anyhow::anyhow!(
                            "the answer did not satisfy the output schema; automatic repair requires a fresh native context"
                        ));
                    }
                    crate::output_schema::SchemaOutcome::default()
                }
            }
        } else if let Some(schema) = schema {
            heap_future(|| {
                crate::output_schema::repair_rounds(
                    response_result,
                    schema,
                    &self.client,
                    &self.cli,
                    &self.cfg,
                    &mut self.session,
                    output,
                    compaction_usage,
                )
            })
            .await
        } else {
            crate::output_schema::SchemaOutcome::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_print_model(
        &mut self,
        msg: String,
        schema: Option<std::sync::Arc<crate::output_schema::OutputSchema>>,
        output: crate::print::HeadlessOutputPolicy,
        identity: (String, String),
        change_baseline: Option<crate::print::WorkspaceChangeBaseline>,
        compaction_usage: crate::event::UsageDelta,
        response_result: HeadlessRounds,
        schema_outcome: crate::output_schema::SchemaOutcome,
    ) -> anyhow::Result<()> {
        let json_output = output.format != OutputFormat::Text;
        if let Some(ss) = self.status_signals.as_ref() {
            ss.send_stop();
        }
        // A write, edit or command may already have run before a terminal
        // provider or budget failure. Persist that progress and the
        // incurred usage before propagating the failure, so a resumed
        // session has a durable record of what happened instead of
        // repeating the effects.
        let HeadlessRounds {
            turn:
                crate::agent::runner::HeadlessTurn {
                    response,
                    usage,
                    interactions,
                    mut failure,
                },
            prompt: recorded_prompt,
            persisted: already_persisted,
        } = response_result;
        // Priced and charged in one shape whatever the provider.
        let usage: rig::completion::Usage = self
            .cfg
            .normalize_usage(&self.session.provider, usage.into())
            .into();
        // The JSON reports the turn's usage together with the
        // auto-compaction that ran before it (mini-agent-i6q98). The
        // session was charged for that already, so it is added to the
        // reported figure only, never to `usage`, which is charged below.
        let json_usage: rig::completion::Usage = crate::event::UsageDelta::from(usage)
            .saturating_add(compaction_usage)
            .into();
        let json_context = if json_output {
            let files_changed = crate::print::files_changed_since(
                self.workspace.root(),
                change_baseline.as_ref(),
                &interactions,
            )
            .await;
            Some((
                files_changed,
                crate::print::HeadlessPricing::from_session(&self.session, &self.cfg),
            ))
        } else {
            None
        };
        // Snapshot before the session moves into the persistence block.
        #[cfg(feature = "goal")]
        let finished_goal = self.session.goal_store.snapshot();
        let pending_question = failure
            .as_ref()
            .and_then(|error| error.downcast_ref::<crate::question::QuestionRequired>())
            .cloned();
        if let Some(question) = &pending_question {
            self.session.pending_question = Some(question.clone());
        }
        let mut persistence_failure = None;
        let mut title_accounting = None;
        if !self.cli.no_session {
            let session = &mut self.session;
            // Prompt, then tool calls/results in provider order, then the
            // assistant message: the same record order the interactive UI
            // writes, so `--continue` replays the turn in sequence.
            //
            // A goal run has already written this round, under the prompt
            // that round actually ran with. Writing it again would double
            // every record in it and charge its tokens twice.
            if !already_persisted {
                crate::print::persist_headless_turn(
                    session,
                    &recorded_prompt,
                    &response,
                    &interactions,
                );
                let anthropic_native = self.cfg.is_anthropic_native(&session.provider);
                session.charge_usage_delta(usage.into(), anthropic_native);
            }
            let saved = if let Some(owner) = &mut self.native_headless {
                owner.progress(session).await
            } else {
                session::storage::save_session(session)
            };
            if let Err(error) = saved {
                persistence_failure = Some(error);
            } else {
                // Persist the actual turn first; title failure cannot lose
                // history. This awaited no-tools request owns its work and
                // adds observed usage at its captured model prices.
                if failure.is_none() {
                    let cost_before = session.total_cost;
                    // This measured large title child must not add its
                    // construction copies to the primary request's poll.
                    let (title_usage, interrupted) = heap_future(|| {
                        session::title::generate_for_session(session, &self.cfg, &self.client)
                    })
                    .await;
                    if title_usage.has_values() {
                        title_accounting = Some(
                            serde_json::json!({"input_tokens":title_usage.input_tokens,"output_tokens":title_usage.output_tokens,"cached_input_tokens":title_usage.cached_input_tokens,"cache_creation_input_tokens":title_usage.cache_creation_input_tokens,"cost":session.total_cost-cost_before}),
                        );
                    }
                    if interrupted {
                        failure = Some(anyhow::anyhow!(crate::agent::runner::HEADLESS_INTERRUPTED));
                    }
                    let saved = if let Some(owner) = &mut self.native_headless {
                        owner.progress(session).await
                    } else {
                        session::storage::save_session(session)
                    };
                    if let Err(error) = saved {
                        persistence_failure = Some(error);
                    }
                }
                let _ =
                    session::chat_history::append_entry(&session::chat_history::ChatHistoryEntry {
                        content: msg,
                        timestamp: session.updated_at.clone(),
                    });
            }
        }
        if let Some(owner) = &mut self.native_headless {
            #[cfg(feature = "hooks")]
            {
                owner
                    .scope
                    .run(crate::extras::hooks::dispatch_session_end("exit"))
                    .await;
                self.native_session_end_done = true;
            }
            if let Err(error) = owner.finish(&mut self.session).await {
                persistence_failure = Some(error);
            }
        }
        if let Some((files_changed, pricing)) = json_context {
            let stop_reason = if pending_question.is_some() {
                crate::print::HeadlessStopReason::AwaitingUser
            } else if failure.as_ref().is_some_and(|error| {
                error
                    .downcast_ref::<crate::spend::run::BudgetStopped>()
                    .is_some()
            }) {
                crate::print::HeadlessStopReason::Budget
            } else if failure.is_some() || persistence_failure.is_some() {
                crate::print::HeadlessStopReason::Failed
            } else if schema_outcome.exhausted {
                crate::print::HeadlessStopReason::SchemaFailed
            } else {
                // A goal that stopped for a reason of its own reports that
                // reason, so a script can branch on the outcome.
                #[cfg(feature = "goal")]
                {
                    finished_goal
                        .as_ref()
                        .map_or(crate::print::HeadlessStopReason::Completed, |goal| {
                            crate::print::HeadlessStopReason::for_goal(goal.status)
                        })
                }
                #[cfg(not(feature = "goal"))]
                {
                    crate::print::HeadlessStopReason::Completed
                }
            };
            let rendered = crate::print::render_headless_json(
                self.workspace.root(),
                &response,
                &interactions,
                json_usage,
                files_changed,
                pricing,
                stop_reason,
                #[cfg(feature = "goal")]
                finished_goal.as_ref(),
            )?;
            let mut rendered: serde_json::Value = serde_json::from_str(&rendered)?;
            if let Some(question) = &pending_question {
                rendered["pending_question"] = serde_json::to_value(question)?;
            }
            if schema.is_some() {
                rendered["result_json"] = schema_outcome
                    .result_json
                    .unwrap_or(serde_json::Value::Null);
            }
            if let Some(accounting) = title_accounting {
                rendered["session_title_usage"] = accounting;
            }
            crate::print::emit_headless_result(
                &rendered.to_string(),
                output.format,
                Some((&identity.0, &identity.1)),
                !self.cli.no_session && persistence_failure.is_none(),
            )
            .await?;
        }
        if output.format == OutputFormat::Text && !output.streams_text() {
            crate::print::headless_stdout_async(format!("{response}\n")).await?;
        }
        // A goal that stopped for a reason of its own exits with that
        // reason's code, whatever the output format. The distinction
        // between "resume me" and "stop retrying" is what an unattended
        // caller acts on, and it does not depend on whether a human asked
        // for JSON.
        #[cfg(feature = "goal")]
        if failure.is_none() && persistence_failure.is_none() {
            let goal_stop = finished_goal
                .as_ref()
                .map(|goal| crate::print::HeadlessStopReason::for_goal(goal.status));
            if let Some(reason) = goal_stop
                && reason.exit_code() != 0
            {
                return Err(crate::print::HeadlessGoalExit(reason).into());
            }
        }
        // The turn's own failure wins: a partial persistence failure is
        // reported too, but neither is ever presented as success.
        if let Some(failure) = failure {
            if let Some(error) = persistence_failure {
                return Err(failure.context(format!(
                    "the partial turn could not be persisted either: {error}"
                )));
            }
            return Err(failure);
        }
        if let Some(error) = persistence_failure {
            return Err(error);
        }
        if schema_outcome.exhausted {
            anyhow::bail!("final answer did not satisfy output schema after bounded JSON repairs");
        }
        Ok(())
    }

    #[cfg(feature = "loop")]
    async fn dispatch_loop(self) -> anyhow::Result<()> {
        // The goal is installed before the agent is built: the preamble's goal
        // block is captured at build time, and an agent built without it never
        // learns it is working toward anything.
        if !crate::extras::r#loop::headless::install_loop_goal(&self.cli, &self.cfg, &self.session)
            .await?
        {
            return Ok(());
        }
        let model_completion = self.client.completion_model_with_cache_ttl(
            self.model.to_string(),
            self.cfg.cache_ttl.unwrap_or_default(),
        );
        let temperature = config::resolve_temperature(&self.cli, &self.cfg, &self.model);
        let extra_body = config::resolve_extra_body(&self.cfg, &self.model);
        let read_tracker = self.session.read_tracker.clone();
        let todo_store = self.session.todos.clone();
        #[cfg(feature = "goal")]
        let goal_store = self.session.goal_store.clone();
        #[cfg(feature = "mcp")]
        let mut mcp_manager = if !self.cli.mcp_is_eligible(&self.cfg) {
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
            todo_store,
            #[cfg(feature = "goal")]
            goal_store,
            &self.session.id,
            Some(self.session.tool_result_spills.clone()),
            true,
            temperature,
            extra_body,
            #[cfg(feature = "js")]
            self.session.js_session_state.clone(),
            #[cfg(feature = "skills")]
            std::sync::Arc::new(crate::extras::js::skills::session::SkillServiceOwner::new()),
            #[cfg(feature = "mcp")]
            mcp_manager.as_ref(),
        )
        .await;
        #[cfg(feature = "mcp")]
        report_headless_mcp_notices(mcp_manager.as_mut(), |notice| eprintln!("{notice}"));
        // Boxed: dispatch_loop's future is built on the stack before it is
        // pinned, so anything inlined into it is charged to the startup frame.
        let result = Box::pin(crate::extras::r#loop::headless::run_headless_loop(
            agent,
            &self.cli,
            &self.cfg,
            &self.context,
            &self.session,
            self.status_signals,
            &self.sandbox,
            &self.client,
        ))
        .await;
        #[cfg(feature = "hooks")]
        crate::extras::hooks::dispatch_session_end("exit").await;
        result
    }

    async fn dispatch_interactive(self) -> anyhow::Result<()> {
        let Startup {
            cli,
            mut cfg,
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
            paths,
            is_interactive,
            #[cfg(feature = "advisor")]
            handoff_rx,
            #[cfg(feature = "hooks")]
            session_start_task,
            native_startup,
            native_ticket,
            ..
        } = self;

        cfg.update_notice =
            crate::update::startup_notice(&cli, &cfg, is_interactive, &paths.state_dir).await;
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
            )
            .with_native_publication(native_startup, native_ticket),
            None,
            ask_rx,
            auto_trigger_msg,
            #[cfg(feature = "advisor")]
            handoff_rx,
            #[cfg(feature = "hooks")]
            session_start_task,
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

/// Startup-banner lines for a JavaScript runtime or learned-skill subsystem the
/// worker-containment preflight refused, carrying that preflight's reason
/// verbatim.
///
/// The gate can remove the `js` tool and the entire learned-skill subsystem
/// with no other visible sign — the tool is simply absent — so the banner is
/// the only surface an operator who never runs `/toggle` or `--print-config`
/// sees. A pending initial report adds no line because no preflight decision
/// has been made yet. A real refusal remains visible even if the other
/// subsystem is pending, available, or not compiled.
///
/// Kept pure and out of the renderer so the reason-propagation contract is
/// unit-testable without a terminal.
pub(crate) fn js_runtime_banner_lines(report: &provider::JsRuntimeReport) -> Vec<String> {
    use provider::RuntimeAvailability::Unavailable;
    fn actual_refusal(availability: &provider::RuntimeAvailability) -> Option<&str> {
        match availability {
            Unavailable { reason } if reason != provider::JS_RUNTIME_PROBE_PENDING_REASON => {
                Some(reason.as_str())
            }
            _ => None,
        }
    }
    match (
        actual_refusal(&report.javascript),
        actual_refusal(&report.learned_skills),
    ) {
        // The learned-skill refusal is derived from the JavaScript one
        // (`requires the contained JavaScript worker: <reason>`), so one line
        // names both subsystems and repeats the single underlying reason.
        (Some(reason), Some(_)) => vec![format!(
            "[!] JavaScript runtime and learned skills unavailable: {reason}"
        )],
        (Some(reason), _) => {
            vec![format!("[!] JavaScript runtime unavailable: {reason}")]
        }
        (_, Some(reason)) => vec![format!("[!] learned skills unavailable: {reason}")],
        _ => Vec::new(),
    }
}

/// Install the goal named by `--goal`, if any.
///
/// Refuses rather than silently discarding an unfinished goal from a resumed
/// session: its rounds, verdicts and reports are the record of work done so
/// far. `--goal-replace` is the explicit override.
#[cfg(feature = "goal")]
fn apply_goal_flags(cli: &Cli, cfg: &config::Config, session: &Session) -> anyhow::Result<()> {
    use crate::extras::goal::{ContinuationMode, DEFAULT_RESTART_SUMMARY_CHARS, Goal};

    // Without tools the agent can never file a report, so every round would
    // look like a stall. Say so at startup rather than after three wasted
    // rounds. This holds for a goal resumed from the session too: `--continue`
    // can carry a live goal into a run that has no way to advance it.
    let has_live_goal = session.goal_store.is_live();
    let Some(objective) = cli
        .goal_args
        .goal
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    else {
        if has_live_goal && cli.resolve_no_tools(cfg) {
            anyhow::bail!(
                "this session has an unfinished goal, which needs the goal_report tool; \
                 remove --no-tools, or clear the goal"
            );
        }
        return Ok(());
    };
    if cli.resolve_no_tools(cfg) {
        anyhow::bail!("a goal needs the goal_report tool; remove --no-tools to use --goal");
    }

    // Configuration first, then the flags: a flag typed on the command line
    // overrides the file it names, and a check typed there is additive to
    // whatever the project already required.
    let mut goal = Goal::configured(
        objective,
        cli.goal_args.goal_done.clone(),
        crate::extras::goal::GoalDefaults {
            cfg,
            provider: &session.provider,
            model: &session.model,
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(max_rounds) = cli.goal_args.goal_max_rounds {
        goal.bounds.max_rounds = max_rounds.max(1);
    }
    if let Some(mode) = cli.goal_args.goal_continuation.as_deref() {
        goal.continuation = match mode {
            "restart" => ContinuationMode::Restart {
                summary_chars: DEFAULT_RESTART_SUMMARY_CHARS,
            },
            _ => ContinuationMode::Continue,
        };
    }
    for command in &cli.goal_args.goal_check {
        goal.checks
            .push(crate::extras::goal::GoalCheck::new(command.clone()));
    }

    session
        .goal_store
        .set(goal, cli.goal_args.goal_replace)
        .map_err(|error| anyhow::anyhow!("{error} (or pass --goal-replace)"))
}

/// Drive `--loop` rounds through the goal engine.
///
/// The loop's own reporting is preserved: each round prints its banner and the
/// gate's decision on stderr, so a loop log still reads as a loop log.
#[cfg(all(feature = "goal", feature = "loop"))]
pub(crate) async fn run_goal_rounds_for_loop(
    agent: &provider::AnyAgent,
    session: &mut Session,
    cli: &Cli,
    cfg: &config::Config,
    sandbox: &crate::sandbox::Sandbox,
    client: &provider::AnyClient,
    prompt: &str,
) -> anyhow::Result<()> {
    // The first iteration gets the same framing every later one does: the
    // objective, the round header, and the plan file re-read fresh. Passing the
    // bare prompt meant a `--loop-max 1` run never saw its own plan.
    let first_prompt = session
        .goal_store
        .snapshot()
        .map(|goal| crate::extras::goal::driver::first_round_prompt(&goal))
        .unwrap_or_else(|| prompt.to_string());

    // A resumed session's history reaches the first iteration. Later ones are
    // restart rounds and deliberately start clean, carrying the objective and
    // a harness-built summary instead.
    let history = crate::agent::runner::convert_history_shared_with_tool_result_retention(
        session,
        cfg.resolve_keep_recent_tool_results(),
    );

    let mut native_ticket = None;
    let mut native_owner = None;
    let rounds = heap_future(|| {
        run_headless_goal_rounds(
            agent,
            session,
            cli,
            cfg,
            sandbox,
            client,
            &first_prompt,
            history,
            false,
            crate::event::UsageDelta::default(),
            &mut native_ticket,
            &mut native_owner,
            #[cfg(feature = "hooks")]
            true,
        )
    })
    .await;
    if let Some(failure) = rounds.turn.failure.as_ref()
        && let Some(question) = failure.downcast_ref::<crate::question::QuestionRequired>()
    {
        session.pending_question = Some(question.clone());
        if !rounds.persisted {
            crate::extras::goal::driver::persist_round(
                session,
                &rounds.prompt,
                crate::agent::runner::HeadlessTurn {
                    response: rounds.turn.response.clone(),
                    usage: rounds.turn.usage,
                    interactions: rounds.turn.interactions.clone(),
                    failure: None,
                },
                cfg,
                cli.no_session,
            )?;
        }
    }
    if let Some(failure) = rounds.turn.failure {
        return Err(failure);
    }

    // A loop that stopped because the agent is blocked, or waiting on the
    // user, or stalled, has not done what was asked. Reporting that as success
    // is how an unattended caller retries forever, or stops retrying something
    // that only needed an answer.
    let stop_reason = session
        .goal_store
        .snapshot()
        .map(|goal| crate::print::HeadlessStopReason::for_goal(goal.status));
    match stop_reason {
        // A loop reaching its iteration cap is the ordinary end of a loop, not
        // a failure, and has always exited zero.
        Some(crate::print::HeadlessStopReason::GoalBudgetLimited) | None => Ok(()),
        Some(reason) if reason.exit_code() == 0 => Ok(()),
        Some(reason) => Err(crate::print::HeadlessGoalExit(reason).into()),
    }
}

/// Run one headless turn, then keep running goal rounds while the gate asks
/// for them.
///
/// Without an active goal this is exactly the single turn `-p` has always run,
/// so nothing changes for callers that do not use goals. With one, each round
/// is a fresh agent run with its own turn and token budget, and the decision
/// that produced it is written to stderr so a CI log shows why the agent kept
/// going.
///
/// The agent is reused between rounds: the goal preamble block is static for
/// the goal's life, so a rebuild would produce an identical system prompt.
#[allow(clippy::too_many_arguments)]
/// The canonical headless invocation retains its writer through model, hook
/// and I/O retirement. Drop transfers it to the existing scoped cleanup owner.
struct NativeHeadlessOwner {
    ticket: Option<crate::session::native_publication::NativePublicationTicket>,
    binding: Option<provider::NativeTurnBinding>,
    scope: std::sync::Arc<crate::agent::runner::AgentWorkScope>,
    cleanup: Option<crate::agent::runner::AgentRunCleanupGuard>,
    outcome: crate::session::native_replay::RetiredNativeOutcome,
}
impl NativeHeadlessOwner {
    fn new(ticket: Option<crate::session::native_publication::NativePublicationTicket>) -> Self {
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cleanup = crate::agent::runner::AgentRunCleanupGuard::auxiliary(scope.clone());
        Self {
            ticket,
            binding: None,
            scope,
            cleanup: Some(cleanup),
            outcome: crate::session::native_replay::RetiredNativeOutcome::Incomplete,
        }
    }
    async fn progress(&mut self, session: &mut Session) -> anyhow::Result<()> {
        if let Some(ticket) = &mut self.ticket {
            crate::session::native_publication::progress_owned(session, ticket, &self.scope)
                .await?;
        }
        Ok(())
    }
    async fn finish(&mut self, session: &mut Session) -> anyhow::Result<()> {
        // SessionEnd may have admitted async command children after the
        // model retired. Cancel and join that exact scope before publishing.
        // Its cleanup owner retains the unique ticket even if this await drops.
        if let Some(mut cleanup) = self.cleanup.take() {
            let retained = std::sync::Arc::new(std::sync::Mutex::new(self.ticket.take()));
            cleanup.retain_until_settled(retained.clone());
            cleanup.settle().await;
            self.ticket = retained.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        // The completed hook scope is cancelled. Final file work gets a new
        // owned IO scope; the binding still proves exact model retirement.
        self.scope = crate::agent::runner::AgentWorkScope::new();
        self.cleanup = Some(crate::agent::runner::AgentRunCleanupGuard::auxiliary(
            self.scope.clone(),
        ));
        let result = async {
            if let Some(ticket) = self.ticket.take() {
                let binding = self
                    .binding
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("native model did not settle"))?;
                if session.native_frontier.refusal().is_some() {
                    binding
                        .publish_refused_owned(session, ticket, &self.scope)
                        .await?;
                } else {
                    binding
                        .publish_retired_owned(session, ticket, self.outcome, &self.scope)
                        .await?;
                }
            }
            Ok(())
        }
        .await;
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.settle().await;
        }
        result
    }
}
impl Drop for NativeHeadlessOwner {
    fn drop(&mut self) {
        if let (Some(cleanup), Some(ticket)) = (&mut self.cleanup, self.ticket.take()) {
            cleanup.retain_until_settled(ticket);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_bound_headless_round(
    agent: &provider::AnyAgent,
    session: &mut Session,
    cli: &Cli,
    cfg: &config::Config,
    message: &str,
    history: std::sync::Arc<[rig::completion::Message]>,
    output: crate::print::HeadlessOutputPolicy,
    question_turn: crate::question::QuestionTurn,
    native_ticket: &mut Option<crate::session::native_publication::NativePublicationTicket>,
    native_owner: &mut Option<NativeHeadlessOwner>,
    #[cfg(feature = "agent-skills")] instruction_turn: Option<
        std::sync::Arc<crate::extras::skills::session::InstructionTurn>,
    >,
    #[cfg(feature = "hooks")] loop_info: Option<crate::extras::hooks::LoopInfo>,
) -> (crate::agent::runner::HeadlessTurn, bool) {
    use crate::session::native_replay::{HistoryProtocol, ReplayBoundary, RetiredNativeOutcome};
    let native = agent.history_protocol() == HistoryProtocol::AnthropicMessages
        || session.native_history_snapshot().is_some();
    if !native {
        let turn = heap_future(|| {
            let agent = agent.clone();
            #[cfg(feature = "agent-skills")]
            let agent = agent.with_instruction_turn(instruction_turn);
            let agent = agent
                .with_spend_for_session(session, cfg, crate::spend::RequestKind::Turn)
                .with_question_turn(Some(question_turn));
            async move {
                agent
                    .run_print_with_output(
                        message,
                        output,
                        &cfg.retry,
                        history,
                        #[cfg(feature = "hooks")]
                        loop_info,
                    )
                    .await
            }
        })
        .await;
        return (turn, false);
    }
    let failed = |error: anyhow::Error| {
        (
            crate::agent::runner::HeadlessTurn::failed_before_start(error),
            false,
        )
    };
    if native_owner.is_some() {
        return failed(anyhow::anyhow!(
            "native automatic round requires its prior retired context owner"
        ));
    }
    if !cli.no_session && native_ticket.is_none() {
        return failed(anyhow::anyhow!("native context requires its startup owner"));
    }
    *native_owner = Some(NativeHeadlessOwner::new(native_ticket.take()));
    let owner = native_owner.as_mut().unwrap();
    let spend = crate::spend::run::Run::for_session(session, cfg, crate::spend::RequestKind::Turn);
    if let Err(error) = owner.scope.run(async { spend.prepare().await }).await {
        return failed(error);
    }
    session.pending_question = None;
    if let Err(error) = owner.progress(session).await {
        return failed(error);
    }
    #[cfg(feature = "goal")]
    if session.goal_store.is_live() {
        return failed(anyhow::anyhow!(
            "native automatic goal work requires its round publication owner; ordinary print is available"
        ));
    }
    let pricing = crate::print::HeadlessPricing::from_session(session, cfg);
    let boundary = match session.native_history_snapshot() {
        Some(saved) => match saved.saved_boundary() {
            Ok(value) => value,
            Err(error) => return failed(error.into()),
        },
        None => ReplayBoundary::CompletedTurn,
    };
    let bound = heap_future(|| {
        let agent = agent.clone();
        #[cfg(feature = "agent-skills")]
        let agent = agent.with_instruction_turn(instruction_turn);
        let agent = agent
            .with_spend(Some(spend))
            .with_question_turn(Some(question_turn));
        std::future::ready(agent.bind_native_turn(
            session,
            history,
            boundary,
            provider::RecordedToolIds::Canonical,
        ))
    })
    .await;
    let (agent, binding) = match bound {
        Ok(value) => value,
        Err(error) => return failed(error.into()),
    };
    if let Some(ticket) = &mut owner.ticket
        && let Err(error) = binding.pair_publication(session, ticket)
    {
        return failed(error);
    }
    let history = binding.history();
    owner.binding = Some(binding);
    let mut turn = heap_future(|| {
        agent.run_print_with_output(
            message,
            output,
            &cfg.retry,
            history,
            #[cfg(feature = "hooks")]
            loop_info,
        )
    })
    .await;
    // The producer returned only after its exact model and scoped effects joined.
    owner.outcome = if turn.failure.is_none() {
        RetiredNativeOutcome::Completed
    } else if turn.failure.as_ref().is_some_and(|error| {
        error
            .downcast_ref::<crate::question::QuestionRequired>()
            .is_some()
    }) {
        RetiredNativeOutcome::LiveToolReply
    } else {
        RetiredNativeOutcome::Incomplete
    };
    session.pending_question = turn
        .failure
        .as_ref()
        .and_then(|error| error.downcast_ref::<crate::question::QuestionRequired>())
        .cloned();
    let usage = cfg.normalize_usage(&session.provider, turn.usage.into());
    let binding = owner.binding.as_ref().unwrap();
    let committed = binding.commit_canonical_into_with_outcome(
        session,
        message,
        &turn.response,
        &turn.interactions,
        usage,
        pricing,
        owner.outcome,
    );
    if let Err(reason) = committed {
        turn.failure = Some(match turn.failure.take() {
            Some(error) => error.context(reason),
            None => reason.into(),
        });
    }
    if session.native_frontier.refusal().is_some() {
        if let Some(ticket) = owner.ticket.take()
            && let Err(error) = binding
                .publish_refused_owned(session, ticket, &owner.scope)
                .await
        {
            turn.failure = Some(error);
        }
    } else if let Err(error) = owner.progress(session).await {
        turn.failure = Some(error);
    }
    (turn, true)
}

#[allow(clippy::too_many_arguments)]
async fn run_headless_goal_rounds(
    agent: &provider::AnyAgent,
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))] session: &mut Session,
    cli: &Cli,
    cfg: &config::Config,
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))] sandbox: &crate::sandbox::Sandbox,
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))] client: &provider::AnyClient,
    message: &str,
    history: std::sync::Arc<[rig::completion::Message]>,
    json_output: bool,
    // Normalized usage of the auto-compaction that ran before the first
    // round. The session has already been charged for it; its tokens count
    // toward the goal's first round (mini-agent-i6q98).
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))]
    compaction_usage: crate::event::UsageDelta,
    // Whether these rounds are a `--loop`. A loop's iterations have always been
    // reported to `Stop` hooks as `loop_iteration`/`loop_active`, and folding
    // the loop onto the goal driver must not silently empty those fields for
    // everyone who reads them.
    native_ticket: &mut Option<crate::session::native_publication::NativePublicationTicket>,
    native_owner: &mut Option<NativeHeadlessOwner>,
    #[cfg(feature = "hooks")] loop_mode: bool,
) -> HeadlessRounds {
    // Plain print has no goal engine to poll. Keep the independently
    // measured automatic-round future behind its own constructor boundary.
    #[cfg(feature = "goal")]
    let plain = !session.goal_store.is_active();
    #[cfg(not(feature = "goal"))]
    let plain = true;
    if plain {
        session.pending_question = None;
        #[cfg(feature = "agent-skills")]
        let instruction_turn = agent.instruction_services().map(|services| {
            crate::extras::skills::session::InstructionTurn::new(services.clone(), None)
        });
        let output = if cli.is_loop_mode() && !cli.print {
            crate::print::HeadlessOutputPolicy::legacy(cli.pure_stdout, !json_output)
        } else {
            crate::print::HeadlessOutputPolicy::from_cli(cli)
        };
        let (turn, persisted) = if agent.history_protocol()
            == crate::session::native_replay::HistoryProtocol::AnthropicMessages
            || session.native_history_snapshot().is_some()
        {
            heap_future(|| {
                run_bound_headless_round(
                    agent,
                    session,
                    cli,
                    cfg,
                    message,
                    history,
                    output,
                    crate::question::QuestionTurn::default(),
                    native_ticket,
                    native_owner,
                    #[cfg(feature = "agent-skills")]
                    instruction_turn,
                    #[cfg(feature = "hooks")]
                    None,
                )
            })
            .await
        } else {
            // An inactive goal has no automatic rounds. Keep its measured
            // 141 KiB polling frame off the ordinary one-turn HTTP path.
            let turn = heap_future(|| {
                let agent = agent.clone();
                #[cfg(feature = "agent-skills")]
                let agent = agent.with_instruction_turn(instruction_turn);
                let agent = agent
                    .with_spend_for_session(session, cfg, crate::spend::RequestKind::Turn)
                    .with_question_turn(Some(crate::question::QuestionTurn::default()));
                #[cfg(feature = "hooks")]
                let loop_info = loop_round_info(session, loop_mode);
                async move {
                    agent
                        .run_print_with_output(
                            message,
                            output,
                            &cfg.retry,
                            history,
                            #[cfg(feature = "hooks")]
                            loop_info,
                        )
                        .await
                }
            })
            .await;
            (turn, false)
        };
        return HeadlessRounds {
            turn,
            prompt: message.to_owned(),
            persisted,
        };
    }
    heap_future(|| {
        run_headless_goal_rounds_inner(
            agent,
            session,
            cli,
            cfg,
            sandbox,
            client,
            message,
            history,
            json_output,
            compaction_usage,
            native_ticket,
            native_owner,
            #[cfg(feature = "hooks")]
            loop_mode,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_headless_goal_rounds_inner(
    agent: &provider::AnyAgent,
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))] session: &mut Session,
    cli: &Cli,
    cfg: &config::Config,
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))] sandbox: &crate::sandbox::Sandbox,
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))] client: &provider::AnyClient,
    message: &str,
    history: std::sync::Arc<[rig::completion::Message]>,
    json_output: bool,
    // Normalized usage of the auto-compaction that ran before the first
    // round. The session has already been charged for it; its tokens count
    // toward the goal's first round (mini-agent-i6q98).
    #[cfg_attr(not(feature = "goal"), allow(unused_variables))]
    compaction_usage: crate::event::UsageDelta,
    // Whether these rounds are a `--loop`. A loop's iterations have always been
    // reported to `Stop` hooks as `loop_iteration`/`loop_active`, and folding
    // the loop onto the goal driver must not silently empty those fields for
    // everyone who reads them.
    native_ticket: &mut Option<crate::session::native_publication::NativePublicationTicket>,
    native_owner: &mut Option<NativeHeadlessOwner>,
    #[cfg(feature = "hooks")] loop_mode: bool,
) -> HeadlessRounds {
    let question_turn = crate::question::QuestionTurn::default();
    #[cfg(feature = "agent-skills")]
    let instruction_turn = agent.instruction_services().map(|services| {
        crate::extras::skills::session::InstructionTurn::new(services.clone(), None)
    });
    session.pending_question = None;
    let output = if cli.is_loop_mode() && !cli.print {
        crate::print::HeadlessOutputPolicy::legacy(cli.pure_stdout, !json_output)
    } else {
        crate::print::HeadlessOutputPolicy::from_cli(cli)
    };
    // Rounds after the first run the gate's instruction rather than the
    // message the operator typed, and each is persisted as it completes. The
    // caller has to know both, or it writes the final turn a second time and
    // files it under a prompt that was never sent.
    #[cfg_attr(not(feature = "goal"), allow(unused_mut))]
    let mut round_prompt = message.to_string();

    // A goal's time bound measures how long the agent worked. Headless runs
    // never wait on a permission prompt, so the round's wall clock is its
    // active time; without this the bound is configurable and inert.
    #[cfg(feature = "goal")]
    let mut round_started = std::time::Instant::now();
    #[cfg(feature = "hooks")]
    let first_loop_info = loop_round_info(session, loop_mode);
    #[cfg_attr(not(feature = "goal"), allow(unused_mut))]
    let (mut turn, mut persisted) = if agent.history_protocol()
        == crate::session::native_replay::HistoryProtocol::AnthropicMessages
        || session.native_history_snapshot().is_some()
    {
        heap_future(|| {
            run_bound_headless_round(
                agent,
                session,
                cli,
                cfg,
                message,
                history,
                output,
                question_turn.clone(),
                native_ticket,
                native_owner,
                #[cfg(feature = "agent-skills")]
                instruction_turn.clone(),
                #[cfg(feature = "hooks")]
                first_loop_info,
            )
        })
        .await
    } else {
        let turn = heap_future(|| {
            // The chained setters move a large AnyAgent. Construct it in this
            // short factory frame before the owned request starts polling HTTP.
            let agent = agent.clone();
            #[cfg(feature = "agent-skills")]
            let agent = agent.with_instruction_turn(instruction_turn.clone());
            let agent = agent
                .with_spend_for_session(session, cfg, crate::spend::RequestKind::Turn)
                .with_question_turn(Some(question_turn.clone()));
            #[cfg(feature = "hooks")]
            let loop_info = loop_round_info(session, loop_mode);
            async move {
                agent
                    .run_print_with_output(
                        message,
                        output,
                        &cfg.retry,
                        history,
                        #[cfg(feature = "hooks")]
                        loop_info,
                    )
                    .await
            }
        })
        .await;
        (turn, false)
    };
    #[cfg(feature = "goal")]
    let mut round_elapsed = round_started.elapsed();

    #[cfg(feature = "goal")]
    {
        use crate::extras::goal::driver::{self, RoundOutcome};

        let verify_configured = cfg
            .verify_command
            .as_deref()
            .is_some_and(|command| !command.trim().is_empty());
        let mut pending_compaction_tokens = compaction_usage
            .input_tokens
            .saturating_add(compaction_usage.output_tokens);

        loop {
            let Some(goal) = session.goal_store.snapshot() else {
                break;
            };
            if goal.status.is_terminal() {
                break;
            }
            // A force-stopped turn (second interrupt or grace deadline) ends
            // the run: no verification, no relaunch; the caller persists it.
            if crate::print::headless_force_stopped() {
                break;
            }

            if native_owner.is_some() && turn.failure.is_some() {
                break;
            }
            let open_todos = session
                .todos
                .snapshot()
                .iter()
                .filter(|item| !matches!(item.status.as_str(), "completed" | "cancelled"))
                .count();
            let mut summary = driver::summary_from_headless_turn(
                &goal,
                &turn.interactions,
                &driver::RoundCost {
                    usage: turn.usage,
                    active: round_elapsed,
                },
                turn.failure.as_ref(),
                open_todos,
                verify_configured,
            );
            // The compaction before the first round was spent on this goal's
            // work, so its tokens join that round's count, exactly once.
            summary.tokens_used = summary
                .tokens_used
                .saturating_add(std::mem::take(&mut pending_compaction_tokens));

            if let Some(question) = turn
                .failure
                .as_ref()
                .and_then(|error| error.downcast_ref::<crate::question::QuestionRequired>())
            {
                session.pending_question = Some(question.clone());
                let outcome = driver::park_for_question(&session.goal_store, summary);
                if let driver::RoundOutcome::Stopped { line, .. } = outcome {
                    let _ = crate::print::headless_stderr_async(format!("{line}\n")).await;
                }
                // The ordinary persistence path below commits this partial
                // round once. No verification/judge or automatic relaunch.
                break;
            }

            let sandbox_for_checks = sandbox.clone();
            let cfg_for_checks = cfg.clone();
            let goal_for_checks = goal.clone();
            let judge = crate::extras::goal::judge::resolve(
                &goal.judge,
                cfg,
                &session.provider,
                &session.model,
            );
            // The round that just ran is persisted after the gate settles, so
            // the session does not contain it yet and the judge would be shown
            // every round except the one it was asked about. The turn's own
            // record fills that gap, here and under --no-session alike.
            let transcript =
                crate::extras::goal::judge::transcript_for_round(session, &turn.interactions);
            let client_for_judge = client.clone();
            let cfg_for_judge = cfg.clone();
            // An interrupt during verification ends the goal rather than
            // feeding a cancelled command back to the agent as a failure it
            // should fix. The flag survives the closure so the loop can stop.
            let verification_interrupted =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let interrupt_flag = verification_interrupted.clone();
            // The judge call's usage, priced at the judge model's own rates,
            // comes back out of the closure so the session is charged for it
            // before the round is persisted (mini-agent-kfsup).
            let judge_charge_slot = std::sync::Arc::new(std::sync::Mutex::new(None));
            let judge_charge_out = judge_charge_slot.clone();
            // Defer notices until usage and the round transcript are durable.
            // A blocked diagnostic must not hold up their settlement.
            let notices = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let check_notices = notices.clone();
            let spend =
                crate::spend::run::Run::for_session(session, cfg, crate::spend::RequestKind::Judge);
            let session_prices = (session.input_token_cost, session.output_token_cost);
            let session_cache_prices = session.cache_prices();
            let outcome =
                driver::settle_round(&session.goal_store, summary, move |request| async move {
                    // An operator who interrupts must not wait for a check;
                    // a check can run as long as a test suite does. One
                    // interrupt covers every tier, so a signal landing between
                    // the checks and the judge is not lost.
                    let interrupt = crate::print::headless_interrupt();
                    tokio::pin!(interrupt);
                    let (checks, interrupted) = crate::spend::run::scope(
                        Some(spend.clone()),
                        crate::extras::goal::checks::run_with_interrupt(
                            &goal_for_checks,
                            &request,
                            &sandbox_for_checks,
                            &cfg_for_checks,
                            interrupt.as_mut(),
                        ),
                    )
                    .await;
                    if interrupted {
                        interrupt_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        // Say what was stopped. The diagnostic used to reach
                        // the operator only because the cancelled check was
                        // fed back to the agent as a failure, which is exactly
                        // what must not happen: nothing was proved either way,
                        // so the round settles as interrupted and the reason
                        // is reported here instead.
                        if let Some(tail) = checks.as_ref().and_then(|o| o.failure_tail.as_deref())
                        {
                            check_notices
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(format!("{tail}\n"));
                        }
                        return driver::Verification {
                            interrupted: true,
                            checks,
                            ..driver::Verification::default()
                        };
                    }
                    // A failing check already decides the claim, and the
                    // gate returns before it looks at the judge. Asking
                    // anyway would spend a model call on an answer nobody
                    // reads, and a transport failure there would count
                    // against the judge's own failure ladder.
                    let checks_rejected = checks.as_ref().is_some_and(|o| !o.all_passed);
                    let judged = match judge {
                        Some(resolved) if request.run_judge && !checks_rejected => {
                            // A judge call can wait on its provider for as long
                            // as the retry policy allows. An interrupt drops it:
                            // a verdict arriving after the operator stopped the
                            // run must not decide the goal.
                            let observed = crate::spend::run::UsageObserver::with_run(
                                spend.for_judge(
                                    &resolved,
                                    &cfg_for_judge,
                                    resolved
                                        .same_provider_as_session
                                        .then(|| {
                                            client_for_judge.listing_key(&resolved.provider).ok()
                                        })
                                        .flatten()
                                        .as_ref(),
                                ),
                            );
                            let verdict = driver::unless_interrupted(
                                crate::extras::goal::judge::ask_with_transcript_observed(
                                    &goal_for_checks,
                                    &request,
                                    &resolved,
                                    &client_for_judge,
                                    &transcript,
                                    &cfg_for_judge,
                                    checks.as_ref(),
                                    observed.clone(),
                                ),
                                interrupt.as_mut(),
                            )
                            .await;
                            let Some(verdict) = verdict else {
                                let cancelled_call =
                                    crate::extras::goal::judge::JudgeCall::interrupted_with_usage(
                                        &cfg_for_judge,
                                        &resolved.provider,
                                        &observed,
                                    );
                                *judge_charge_out
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    cancelled_call.charge(
                                        &resolved,
                                        &cfg_for_judge,
                                        crate::extras::goal::judge::judge_prices_with_cache_prices(
                                            &resolved,
                                            &cfg_for_judge,
                                            session_prices,
                                            session_cache_prices,
                                        ),
                                        session_cache_prices,
                                    );
                                interrupt_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                return driver::Verification {
                                    interrupted: true,
                                    ..driver::Verification::default()
                                };
                            };
                            *judge_charge_out
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = verdict
                                .charge(
                                    &resolved,
                                    &cfg_for_judge,
                                    crate::extras::goal::judge::judge_prices_with_cache_prices(
                                        &resolved,
                                        &cfg_for_judge,
                                        session_prices,
                                        session_cache_prices,
                                    ),
                                    session_cache_prices,
                                );
                            Some(verdict)
                        }
                        _ => None,
                    };
                    driver::Verification {
                        checks,
                        judge_tokens: judged.as_ref().map_or(0, |call| call.tokens),
                        judge: judged.map(|call| call.outcome),
                        ..driver::Verification::default()
                    }
                })
                .await;

            let judge_charge = judge_charge_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(charge) = judge_charge {
                let first_unpriced = session.unpriced_judge_tokens == 0;
                if !session.charge_judge_call(&charge) && first_unpriced {
                    notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(format!(
                        "goal: no known price for judge model {}; its tokens are counted without cost\n",
                        charge.model
                    ));
                }
            }
            // The round that just ran is history for whatever comes next, and its
            // work has to survive an interrupt, so it is persisted before the
            // gate's decision is acted on rather than only on a relaunch.
            if !persisted
                && let Err(error) = crate::extras::goal::driver::persist_round(
                    session,
                    &round_prompt,
                    crate::agent::runner::HeadlessTurn {
                        response: turn.response.clone(),
                        usage: turn.usage,
                        interactions: turn.interactions.clone(),
                        failure: None,
                    },
                    cfg,
                    cli.no_session,
                )
            {
                notices
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!(
                        "warning: failed to save session between goal rounds: {error}\n"
                    ));
            } else {
                persisted = true;
            }
            let notices = std::mem::take(
                &mut *notices
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let mut notice_failed = false;
            for notice in notices {
                if let Err(error) = crate::print::headless_stderr_async(notice).await {
                    turn.failure = Some(error);
                    notice_failed = true;
                    break;
                }
            }
            if notice_failed {
                break;
            }
            if let Some(goal) = session.goal_store.snapshot()
                && let Err(error) = output.emit_event(serde_json::json!({"type":"goal", "goal":crate::print::HeadlessGoal::from_goal(&goal)})).await
            {
                turn.failure = Some(error);
                break;
            }
            // An interrupt that landed while the tiers were running is
            // reported in its own words: the round settled as interrupted, so
            // the gate's line would say only that, and an operator needs to
            // know the check they stopped is why.
            if verification_interrupted.load(std::sync::atomic::Ordering::Relaxed) {
                if let Err(error) = crate::print::headless_stderr_async(
                    "goal: interrupted during verification\n".to_owned(),
                )
                .await
                {
                    turn.failure = Some(error);
                }
                break;
            }

            let RoundOutcome::Relaunch { relaunch, line } = outcome else {
                if let RoundOutcome::Stopped { line, .. } = outcome
                    && let Err(error) =
                        crate::print::headless_stderr_async(format!("{line}\n")).await
                {
                    turn.failure = Some(error);
                }
                break;
            };
            if let Err(error) = crate::print::headless_stderr_async(format!("{line}\n")).await {
                turn.failure = Some(error);
                break;
            }

            let next_history: std::sync::Arc<[rig::completion::Message]> = match relaunch.history {
                driver::HistoryMode::Retained => {
                    crate::agent::runner::convert_history_shared_with_tool_result_retention(
                        session,
                        cfg.resolve_keep_recent_tool_results(),
                    )
                }
                driver::HistoryMode::Empty => std::sync::Arc::from(Vec::new()),
            };

            // The wrap-up round is asked to summarize and stop, so it runs on
            // an agent capped to that budget. Announcing a bound and then
            // handing the round the full per-run budget is not a bound.
            let capped;
            let round_agent = match relaunch.max_agent_turns {
                Some(max_turns) => {
                    capped = agent.with_max_agent_turns(max_turns);
                    &capped
                }
                None => agent,
            };
            round_prompt = relaunch.prompt.clone();
            persisted = false;
            round_started = std::time::Instant::now();
            if native_owner.is_some() {
                turn.failure = Some(anyhow::anyhow!(
                    "native goal continuation requires its retired round owner"
                ));
                break;
            }
            let next = heap_future(|| {
                // Automatic rounds retain the same clarification identity;
                // only this round's owned financial/request profile is fresh.
                let round_agent = round_agent.clone();
                #[cfg(feature = "agent-skills")]
                let round_agent = round_agent.with_instruction_turn(instruction_turn.clone());
                let round_agent = round_agent
                    .with_spend_for_session(session, cfg, crate::spend::RequestKind::Turn)
                    .with_question_turn(Some(question_turn.clone()));
                #[cfg(feature = "hooks")]
                let loop_info = loop_round_info(session, loop_mode);
                async move {
                    round_agent
                        .run_print_with_output(
                            &relaunch.prompt,
                            output,
                            &cfg.retry,
                            next_history,
                            #[cfg(feature = "hooks")]
                            loop_info,
                        )
                        .await
                }
            })
            .await;
            round_elapsed = round_started.elapsed();
            // Only the final round's transcript is returned; earlier rounds are
            // already persisted above.
            turn = next;
        }
    }

    HeadlessRounds {
        turn,
        prompt: round_prompt,
        persisted,
    }
}

/// The `--loop` fields a `Stop` hook expects, derived from the goal.
///
/// A loop iteration is a goal round, so the round about to run is the
/// iteration about to run, and the loop is active while the goal is.
#[cfg(feature = "hooks")]
pub(crate) fn loop_round_info(
    session: &Session,
    loop_mode: bool,
) -> Option<crate::extras::hooks::LoopInfo> {
    if !loop_mode {
        return None;
    }
    // `loop` implies `goal`, so a build without goals has no loop to report.
    #[cfg(feature = "goal")]
    {
        let goal = session.goal_store.snapshot()?;
        Some(crate::extras::hooks::LoopInfo {
            iteration: goal.progress.current_round(),
            active: goal.status.is_running(),
        })
    }
    #[cfg(not(feature = "goal"))]
    {
        let _ = session;
        None
    }
}

/// What a headless run leaves for its caller to finish.
///
/// A goal run persists each round as it completes, so the caller must not
/// write the last one again, and must not attribute it to the message the
/// operator typed: every round after the first ran the gate's instruction.
pub(crate) struct HeadlessRounds {
    pub turn: crate::agent::runner::HeadlessTurn,
    pub prompt: String,
    pub persisted: bool,
}

#[cfg(test)]
mod tests {
    use super::ACTIVE_OPENROUTER_PRICING_REAPERS;
    use super::{
        OpenRouterPricingRefresh, ResumeProviderDecision, apply_openrouter_pricing_refresh_result,
        apply_resume_provider_decision, compact_headless_session_with, headless_compaction_plan,
        interactive_initial_message, js_runtime_banner_lines, needs_openrouter_context_refresh,
        require_recent_session, resolve_resume_provider_decision, run_startup_probes_concurrently,
        select_interactive_auto_trigger, unavailable_sandbox_must_fail,
        validate_startup_permission_policy,
    };
    use crate::cli::Cli;

    /// A refused containment preflight must reach the startup banner with its
    /// reason intact, and a healthy (or not-compiled) runtime must add nothing.
    #[test]
    fn quick_model_price_presence_startup_selected_alias_charges_paid_catalogue() {
        let cfg: Config = toml::from_str("provider='anthropic'\nmodel='paid'\n[quick_models.paid]\nprovider='anthropic'\nmodel='claude-sonnet-5-5'").unwrap();
        let expected = Config::catalog_input_output_cost("anthropic", "claude-sonnet-5-5").unwrap();
        for cli in [
            Cli::default(),
            Cli {
                quick_model: Some("paid".into()),
                ..Default::default()
            },
        ] {
            let prices = super::resolve_startup_model_pricing(
                &cli,
                &cfg,
                "anthropic",
                "claude-sonnet-5-5",
                None,
            );
            assert_eq!(prices, Some(expected));
            let mut session = Session::new("anthropic", "claude-sonnet-5-5", 1_000_000, "");
            session.set_input_output_prices(prices);
            session.charge_usage_delta(
                crate::event::UsageDelta {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    ..Default::default()
                },
                true,
            );
            assert_eq!(session.total_cost, expected.0 + expected.1);
        }
    }

    #[test]
    fn js_runtime_banner_names_the_verbatim_containment_reason() {
        use crate::provider::{
            JS_RUNTIME_PROBE_PENDING_REASON, JsRuntimeReport, RuntimeAvailability,
        };
        const REASON: &str =
            "MACOS_CONTAINMENT_UNSUPPORTED_VERSION: macOS 15 is not a validated major";

        assert!(
            js_runtime_banner_lines(&JsRuntimeReport::unreported()).is_empty(),
            "the pending startup probe must not be called unavailable"
        );

        for report in [
            JsRuntimeReport {
                javascript: RuntimeAvailability::Available,
                learned_skills: RuntimeAvailability::Available,
            },
            JsRuntimeReport {
                javascript: RuntimeAvailability::NotCompiled,
                learned_skills: RuntimeAvailability::NotCompiled,
            },
        ] {
            assert!(
                js_runtime_banner_lines(&report).is_empty(),
                "the banner must stay silent for {report:?}"
            );
        }

        let both = js_runtime_banner_lines(&JsRuntimeReport {
            javascript: RuntimeAvailability::Unavailable {
                reason: REASON.to_string(),
            },
            learned_skills: RuntimeAvailability::Unavailable {
                reason: format!("requires the contained JavaScript worker: {REASON}"),
            },
        });
        assert_eq!(both.len(), 1, "{both:?}");
        assert!(
            both[0].contains("JavaScript runtime and learned skills unavailable"),
            "the banner must name both subsystems: {both:?}"
        );
        assert!(
            both[0].contains(REASON),
            "the banner must repeat the preflight reason verbatim: {both:?}"
        );

        assert_eq!(
            js_runtime_banner_lines(&JsRuntimeReport {
                javascript: RuntimeAvailability::Unavailable {
                    reason: REASON.to_string(),
                },
                learned_skills: RuntimeAvailability::NotCompiled,
            }),
            vec![format!("[!] JavaScript runtime unavailable: {REASON}")]
        );
        assert_eq!(
            js_runtime_banner_lines(&JsRuntimeReport {
                javascript: RuntimeAvailability::Available,
                learned_skills: RuntimeAvailability::Unavailable {
                    reason: "learned-skill services did not initialize".to_string(),
                },
            }),
            vec![
                "[!] learned skills unavailable: learned-skill services did not initialize"
                    .to_string()
            ]
        );

        assert_eq!(
            js_runtime_banner_lines(&JsRuntimeReport {
                javascript: RuntimeAvailability::Unavailable {
                    reason: JS_RUNTIME_PROBE_PENDING_REASON.to_string(),
                },
                learned_skills: RuntimeAvailability::Unavailable {
                    reason: REASON.to_string(),
                },
            }),
            vec![format!("[!] learned skills unavailable: {REASON}")],
            "a pending JS probe cannot mask a real skills failure"
        );
        assert_eq!(
            js_runtime_banner_lines(&JsRuntimeReport {
                javascript: RuntimeAvailability::Unavailable {
                    reason: REASON.to_string(),
                },
                learned_skills: RuntimeAvailability::Unavailable {
                    reason: JS_RUNTIME_PROBE_PENDING_REASON.to_string(),
                },
            }),
            vec![format!("[!] JavaScript runtime unavailable: {REASON}")],
            "a pending skills probe cannot be labeled as a refusal"
        );
    }

    #[test]
    fn continue_requires_a_readable_existing_session() {
        let workspace = std::path::Path::new("/workspace/project");
        let missing = require_recent_session(Ok(Vec::new()), workspace).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("no saved sessions for this workspace")
        );
        assert!(
            missing
                .to_string()
                .contains(&workspace.display().to_string())
        );
        assert!(missing.to_string().contains("use --session or /sessions"));

        let unreadable =
            require_recent_session(Err(anyhow::anyhow!("corrupt store")), workspace).unwrap_err();
        assert!(
            unreadable
                .to_string()
                .contains("failed to continue session")
        );
        assert!(unreadable.to_string().contains("corrupt store"));
    }

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

    #[cfg(feature = "memory")]
    #[test]
    fn headless_compaction_plan_keeps_zero_headroom_for_maximum_reserve_and_memory() {
        let mut session = Session::new("openai", "model", 100, "");
        session.overhead_tokens = 90;
        session.add_message(crate::session::MessageRole::User, &"a".repeat(40));
        session.add_message(crate::session::MessageRole::Assistant, &"b".repeat(40));
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "compact_enabled": true,
            "reserve_tokens": u64::MAX,
            "keep_recent_tokens": 5
        }))
        .unwrap();
        let before = serde_json::to_vec(&session).unwrap();
        for memory in [None, Some(""), Some("captured memory 🙂界")] {
            let plan = headless_compaction_plan(&session, &cfg, 1, memory)
                .expect("exhausted headroom must keep the over-budget compaction gate active");
            assert_eq!(plan.cut_idx, 1);
            assert_eq!(plan.input_token_budget, 0);
            assert_eq!(plan.response_token_budget, u64::MAX);
        }
        let normal = Config {
            reserve_tokens: Some(20),
            ..cfg
        };
        let without = headless_compaction_plan(&session, &normal, 1, None).unwrap();
        let with = headless_compaction_plan(&session, &normal, 1, Some("abcd")).unwrap();
        assert_eq!(
            (without.input_token_budget, without.response_token_budget),
            (80, 20)
        );
        assert_eq!(
            with.input_token_budget,
            80 - Session::estimate_tokens("abcd")
        );
        assert_eq!(
            with.response_token_budget,
            20 + Session::estimate_tokens("abcd")
        );
        assert_eq!(
            serde_json::to_vec(&session).unwrap(),
            before,
            "planning cannot compact or charge the session"
        );
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
                Ok(crate::provider::CompactionOutput {
                    summary_byte_budget: 16 * 1024,
                    summary: "HEADLESS_SUMMARY".to_string(),
                    messages_included: 1,
                    usage: crate::event::UsageDelta::default(),
                })
            },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(result.summary, "HEADLESS_SUMMARY");
        assert_eq!(result.first_kept_index, 1);
        assert_eq!(session.compactions.len(), 1);
        assert_eq!(session.messages[0].content, "HEADLESS_SUMMARY");
        assert_eq!(session.messages.len(), 2);
    }

    // mini-agent-i6q98: the summarizer's requests are charged to the session
    // at the session model's prices, normalized for its provider.
    #[tokio::test]
    async fn headless_auto_compaction_charges_summarizer_usage_to_the_session() {
        let mut session = crate::session::Session::new("gemini", "model", 100, "");
        session.overhead_tokens = 90;
        session.input_token_cost = 2.0;
        session.output_token_cost = 10.0;
        session.add_message(crate::session::MessageRole::User, &"a".repeat(40));
        session.add_message(crate::session::MessageRole::Assistant, &"b".repeat(40));
        let cfg = crate::config::Config {
            compact_enabled: Some(true),
            reserve_tokens: Some(20),
            keep_recent_tokens: Some(5),
            ..crate::config::Config::default()
        };
        let cost_before = session.total_cost;
        let usage = crate::event::UsageDelta {
            input_tokens: 300_000,
            output_tokens: 20_000,
            total_tokens: 330_000,
            reasoning_tokens: 10_000,
            ..crate::event::UsageDelta::default()
        };

        let result = compact_headless_session_with(
            &mut session,
            &cfg,
            0,
            #[cfg(feature = "memory")]
            None,
            |_, messages, _, _, _| async move {
                Ok(crate::provider::CompactionOutput {
                    summary_byte_budget: 16 * 1024,
                    summary: "SUMMARY".to_string(),
                    messages_included: messages.len(),
                    usage,
                })
            },
        )
        .await
        .unwrap()
        .expect("the session is over budget and compacts");

        // Gemini reports thinking beside output; it is billed as output.
        assert_eq!(result.usage.output_tokens, 30_000);
        assert_eq!(session.total_input_tokens, 300_000);
        assert_eq!(session.total_output_tokens, 30_000);
        let expected = crate::pricing::estimate_cost(300_000, 30_000, 2.0, 10.0);
        assert!(expected > 0.0);
        assert!(
            (session.total_cost - cost_before - expected).abs() < 1e-9,
            "compaction must add its cost: {} -> {}",
            cost_before,
            session.total_cost
        );
    }

    // mini-agent-g8m5v: a second compaction passes the previous summary once,
    // as `previous_summary`, not also as the first message of the cut.
    #[tokio::test]
    async fn recompaction_sends_the_previous_summary_once() {
        let mut session = crate::session::Session::new("openai", "model", 100, "");
        session.overhead_tokens = 60;
        session.add_message(crate::session::MessageRole::User, &"z".repeat(40));
        session.compress("OLD_SUMMARY".to_string(), 1, 10);
        session.add_message(crate::session::MessageRole::User, &"a".repeat(40));
        session.add_message(crate::session::MessageRole::Assistant, &"b".repeat(40));
        session.add_message(crate::session::MessageRole::User, &"c".repeat(40));
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
            |_, messages, previous_summary, _, _| async move {
                assert_eq!(previous_summary.as_deref(), Some("OLD_SUMMARY"));
                assert!(
                    messages
                        .iter()
                        .all(|message| message.content != "OLD_SUMMARY"),
                    "the previous summary must not also be a summarized message"
                );
                assert_eq!(messages.len(), 2, "{messages:?}");
                Ok(crate::provider::CompactionOutput {
                    summary_byte_budget: 16 * 1024,
                    summary: "NEW_SUMMARY".to_string(),
                    messages_included: messages.len(),
                    usage: crate::event::UsageDelta::default(),
                })
            },
        )
        .await
        .unwrap()
        .unwrap();

        // The old summary record plus both summarized messages are drained.
        assert_eq!(result.summary, "NEW_SUMMARY");
        assert_eq!(result.first_kept_index, 3);
        assert_eq!(session.messages[0].content, "NEW_SUMMARY");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "c".repeat(40));
    }
    use crate::config::Config;
    use crate::sandbox::{Sandbox, SandboxPolicy};
    use crate::session::Session;

    struct LocalPricingServer {
        url: String,
        accepted: std::sync::mpsc::Receiver<()>,
        started: std::sync::mpsc::Receiver<Vec<u8>>,
        closed: std::sync::mpsc::Receiver<bool>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    }

    impl LocalPricingServer {
        fn finish(mut self) -> std::io::Result<()> {
            self.thread
                .take()
                .unwrap()
                .join()
                .expect("pricing fixture panicked")
        }
    }

    impl Drop for LocalPricingServer {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let result = thread.join();
                if !std::thread::panicking() {
                    result
                        .expect("pricing fixture panicked")
                        .expect("pricing fixture failed");
                }
            }
        }
    }

    fn pricing_fixture_io<T>(
        stop: &std::sync::atomic::AtomicBool,
        deadline: std::time::Instant,
        mut operation: impl FnMut() -> std::io::Result<T>,
    ) -> std::io::Result<Option<T>> {
        loop {
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(None);
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "pricing fixture phase deadline",
                ));
            }
            match operation() {
                Ok(value) => return Ok(Some(value)),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn spawn_local_pricing_server(response: Option<(u16, &'static str)>) -> LocalPricingServer {
        use std::io::{Read, Write};
        const PHASE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
        const MAX_HEADERS: usize = 16 * 1024;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted) = std::sync::mpsc::channel();
        let (started_tx, started) = std::sync::mpsc::channel();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let stop = &worker_stop;
            let Some((mut stream, _)) =
                pricing_fixture_io(stop, std::time::Instant::now() + PHASE_BUDGET, || {
                    listener.accept()
                })?
            else {
                return Ok(());
            };
            stream.set_nonblocking(true)?;
            let _ = accepted_tx.send(());
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let deadline = std::time::Instant::now() + PHASE_BUDGET;
            loop {
                let remaining = MAX_HEADERS - request.len();
                if remaining == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "pricing fixture headers exceed limit",
                    ));
                }
                let capacity = remaining.min(buffer.len());
                let Some(count) =
                    pricing_fixture_io(stop, deadline, || stream.read(&mut buffer[..capacity]))?
                else {
                    return Ok(());
                };
                if count == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "pricing client closed before sending headers",
                    ));
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = started_tx.send(request);
            if let Some((status, body)) = response {
                let reason = if status == 200 { "OK" } else { "Failure" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut pending = response.as_bytes();
                let deadline = std::time::Instant::now() + PHASE_BUDGET;
                while !pending.is_empty() {
                    let Some(count) = pricing_fixture_io(stop, deadline, || stream.write(pending))?
                    else {
                        return Ok(());
                    };
                    if count == 0 {
                        return Err(std::io::ErrorKind::WriteZero.into());
                    }
                    pending = &pending[count..];
                }
                return Ok(());
            }

            let deadline = std::time::Instant::now() + PHASE_BUDGET;
            let closed_cleanly = loop {
                match pricing_fixture_io(stop, deadline, || stream.read(&mut buffer)) {
                    Ok(Some(0)) => break true,
                    Ok(Some(_)) => {}
                    Ok(None) => return Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => break false,
                    Err(_) => break true,
                }
            };
            let _ = closed_tx.send(closed_cleanly);
            Ok(())
        });
        LocalPricingServer {
            url: format!("http://{address}/models"),
            accepted,
            started,
            closed,
            stop,
            thread: Some(thread),
        }
    }

    #[test]
    fn pricing_server_drop_closes_idle_partial_and_active_connections() {
        use std::io::{Read, Write};
        for request in [
            None,
            Some("GET /models HTTP/1.1\r\n"),
            Some("GET /models HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        ] {
            for unwind in [false, true] {
                let mut client = None;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let server = spawn_local_pricing_server(None);
                    let endpoint: std::net::SocketAddr = server
                        .url
                        .strip_prefix("http://")
                        .unwrap()
                        .strip_suffix("/models")
                        .unwrap()
                        .parse()
                        .unwrap();
                    if let Some(request) = request {
                        let mut stream = std::net::TcpStream::connect(endpoint).unwrap();
                        stream
                            .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        // Configure the read bound before teardown: macOS may
                        // reject socket options once the peer has closed.
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        stream.write_all(request.as_bytes()).unwrap();
                        client = Some(stream);
                        server
                            .accepted
                            .recv_timeout(std::time::Duration::from_secs(2))
                            .unwrap();
                        if request.ends_with("\r\n\r\n") {
                            server
                                .started
                                .recv_timeout(std::time::Duration::from_secs(2))
                                .unwrap();
                        }
                    }
                    if unwind {
                        panic!("injected pricing fixture failure");
                    }
                    drop(server);
                }));
                if unwind {
                    let panic = result.expect_err("fixture did not unwind");
                    assert_eq!(
                        panic.downcast_ref::<&str>(),
                        Some(&"injected pricing fixture failure")
                    );
                } else {
                    result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                }
                // What the fixture's teardown guarantees is asserted by the
                // teardown itself: the listener is moved into the worker
                // thread, and `Drop` joins that thread, so once the drop
                // returns the socket is closed and the worker's own result has
                // been checked.
                //
                // Deliberately not probed by connecting to the address again.
                // A released port is a machine-wide resource that is recycled
                // immediately, so "someone answers there" says nothing about
                // this fixture: under a parallel suite it is another test's
                // listener, and the probe fails about one run in three. What
                // the drop does to a connection that already exists is a
                // property this test owns, and is checked below.
                if let Some(mut stream) = client {
                    match stream.read(&mut [0; 1]) {
                        Ok(0) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) => {}
                        result => panic!("pricing stream survived fixture drop: {result:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn pricing_server_bounds_headers_and_rejects_incomplete_requests() {
        use std::io::Write;
        for size in [16 * 1024 - 1, 16 * 1024, 16 * 1024 + 1] {
            let server = spawn_local_pricing_server(Some((200, "{}")));
            let endpoint = server
                .url
                .strip_prefix("http://")
                .unwrap()
                .strip_suffix("/models")
                .unwrap();
            let mut client = std::net::TcpStream::connect(endpoint).unwrap();
            client
                .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = b"GET /models HTTP/1.1\r\nX-Padding: ".to_vec();
            request.resize(size - 4, b'x');
            request.extend_from_slice(b"\r\n\r\n");
            let _ = client.write_all(&request);
            let result = server.finish();
            if size <= 16 * 1024 {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
            }
        }
        for disconnect in [true, false] {
            let server = spawn_local_pricing_server(None);
            let endpoint = server
                .url
                .strip_prefix("http://")
                .unwrap()
                .strip_suffix("/models")
                .unwrap();
            let mut client = std::net::TcpStream::connect(endpoint).unwrap();
            client
                .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            client.write_all(b"GET /models HTTP/1.1\r\n").unwrap();
            server
                .accepted
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            if disconnect {
                client.shutdown(std::net::Shutdown::Write).unwrap();
            }
            let error = server.finish().unwrap_err();
            assert_eq!(
                error.kind(),
                if disconnect {
                    std::io::ErrorKind::UnexpectedEof
                } else {
                    std::io::ErrorKind::TimedOut
                }
            );
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

    fn local_pricing_provider(url: &str) -> crate::config::CustomProviderConfig {
        crate::config::CustomProviderConfig {
            provider_type: "openrouter".into(),
            inherit_builtin_key: false,
            base_url: url.strip_suffix("/models").unwrap().into(),
            api_key_env: None,
            danger_accept_invalid_certs: None,
            api_style: None,
            headers: std::collections::HashMap::new(),
            timeout_secs: None,
            connect_timeout_secs: None,
            stream_idle_timeout_secs: None,
            model: None,
        }
    }

    fn local_pricing_refresh(url: String) -> OpenRouterPricingRefresh {
        let custom =
            std::collections::HashMap::from([("openrouter".into(), local_pricing_provider(&url))]);
        OpenRouterPricingRefresh::start("test/model".into(), true, true, 128_000, async move {
            crate::provider::fetch_openrouter_pricing(None, &custom, None).await
        })
    }

    #[tokio::test]
    async fn pending_pricing_refresh_never_blocks_readiness_and_allows_relaunch() {
        for abandon in [false, true] {
            let server = spawn_local_pricing_server(None);
            let mut refresh = local_pricing_refresh(server.url.clone());
            recv_channel(
                &server.started,
                "pricing request did not reach delayed server",
            )
            .await;

            if abandon {
                drop(refresh);
            } else {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    refresh.finish_without_wait(),
                )
                .await
                .expect("readiness waited for the delayed pricing response");
                assert!(result.is_none(), "pending refresh returned a result");
            }
            assert!(
                recv_channel(&server.closed, "cancelled pricing connection stayed open").await,
                "cancelled pricing connection was not closed"
            );
            server.finish().unwrap();
        }

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
        replacement_server.finish().unwrap();
    }

    #[test]
    fn unpolled_pricing_reaper_releases_its_accounting() {
        let before = ACTIVE_OPENROUTER_PRICING_REAPERS.load(std::sync::atomic::Ordering::Acquire);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let handle = tokio::spawn(std::future::pending::<
                anyhow::Result<super::OpenRouterPricingMap>,
            >());
            handle.abort();
            super::reap_aborted_openrouter_pricing_refresh(handle);
            assert_eq!(
                ACTIVE_OPENROUTER_PRICING_REAPERS.load(std::sync::atomic::Ordering::Acquire),
                before + 1
            );
            // Do not yield: neither spawned future may be polled before shutdown.
        });
        drop(runtime);
        assert_eq!(
            ACTIVE_OPENROUTER_PRICING_REAPERS.load(std::sync::atomic::Ordering::Acquire),
            before
        );
    }

    #[cfg(feature = "multithread")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_non_cooperative_pricing_refresh_has_a_bounded_join() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut refresh =
            OpenRouterPricingRefresh::start("test/model".into(), true, true, 128_000, async move {
                let _ = started_tx.send(());
                // Dropping the sender also releases this poll if an assertion
                // fails, so runtime shutdown cannot strand a blocked worker.
                let _ = release_rx.recv();
                let _ = finished_tx.send(());
                Ok(std::collections::HashMap::new())
            });
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("pricing task did not start")
            .unwrap();

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                refresh.finish_without_wait()
            )
            .await
            .expect("readiness joined the unreleased pricing task")
            .is_none()
        );
        assert!(matches!(
            finished_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            ACTIVE_OPENROUTER_PRICING_REAPERS.load(std::sync::atomic::Ordering::Acquire),
            1,
            "unreleased pricing task lost its reaper"
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), finished_rx)
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
        const BODY: &str = r#"{"data":[
            {"id":"test/model","pricing":{"prompt":"0.000001","completion":"0.000002"},"context_length":64000},
            {"id":"nan","pricing":{"prompt":"NaN","completion":"0.000002"}},
            {"id":"negative","pricing":{"prompt":"0.000001","completion":"-0.000002"}},
            {"id":"infinity","pricing":{"prompt":"inf","completion":"-inf"},"context_length":32000},
            {"id":"overflow","pricing":{"prompt":"1e308","completion":"0.000002"}},
            {"id":"malformed","pricing":{"prompt":"invalid","completion":"0.000002"}},
            {"id":"free","pricing":{"prompt":"0","completion":"0"},"context_length":16000},
            {"id":"free-no-context","pricing":{"prompt":"0","completion":"0"}},
            {"id":"missing-output","pricing":{"prompt":"0.000001"},"context_length":24000},
            {"id":"numeric-rates","pricing":{"prompt":0,"completion":0},"context_length":12000},
            {"id":"context-only","context_length":8000},
            {"id":"zero-context","pricing":{"prompt":"0.000001","completion":"0.000002"},"context_length":0},
            {"id":"no-metadata"},
            {"id":"no-price-zero-context","context_length":0},
            {"id":"invalid-only","pricing":{"prompt":"NaN","completion":"inf"}}
        ]}"#;
        let server = spawn_local_pricing_server(Some((200, BODY)));
        let mut refresh = local_pricing_refresh(server.url.clone());
        recv_channel(
            &server.started,
            "pricing request did not reach success server",
        )
        .await;
        wait_for_pricing_refresh(&refresh).await;
        let infos = refresh.finish_without_wait().await.unwrap().unwrap();
        server.finish().unwrap();

        for (model, input, output, context) in [
            ("test/model", 1.0, 2.0, Some(64_000)),
            ("nan", 0.0, 2.0, None),
            ("negative", 1.0, 0.0, None),
            ("infinity", 0.0, 0.0, Some(32_000)),
            ("overflow", 0.0, 2.0, None),
            ("malformed", 0.0, 2.0, None),
            ("free", 0.0, 0.0, Some(16_000)),
            ("free-no-context", 0.0, 0.0, None),
            ("missing-output", 1.0, 0.0, Some(24_000)),
            ("numeric-rates", 0.0, 0.0, Some(12_000)),
            ("context-only", 0.0, 0.0, Some(8_000)),
            ("zero-context", 1.0, 2.0, None),
        ] {
            let info = infos.get(model).unwrap();
            assert_eq!(info.input_cost, input, "{model} input");
            assert_eq!(info.output_cost, output, "{model} output");
            assert_eq!(info.context_length, context, "{model} context");
            let prices = matches!(
                model,
                "test/model" | "zero-context" | "free" | "free-no-context"
            )
            .then_some((input, output));
            assert_eq!(info.known_prices(), prices, "{model} price presence");
            refresh.model = model.into();
            let mut session = Session::new("openrouter", model, 128_000, "");
            apply_openrouter_pricing_refresh_result(
                &mut session,
                &refresh,
                Some(Ok(infos.clone())),
            );
            assert_eq!(session.known_input_output_prices(), prices, "{model}");
            let (expected_input, expected_output) = prices.unwrap_or((0.0, 0.0));
            assert_eq!(session.input_token_cost, expected_input, "{model}");
            assert_eq!(session.output_token_cost, expected_output, "{model}");
            let account = crate::spend::Account::new(&session.id, Some(1.0), 0.0).unwrap();
            let snapshot = crate::spend::PriceSnapshot::from_session(&session, &Config::default());
            assert_eq!(
                account.admit(snapshot).is_ok(),
                prices.is_some(),
                "{model} capped admission"
            );
            assert_eq!(
                session.context_window,
                context.unwrap_or(128_000),
                "{model}"
            );
            let persisted = serde_json::to_string(&session).unwrap();
            let restored: Session = serde_json::from_str(&persisted).unwrap();
            assert_eq!(
                restored.known_input_output_prices(),
                prices,
                "{model} persisted presence"
            );
            assert_eq!(restored.input_token_cost, expected_input, "{model}");
            assert_eq!(restored.output_token_cost, expected_output, "{model}");
            assert_eq!(restored.context_window, session.context_window, "{model}");
        }
        assert_eq!(
            infos.len(),
            12,
            "entries without usable metadata must be omitted"
        );
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
                    input_token_cost: Some(0.0),
                    output_token_cost: Some(0.0),
                    cache_read_token_cost: None,
                    cache_write_token_cost: None,
                    cache_write_1h_token_cost: None,
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
        let mut server = spawn_local_pricing_server(None);
        server.url = server.url.replace("/models", "/gateway/v1/models");
        let mut config = local_pricing_provider(&server.url);
        config.timeout_secs = Some(1);
        config.headers.insert(
            "x-gateway-key".into(),
            "pricing-header-test-sentinel".into(),
        );
        let custom = std::collections::HashMap::from([("openrouter".into(), config)]);
        let mut refresh =
            OpenRouterPricingRefresh::start("test/model".into(), true, true, 128_000, async move {
                crate::provider::fetch_openrouter_pricing(
                    Some("pricing-bearer-test-sentinel"),
                    &custom,
                    None,
                )
                .await
            });
        let request = recv_channel(
            &server.started,
            "custom-timeout pricing request did not start",
        )
        .await;
        let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /gateway/v1/models http/1.1\r\n"));
        assert!(request.contains("\r\nauthorization: bearer pricing-bearer-test-sentinel\r\n"));
        assert!(request.contains("\r\nx-gateway-key: pricing-header-test-sentinel\r\n"));

        wait_for_pricing_refresh(&refresh).await;
        assert!(matches!(refresh.finish_without_wait().await, Some(Err(_))));
        assert!(
            recv_channel(&server.closed, "timed-out pricing connection stayed open").await,
            "timed-out pricing connection was not closed"
        );
        server.finish().unwrap();
    }

    #[test]
    fn startup_pricing_refresh_settles_before_prompt_error_propagation() {
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
            .find("startup::heap_future(|| startup.dispatch()).await")
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
    fn startup_probe_captures_policy_without_sharing_runtime_owners() {
        // Like Startup's publication/cleanup owner, this carrier is !Sync.
        struct Inputs {
            cli: Cli,
            cfg: crate::config::Config,
            owner: std::cell::RefCell<u8>,
        }
        let inputs = Inputs {
            cli: Cli {
                no_sandbox: true,
                ..Default::default()
            },
            cfg: Default::default(),
            owner: std::cell::RefCell::new(7),
        };
        let (general, worker) = run_startup_probes_concurrently(
            super::configured_sandbox_validation_probe(&inputs.cli, &inputs.cfg),
            || "captured worker status",
        )
        .unwrap();
        general.unwrap();
        assert_eq!(worker, "captured worker status");
        assert_eq!(*inputs.owner.borrow(), 7);
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
    fn startup_pricing_preserves_selected_aliases_but_raw_overrides_use_the_catalog() {
        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "provider": "anthropic", "model": "sonnet",
            "quick_models": { "sonnet": { "provider": "anthropic", "model": "claude-sonnet-5-5",
                "input_token_cost": 42.0, "output_token_cost": 84.0 } }
        }))
        .unwrap();
        let cli = Cli::default();
        let (provider, model) = cli.resolve_provider_and_model(&cfg);
        assert_eq!(
            (provider.as_str(), model.as_str()),
            ("anthropic", "claude-sonnet-5-5")
        );
        assert_eq!(
            super::resolve_startup_model_pricing(&cli, &cfg, &provider, &model, None),
            Some((42.0, 84.0))
        );
        let raw = Cli {
            model: Some(model.to_string()),
            ..Cli::default()
        };
        let catalog = crate::config::Config::catalog_input_output_cost(&provider, &model).unwrap();
        assert_ne!(catalog, (42.0, 84.0));
        assert_eq!(
            super::resolve_startup_model_pricing(&raw, &cfg, &provider, &model, None),
            Some(catalog)
        );
        let explicit = Cli {
            quick_model: Some("sonnet".into()),
            ..raw
        };
        assert_eq!(
            super::resolve_startup_model_pricing(&explicit, &cfg, &provider, &model, None),
            Some((42.0, 84.0))
        );

        let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
            "provider": "openrouter", "model": "selected",
            "quick_models": { "selected": { "provider": "openrouter", "model": "test/raw-env-model", "input_token_cost": 42.0, "output_token_cost": 84.0 },
                "a-other": { "provider": "openrouter", "model": "test/raw-env-model", "input_token_cost": 1.0, "output_token_cost": 2.0 } }
        })).unwrap();
        assert_eq!(
            super::resolve_startup_model_pricing(
                &cli,
                &cfg,
                "openrouter",
                "test/raw-env-model",
                None
            ),
            Some((42.0, 84.0))
        );
        assert_eq!(
            super::resolve_startup_model_pricing(
                &cli,
                &cfg,
                "openrouter",
                "test/raw-env-model",
                Some("test/raw-env-model")
            ),
            Some((1.0, 2.0))
        );
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

    #[tokio::test]
    async fn fork75_actual_startup_resume_creates_second_file_without_changing_source() {
        let root = crate::tests::OwnedTestDirectory::new("fork75-startup");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let paths = crate::paths::process_paths().unwrap();
        for continue_session in [false, true] {
            let mut saved = Session::new("openrouter", "saved-model", 128_000, "original");
            saved.working_dir = workspace.root().to_string_lossy().into();
            saved.add_message(crate::session::MessageRole::User, "retained turn");
            crate::session::storage::save_session(&saved).unwrap();
            let source_path = paths.sessions_dir().join(format!("{}.json", saved.id));
            let before = std::fs::read(&source_path).unwrap();
            let cli = Cli {
                api_key: Some("unused-test-key".into()),
                no_context_files: true,
                no_tools: true,
                no_sandbox: true,
                continue_session,
                session: (!continue_session).then(|| saved.id.to_string()),
                fork_session: true,
                ..Default::default()
            };
            let startup = super::Startup::init(
                cli,
                Config::default(),
                paths.clone(),
                workspace.clone(),
                false,
                false,
                false,
            )
            .await
            .unwrap();
            assert_ne!(startup.session.id, saved.id);
            assert_eq!(startup.session.name, "original (fork)");
            assert_eq!(startup.session.messages[0].content, "retained turn");
            let restored = crate::session::storage::load_session_exact(&startup.session.id)
                .unwrap()
                .unwrap();
            assert_eq!(restored.messages[0].content, "retained turn");
            assert_eq!(std::fs::read(&source_path).unwrap(), before);
            crate::session::lock::release_session(&startup.session.id);
            crate::session::lock::release_session(&saved.id);
        }
    }

    #[tokio::test]
    async fn session_resume_name_is_applied_and_persisted_unless_ephemeral() {
        let root = crate::tests::OwnedTestDirectory::new("startup-session-name");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        let paths = crate::paths::process_paths().unwrap();
        for continue_session in [false, true] {
            for ephemeral in [false, true] {
                let mut saved = Session::new("openrouter", "saved-model", 128_000, "old-name");
                saved.working_dir = workspace.root().to_string_lossy().into();
                saved.add_message(crate::session::MessageRole::User, "retained turn");
                crate::session::storage::save_session(&saved).unwrap();
                let cli = Cli {
                    api_key: Some("unused-test-key".into()),
                    no_context_files: true,
                    no_tools: true,
                    no_sandbox: true,
                    no_session: ephemeral,
                    continue_session,
                    session: (!continue_session).then(|| saved.id.to_string()),
                    name: Some("new-name".into()),
                    ..Default::default()
                };
                let startup = super::Startup::init(
                    cli,
                    Config::default(),
                    paths.clone(),
                    workspace.clone(),
                    false,
                    false,
                    false,
                )
                .await
                .unwrap();
                assert_eq!(startup.session.id, saved.id);
                assert_eq!(startup.session.name, "new-name");
                assert_eq!(startup.session.messages[0].content, "retained turn");
                let restored = crate::session::storage::load_session_exact(&saved.id)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    restored.name,
                    if ephemeral { "old-name" } else { "new-name" }
                );
                crate::session::lock::release_session(&saved.id);
                crate::session::storage::delete_session(&saved.id).unwrap();
            }
        }
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

#[cfg(test)]
mod heap_future_tests {
    use super::heap_future;

    #[test]
    fn heap_future_runs_the_future_it_builds_with_borrowed_and_owned_inputs() {
        let borrowed = String::from("stack");
        let owned = String::from("heap");
        let borrowed_ref = &borrowed;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = runtime.block_on(heap_future(move || async move {
            format!("{borrowed_ref}-{owned}")
        }));
        assert_eq!(result, "stack-heap");
        assert_eq!(borrowed, "stack");
    }
}

#[cfg(test)]
mod native_publication_startup_tests {
    use super::*;

    #[test]
    fn native_publication_actual_startup_idle_zero_cap_preserves_denial_on_one_mib() {
        std::thread::Builder::new()
            .name("native-startup-one-mib".into())
            .stack_size(1_048_576)
            .spawn(|| {
                let root = crate::tests::OwnedTestDirectory::new("native-startup-actual");
                let _environment = crate::tests::ScopedProcessEnv::set(&[
                    (
                        "ZS_CONFIG_DIR",
                        Some(root.path().join("config").into_os_string()),
                    ),
                    (
                        "ZS_DATA_DIR",
                        Some(root.path().join("data").into_os_string()),
                    ),
                    (
                        "ZS_STATE_DIR",
                        Some(root.path().join("state").into_os_string()),
                    ),
                    (
                        "ZS_CACHE_DIR",
                        Some(root.path().join("cache").into_os_string()),
                    ),
                ]);
                let workspace = std::sync::Arc::new(
                    crate::paths::WorkspaceBinding::capture(root.path()).unwrap(),
                );
                let paths = crate::paths::process_paths().unwrap();
                let mut saved = Session::new("anthropic", "claude-fixture", 32_000, "old");
                saved.working_dir = workspace.root().to_string_lossy().into();
                saved.add_message(crate::session::MessageRole::User, "unchanged prior turn");
                crate::session::storage::save_session(&saved).unwrap();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let cli = Cli {
                        api_key: Some("unused-native-key".into()),
                        print: true,
                        message: vec!["retained idle literal".into()],
                        session: Some(saved.id.to_string()),
                        no_context_files: true,
                        no_tools: true,
                        no_sandbox: true,
                        name: Some("idle renamed".into()),
                        ..Default::default()
                    };
                    let cfg = Config {
                        max_budget_usd: Some(0.0),
                        ..Default::default()
                    };
                    let startup = Startup::init(
                        cli.clone(),
                        cfg.clone(),
                        paths.clone(),
                        workspace.clone(),
                        false,
                        false,
                        false,
                    )
                    .await
                    .unwrap();
                    assert!(matches!(&startup.client, AnyClient::Anthropic(_)));
                    assert!(startup.native_startup.is_some());
                    assert!(startup.native_ticket.is_none());
                    assert_eq!(startup.session.spend_account.snapshot().cap, Some(0.0));
                    assert_eq!(startup.session.total_input_tokens, 0);
                    let loaded = crate::session::storage::load_session_exact(&saved.id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(loaded.name, "idle renamed");
                    assert_eq!(loaded.messages[0].content, "unchanged prior turn");
                    assert_eq!(
                        loaded.native_frontier.refusal(),
                        Some(crate::session::native_replay::RestartReason::InFlight)
                    );
                    let checkpoint = crate::session::storage::session_path(&saved.id).unwrap();
                    let before = std::fs::read(&checkpoint).unwrap();
                    drop(startup); // No model completion or healthy checkpoint is fabricated.
                    tokio::task::yield_now().await;
                    let ephemeral = Cli {
                        no_session: true,
                        name: None,
                        ..cli
                    };
                    let error = Startup::init(
                        ephemeral,
                        cfg,
                        paths.clone(),
                        workspace.clone(),
                        false,
                        false,
                        false,
                    )
                    .await
                    .err()
                    .unwrap();
                    assert!(
                        error
                            .to_string()
                            .contains("saved native context requires persistence")
                    );
                    let after = crate::session::storage::load_session_exact(&saved.id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(after.name, "idle renamed");
                    assert_eq!(std::fs::read(&checkpoint).unwrap(), before);
                });
                crate::session::lock::release_session(&saved.id);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
