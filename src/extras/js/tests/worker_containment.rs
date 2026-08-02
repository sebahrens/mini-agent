use std::process::{Command, Output};

use crate::sandbox::worker::{
    WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus, containment_status,
};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn run_seatbelt(profile: &str, executable: &str, arguments: &[&str]) -> Output {
    Command::new(SANDBOX_EXEC)
        .env_clear()
        .args(["-p", profile, executable])
        .args(arguments)
        .output()
        .expect("the supported macOS probe requires /usr/bin/sandbox-exec")
}

#[test]
fn macos_worker_status_is_typed_deprecated_best_effort_and_fail_closed() {
    let WorkerContainmentStatus::Unavailable {
        backend,
        assurance,
        reason,
    } = containment_status()
    else {
        panic!("macOS must remain unavailable while post-launch exec cannot be denied");
    };

    assert_eq!(backend, WorkerBackend::Seatbelt);
    assert_eq!(assurance, WorkerContainmentAssurance::DeprecatedBestEffort);
    assert!(reason.contains("sandbox-exec"));
    assert!(reason.contains("initial exec"));
    assert!(reason.contains("reusable"));
    assert!(reason.contains("tighten"));
}

/// Real evidence for the macOS fail-closed gate.
///
/// A single Seatbelt profile cannot both let `sandbox-exec` enter the worker image and deny a
/// native-compromised worker from executing that same image later. macOS also refuses to apply a
/// second, tighter profile to the already sandboxed process. If a future supported macOS changes
/// any of those behaviors this gate fails so the production launcher can be reconsidered.
#[test]
#[ignore = "requires the real macOS Seatbelt backend outside an enclosing sandbox"]
fn macos_js_worker_containment() {
    assert!(
        Command::new("/usr/bin/true")
            .env_clear()
            .status()
            .expect("unsandboxed executable control")
            .success(),
        "the unsandboxed executable control must succeed"
    );

    let deny_initial_exec = run_seatbelt(
        r#"(version 1)
(deny default)
(allow file-read*)"#,
        "/usr/bin/true",
        &[],
    );
    assert!(
        !deny_initial_exec.status.success(),
        "deny process-exec unexpectedly permitted sandbox-exec's initial image"
    );
    let deny_initial_stderr = String::from_utf8_lossy(&deny_initial_exec.stderr);
    assert!(
        deny_initial_stderr.contains("execvp")
            && deny_initial_stderr.contains("Operation not permitted"),
        "initial launch failed for an unrelated reason: {deny_initial_stderr}"
    );

    let reusable_exact_exec = run_seatbelt(
        r#"(version 1)
(deny default)
(allow process-exec
    (literal "/usr/bin/env")
    (literal "/usr/bin/true"))
(allow file-read*)"#,
        "/usr/bin/env",
        &["-i", "/usr/bin/true"],
    );
    assert!(
        reusable_exact_exec.status.success(),
        "an exact initial image allow was not reusable; reassess the fail-closed gate: {}",
        String::from_utf8_lossy(&reusable_exact_exec.stderr)
    );

    let outer_profile = r#"(version 1)
(deny default)
(allow process-exec
    (literal "/usr/bin/sandbox-exec")
    (literal "/usr/bin/true"))
(allow file-read*)"#;
    let inner_profile = r#"(version 1)
(deny default)
(allow process-exec (literal "/usr/bin/true"))
(allow file-read*)"#;
    let inner_control = run_seatbelt(inner_profile, "/usr/bin/true", &[]);
    assert!(
        inner_control.status.success(),
        "the tighter profile must work when applied first: {}",
        String::from_utf8_lossy(&inner_control.stderr)
    );
    let rejected_tightening = run_seatbelt(
        outer_profile,
        SANDBOX_EXEC,
        &["-p", inner_profile, "/usr/bin/true"],
    );
    assert!(
        !rejected_tightening.status.success(),
        "macOS unexpectedly accepted a second Seatbelt profile; reassess the launcher design"
    );
    let rejected_tightening_stderr = String::from_utf8_lossy(&rejected_tightening.stderr);
    assert!(
        rejected_tightening_stderr.contains("sandbox_apply")
            && rejected_tightening_stderr.contains("Operation not permitted"),
        "profile tightening failed for an unrelated reason: {rejected_tightening_stderr}"
    );

    macos_worker_status_is_typed_deprecated_best_effort_and_fail_closed();
}
