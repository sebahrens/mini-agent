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
mod logging;
mod models_catalog;
mod paths;
mod permission;
mod pricing;
mod print;
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

#[cfg_attr(
    feature = "multithread",
    tokio::main(flavor = "multi_thread", worker_threads = 4)
)]
#[cfg_attr(not(feature = "multithread"), tokio::main(flavor = "current_thread"))]
async fn main() -> anyhow::Result<()> {
    run().await.context(
        "This error might derive from an incomplete configuration: run `mini-agent --setup` to configure your providers and models interactively, or `mini-agent --tutor` to see the getting started guide",
    )
}

async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    #[cfg(all(feature = "loop", unix))]
    if cli.loop_verification_policy_check {
        extras::r#loop::verify_workflow_only_headless_relevance()?;
        println!("workflow-only headless loop verification check: PASS");
        return Ok(());
    }

    let workspace_root =
        std::env::current_dir().context("failed to resolve the startup workspace root")?;
    let app_paths = paths::AppPaths::from_process(Some(workspace_root))?;
    paths::install_process_paths(&app_paths)?;
    paths::prepare_storage_roots(&app_paths)?;

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

    if cli.print_config {
        print::print_config(&cli, &cfg)?;
        return Ok(());
    }

    if cli.setup {
        match setup::run(&mut cfg)? {
            setup::SetupOutcome::Quit => return Ok(()),
            setup::SetupOutcome::LaunchAutoconfigure => {
                // autoconfigure was already applied in setup; fall through to launch
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
        is_first_startup,
        version_changed,
        is_interactive,
    )
    .await?;

    // ACP mode: serve and exit before feature init
    #[cfg(feature = "acp")]
    if startup.cli.acp_enabled {
        return extras::acp::serve(startup.cli, startup.cfg, startup.context).await;
    }

    startup.init_features().await?;
    startup.resolve_prompts().await?;
    startup.dispatch().await
}
