#![deny(unsafe_code)]

mod acp_auth;
mod agent;
mod auth;
mod cli;
mod config;
mod context;
mod docs;
mod event;
mod extras;
mod fs;
mod git;
mod hex;
mod logging;
mod models_catalog;
mod paths;
mod permission;
mod pricing;
mod print;
mod process_creation;
mod product;
mod provider;
mod retry;
mod sandbox;
mod session;
mod setup;
mod startup;
mod ui;

#[cfg(test)]
mod tests;

use anyhow::Context;
use clap::Parser;
use std::io::IsTerminal;
use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    #[cfg(target_os = "windows")]
    if let Some(exit_code) = sandbox::windows::maybe_run_from_args() {
        std::process::exit(exit_code);
    }

    #[cfg(all(feature = "js", target_os = "windows"))]
    if let Some(exit_code) = sandbox::worker::maybe_run_windows_preflight_helper() {
        return Ok(exit_code);
    }

    #[cfg(all(feature = "js", target_os = "macos"))]
    if let Some(exit_code) = sandbox::worker::maybe_run_macos_hosted_lifecycle() {
        return Ok(exit_code);
    }

    #[cfg(all(feature = "js", target_os = "macos"))]
    if let Some(exit_code) = sandbox::worker::maybe_run_macos_guardian() {
        return Ok(exit_code);
    }

    #[cfg(feature = "js")]
    if let Some(exit_code) = extras::js::worker::maybe_run_internal_worker() {
        return Ok(exit_code);
    }

    let runtime = normal_runtime().context("failed to initialize the async runtime")?;
    runtime.block_on(run()).context(
        "This error might derive from an incomplete configuration: run `mini-agent --setup` to configure your providers and models interactively, or `mini-agent --tutor` to see the getting started guide",
    )?;
    Ok(ExitCode::SUCCESS)
}

fn normal_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    #[cfg(feature = "multithread")]
    let mut builder = {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(4);
        builder
    };
    #[cfg(not(feature = "multithread"))]
    let mut builder = tokio::runtime::Builder::new_current_thread();

    builder.enable_all().build().map_err(Into::into)
}

async fn run() -> anyhow::Result<()> {
    let result = run_inner().await;
    #[cfg(feature = "js")]
    {
        let shutdown = extras::js::supervisor::JsWorkerSupervisor::shutdown_shared().await;
        match (result, shutdown) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(anyhow::anyhow!(
                "failed to shut down JavaScript worker: {error}"
            )),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(shutdown)) => {
                Err(error.context(format!("JavaScript worker cleanup also failed: {shutdown}")))
            }
        }
    }
    #[cfg(not(feature = "js"))]
    result
}

async fn run_inner() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    #[cfg(all(feature = "loop", unix))]
    if cli.loop_verification_policy_check {
        extras::r#loop::verify_workflow_only_headless_relevance()?;
        println!("workflow-only headless loop verification check: PASS");
        return Ok(());
    }

    let workspace_root =
        std::env::current_dir().context("failed to resolve the startup workspace root")?;
    let workspace = std::sync::Arc::new(
        paths::WorkspaceBinding::capture(&workspace_root)
            .context("failed to bind the startup workspace root")?,
    );
    let app_paths = paths::AppPaths::from_process(Some(workspace_root))?;
    paths::install_process_paths(&app_paths)?;
    paths::prepare_storage_roots(&app_paths)?;

    #[cfg(feature = "js")]
    if cli.js_runtime_check {
        extras::js::verify_runtime(workspace).await?;
        println!("JS runtime check: PASS (2)");
        return Ok(());
    }

    if let Some(source) = cli.import_agent_skill.as_deref() {
        let imported = extras::skills::import_agent_skill(source, &app_paths)?;
        println!(
            "Agent Skill imported: {} digest={} path={} reimported={}",
            imported.manifest.name,
            imported.identity.digest,
            imported.install_path.display(),
            imported.reimported
        );
        return Ok(());
    }

    if cli.config_preservation_check {
        config::verify_config_preservation(&app_paths)?;
        println!("config preservation check: PASS");
        return Ok(());
    }

    if cli.project_config_trust_check {
        config::verify_project_config_trust()?;
        println!("project config trust check: PASS");
        return Ok(());
    }

    #[cfg(unix)]
    if cli.memory_editor_preservation_check {
        ui::slash::verify_memory_editor_preservation()?;
        println!("memory editor preservation check: PASS");
        return Ok(());
    }

    if cli.resume_provider_safety_check {
        startup::verify_resume_provider_safety()?;
        println!("session resume provider safety check: PASS");
        return Ok(());
    }

    if cli.acp_authentication_check {
        acp_auth::verify_tcp_authentication()?;
        println!("ACP TCP authentication check: PASS");
        return Ok(());
    }

    if cli.acp_permission_policy_check {
        permission::verify_acp_permission_policy().await?;
        println!("ACP headless permission policy check: PASS");
        return Ok(());
    }

    let is_interactive =
        !cli.print && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    #[cfg(feature = "acp")]
    let is_interactive = is_interactive && !cli.acp_enabled;
    #[cfg(feature = "loop")]
    let is_interactive = is_interactive && !cli.loop_mode;

    paths::converge_legacy_artifacts(&app_paths, is_interactive)?;
    logging::install_panic_hook();
    logging::init(&cli);

    let (mut cfg, is_first_startup) = config::load_with_paths(&app_paths, is_interactive);

    #[cfg(feature = "skills")]
    if cli.purge_learned_skill.is_some()
        || cli.compact_learned_skill_events
        || cli.learned_skill_feedback.is_some()
        || cli.import_learned_skill.is_some()
        || cli.install_learned_skill_seeds
        || cli.approve_learned_skill.is_some()
        || cli.reject_learned_skill.is_some()
        || cli.activate_learned_skill.is_some()
    {
        let feedback = cli.learned_skill_feedback.as_deref().map(|skill_id| {
            extras::js::skills::operations::FeedbackOperation {
                skill_id,
                invocation_id: cli.learned_skill_feedback_invocation.as_deref(),
                kind: cli
                    .learned_skill_feedback_kind
                    .as_deref()
                    .expect("clap requires feedback kind"),
                reason_code: cli
                    .learned_skill_feedback_reason
                    .as_deref()
                    .expect("clap requires feedback reason"),
                idempotency_key: cli
                    .learned_skill_feedback_key
                    .as_deref()
                    .expect("clap requires feedback key"),
            }
        });
        let library = cli
            .import_learned_skill
            .as_deref()
            .map(extras::js::skills::operations::LibraryOperation::Import)
            .or_else(|| {
                cli.install_learned_skill_seeds
                    .then_some(extras::js::skills::operations::LibraryOperation::InstallSeeds)
            })
            .or_else(|| {
                cli.approve_learned_skill
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::Approve)
            })
            .or_else(|| {
                cli.reject_learned_skill
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::Reject)
            })
            .or_else(|| {
                cli.activate_learned_skill
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::Activate)
            });
        extras::js::skills::operations::run(
            cli.purge_learned_skill.as_deref(),
            cli.compact_learned_skill_events,
            feedback,
            library,
            &app_paths,
            cfg.embedding.as_ref(),
        )?;
        return Ok(());
    }

    if cli.print_config {
        print::print_config(&cli, &cfg)?;
        return Ok(());
    }

    if cli.setup {
        match setup::run(&mut cfg)? {
            setup::SetupOutcome::Quit => return Ok(()),
            setup::SetupOutcome::LaunchAutoconfigure => {
                // Environment-backed credentials are resolved at runtime; fall through to launch.
            }
            setup::SetupOutcome::Launch => {
                // fall through to launch
            }
        }
    }

    if cli.tutor {
        return docs::show_get_started();
    }

    if cli.resume && cli.session.is_none() {
        print::print_sessions();
        return Ok(());
    }

    let version_changed = docs::ensure_global()?;
    // ── Hooks: load settings.json config, apply trust, install dispatcher ──
    // Done this early (before provider/API-key resolution) so `--hooks-test`
    // is a pure config/dispatch dry run that needs no API key and makes no
    // model call.
    #[cfg(feature = "hooks")]
    {
        crate::extras::hooks::init_dispatcher(crate::extras::hooks::trust::load_dispatcher(
            &app_paths,
            cli.no_hooks,
            !is_interactive,
            &cli.resolve_sandbox_backend(&cfg),
        ));

        if let Some(tool_name) = &cli.hooks_test {
            let tool_input: serde_json::Value = cli
                .hooks_test_input
                .as_deref()
                .map(|s| serde_json::from_str(s).unwrap_or(serde_json::Value::Null))
                .unwrap_or_else(|| serde_json::json!({}));
            println!(
                "{}",
                crate::extras::hooks::hooks_test_dry_run(tool_name, tool_input).await
            );
            return Ok(());
        }
    }

    let mut startup = startup::Startup::init(
        cli,
        cfg,
        app_paths,
        workspace,
        is_first_startup,
        version_changed,
        is_interactive,
    )
    .await?;

    // ACP mode skips feature initialization, so validate the shared process
    // sandbox contract before entering either execution surface.
    startup.preflight_startup_capabilities()?;

    // ACP mode: serve and exit before feature init
    #[cfg(feature = "acp")]
    if startup.cli.acp_enabled {
        return extras::acp::serve(startup.cli, startup.cfg, startup.context).await;
    }

    startup.start_openrouter_pricing_refresh();
    startup.init_features().await?;
    let prompts = startup.resolve_prompts().await;
    startup.finish_openrouter_pricing_refresh().await;
    prompts?;
    startup.dispatch().await
}
