#![deny(unsafe_code)]

mod acp_auth;
mod agent;
mod auth;
mod byte_text;
mod cli;
mod config;
mod context;
mod diagnostics;
mod docs;
mod event;
mod extras;
mod fs;
mod git;
mod headless_control;
mod headless_media;
mod hex;
mod key_command;
mod logging;
mod models_catalog;
mod output_schema;
mod paths;
mod permission;
mod pricing;
mod print;
mod process_creation;
mod product;
mod project_root;
mod provider;
mod provider_http;
mod provider_listing;
mod question;
mod retry;
mod sandbox;
mod self_update;
mod session;
mod setup;
mod spend;
mod startup;
mod ui;
mod update;

#[cfg(test)]
mod tests;

use anyhow::Context;
use std::io::IsTerminal;
use std::process::ExitCode;

#[cfg(feature = "multithread")]
const ASYNC_WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

fn main() -> anyhow::Result<ExitCode> {
    // Linux bwrap network allowlist: relay helper, runs before anything else.
    #[cfg(unix)]
    if let Some(exit_code) = sandbox::net_forwarder::maybe_run_from_args() {
        return Ok(exit_code);
    }

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

    // Parse before the runtime starts. Clap's derived argument builder is the
    // largest single stack frame at startup (hundreds of KiB in an unoptimized
    // build); here it sits directly on `main` rather than on top of the
    // runtime and startup frames, all under the 1 MiB Windows default stack.
    // `mini-agent update` is handled before clap so that only its exact forms
    // are a subcommand; `mini-agent update the docs` stays an ordinary prompt.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match self_update::parse_update_invocation(&argv) {
        Some(self_update::UpdateInvocation::Run(update)) => {
            let runtime = normal_runtime().context("failed to initialize the async runtime")?;
            let code = runtime.block_on(self_update::run(update));
            return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
        }
        Some(self_update::UpdateInvocation::Help) => {
            println!("{}", self_update::UPDATE_USAGE);
            return Ok(ExitCode::SUCCESS);
        }
        Some(self_update::UpdateInvocation::Invalid(message)) => {
            eprintln!("error: {message}\n\n{}", self_update::UPDATE_USAGE);
            return Ok(ExitCode::from(2));
        }
        None => {}
    }
    let cli = cli::Cli::parse_command_line();
    if let Some(shell) = cli.completions {
        match cli::write_completions(shell, &mut std::io::stdout().lock()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(error) => return Err(error).context("failed to write shell completions"),
        }
        return Ok(ExitCode::SUCCESS);
    }
    if cli.is_headless() {
        // Headless turns nest the provider stream, tool execution and output
        // futures on whichever thread drives the runtime, and each round of
        // relief, compaction and restart adds frames. Run them on a sized
        // thread so the platform's main-thread default (1 MiB on Windows)
        // does not bound a turn's depth.
        return cli::on_roomy_stack(move || run_on_runtime(cli, true));
    }
    run_on_runtime(cli, false)
}

fn run_on_runtime(cli: cli::Cli, runtime_headless: bool) -> anyhow::Result<ExitCode> {
    let runtime = normal_runtime().context("failed to initialize the async runtime")?;
    // Runtime adapters otherwise copy the entire startup future into their
    // own stack frames before polling it.
    #[cfg(feature = "mcp")]
    let mcp_server = cli.mcp_server;
    // A `-p` run with machine output always ends with one result record, even
    // when startup fails before any turn runs (no credential, bad session).
    let machine_output_cli =
        (cli.print && cli.output_format() != cli::OutputFormat::Text).then(|| cli.clone());
    let result = runtime.block_on(Box::pin(run(cli)));
    if let (Err(error), Some(cli)) = (&result, &machine_output_cli)
        && !print::headless_result_emitted()
        && !error.is::<question::QuestionRequired>()
        && !is_goal_exit(error)
    {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let _ = runtime.block_on(Box::pin(print::emit_headless_failure(
            cli,
            &root,
            &format!("{error:#}"),
            None,
        )));
    }
    #[cfg(feature = "mcp")]
    if mcp_server {
        // serve() has retired its owned runs. Tokio's standard-I/O adapters
        // can still have uncancellable blocking reads/writes in the pool;
        // waiting for that pool on Runtime::drop would keep the server alive
        // after client EOF or a backpressured writer deadline.
        runtime.shutdown_timeout(std::time::Duration::from_millis(250));
        return result.map(|()| ExitCode::SUCCESS);
    }
    // A headless run that stopped waiting for stalled cleanup (a second
    // interrupt, or the grace deadline) has already persisted what it had.
    // Exit now: dropping the runtime would wait for that work forever.
    if print::headless_force_stopped() {
        if let Err(error) = &result {
            report_runtime_headless_error(&runtime, format!("Error: {error:?}\n"));
        }
        print::exit_force_stopped();
    }
    if let Err(error) = &result
        && error.is::<startup::HeadlessUsageError>()
    {
        report_runtime_headless_error(&runtime, format!("Error: {error}\n"));
        if print::headless_force_stopped() {
            print::exit_force_stopped();
        }
        return Ok(ExitCode::from(2));
    }
    if result
        .as_ref()
        .is_err_and(|error| error.is::<question::QuestionRequired>())
    {
        report_runtime_headless_error(
            &runtime,
            "awaiting your answer; send a new user message to continue\n".into(),
        );
        return Ok(ExitCode::from(22));
    }
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        // A goal that stopped short is an outcome, not a crash: it exits with
        // its own code so a script can branch on it, and without an error
        // message claiming the turn failed.
        #[cfg(feature = "goal")]
        Err(error) => match error.downcast::<print::HeadlessGoalExit>() {
            Ok(exit) => Ok(ExitCode::from(exit.0.exit_code() as u8)),
            Err(error) => finish_runtime_error(&runtime, error, runtime_headless),
        },
        #[cfg(not(feature = "goal"))]
        Err(error) => finish_runtime_error(&runtime, error, runtime_headless),
    }
}

fn is_goal_exit(error: &anyhow::Error) -> bool {
    #[cfg(feature = "goal")]
    {
        error.is::<print::HeadlessGoalExit>()
    }
    #[cfg(not(feature = "goal"))]
    {
        let _ = error;
        false
    }
}

fn report_runtime_headless_error(runtime: &tokio::runtime::Runtime, message: String) {
    let (message, _) = print::bounded_progress(&message, print::progress_bytes());
    let _ = runtime.block_on(Box::pin(print::headless_stderr_async(message)));
}

fn finish_runtime_error(
    runtime: &tokio::runtime::Runtime,
    error: anyhow::Error,
    headless: bool,
) -> anyhow::Result<ExitCode> {
    if !headless {
        return Err(error);
    }
    report_runtime_headless_error(runtime, format!("Error: {error:?}\n"));
    if print::headless_force_stopped() {
        print::exit_force_stopped();
    }
    Ok(ExitCode::from(1))
}

fn normal_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    #[cfg(feature = "multithread")]
    let mut builder = {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        // Debug all-features builds retain substantially larger provider and
        // tool futures than optimized builds. Tokio's implicit worker-stack
        // default is too small for those futures on WSL, so make the same
        // cross-platform budget used by the test matrix part of the runtime
        // contract instead of relying on an ambient RUST_MIN_STACK.
        builder
            .worker_threads(4)
            .thread_stack_size(ASYNC_WORKER_STACK_SIZE);
        builder
    };
    #[cfg(not(feature = "multithread"))]
    let mut builder = tokio::runtime::Builder::new_current_thread();

    builder.enable_all().build().map_err(Into::into)
}

async fn run(cli: cli::Cli) -> anyhow::Result<()> {
    // Keep the large startup state out of this wrapper's future as CLI grows.
    let result = startup::heap_future(|| run_inner(cli)).await;
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

/// A command that does one thing and exits instead of starting a session.
///
/// Variants are listed in the order `run_inner` has always tried them, and
/// [`one_shot_command`] returns the first requested one, so adding a flag means
/// adding a variant, its [`OneShot::needs`] stage and an arm in
/// [`run_one_shot`], not choosing a position in a 600-line function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OneShot {
    ConfigSchema,
    #[cfg(all(feature = "loop", unix))]
    LoopPolicyCheck,
    Login,
    Logout,
    Doctor,
    #[cfg(feature = "js")]
    JsRuntimeCheck,
    #[cfg(feature = "agent-skills")]
    ImportAgentSkill,
    SelfCheck,
    #[cfg(feature = "mcp")]
    McpServer,
    Tutor,
    #[cfg(feature = "mcp")]
    McpLogin,
    #[cfg(feature = "git-worktree")]
    GitRuntimeCheck,
    #[cfg(feature = "skills")]
    LearnedSkill,
    PrintConfig,
}

/// How much startup has happened when a one-shot command runs. The order is
/// the order of the stages in `run_inner`, so a command only ever sees what
/// its stage has set up and never starts work it will drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Needs {
    /// Nothing: not even argument handling.
    Nothing,
    /// Arguments validated; no paths or configuration.
    Arguments,
    /// Process paths and the workspace binding are installed.
    Paths,
    /// Storage roots exist; configuration is not loaded.
    Storage,
    /// Configuration is loaded and overridden, before diagnostics are added.
    Config,
    /// Configuration plus the notices that follow its load.
    ConfigNotices,
}

impl OneShot {
    fn needs(self) -> Needs {
        match self {
            Self::ConfigSchema => Needs::Nothing,
            #[cfg(all(feature = "loop", unix))]
            Self::LoopPolicyCheck => Needs::Arguments,
            Self::Login | Self::Logout | Self::Doctor => Needs::Paths,
            #[cfg(feature = "js")]
            Self::JsRuntimeCheck => Needs::Storage,
            #[cfg(feature = "agent-skills")]
            Self::ImportAgentSkill => Needs::Storage,
            Self::SelfCheck | Self::Tutor => Needs::Storage,
            #[cfg(feature = "mcp")]
            Self::McpServer => Needs::Storage,
            #[cfg(feature = "mcp")]
            Self::McpLogin => Needs::Config,
            #[cfg(feature = "git-worktree")]
            Self::GitRuntimeCheck => Needs::ConfigNotices,
            #[cfg(feature = "skills")]
            Self::LearnedSkill => Needs::ConfigNotices,
            Self::PrintConfig => Needs::ConfigNotices,
        }
    }
}

/// The one-shot command the arguments ask for, if any: the first in
/// [`OneShot`] order. `--tutor` with `--setup` is not one-shot (setup runs
/// first, then launches), so it is classified later by `run_inner`.
fn one_shot_command(cli: &cli::Cli) -> Option<OneShot> {
    if cli.config_schema {
        return Some(OneShot::ConfigSchema);
    }
    #[cfg(all(feature = "loop", unix))]
    if cli.loop_verification_policy_check {
        return Some(OneShot::LoopPolicyCheck);
    }
    if cli.login.is_some() {
        return Some(OneShot::Login);
    }
    if cli.logout.is_some() {
        return Some(OneShot::Logout);
    }
    if cli.doctor {
        return Some(OneShot::Doctor);
    }
    #[cfg(feature = "js")]
    if cli.js_runtime_check {
        return Some(OneShot::JsRuntimeCheck);
    }
    #[cfg(feature = "agent-skills")]
    if cli.import_agent_skill.is_some() {
        return Some(OneShot::ImportAgentSkill);
    }
    if self_check_requested(cli) {
        return Some(OneShot::SelfCheck);
    }
    #[cfg(feature = "mcp")]
    if cli.mcp_server {
        return Some(OneShot::McpServer);
    }
    if cli.tutor && !cli.setup {
        return Some(OneShot::Tutor);
    }
    #[cfg(feature = "mcp")]
    if cli.mcp_login.is_some() {
        return Some(OneShot::McpLogin);
    }
    #[cfg(feature = "git-worktree")]
    if cli.git_runtime_check {
        return Some(OneShot::GitRuntimeCheck);
    }
    #[cfg(feature = "skills")]
    if cli.has_learned_skill_command() {
        return Some(OneShot::LearnedSkill);
    }
    if cli.print_config {
        return Some(OneShot::PrintConfig);
    }
    None
}

/// What a one-shot command may use, filled in as startup progresses.
struct OneShotEnv<'a> {
    app_paths: &'a paths::AppPaths,
    workspace: &'a std::sync::Arc<paths::WorkspaceBinding>,
    config_overrides: &'a [toml::Value],
    cfg: Option<&'a config::Config>,
}

/// The one-shot commands that need no startup state at all: they run from the
/// arguments alone, at the stage `needs` names.
fn run_argument_one_shot(command: Option<OneShot>, stage: Needs) -> anyhow::Result<bool> {
    let Some(command) = command.filter(|command| command.needs() == stage) else {
        return Ok(false);
    };
    match command {
        OneShot::ConfigSchema => {
            use std::io::Write as _;
            serde_json::to_writer_pretty(std::io::stdout().lock(), &config::schema::generate())?;
            std::io::stdout().lock().write_all(b"\n")?;
            Ok(true)
        }
        #[cfg(all(feature = "loop", unix))]
        OneShot::LoopPolicyCheck => {
            extras::r#loop::verify_workflow_only_headless_relevance()?;
            println!("workflow-only headless loop verification check: PASS");
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Run `command` if it belongs to the stage `run_inner` has reached.
async fn run_one_shot_at(
    command: Option<OneShot>,
    stage: Needs,
    cli: &cli::Cli,
    env: &OneShotEnv<'_>,
) -> anyhow::Result<bool> {
    match command.filter(|command| command.needs() == stage) {
        Some(command) => run_one_shot(command, cli, env).await.map(|()| true),
        None => Ok(false),
    }
}

async fn run_one_shot(
    command: OneShot,
    cli: &cli::Cli,
    env: &OneShotEnv<'_>,
) -> anyhow::Result<()> {
    match command {
        // Run before any paths exist, by `run_argument_one_shot`.
        OneShot::ConfigSchema => unreachable!("handled before startup state exists"),
        #[cfg(all(feature = "loop", unix))]
        OneShot::LoopPolicyCheck => unreachable!("handled before startup state exists"),
        OneShot::Login => {
            let provider = cli.login.as_deref().expect("classified from --login");
            if provider.eq_ignore_ascii_case("copilot") {
                anyhow::ensure!(
                    cli.login_account.is_none(),
                    "Copilot stores one account per GitHub host; omit --login-account"
                );
                startup::heap_future(auth::copilot::sign_in).await?;
                println!(
                    "Copilot sign-in saved. Start mini-agent with --provider copilot --model MODEL to use it."
                );
            } else {
                startup::heap_future(|| {
                    auth::oauth::sign_in(provider, cli.login_account.as_deref())
                })
                .await?;
                println!(
                    "ChatGPT sign-in saved. Start mini-agent with --provider openai to use it."
                );
            }
            Ok(())
        }
        OneShot::Logout => {
            let provider = cli.logout.as_deref().expect("classified from --logout");
            let message = if provider.eq_ignore_ascii_case("copilot") {
                auth::copilot::logout().await?
            } else {
                anyhow::ensure!(
                    provider.eq_ignore_ascii_case("openai"),
                    "Supported provider logins: openai, copilot"
                );
                auth::oauth::logout().await?
            };
            println!("{message}");
            Ok(())
        }
        OneShot::Doctor => {
            let (app_paths, workspace, config_overrides) =
                (env.app_paths, env.workspace, env.config_overrides);
            git::global_snapshot::wait_initialized().await;
            let mut cfg = config::load_for_diagnostics_in_workspace(app_paths, workspace);
            config::layers::apply_overrides(&mut cfg, config_overrides)
                .map_err(anyhow::Error::msg)?;
            context::agents::set_claude_project_agents(cfg.resolve_claude_project_agents());
            context::agents::set_claude_user_agents(cfg.resolve_claude_user_agents());
            let mut report =
                diagnostics::DoctorReport::capture(cli, &cfg, app_paths, None, None, true);
            report.section("Terminal");
            use std::io::{IsTerminal as _, Write as _};
            report.row(
                "stdout terminal",
                std::io::stdout().is_terminal().to_string(),
            );
            report.row("keyboard and bracketed paste", "not queried or enabled");
            report.row(
                "true color",
                "not negotiated; environment hints are not capability proof",
            );
            std::io::stdout()
                .lock()
                .write_all(report.render().as_bytes())?;
            let broken = diagnostics::error_class_warnings(&cfg.load_warnings);
            anyhow::ensure!(
                broken.is_empty(),
                "doctor found {} configuration problem(s) that prevent a layer from loading (exit 1)",
                broken.len()
            );
            Ok(())
        }
        #[cfg(feature = "js")]
        OneShot::JsRuntimeCheck => {
            let workspace = env.workspace.clone();
            git::global_snapshot::wait_initialized().await;
            startup::heap_future(|| extras::js::verify_runtime(workspace)).await?;
            println!("JS runtime check: PASS (2)");
            Ok(())
        }
        #[cfg(feature = "agent-skills")]
        OneShot::ImportAgentSkill => {
            let app_paths = env.app_paths;
            let source = cli
                .import_agent_skill
                .as_deref()
                .expect("classified from --import-agent-skill");
            let imported = extras::skills::import_agent_skill(source, app_paths)?;
            println!(
                "Agent Skill imported: {} digest={} identity_version={} entries={} files={} \
             bytes={} path={} reimported={}",
                imported.manifest.name,
                imported.identity.digest,
                imported.identity.version,
                imported.identity.entries,
                imported.identity.files,
                imported.identity.expanded_bytes,
                imported.install_path.display(),
                imported.reimported
            );
            for diagnostic in &imported.manifest.diagnostics {
                println!("Agent Skill note: {diagnostic}");
            }
            Ok(())
        }
        #[cfg(feature = "mcp")]
        OneShot::McpServer => {
            #[cfg(feature = "acp")]
            if cli.acp_enabled {
                return Err(startup::HeadlessUsageError(
                    "--mcp-server cannot be combined with --acp".to_owned(),
                )
                .into());
            }
            #[cfg(feature = "loop")]
            if cli.loop_mode {
                return Err(startup::HeadlessUsageError(
                    "--mcp-server cannot be combined with --loop".to_owned(),
                )
                .into());
            }
            extras::mcp::server::serve_stdio().await
        }
        #[cfg(feature = "mcp")]
        OneShot::McpLogin => {
            let cfg = env.cfg.expect("config stage");
            let server = cli
                .mcp_login
                .as_deref()
                .expect("classified from --mcp-login");
            let message =
                startup::heap_future(|| extras::mcp::oauth::cli_login(server, cfg)).await?;
            println!("{message}");
            Ok(())
        }
        #[cfg(feature = "git-worktree")]
        OneShot::GitRuntimeCheck => {
            let cfg = env.cfg.expect("config stage");
            let workspace = env.workspace.clone();
            // The Git tool reads the startup-frozen global Git snapshot.
            git::global_snapshot::wait_initialized().await;
            let backend = cli.resolve_sandbox_backend(cfg);
            let sandbox = sandbox::Sandbox::new(cli.resolve_sandbox(cfg), &backend)
                .with_general_profile(
                    cli.resolve_sandbox_profile(cfg),
                    cfg.sandbox_read_roots.clone(),
                    cfg.sandbox_write_roots.clone(),
                )
                .with_windows_appcontainer_roots(
                    cli.resolve_windows_appcontainer_read_roots(cfg),
                    cli.resolve_windows_appcontainer_write_roots(cfg),
                )
                .with_bwrap_read_roots(cli.resolve_sandbox_toolchain_roots(cfg))
                .with_workspace_binding(workspace.clone());
            let verification =
                startup::heap_future(|| git::tool::GitTool::verify_runtime(workspace, sandbox))
                    .await
                    .map_err(anyhow::Error::msg)?;
            println!("Git runtime check: PASS ({})", verification["operation"]);
            Ok(())
        }
        OneShot::PrintConfig => {
            let cfg = env.cfg.expect("config stage");
            print::print_config(cli, cfg)?;
            Ok(())
        }
        OneShot::SelfCheck => {
            run_self_check(cli, env.app_paths).await?;
            Ok(())
        }
        OneShot::Tutor => docs::show_get_started(),
        #[cfg(feature = "skills")]
        OneShot::LearnedSkill => {
            run_learned_skill_command(cli, env.cfg.expect("config stage"), env.app_paths).await?;
            Ok(())
        }
    }
}

/// Run the verification-only `--*-check` flags that need nothing but the
/// process paths. Returns whether one ran, so the caller exits.
fn self_check_requested(cli: &cli::Cli) -> bool {
    #[allow(unused_mut)]
    let mut requested = cli.config_preservation_check
        || cli.project_config_trust_check
        || cli.resume_provider_safety_check
        || cli.acp_authentication_check
        || cli.acp_permission_policy_check;
    #[cfg(unix)]
    {
        requested |= cli.memory_editor_preservation_check;
    }
    requested
}

async fn run_self_check(cli: &cli::Cli, app_paths: &paths::AppPaths) -> anyhow::Result<bool> {
    if cli.config_preservation_check {
        config::verify_config_preservation(app_paths)?;
        println!("config preservation check: PASS");
        return Ok(true);
    }

    if cli.project_config_trust_check {
        config::verify_project_config_trust()?;
        println!("project config trust check: PASS");
        return Ok(true);
    }

    #[cfg(unix)]
    if cli.memory_editor_preservation_check {
        ui::slash::verify_memory_editor_preservation()?;
        println!("memory editor preservation check: PASS");
        return Ok(true);
    }

    if cli.resume_provider_safety_check {
        startup::verify_resume_provider_safety()?;
        println!("session resume provider safety check: PASS");
        return Ok(true);
    }

    if cli.acp_authentication_check {
        acp_auth::verify_tcp_authentication()?;
        println!("ACP TCP authentication check: PASS");
        return Ok(true);
    }

    if cli.acp_permission_policy_check {
        startup::heap_future(permission::verify_acp_permission_policy).await?;
        println!("ACP headless permission policy check: PASS");
        return Ok(true);
    }

    Ok(false)
}

/// Run the learned-skill operator commands (store-only, plus distillation,
/// the only one that reaches a provider). Returns whether one ran.
#[cfg(feature = "skills")]
async fn run_learned_skill_command(
    cli: &cli::Cli,
    cfg: &config::Config,
    app_paths: &paths::AppPaths,
) -> anyhow::Result<bool> {
    // Learned-skill operator commands may launch contained workers.
    git::global_snapshot::wait_initialized().await;
    extras::js::skills::operations::set_json_output(cli.learned_skill_json);

    if cli.learned_skill_stats {
        extras::js::skills::operations::print_skill_stats(app_paths)?;
        return Ok(true);
    }

    if cli.list_learned_skill_proposals {
        extras::js::skills::operations::print_proposal_queue(app_paths)?;
        return Ok(true);
    }

    if let Some(proposal_id) = cli.learned_skill_proposal.as_deref() {
        extras::js::skills::operations::print_proposal(app_paths, proposal_id)?;
        return Ok(true);
    }

    if cli.learned_skill_feedback_record.is_some()
        || cli.list_learned_skill_feedback.is_some()
        || cli.correct_learned_skill_feedback.is_some()
    {
        extras::js::skills::operations::run_feedback_management(
            cli.learned_skill_feedback_record.as_deref(),
            cli.list_learned_skill_feedback.as_deref(),
            cli.learned_skill_feedback_after.as_deref(),
            cli.correct_learned_skill_feedback.as_deref(),
            app_paths,
        )?;
        return Ok(true);
    }

    // Distillation is the only learned-skill command that reaches a provider,
    // so it sits apart from the store-only operator funnel below. It still
    // writes nothing to the library: its output is a package file the operator
    // reviews and then hands to `--import-learned-skill`.
    if let Some(target) = cli.distill_learned_skill.as_deref() {
        let [session_id, tool_call_id] = target else {
            anyhow::bail!("--distill-learned-skill takes exactly a session id and a tool call id");
        };
        startup::heap_future(|| {
            extras::js::skills::distill::run(cli, cfg, app_paths, session_id, tool_call_id)
        })
        .await?;
        return Ok(true);
    }

    if cli.purge_learned_skill.is_some()
        || cli.compact_learned_skill_events
        || cli.learned_skill_feedback.is_some()
        || cli.import_learned_skill.is_some()
        || cli.install_learned_skill_seeds
        || cli.approve_learned_skill.is_some()
        || cli.reject_learned_skill.is_some()
        || cli.activate_learned_skill.is_some()
        || cli.promote_learned_skill.is_some()
        || cli.retire_learned_skill.is_some()
        || cli.reevaluate_learned_skill.is_some()
        || cli.list_learned_skill_suites
        || cli.disable_learned_skill_suite.is_some()
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
            })
            .or_else(|| {
                cli.promote_learned_skill
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::Promote)
            })
            .or_else(|| {
                cli.retire_learned_skill
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::Retire)
            })
            .or_else(|| {
                cli.reevaluate_learned_skill
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::Reevaluate)
            })
            .or_else(|| {
                cli.list_learned_skill_suites
                    .then_some(extras::js::skills::operations::LibraryOperation::ListSuites)
            })
            .or_else(|| {
                cli.disable_learned_skill_suite
                    .as_deref()
                    .map(extras::js::skills::operations::LibraryOperation::DisableSuite)
            });
        let purge = cli.purge_learned_skill.as_deref().map(|skill_id| {
            extras::js::skills::operations::PurgeOperation {
                skill_id,
                force: cli.purge_learned_skill_force,
            }
        });
        extras::js::skills::operations::run(
            purge,
            cli.compact_learned_skill_events,
            feedback,
            library,
            app_paths,
            cfg.embedding.as_ref(),
        )?;
        return Ok(true);
    }
    Ok(false)
}

async fn run_inner(mut cli: cli::Cli) -> anyhow::Result<()> {
    let one_shot = one_shot_command(&cli);
    if run_argument_one_shot(one_shot, Needs::Nothing)? {
        return Ok(());
    }
    let config_overrides = config::layers::parse_cli_overrides(&cli.config_overrides)
        .map_err(|message| anyhow::Error::new(startup::HeadlessUsageError(message)))?;
    if let Err(error) = output_schema::prepare(&mut cli) {
        print::emit_headless_preflight_failure(&cli, std::path::Path::new("."), &error.to_string())
            .await?;
        return Err(error);
    }
    // Reject malformed seeds before provider/operator execution or session paths.
    permission::cli_rules::config(&cli)?;
    if run_argument_one_shot(one_shot, Needs::Arguments)? {
        return Ok(());
    }
    let launch_dir =
        std::env::current_dir().context("failed to resolve the startup workspace root")?;
    // Project-root discovery: opt-in (user config or --config) move to the
    // nearest .git ancestor; otherwise say which root assets were not loaded.
    let user_config = paths::AppPaths::from_process(Some(launch_dir.clone()))
        .ok()
        .map(|early| config::pick_existing(&early.config_dir));
    let workspace_root = match project_root::apply(
        &launch_dir,
        user_config.as_deref(),
        &config_overrides,
    )
    .context("failed to move to the project root")?
    {
        Some(root) => {
            // The process directory is now the root: pin the launch-relative
            // file arguments to where the command was typed.
            cli.absolutize_launch_paths(&launch_dir);
            eprintln!(
                "notice: project root {} (started in {}): project config, MCP servers, agents, skills, instructions and memory come from the root",
                root.display(),
                launch_dir.display()
            );
            root
        }
        None => {
            if let Some(notice) = project_root::skipped_assets_notice(
                &launch_dir,
                paths::process_home_dir().as_deref(),
            ) {
                eprintln!("notice: {notice}");
            }
            launch_dir
        }
    };
    let workspace = std::sync::Arc::new(
        paths::WorkspaceBinding::capture(&workspace_root)
            .context("failed to bind the startup workspace root")?,
    );
    let app_paths = paths::AppPaths::from_process(Some(workspace_root))?;
    paths::install_process_paths(&app_paths)?;
    config::managed::install().map_err(anyhow::Error::msg)?;
    // Freeze operator Git source authority before hooks/MCP/project/model work.
    // Git retains a closed diagnostic if capture fails; other modes can run.
    // The capture overlaps read-only config/docs/context loading and is joined
    // (`wait_initialized`) before any hook, MCP, sandbox or project effect.
    git::global_snapshot::begin(workspace.root().to_path_buf());
    macro_rules! one_shot_stage {
        ($stage:expr, $cfg:expr) => {
            run_one_shot_at(
                one_shot,
                $stage,
                &cli,
                &OneShotEnv {
                    app_paths: &app_paths,
                    workspace: &workspace,
                    config_overrides: &config_overrides,
                    cfg: $cfg,
                },
            )
            .await?
        };
    }
    if one_shot_stage!(Needs::Paths, None) {
        return Ok(());
    }
    paths::prepare_storage_roots(&app_paths)?;
    if cli.no_session {
        paths::disable_session_artifacts();
    }
    if one_shot_stage!(Needs::Storage, None) {
        return Ok(());
    }

    // Gated with the flag in `cli.rs`: the catalog, index and loader that read an
    // installed tree only exist in a `agent-skills` build, so importing anywhere else
    // would validate and install a tree nothing ever reads.
    let is_interactive =
        !cli.print && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    #[cfg(feature = "acp")]
    let is_interactive = is_interactive && !cli.acp_enabled;
    #[cfg(feature = "loop")]
    let is_interactive = is_interactive && !cli.loop_mode;

    anyhow::ensure!(
        !cli.setup || (std::io::stdin().is_terminal() && std::io::stdout().is_terminal()),
        "--setup needs an interactive terminal; edit {} or set the provider's API key environment variable instead",
        config::config_file_path().display()
    );
    paths::converge_legacy_artifacts(&app_paths, is_interactive)?;
    logging::install_panic_hook();
    logging::init(&cli, is_interactive);

    // Commands that only report state never ask for project trust, and
    // `--print-config` never creates the default config file.
    config::set_create_default_config(!cli.print_config);
    print::arm_fatal_config_record(&cli);
    let (mut cfg, is_first_startup) = config::load_in_workspace(
        &app_paths,
        &workspace,
        is_interactive && !cli.is_informational_command(),
    );
    context::agents::set_claude_project_agents(cfg.resolve_claude_project_agents());
    context::agents::set_claude_user_agents(cfg.resolve_claude_user_agents());

    // Headless and ACP runs never prompt: personas stay inert, so say so once.
    if !is_interactive && let Some(notice) = context::agents::untrusted_personas_notice(&workspace)
    {
        eprintln!("notice: {notice}");
    }

    let loaded_warning_count = cfg.load_warnings.len();
    config::layers::apply_overrides(&mut cfg, &config_overrides).map_err(anyhow::Error::msg)?;
    sandbox::set_command_timeout_secs(
        cfg.command_timeout_secs
            .unwrap_or(sandbox::DEFAULT_COMMAND_TIMEOUT_SECS),
    );
    context::agents::set_claude_project_agents(cfg.resolve_claude_project_agents());
    context::agents::set_claude_user_agents(cfg.resolve_claude_user_agents());
    provider::set_provider_timeouts(cfg.provider_timeouts);
    provider::set_cli_fallback_model(cli.fallback_model.clone());
    for warning in &cfg.load_warnings[loaded_warning_count..] {
        eprintln!("warning: {warning}");
    }
    if one_shot_stage!(Needs::Config, Some(&cfg)) {
        return Ok(());
    }
    #[cfg(feature = "lsp")]
    for warning in cfg.lsp_config_warnings() {
        eprintln!("warning: {warning}");
        cfg.load_warnings.push(warning);
    }

    if one_shot_stage!(Needs::ConfigNotices, Some(&cfg)) {
        return Ok(());
    }

    if cli.setup {
        match setup::run(&mut cfg, cli.api_key.as_deref(), &app_paths.credentials_dir)? {
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
        if is_interactive {
            match ui::pickers::sessions::choose(workspace.clone(), cli.resume_all, cli.no_color)? {
                Some(ui::pickers::sessions::SessionPick::Selected(selection)) => {
                    cli.resume_selection = Some(Box::new(selection))
                }
                _ => return Ok(()),
            }
        } else {
            print::print_sessions(&workspace, cli.resume_all)?;
            return Ok(());
        }
    }

    // Started only once every one-shot command has returned: a network task
    // would otherwise be spawned and dropped by `--mcp-login`, `--setup`, etc.
    models_catalog::spawn_refresh_if_stale(cfg.models_catalog_refresh_days);

    // Overlap the host sandbox backend probe (a process spawn) with the
    // remaining startup; the later availability check joins its result.
    if cli.resolve_sandbox(&cfg) && cli.general_sandbox_is_eligible(&cfg) {
        sandbox::begin_backend_probe(&cli.resolve_sandbox_backend(&cfg));
    }
    // The docs copy is non-essential: an unwritable data dir or a racing start
    // must not abort startup (including headless runs).
    let version_changed = docs::ensure_global().unwrap_or_else(|error| {
        eprintln!("warning: could not install bundled docs: {error:#}");
        false
    });
    #[cfg(feature = "agent-skills")]
    {
        extras::skills::index::AgentSkillSearchPolicy::configure(&cfg);
        // Config entries resolve against the directory of the config file that
        // set them, command-line entries against the launch directory.
        let launch = std::env::current_dir().unwrap_or_default();
        let entries = |key: &str,
                       list: &Option<Vec<String>>|
         -> Vec<extras::skills::session::ExtraDirEntry> {
            let base = cfg
                .provenance
                .base_dir_for(key)
                .unwrap_or_else(|| launch.clone());
            list.iter()
                .flatten()
                .map(|entry| (entry.clone(), base.clone()))
                .collect()
        };
        let mut add_dirs = entries("skills_extra_dirs", &cfg.skills_extra_dirs);
        add_dirs.extend(
            cli.add_dir
                .iter()
                .map(|dir| (dir.to_string_lossy().into_owned(), launch.clone())),
        );
        for note in extras::skills::session::configure_extra_roots(
            add_dirs,
            entries("skills_plugin_dirs", &cfg.skills_plugin_dirs),
        ) {
            eprintln!("warning: {note}");
        }
        let home = paths::process_home_dir();
        if let Some(note) =
            extras::skills::session::package_cap_startup_note(&launch, home.as_deref())
        {
            eprintln!("warning: {note}");
        }
    }
    // ── Hooks: load settings.json config, apply trust, install dispatcher ──
    // Done this early (before provider/API-key resolution) so `--hooks-test`
    // is a pure config/dispatch dry run that needs no API key and makes no
    // model call.
    #[cfg(feature = "hooks")]
    {
        crate::extras::hooks::set_http_hooks_enabled(cfg.hooks_http_enabled.unwrap_or(false));
        crate::extras::hooks::backend::set_mcp_tool_hooks_enabled(
            cfg.hooks_mcp_tool_enabled.unwrap_or(false),
        );
        crate::extras::hooks::backend::set_mcp_tool_hooks_unprompted(
            cfg.hooks_mcp_tool_unprompted.unwrap_or(false),
        );
        crate::extras::hooks::backend::set_agent_hook_reads(
            cfg.hooks_agent_read_files.unwrap_or(false),
        );
        crate::extras::hooks::backend::set_model_hooks_enabled(
            cfg.hooks_model_enabled.unwrap_or(false),
        );
        crate::extras::hooks::set_permission_decisions_enabled(
            cfg.hooks_permission_decisions.unwrap_or(false),
        );
        if let Some(mode) = cli.hooks_trust.as_deref() {
            use crate::extras::hooks::trust::HooksTrustMode;
            let mode = match mode {
                "list" => HooksTrustMode::List,
                "yes" => HooksTrustMode::Yes,
                _ => HooksTrustMode::Ask,
            };
            let report = crate::extras::hooks::trust::run_hooks_trust(
                &app_paths,
                mode,
                &cli.resolve_sandbox_backend(&cfg),
            )
            .map_err(|error| anyhow::anyhow!(error))?;
            println!("{report}");
            return Ok(());
        }
        crate::extras::hooks::init_dispatcher(crate::extras::hooks::trust::load_dispatcher(
            &app_paths,
            cli.no_hooks,
            !is_interactive,
            &cli.resolve_sandbox_backend(&cfg),
            cli.trust_project_hooks,
        ));

        if let Some(tool_name) = &cli.hooks_test {
            let tool_input: serde_json::Value = cli
                .hooks_test_input
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("--hooks-test-input is not valid JSON")?
                .unwrap_or_else(|| serde_json::json!({}));
            println!(
                "{}",
                startup::heap_future(|| crate::extras::hooks::hooks_test_dry_run(
                    tool_name, tool_input
                ))
                .await
            );
            return Ok(());
        }
    }

    let mut startup = Box::new(
        startup::heap_future(|| {
            startup::Startup::init(
                cli,
                cfg,
                app_paths,
                workspace,
                is_first_startup,
                version_changed,
                is_interactive,
            )
        })
        .await?,
    );

    // ACP mode skips the remaining feature initialization, so validate the
    // shared process sandbox contract before entering either execution surface.
    startup.preflight_startup_capabilities()?;

    // ACP mode: serve and exit before feature init
    #[cfg(feature = "acp")]
    if startup.cli.acp_enabled {
        // ACP builds agents lazily per session, but the task tool's immutable
        // provider/model runtime must exist before ACP advertises or serves it.
        #[cfg(feature = "subagents")]
        startup.init_subagents();
        #[cfg(feature = "js")]
        let probe = startup.take_worker_probe();
        let result =
            startup::heap_future(|| extras::acp::serve(startup.cli, startup.cfg, startup.context))
                .await;
        #[cfg(feature = "js")]
        if let Some(probe) = probe {
            probe.retire_after_owner_exit().await?;
        }
        return result;
    }

    // `--loop-max 0` runs nothing. Answer it before feature start-up and the
    // startup prompts, so asking for no iterations cannot block on a prompt,
    // read stdin or start servers first.
    #[cfg(feature = "loop")]
    if startup.cli.loop_mode && startup.cli.loop_max == Some(0) {
        eprintln!("[loop] max iterations (0) reached, stopping");
        return Ok(());
    }

    startup.start_openrouter_pricing_refresh();
    startup::heap_future(|| startup.init_features()).await?;
    let prompts = startup::heap_future(|| startup.resolve_prompts()).await;
    startup.finish_openrouter_pricing_refresh().await;
    prompts?;
    startup::heap_future(|| startup.dispatch()).await
}

#[cfg(test)]
mod one_shot_tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> cli::Cli {
        cli::Cli::parse_from(std::iter::once("mini-agent").chain(args.iter().copied()))
    }

    #[test]
    fn no_flag_is_no_one_shot() {
        assert_eq!(one_shot_command(&cli(&[])), None);
        assert_eq!(one_shot_command(&cli(&["--setup"])), None);
    }

    #[test]
    fn each_command_runs_at_the_stage_its_needs_name() {
        let config_schema = one_shot_command(&cli(&["--config-schema"])).unwrap();
        assert_eq!(config_schema, OneShot::ConfigSchema);
        assert_eq!(config_schema.needs(), Needs::Nothing);
        assert_eq!(
            one_shot_command(&cli(&["--doctor"])).unwrap().needs(),
            Needs::Paths
        );
        assert_eq!(
            one_shot_command(&cli(&["--tutor"])).unwrap().needs(),
            Needs::Storage
        );
        assert_eq!(
            one_shot_command(&cli(&["--print-config"])).unwrap().needs(),
            Needs::ConfigNotices
        );
    }

    /// Stages are ordered by how much startup has happened, so a command is
    /// never dispatched before the state it reads exists.
    #[test]
    fn stages_are_ordered_by_startup_progress() {
        assert!(Needs::Nothing < Needs::Arguments);
        assert!(Needs::Arguments < Needs::Paths);
        assert!(Needs::Paths < Needs::Storage);
        assert!(Needs::Storage < Needs::Config);
        assert!(Needs::Config < Needs::ConfigNotices);
    }

    /// With several flags the earlier command in the historical order wins, as
    /// the sequential ladder did.
    #[test]
    fn the_first_requested_command_in_historical_order_wins() {
        let both = cli::Cli {
            doctor: true,
            print_config: true,
            ..cli::Cli::default()
        };
        assert_eq!(one_shot_command(&both), Some(OneShot::Doctor));
        // `--setup` runs before `--tutor`, so the pair is not one-shot.
        let setup_tutor = cli::Cli {
            setup: true,
            tutor: true,
            ..cli::Cli::default()
        };
        assert_eq!(one_shot_command(&setup_tutor), None);
    }
}
