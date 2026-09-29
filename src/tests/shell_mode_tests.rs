#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use crate::sandbox::{
    CommandCancellation, CommandOutputLimit, SupportCommandAudit, SupportCommandLimits,
};
use crate::sandbox::{CommandLimits, CommandStatus, ExplicitShellBoundary, Sandbox};
#[cfg(unix)]
use tokio::time::timeout;
use tokio::time::{Duration, sleep};

const SHORT_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_millis(300),
    stdout_bytes: 4096,
    stderr_bytes: 4096,
    combined_bytes: 6144,
};

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_preserves_exact_authored_script_and_reports_bypass() {
    let sandbox = Sandbox::new(false, "bwrap");
    let run = sandbox
        .run_explicit_shell("!  printf exact  ", SHORT_LIMITS, None)
        .await
        .unwrap();

    assert_eq!(run.audit.command, "  printf exact  ");
    assert_eq!(run.audit.cwd, std::env::current_dir().unwrap());
    assert_eq!(run.audit.boundary, ExplicitShellBoundary::UserTrustedBypass);
    assert_eq!(run.output.status, CommandStatus::Completed);
    assert_eq!(run.rendered_output(), "exact");
}

#[test]
fn implicit_unavailable_default_is_not_reported_as_an_explicit_trusted_bypass() {
    let sandbox = Sandbox::new(false, "missing-default").with_unavailable_default_fallback();

    assert_eq!(
        sandbox.explicit_shell_boundary(),
        ExplicitShellBoundary::UnavailableDefaultFallback {
            backend: "missing-default".to_string(),
        }
    );
    assert_eq!(
        sandbox.explicit_shell_boundary().label(),
        "unsandboxed-unavailable-default-fallback:missing-default"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn implicit_unavailable_default_clears_ambient_credentials() {
    let _environment = crate::tests::ScopedProcessEnv::set(&[(
        "MINI_AGENT_FALLBACK_SECRET",
        Some("must-not-cross".into()),
    )]);
    let sandbox = Sandbox::new(false, "missing-default").with_unavailable_default_fallback();

    let output = sandbox
        .output_command_with_limits(
            "test -z \"${MINI_AGENT_FALLBACK_SECRET+x}\" && printf clean",
            SHORT_LIMITS,
        )
        .await
        .unwrap();

    assert_eq!(output.status, CommandStatus::Completed);
    assert!(output.exit_status.is_some_and(|status| status.success()));
    assert_eq!(output.stdout, b"clean");
    assert_eq!(
        sandbox.capability_matrix().environment,
        "cleared, then populated from a non-credential allow-list"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_success_and_nonzero_share_one_status_policy() {
    let sandbox = Sandbox::new(false, "bwrap");
    let success = sandbox
        .run_explicit_shell("!printf ok", SHORT_LIMITS, None)
        .await
        .unwrap();
    let nonzero = sandbox
        .run_explicit_shell("!printf bad >&2; exit 42", SHORT_LIMITS, None)
        .await
        .unwrap();

    assert!(success.succeeded());
    assert_eq!(success.rendered_output(), "ok");
    assert!(!nonzero.succeeded());
    assert_eq!(nonzero.output.exit_status.unwrap().code(), Some(42));
    assert_eq!(
        nonzero.rendered_output(),
        "bad\n[explicit shell exited with status 42; boundary=user-trusted-bypass]"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_lossily_renders_invalid_utf8_without_panicking() {
    let sandbox = Sandbox::new(false, "bwrap");
    let run = sandbox
        .run_explicit_shell("!printf '\\377x'", SHORT_LIMITS, None)
        .await
        .unwrap();

    assert_eq!(run.rendered_output(), "�x");
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_captures_exact_cwd_and_inherited_bypass_environment() {
    let sandbox = Sandbox::new(false, "bwrap");
    let run = sandbox
        .run_explicit_shell("!printf '%s\\n%s' \"$PWD\" \"$PATH\"", SHORT_LIMITS, None)
        .await
        .unwrap();
    let expected_cwd = std::env::current_dir().unwrap();
    let rendered = run.rendered_output();
    let mut lines = rendered.lines();

    assert_eq!(Path::new(lines.next().unwrap()), expected_cwd);
    assert_eq!(lines.next().unwrap(), std::env::var("PATH").unwrap());
    assert_eq!(run.audit.cwd, expected_cwd);
}

#[tokio::test]
async fn explicit_shell_requested_unavailable_backend_fails_closed_and_reports_it() {
    let sandbox = Sandbox::new(true, "definitely-not-a-real-backend");
    let run = sandbox
        .run_explicit_shell("!printf should-not-run", SHORT_LIMITS, None)
        .await
        .unwrap();

    assert_eq!(
        run.audit.boundary,
        ExplicitShellBoundary::RequestedButUnavailable {
            backend: "definitely-not-a-real-backend".to_string(),
        }
    );
    assert_eq!(run.output.status, CommandStatus::Failed);
    assert!(
        run.rendered_output()
            .contains("refusing to run unsandboxed")
    );
    assert!(run.rendered_output().contains("requested-but-unavailable"));
}

#[test]
fn explicit_shell_available_backend_is_reported_truthfully() {
    #[cfg(target_os = "macos")]
    let backend = "seatbelt";
    #[cfg(target_os = "linux")]
    let backend = "bwrap";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let backend = "definitely-not-a-real-backend";

    let sandbox = Sandbox::new(true, backend);
    let boundary = sandbox.explicit_shell_boundary();
    if sandbox.policy() == crate::sandbox::SandboxPolicy::RequiredAndAvailable {
        assert_eq!(
            boundary,
            ExplicitShellBoundary::GeneralSandbox {
                backend: backend.to_string(),
            }
        );
    } else {
        assert_eq!(
            boundary,
            ExplicitShellBoundary::RequestedButUnavailable {
                backend: backend.to_string(),
            }
        );
    }
}

#[tokio::test]
async fn explicit_shell_available_backend_runs_or_fails_closed_before_payload() {
    #[cfg(target_os = "macos")]
    let backend = "seatbelt";
    #[cfg(target_os = "linux")]
    let backend = "bwrap";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let backend = "definitely-not-a-real-backend";

    let sandbox = Sandbox::new(true, backend);
    if sandbox.policy() != crate::sandbox::SandboxPolicy::RequiredAndAvailable {
        return;
    }
    let marker = workspace_marker("backend-payload");
    let run = sandbox
        .run_explicit_shell(
            &format!(
                "!printf ran > {}; test -z \"${{OPENROUTER_API_KEY+x}}\" && printf sandboxed",
                marker.display()
            ),
            SHORT_LIMITS,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        run.audit.boundary,
        ExplicitShellBoundary::GeneralSandbox {
            backend: backend.to_string(),
        }
    );
    if run.succeeded() {
        assert_eq!(run.rendered_output(), "sandboxed");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
    } else {
        assert!(
            !marker.exists(),
            "sandbox setup failure must occur before the payload"
        );
        assert!(
            is_backend_setup_denial(&run.rendered_output()),
            "unexpected sandboxed payload failure: {}",
            run.rendered_output()
        );
    }
    let _ = std::fs::remove_file(marker);
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_timeout_kills_term_ignoring_descendant_and_recovers() {
    let marker = unique_marker("timeout-descendant");
    let sandbox = Sandbox::new(false, "bwrap");
    let command = format!(
        "!trap '' TERM; sh -c 'trap \"\" TERM; sleep 1; printf leaked > {}' & wait",
        marker.display()
    );
    let run = sandbox
        .run_explicit_shell(&command, SHORT_LIMITS, None)
        .await
        .unwrap();

    assert_eq!(run.output.status, CommandStatus::TimedOut);
    assert!(run.rendered_output().contains("timed out"));
    sleep(Duration::from_millis(1200)).await;
    assert!(!marker.exists());
    assert_eq!(sandbox.active_group_count(), 0);

    let next = sandbox
        .run_explicit_shell("!printf recovered", SHORT_LIMITS, None)
        .await
        .unwrap();
    assert_eq!(next.rendered_output(), "recovered");
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_operation_cancellation_kills_and_reaps_tree() {
    let marker = unique_marker("cancel-descendant");
    let started = unique_marker("cancel-started");
    let sandbox = Sandbox::new(false, "bwrap");
    let cancellation = CommandCancellation::new();
    let command = format!(
        "!sh -c 'printf started > {}; sleep 1; printf leaked > {}' & wait",
        started.display(),
        marker.display(),
    );
    let handle = tokio::spawn({
        let sandbox = sandbox.clone();
        let cancellation = cancellation.clone();
        async move {
            sandbox
                .run_explicit_shell(&command, SHORT_LIMITS, Some(&cancellation))
                .await
        }
    });

    wait_until(|| sandbox.active_group_count() == 1 && started.exists()).await;
    cancellation.cancel();
    let run = timeout(Duration::from_secs(2), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(run.output.status, CommandStatus::Cancelled);
    sleep(Duration::from_millis(1200)).await;
    assert!(!marker.exists());
    assert_eq!(sandbox.active_group_count(), 0);
    let _ = std::fs::remove_file(started);
}

#[cfg(unix)]
#[test]
fn explicit_shell_caller_drop_audits_after_tree_cleanup() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let expected_cwd = std::env::current_dir().unwrap();

    let (audit_receipts, marker) = runtime.block_on(async {
        let marker = unique_marker("shell-drop-descendant");
        let started = unique_marker("shell-drop-started");
        let mut sandbox = Sandbox::new(false, "bwrap");
        let audit_receipts = sandbox.observe_explicit_shell_audits();
        let interaction = format!(
            "!trap '' TERM; sh -c 'printf started > {}; sleep 1; printf leaked > {}' & wait",
            started.display(),
            marker.display()
        );
        let handle = tokio::spawn({
            let sandbox = sandbox.clone();
            async move {
                sandbox
                    .run_explicit_shell(&interaction, SHORT_LIMITS, None)
                    .await
            }
        });

        wait_until_named("explicit shell process group to start", || {
            sandbox.active_group_count() == 1 && started.exists()
        })
        .await;
        handle.abort();
        let _ = handle.await;
        wait_until_named("explicit shell process group cleanup", || {
            sandbox.active_group_count() == 0
        })
        .await;
        wait_until_for_named(
            Duration::from_secs(30),
            "explicit shell terminal audit",
            || !audit_receipts.lock().unwrap().is_empty(),
        )
        .await;
        sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists());
        let _ = std::fs::remove_file(started);
        (audit_receipts, marker)
    });

    let receipts = audit_receipts.lock().unwrap();
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    let (audit, status) = &receipts[0];
    assert_eq!(*status, CommandStatus::Cancelled);
    assert_eq!(audit.cwd, expected_cwd);
    assert!(audit.command.starts_with("trap '' TERM;"));
    assert_eq!(audit.boundary, ExplicitShellBoundary::UserTrustedBypass);
    assert!(!marker.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_shell_bounds_stdout_stderr_and_mixed_floods() {
    let sandbox = Sandbox::new(false, "bwrap");
    let cases = [
        ("!yes o", SHORT_LIMITS, CommandOutputLimit::Stdout),
        ("!yes e >&2", SHORT_LIMITS, CommandOutputLimit::Stderr),
        (
            "!yes o & yes e >&2 & wait",
            CommandLimits {
                timeout: SHORT_LIMITS.timeout,
                stdout_bytes: 8192,
                stderr_bytes: 8192,
                combined_bytes: SHORT_LIMITS.combined_bytes,
            },
            CommandOutputLimit::Combined,
        ),
    ];

    for (command, limits, expected_limit) in cases {
        let run = sandbox
            .run_explicit_shell(command, limits, None)
            .await
            .unwrap();
        assert!(matches!(
            run.output.status,
            CommandStatus::OutputLimitExceeded(limit) if limit == expected_limit
        ));
        assert!(run.output.stdout.len() <= limits.stdout_bytes);
        assert!(run.output.stderr.len() <= limits.stderr_bytes);
        assert!(run.output.stdout.len() + run.output.stderr.len() <= limits.combined_bytes);
        assert_eq!(sandbox.active_group_count(), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn support_utility_runner_bounds_and_reaps_an_interactive_process_tree() {
    let marker = unique_marker("support-descendant");
    let sandbox = Sandbox::new(false, "bwrap");
    let command = format!(
        "trap '' TERM; sh -c 'sleep 1; printf leaked > {}' & wait",
        marker.display()
    );
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c").arg(command);
    let status = sandbox
        .status_support_command(
            cmd,
            SupportCommandLimits {
                timeout: Duration::from_millis(200),
            },
            SupportCommandAudit::new("test-support-timeout", "user-trusted-bypass"),
        )
        .await
        .unwrap();

    assert_eq!(status.status, CommandStatus::TimedOut);
    sleep(Duration::from_millis(1200)).await;
    assert!(!marker.exists());
    assert_eq!(sandbox.active_group_count(), 0);
}

#[cfg(unix)]
#[test]
fn lazygit_style_caller_drop_audits_cleanup_and_allows_the_next_launch() {
    let sandbox = Sandbox::new(false, "bwrap");
    let capture = AuditCapture::new(&sandbox);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let expected_cwd = std::env::current_dir().unwrap();

    // Everything runs inside the runtime and inside the subscriber's scope.
    // Asserting after this closure returns would uninstall the subscriber,
    // and a record emitted a moment later would then be lost rather than
    // late — a failure that cannot show its own cause.
    tracing::subscriber::with_default(capture.subscriber(), || {
        runtime.block_on(async {
            prime_support_audit_capture(&sandbox, &capture).await;

            let marker = unique_marker("support-drop-descendant");
            let started = unique_marker("support-drop-started");
            let script = format!(
                "trap '' TERM; sh -c 'printf started > {}; sleep 1; printf leaked > {}' & wait",
                started.display(),
                marker.display()
            );
            let mut command = tokio::process::Command::new("bash");
            command.arg("-c").arg(script);
            let handle = tokio::spawn({
                let sandbox = sandbox.clone();
                async move {
                    sandbox
                        .status_support_command(
                            command,
                            SupportCommandLimits {
                                timeout: SUPPORT_STAGE_BUDGET,
                            },
                            SupportCommandAudit::new(CALLER_DROP_UTILITY, "user-trusted-bypass"),
                        )
                        .await
                }
            });

            // Spawn and publication are separate stages: the worker
            // registers the group as soon as the leader exists, and the
            // leader's child publishes its start marker once it runs.
            let diagnostics = || {
                format!(
                    "active groups: {}, start marker present: {}, captured audit: {}",
                    sandbox.active_group_count(),
                    started.exists(),
                    capture.summary(),
                )
            };
            support_stage(
                "the utility registered its process group",
                || sandbox.active_group_count() == 1,
                diagnostics,
            )
            .await;
            support_stage(
                "the utility's child published its start marker",
                || started.exists(),
                diagnostics,
            )
            .await;

            handle.abort();
            let _ = handle.await;
            support_stage(
                "the cancelled utility's process group was released",
                || sandbox.active_group_count() == 0,
                diagnostics,
            )
            .await;
            // The worker emits the audit record before it releases the
            // group's accounting, so a zero count means the record exists.
            let (record, groups_at_write) =
                capture.record(CALLER_DROP_UTILITY).unwrap_or_else(|| {
                    panic!(
                        "support utility caller-drop stalled: the cancelled utility's audit \
                         record was not written before its group was released ({})",
                        diagnostics()
                    )
                });
            assert_eq!(
                groups_at_write, 1,
                "the cancelled utility's audit record must be written before its group is \
                 released: {record}"
            );
            assert!(record.contains("outcome=\"cancelled\""), "{record}");
            assert!(
                record.contains(&format!("cwd={}", expected_cwd.display())),
                "{record}"
            );
            assert!(
                record.contains("support utility ended after process cleanup"),
                "{record}"
            );

            sleep(Duration::from_millis(1200)).await;
            assert!(!marker.exists());
            let _ = std::fs::remove_file(&started);

            let mut next = tokio::process::Command::new("bash");
            next.args(["-c", "exit 0"]);
            let next = timeout(
                SUPPORT_STAGE_BUDGET,
                sandbox.status_support_command(
                    next,
                    SupportCommandLimits {
                        timeout: SUPPORT_STAGE_BUDGET,
                    },
                    SupportCommandAudit::new("lazygit-next-launch-test", "user-trusted-bypass"),
                ),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "support utility caller-drop stalled: the next launch did not finish within \
                     {SUPPORT_STAGE_BUDGET:?} ({})",
                    diagnostics()
                )
            })
            .unwrap();
            assert_eq!(next.status, CommandStatus::Completed);
            assert!(next.exit_status.is_some_and(|status| status.success()));
            assert_eq!(sandbox.active_group_count(), 0);
        });
    });
}

#[cfg(unix)]
#[test]
fn support_utility_audit_is_written_before_its_process_group_is_released() {
    let sandbox = Sandbox::new(false, "bwrap");
    let capture = AuditCapture::new(&sandbox);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::subscriber::with_default(capture.subscriber(), || {
        runtime.block_on(async {
            prime_support_audit_capture(&sandbox, &capture).await;
            let (record, groups_at_write) = capture
                .record(PROBE_UTILITY)
                .expect("priming proved the probe record was captured");
            // Zero active groups is the observable end of a support
            // utility's lifecycle, so its terminal audit must already exist
            // while the group is still accounted as active.
            assert_eq!(groups_at_write, 1, "{record}");
            assert!(record.contains("outcome=\"nonzero\""), "{record}");
            assert_eq!(sandbox.active_group_count(), 0);
        });
    });
}

#[test]
fn headless_and_tui_paths_delegate_to_the_shared_explicit_shell_runner() {
    let startup = include_str!("../startup.rs");
    let app = include_str!("../ui/app.rs");

    assert!(startup.contains(".run_explicit_shell("));
    assert!(app.contains(".run_explicit_shell("));
    assert!(app.contains(".output_support_command("));
    assert!(app.contains(".status_support_command("));
    assert!(!startup.contains("std::process::Command::new(\"bash\")"));
    assert!(!app.contains("std::process::Command::new(\"bash\")"));
}

fn unique_marker(label: &str) -> std::path::PathBuf {
    let marker = std::env::temp_dir().join(format!(
        "mini-agent-explicit-shell-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4(),
    ));
    let _ = std::fs::remove_file(&marker);
    marker
}

fn workspace_marker(label: &str) -> std::path::PathBuf {
    let marker = std::env::current_dir().unwrap().join(format!(
        ".mini-agent-explicit-shell-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4(),
    ));
    let _ = std::fs::remove_file(&marker);
    marker
}

fn is_backend_setup_denial(rendered: &str) -> bool {
    [
        "sandbox_apply: Operation not permitted",
        "Operation not permitted",
        "Permission denied",
        "Creating new namespace failed",
    ]
    .iter()
    .any(|needle| rendered.contains(needle))
}

async fn wait_until(predicate: impl FnMut() -> bool) {
    wait_until_for_named(Duration::from_secs(30), "condition", predicate).await;
}

async fn wait_until_named(label: &str, predicate: impl FnMut() -> bool) {
    wait_until_for_named(Duration::from_secs(30), label, predicate).await;
}

async fn wait_until_for_named(timeout: Duration, label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {label}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// Budget for each stage of the support-utility caller-drop fixtures. Each
/// stage is timed from the end of the previous one, so load that delays one
/// stage is never charged against the next.
#[cfg(unix)]
const SUPPORT_STAGE_BUDGET: Duration = Duration::from_secs(30);

/// Support utility whose non-success audit proves that this thread's scoped
/// subscriber receives the support-utility audit callsite.
#[cfg(unix)]
const PROBE_UTILITY: &str = "support-audit-capture-probe-test";

#[cfg(unix)]
const CALLER_DROP_UTILITY: &str = "lazygit-caller-drop-test";

/// Waits for `ready` within [`SUPPORT_STAGE_BUDGET`] and names the stage,
/// with `diagnostics`, when it stalls.
#[cfg(unix)]
async fn support_stage(
    name: &str,
    mut ready: impl FnMut() -> bool,
    diagnostics: impl Fn() -> String,
) {
    let deadline = std::time::Instant::now() + SUPPORT_STAGE_BUDGET;
    while !ready() {
        if std::time::Instant::now() >= deadline {
            panic!(
                "support utility caller-drop stalled: {name} did not happen within \
                 {SUPPORT_STAGE_BUDGET:?} ({})",
                diagnostics()
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// Makes this test's scoped subscriber observe the support-utility audit
/// callsite before the scenario relies on it.
///
/// tracing caches each callsite's interest process-wide. While a scoped
/// subscriber is the only live dispatcher, tracing-core resolves a
/// callsite's first registration against the registering thread's default
/// alone. A concurrent test that first reaches the same audit `warn!` on a
/// thread without a subscriber therefore caches `Interest::never`, and every
/// later record from that callsite, including this test's, is discarded
/// before any subscriber sees it. That is how the cancelled utility's
/// record went missing under a loaded parallel runner (mini-agent-jwvsr).
///
/// Rebuilding the cache from inside this scope and proving capture with a
/// probe that ends non-successfully (the same callsite) makes a record that
/// is missing later a production fault rather than a lost line: after a
/// captured probe the callsite is registered, and later rebuilds include
/// this still-live subscriber.
#[cfg(unix)]
async fn prime_support_audit_capture(sandbox: &Sandbox, capture: &AuditCapture) {
    let deadline = std::time::Instant::now() + SUPPORT_STAGE_BUDGET;
    let mut probes = 0;
    loop {
        tracing::callsite::rebuild_interest_cache();
        probes += 1;
        let mut probe = tokio::process::Command::new("bash");
        probe.args(["-c", "exit 3"]);
        let output = sandbox
            .status_support_command(
                probe,
                SupportCommandLimits {
                    timeout: SUPPORT_STAGE_BUDGET,
                },
                SupportCommandAudit::new(PROBE_UTILITY, "user-trusted-bypass"),
            )
            .await
            .unwrap();
        assert_eq!(output.status, CommandStatus::Completed);
        if capture.record(PROBE_UTILITY).is_some() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "support utility caller-drop stalled: the audit capture was not primed after \
             {probes} probes within {SUPPORT_STAGE_BUDGET:?} (captured audit: {})",
            capture.summary()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// Test subscriber sink that keeps each formatted event together with the
/// sandbox's active process-group count at the moment it was written, so a
/// test can prove an audit record precedes the release of its group.
#[cfg(unix)]
#[derive(Clone)]
struct AuditCapture {
    records: Arc<Mutex<Vec<(String, usize)>>>,
    sandbox: Sandbox,
}

#[cfg(unix)]
impl AuditCapture {
    fn new(sandbox: &Sandbox) -> Self {
        Self {
            records: Arc::new(Mutex::new(Vec::new())),
            sandbox: sandbox.clone(),
        }
    }

    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        let writer = self.clone();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish()
    }

    /// The terminal audit record of `utility`, with the active group count
    /// observed when it was written.
    fn record(&self, utility: &str) -> Option<(String, usize)> {
        let field = format!("utility=\"{utility}\"");
        self.records
            .lock()
            .unwrap()
            .iter()
            .find(|(line, _)| line.contains(&field) && line.contains("after process cleanup"))
            .cloned()
    }

    fn summary(&self) -> String {
        let records = self.records.lock().unwrap();
        let lines: Vec<&str> = records
            .iter()
            .map(|(line, _)| line.trim_end())
            .filter(|line| line.contains("support utility"))
            .collect();
        if lines.is_empty() {
            "none".to_string()
        } else {
            lines.join(" | ")
        }
    }
}

#[cfg(unix)]
impl Write for AuditCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let groups = self.sandbox.active_group_count();
        self.records
            .lock()
            .unwrap()
            .push((String::from_utf8_lossy(bytes).into_owned(), groups));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
