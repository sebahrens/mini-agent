//! Live health checks for the one-shot `--doctor` command.
//!
//! The captured-state report cannot say whether the sandbox or the `js` tool
//! work on this machine. These checks run the real preflights, each bounded by
//! a timeout, and never make a provider call or print a credential value
//! (mini-agent-nvnkx.225).

use std::path::Path;
use std::time::Duration;

use super::CredentialReport;

/// Upper bound for any single live check.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for one `tool --version` probe through the sandbox.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DETAIL_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Pass(String),
    /// A degradation worth reading that does not make the install unusable.
    Warn(String),
    /// A required capability is missing.
    Fail(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Check {
    pub name: &'static str,
    pub outcome: Outcome,
}

impl Check {
    fn new(name: &'static str, outcome: Outcome) -> Self {
        Self { name, outcome }
    }
}

fn bounded(text: impl AsRef<str>) -> String {
    let mut out: String = text
        .as_ref()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_DETAIL_CHARS)
        .collect();
    if text.as_ref().chars().count() > MAX_DETAIL_CHARS {
        out.push('…');
    }
    out
}

/// Windows canonicalisation returns the `\\?\` verbatim spelling of a plain path;
/// strip it, unify separators, and compare drive-letter paths case-insensitively
/// so the same file is not reported as a symlink.
fn same_executable_path(invoked: &Path, canonical: &Path) -> bool {
    if invoked == canonical {
        return true;
    }
    let normalize = |path: &Path| {
        let text = path.to_string_lossy().replace('/', "\\");
        let text = match text.strip_prefix(r"\\?\UNC\") {
            Some(rest) => format!(r"\\{rest}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
        };
        let drive = text.as_bytes().get(1) == Some(&b':');
        if drive {
            text.to_ascii_lowercase()
        } else {
            text
        }
    };
    normalize(invoked) == normalize(canonical)
}

/// Warn when the invoked executable path differs from its canonical path.
pub(crate) fn install_layout(invoked: &Path, canonical: &Path) -> Outcome {
    if same_executable_path(invoked, canonical) {
        Outcome::Pass(format!("launched from {}", invoked.display()))
    } else {
        Outcome::Warn(bounded(format!(
            "launched through a symlink ({}); the real executable is {}. Older builds refused \
             the JavaScript worker for a symlinked launcher; use the real path if js is unavailable",
            invoked.display(),
            canonical.display()
        )))
    }
}

/// A route that cannot authenticate is a failed required capability.
pub(crate) fn credential(report: &CredentialReport) -> Outcome {
    use crate::auth::LoginRecordState as S;
    match report {
        CredentialReport::Missing => {
            Outcome::Fail("no credential for the selected provider".into())
        }
        CredentialReport::InvalidRoute => Outcome::Fail("provider configuration is invalid".into()),
        CredentialReport::ChatGpt(S::SignInRequired) => {
            Outcome::Fail("ChatGPT sign-in required".into())
        }
        CredentialReport::Copilot(S::Absent | S::SignInRequired) => {
            Outcome::Fail("Copilot sign-in required".into())
        }
        _ => Outcome::Pass("credential source present (value omitted)".into()),
    }
}

pub(crate) fn price(prices: Option<(f64, f64)>, model: &str) -> Outcome {
    match prices {
        Some((input, output)) => Outcome::Pass(format!(
            "input {input}; output {output} USD per million tokens"
        )),
        None => Outcome::Warn(bounded(format!(
            "no price resolved for {model}; spend limits and cost totals cannot account for it"
        ))),
    }
}

pub(crate) fn sandbox_status(status: &str, reason: Option<&str>, enabled: bool) -> Outcome {
    if !enabled {
        return Outcome::Pass("sandbox disabled by configuration".into());
    }
    if status == "required-and-available" {
        Outcome::Pass("sandbox preflight passed".into())
    } else {
        Outcome::Fail(bounded(format!(
            "sandbox is enabled but unavailable: {}",
            reason.unwrap_or(status)
        )))
    }
}

fn first_output_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Run `<program> --version` through the sandbox; failure is a warning because
/// the tool may simply be absent.
pub(crate) async fn tool_in_sandbox(sandbox: &crate::sandbox::Sandbox, program: &str) -> Outcome {
    let command = match sandbox.wrap_command(&format!("{program} --version")) {
        Ok(command) => command,
        Err(error) => {
            return Outcome::Warn(bounded(format!("{program}: could not launch: {error}")));
        }
    };
    // The sandbox owns launch through the process-creation boundary and bounds
    // time and output.
    let limits = crate::sandbox::CommandLimits {
        timeout: TOOL_TIMEOUT,
        stdout_bytes: 16 * 1024,
        stderr_bytes: 16 * 1024,
        combined_bytes: 32 * 1024,
    };
    match sandbox
        .output_built_command_with_limits(command, limits)
        .await
    {
        Err(error) => Outcome::Warn(bounded(format!("{program}: {error}"))),
        Ok(output) if output.status == crate::sandbox::CommandStatus::TimedOut => {
            Outcome::Warn(format!("{program}: timed out inside the sandbox"))
        }
        Ok(output) if output.exit_status.is_some_and(|status| status.success()) => {
            Outcome::Pass(bounded(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or(program),
            ))
        }
        Ok(output) => Outcome::Warn(bounded(format!(
            "{program} failed inside the sandbox ({:?}): {}",
            output.status,
            first_output_line(&output.stderr)
                .or_else(|| first_output_line(&output.stdout))
                .unwrap_or_else(|| "no output".to_string())
        ))),
    }
}

/// The effective state of the three opt-in AppContainer shell extras.
pub(crate) fn appcontainer_extras_line(sandbox: &crate::sandbox::Sandbox) -> String {
    let state = |on: bool| if on { "on" } else { "off" };
    let mut line = format!(
        "ancestor-listing={}, null-device={}, toolchain-discovery={}",
        state(sandbox.windows_ancestor_listing()),
        state(sandbox.windows_null_device()),
        state(sandbox.windows_toolchain_discovery()),
    );
    if sandbox.windows_toolchain_discovery() {
        let roots = crate::sandbox::discovered_toolchain_root_count();
        line.push_str(&format!(" ({roots} toolchain root(s) resolved)"));
    }
    line
}

/// Command names the sandboxed Python probe tries, in order. Windows has no
/// `python3`: the interpreter is `python`, or the `py` launcher.
pub(crate) fn python_probe_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["python", "py"]
    } else {
        &["python3"]
    }
}

fn python_check_name() -> &'static str {
    if cfg!(windows) {
        "sandboxed python"
    } else {
        "sandboxed python3"
    }
}

/// Probe each name in turn; the first that runs wins, otherwise the first
/// failure is reported (with the remaining names noted).
pub(crate) async fn first_tool_in_sandbox(
    sandbox: &crate::sandbox::Sandbox,
    names: &[&str],
) -> Outcome {
    let mut first_failure = None;
    for name in names {
        match tool_in_sandbox(sandbox, name).await {
            Outcome::Pass(text) => return Outcome::Pass(text),
            other => {
                first_failure.get_or_insert(other);
            }
        }
    }
    match first_failure {
        Some(Outcome::Warn(text)) if names.len() > 1 => Outcome::Warn(bounded(format!(
            "{text} (also tried: {})",
            names[1..].join(", ")
        ))),
        Some(outcome) => outcome,
        None => Outcome::Warn("no command names to probe".into()),
    }
}

/// The unchanged worker preflight (the one `--js-runtime-check` depends on).
#[cfg(feature = "js")]
pub(crate) async fn js_worker() -> Outcome {
    use crate::sandbox::worker::{WorkerContainmentStatus, containment_status};
    match tokio::time::timeout(
        CHECK_TIMEOUT,
        tokio::task::spawn_blocking(containment_status),
    )
    .await
    {
        Err(_) => Outcome::Fail("worker preflight timed out".into()),
        Ok(Err(error)) => Outcome::Fail(bounded(format!("worker preflight failed: {error}"))),
        Ok(Ok(WorkerContainmentStatus::Available { .. })) => {
            Outcome::Pass("worker containment preflight passed".into())
        }
        Ok(Ok(WorkerContainmentStatus::Unavailable { reason, .. })) => {
            Outcome::Warn(bounded(format!("js tool unavailable: {reason}")))
        }
    }
}

/// Run every live check for the one-shot `--doctor` command.
pub(crate) async fn run_all(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    paths: &crate::paths::AppPaths,
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Ok(invoked) = std::env::current_exe() {
        let canonical = invoked.canonicalize().unwrap_or_else(|_| invoked.clone());
        checks.push(Check::of(
            "install path",
            install_layout(&invoked, &canonical),
        ));
    }
    let (provider, model) = cli.resolve_provider_and_model(cfg);
    let credential_report = super::provider_credential_with_env(
        cfg,
        &provider,
        cli.api_key.as_deref(),
        &paths.credentials_dir,
        |name| std::env::var(name),
    );
    checks.push(Check::of("credential", credential(&credential_report)));
    let (prices, _) = crate::startup::diagnostic_startup_pricing(cli, cfg, &provider, &model);
    checks.push(Check::of("price", price(prices, &model)));

    let enabled = cli.resolve_sandbox(cfg);
    let backend = cli.resolve_sandbox_backend(cfg);
    let sandbox = crate::sandbox::Sandbox::new(enabled, &backend)
        .with_general_profile(
            cli.resolve_sandbox_profile(cfg),
            cfg.sandbox_read_roots.clone(),
            cfg.sandbox_write_roots.clone(),
        )
        .with_windows_appcontainer_roots(
            cli.resolve_windows_appcontainer_read_roots(cfg),
            cli.resolve_windows_appcontainer_write_roots(cfg),
        )
        .with_shell(&cli.resolve_shell(cfg))
        .with_windows_toolchain_discovery(
            cfg.windows_appcontainer_toolchain_discovery
                .unwrap_or(false),
        )
        .with_windows_ancestor_listing(cfg.windows_appcontainer_ancestor_listing.unwrap_or(false))
        .with_windows_null_device(cfg.windows_appcontainer_null_device.unwrap_or(false))
        .with_bwrap_read_roots(cli.resolve_sandbox_toolchain_roots(cfg))
        .with_workspace_binding(workspace);
    // Doctor is read-only: run the sandboxed probes against a throwaway cache
    // rather than creating `<cache>/sandbox-runtime` in the user's profile.
    let scratch = std::env::temp_dir().join(format!(
        "mini-agent-doctor-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    let sandbox = match sandbox.clone().with_ephemeral_cache_dir(&scratch) {
        Ok(pinned) => pinned,
        Err(_) => sandbox,
    };
    let probe = sandbox.clone();
    let matrix = tokio::time::timeout(
        CHECK_TIMEOUT,
        tokio::task::spawn_blocking(move || probe.capability_matrix()),
    )
    .await;
    let sandbox_ok = match matrix {
        Ok(Ok(matrix)) => {
            let outcome = sandbox_status(matrix.status, matrix.reason.as_deref(), enabled);
            let ok = !matches!(outcome, Outcome::Fail(_));
            checks.push(Check::of("sandbox", outcome));
            ok
        }
        _ => {
            checks.push(Check::of(
                "sandbox",
                Outcome::Fail("sandbox preflight timed out or failed".into()),
            ));
            false
        }
    };
    if enabled && backend == "appcontainer" {
        let for_line = sandbox.clone();
        let line = tokio::task::spawn_blocking(move || appcontainer_extras_line(&for_line))
            .await
            .unwrap_or_else(|_| "unavailable".to_string());
        checks.push(Check::of("appcontainer extras", Outcome::Pass(line)));
        let retained = crate::sandbox::retained_stale_grants();
        if retained > 0 {
            checks.push(Check::of(
                "stale grants",
                Outcome::Warn(format!(
                    "{retained} earlier session(s) left access grants an unelevated launcher could not remove; start mini-agent elevated once to clear them"
                )),
            ));
        }
    }
    if enabled && sandbox_ok {
        checks.push(Check::of(
            "sandboxed git",
            tool_in_sandbox(&sandbox, "git").await,
        ));
        checks.push(Check::of(
            python_check_name(),
            first_tool_in_sandbox(&sandbox, python_probe_names()).await,
        ));
    }
    #[cfg(feature = "js")]
    if cli.tool_is_eligible(cfg, "js") {
        checks.push(Check::of("js worker", js_worker().await));
    }
    let _ = std::fs::remove_dir_all(&scratch);
    checks
}

/// `(failed, warned)` counts and the final summary line.
pub(crate) fn summary(checks: &[Check]) -> (usize, usize, String) {
    let failed = checks
        .iter()
        .filter(|check| matches!(check.outcome, Outcome::Fail(_)))
        .count();
    let warned = checks
        .iter()
        .filter(|check| matches!(check.outcome, Outcome::Warn(_)))
        .count();
    (
        failed,
        warned,
        format!(
            "{} check(s) run: {} failed, {} warning(s)",
            checks.len(),
            failed,
            warned
        ),
    )
}

pub(crate) fn render_into(report: &mut super::DoctorReport, checks: &[Check]) {
    report.section("Live checks");
    for check in checks {
        let (tag, text) = match &check.outcome {
            Outcome::Pass(text) => ("ok", text),
            Outcome::Warn(text) => ("warn", text),
            Outcome::Fail(text) => ("FAIL", text),
        };
        report.row(check.name, format!("[{tag}] {text}"));
    }
    report.row("summary", summary(checks).2);
}

impl Check {
    pub(crate) fn of(name: &'static str, outcome: Outcome) -> Self {
        Self::new(name, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::LoginRecordState as S;

    #[test]
    fn the_sandboxed_python_probe_uses_the_names_of_the_host_platform() {
        if cfg!(windows) {
            assert_eq!(python_probe_names(), ["python", "py"]);
            assert_eq!(python_check_name(), "sandboxed python");
        } else {
            assert_eq!(python_probe_names(), ["python3"]);
            assert_eq!(python_check_name(), "sandboxed python3");
        }
    }

    #[test]
    fn a_failed_probe_reports_the_first_non_empty_line_of_stderr_or_stdout() {
        assert_eq!(
            first_output_line(b"\r\n  \nfatal: nope\nmore"),
            Some("fatal: nope".to_string())
        );
        assert_eq!(first_output_line(b"  \n"), None);
    }

    #[test]
    fn symlinked_launch_is_reported_with_the_real_path() {
        let same = install_layout(Path::new("/a/mini-agent"), Path::new("/a/mini-agent"));
        assert!(matches!(same, Outcome::Pass(_)));
        let linked = install_layout(
            Path::new("/home/u/.local/bin/mini-agent"),
            Path::new("/c/bin/mini-agent"),
        );
        let Outcome::Warn(text) = linked else {
            panic!("{linked:?}")
        };
        assert!(text.contains("/c/bin/mini-agent") && text.contains("symlink"));
    }

    #[test]
    fn verbatim_prefix_and_case_do_not_count_as_a_symlink() {
        let plain = Path::new(r"D:\a\_temp\ma\bin\mini-agent.exe");
        let verbatim = Path::new(r"\\?\d:\a\_temp\ma\bin\mini-agent.exe");
        assert!(matches!(install_layout(plain, verbatim), Outcome::Pass(_)));
        let unc = Path::new(r"\\?\UNC\host\share\mini-agent.exe");
        assert!(same_executable_path(
            Path::new(r"\\host\share\mini-agent.exe"),
            unc
        ));
        let other = Path::new(r"\\?\D:\elsewhere\mini-agent.exe");
        assert!(matches!(install_layout(plain, other), Outcome::Warn(_)));
    }

    #[test]
    fn a_missing_credential_fails_and_a_present_one_never_prints_a_value() {
        assert!(matches!(
            credential(&CredentialReport::Missing),
            Outcome::Fail(_)
        ));
        assert!(matches!(
            credential(&CredentialReport::ChatGpt(S::SignInRequired)),
            Outcome::Fail(_)
        ));
        let present = credential(&CredentialReport::Source(
            crate::auth::CredentialSource::Environment("OPENROUTER_API_KEY".into()),
        ));
        assert!(matches!(present, Outcome::Pass(_)));
    }

    #[test]
    fn missing_price_and_unavailable_sandbox_are_classified() {
        assert!(matches!(price(None, "m"), Outcome::Warn(_)));
        assert!(matches!(price(Some((1.0, 2.0)), "m"), Outcome::Pass(_)));
        assert!(matches!(
            sandbox_status("x", Some("no bwrap"), true),
            Outcome::Fail(_)
        ));
        assert!(matches!(sandbox_status("x", None, false), Outcome::Pass(_)));
        assert!(matches!(
            sandbox_status("required-and-available", None, true),
            Outcome::Pass(_)
        ));
    }

    #[test]
    fn details_are_bounded_and_control_free() {
        let text = bounded(format!("\u{1b}[31m{}", "x".repeat(1000)));
        assert!(text.chars().count() <= MAX_DETAIL_CHARS + 1 && !text.contains('\u{1b}'));
    }

    #[test]
    fn summary_counts_failures_and_warnings() {
        let checks = [
            Check::of("a", Outcome::Pass("".into())),
            Check::of("b", Outcome::Warn("".into())),
            Check::of("c", Outcome::Fail("".into())),
        ];
        let (failed, warned, line) = summary(&checks);
        assert_eq!((failed, warned), (1, 1));
        assert!(line.contains("3 check(s) run"));
    }

    #[tokio::test]
    async fn tool_probe_is_bounded_and_missing_tools_only_warn() {
        let sandbox = crate::sandbox::Sandbox::new(false, "none");
        let outcome = tool_in_sandbox(&sandbox, "mini-agent-definitely-absent-tool").await;
        assert!(matches!(outcome, Outcome::Warn(_)), "{outcome:?}");
    }
}
