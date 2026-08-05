#[cfg(target_os = "macos")]
use std::process::{Command, Output};

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
fn run_seatbelt(profile: &str, executable: &str, arguments: &[&str]) -> Output {
    Command::new(SANDBOX_EXEC)
        .env_clear()
        .args(["-p", profile, executable])
        .args(arguments)
        .output()
        .expect("the supported macOS probe requires /usr/bin/sandbox-exec")
}

/// Real evidence for the macOS fail-closed gate.
///
/// A single Seatbelt profile cannot both let `sandbox-exec` enter the worker image and deny a
/// native-compromised worker from executing that same image later. macOS also refuses to apply a
/// second, tighter profile to the already sandboxed process. If a future supported macOS changes
/// any of those behaviors this gate fails so the production launcher can be reconsidered.
#[test]
#[cfg(target_os = "macos")]
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

    // Apply a deliberately permissive first profile. Because it denies no operation, the nested
    // `sandbox_apply` failure below cannot be attributed to a missing file, Mach, IPC, or process
    // allowance in the outer profile; the only boundary introduced here is that the process is
    // already sandboxed.
    let outer_profile = r#"(version 1)
(allow default)"#;
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
}

#[test]
fn macos_worker_launcher_source_owns_the_one_time_publication_transition() {
    let source = include_str!("../../../sandbox/worker/macos.rs");

    for required in [
        "OneTimeWorkerImage",
        "sweep_production_publications",
        "deny default",
        "process-exec",
        "env_clear()",
        "process_group(0)",
        "setrlimit",
        "finalize_authenticated_ready",
        "unlink_after_exec",
        "retire_after_reap",
        "ParentFrame::Shutdown",
        "VALIDATED_MACOS_MAJORS: &[u32] = &[26]",
        "run_full_containment_preflight",
        "attest_hosted_worker_containment",
        "probe_guardian_parent_death",
        "sweep_hosted_parent_death_publications",
        "MACOS_CONTAINMENT_MATRIX_V1=passed",
    ] {
        assert!(
            source.contains(required),
            "missing macOS scoped-boundary control: {required}"
        );
    }
}

#[test]
fn linux_worker_launcher_source_is_broker_only_and_fail_closed() {
    let source = include_str!("../../../sandbox/worker/linux.rs");

    for required in [
        "--clearenv",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--cap-drop",
        "ALL",
        "--die-with-parent",
        "--new-session",
        "--proc",
        "--dev",
        "--tmpfs",
        "INTERNAL_WORKER_MARKER",
    ] {
        assert!(
            source.contains(required),
            "missing Linux worker policy: {required}"
        );
    }

    assert!(!source.contains("Sandbox::wrap_command"));
    assert!(!source.contains("--bind\").arg(workspace"));
    assert!(!source.contains("cache_dir"));
    assert!(!source.contains("fn launch_unconfined"));
    assert!(source.contains("trusted_runtime_files"));
    assert!(source.contains("SYS_close_range"));
    assert!(source.contains("is_trusted_runtime_file"));
    let runtime_validation = source
        .split("fn trusted_runtime_files")
        .nth(1)
        .and_then(|tail| tail.split("fn interpreter_aliases").next())
        .expect("runtime-file validation source must remain inspectable");
    assert!(!runtime_validation.contains("is_trusted_system_path"));
    assert!(!source.contains("command.args([\"--ro-bind\", runtime, runtime])"));
}

#[test]
fn linux_worker_finalizer_is_before_ready_and_denies_process_creation() {
    let worker = include_str!("../worker.rs");
    let linux = include_str!("../../../sandbox/worker/linux.rs");
    let finalize = worker
        .find("finalize_internal_worker().map_err(|_| ())?")
        .expect("worker bootstrap must finalize native containment");
    let ready = worker
        .find("let ready: WorkerWireFrame")
        .expect("worker bootstrap must emit Ready");
    let accepted_hello = worker
        .find("protocol.on_receive(&hello).map_err(|_| ())?")
        .expect("worker bootstrap must authenticate Hello");
    assert!(
        accepted_hello < finalize && finalize < ready,
        "Linux finalizer must run after authenticated Hello and before Ready"
    );

    for required in [
        "set_no_new_privs",
        "Resource::As",
        "Resource::Cpu",
        "Resource::Nofile",
        "Resource::Core",
        "Resource::Fsize",
        "SYS_fork",
        "SYS_vfork",
        "SYS_clone",
        "SYS_clone3",
        "SYS_execve",
        "SYS_execveat",
        "SYS_socket",
        "SYS_socketpair",
        "SYS_unshare",
        "SYS_setns",
        "SYS_mount",
        "SYS_umount2",
        "SYS_pivot_root",
        "SYS_chroot",
        "apply_filter_all_threads",
        "X32_SYSCALL_BIT",
        "BPF_JSET",
        "assert_x32_syscall_range_denied",
        "CapEff:",
    ] {
        assert!(
            linux.contains(required),
            "missing Linux finalizer control: {required}"
        );
    }
}

#[test]
fn linux_worker_launcher_owns_group_teardown_and_pipe_failures() {
    let source = include_str!("../../../sandbox/worker/linux.rs");
    assert!(source.contains("process_group(0)"));
    assert!(source.contains("terminate_tree"));
    assert!(source.contains("cleanup_failed_launch"));
    assert!(source.contains("MissingPipe"));
    assert!(source.contains("run_worker_lifecycle_probes"));
    assert!(source.contains("complete_hello_ready"));
    assert!(source.contains("ParentFrame::RunStep"));
    assert!(source.contains("controlled_sleeper_identity"));
    assert!(source.contains("process_descendants"));
    assert!(source.contains("contained descendant survived parent teardown"));
    assert!(source.contains("CPU_LIMIT_ARMED"));
    assert!(source.contains("SIGXCPU"));
    assert!(source.contains("run_core_limit_probe"));
    assert!(source.contains("DumpableBehavior::NotDumpable"));
    assert!(source.contains("core_dumped()"));
    assert!(source.contains("core artifact"));
}

#[test]
fn windows_worker_runtime_probe_source_covers_the_required_security_matrix() {
    let source = include_str!("../../../sandbox/worker/windows.rs");

    for required in [
        "run_containment_probe",
        "run_containment_child_probe",
        "ParentFrame::ContainmentProbe(ContainmentProbe {})",
        "WorkerFrame::ContainmentAttested(ContainmentAttestation::Passed)",
        "child_token_is_zero_capability_lpac().unwrap_or(false)",
        "exact_protocol_std_handles()",
        "no_console_devices()",
        "ProcessChildProcessPolicy",
        "PROCESS_MITIGATION_CHILD_PROCESS_POLICY",
        "run_runtime_preflight_owned",
        "mini-agent-windows-preflight",
        "recv_timeout(remaining)",
        "WINDOWS_CONTAINMENT_PASS backend=lpac job_close=pass nested_parent_job=pass protocol=pass",
        "workspace_read_denied",
        "workspace_write_denied",
        "skill_database_read_denied",
        "skill_database_write_denied",
        "credential_environment_absent",
        "network_denied",
        "child_process_denied",
        "unlisted_file_handle_denied",
        "unlisted_socket_handle_denied",
        "file_canary: Option<File>",
        "socket_canary: Option<TcpListener>",
        "configuration.file_canary.take()",
        "configuration.socket_canary.take()",
        "creation_time_job_limits_match",
        "job_close_kills_worker",
        "nested_parent_job",
        "mitigation_policy_matches",
        "ProcessDynamicCodePolicy",
        "ProcessSystemCallDisablePolicy",
        "run_production_protocol_round_trip",
        "mini agent λ lpac",
        "MINI_AGENT_LPAC_PROTECTED_EXE",
        "ProtectedMachineWide",
        "verify_current_user_cannot_modify",
        "snapshot_file_security",
        "protected machine-wide negative-control owner or DACL changed",
        "CONTAINMENT_FAILURE_PREFIX",
        "failed closed checks code=",
        ".windows(CONTAINMENT_FAILURE_PREFIX.len())",
        "let _ = failure_stream.flush()",
        "child_exit_code=",
        "exit_deadline",
        "std::process::exit(0x1_0000 | i32::from(code))",
        "emit_containment_failure(0x8001)",
    ] {
        assert!(
            source.contains(required),
            "missing Windows real-containment evidence: {required}"
        );
    }
}

#[test]
fn windows_preflight_deadline_dominates_creation_lock_and_process_launch() {
    let source = include_str!("../../../sandbox/worker/windows.rs");
    let launch = source
        .split("pub(super) fn launch_production")
        .nth(1)
        .and_then(|tail| tail.split("enum RuntimePreflightTarget").next())
        .expect("production Windows launch source must remain inspectable");
    let creation_lock = launch
        .find("crate::process_creation::creation_guard()?")
        .expect("production launch must acquire the shared creation lock");
    let post_lock_deadline = launch[creation_lock..]
        .find("hooks.require_before_deadline()?")
        .map(|offset| creation_lock + offset)
        .expect("production launch must recheck its deadline after waiting for the lock");
    let create_process = launch
        .find("CreateProcessW(")
        .expect("production launch must retain the reviewed CreateProcessW boundary");
    let pre_create_deadline = launch[..create_process]
        .rfind("hooks.require_before_deadline()?")
        .expect("production launch must check its deadline immediately before CreateProcessW");

    assert!(creation_lock < post_lock_deadline);
    assert!(post_lock_deadline < pre_create_deadline);
    assert!(pre_create_deadline < create_process);
    assert!(source.contains(".with_deadline(deadline)"));
    assert!(source.contains("CreateProcessW itself is not cancellable"));
}

#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires the real Windows LPAC/AppContainer and Job backend"]
fn windows_js_worker_containment() {
    crate::sandbox::worker::run_windows_containment_probe()
        .expect("Windows JS worker containment and install-location probes must pass");
}

#[cfg(target_os = "windows")]
#[test]
fn windows_containment_probe_child() {
    if std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER).as_deref()
        != Some(std::ffi::OsStr::new("windows-containment-probe-v1"))
    {
        return;
    }
    crate::sandbox::worker::run_windows_containment_child_probe()
        .expect("contained Windows child probe must pass");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a real trusted bubblewrap backend and Linux namespace/seccomp support"]
fn linux_js_worker_containment() {
    crate::sandbox::worker::run_linux_containment_probe()
        .expect("Linux JS worker containment probe must pass");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_containment_probe_child() {
    let Some(marker) = std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER) else {
        return;
    };
    if !marker.to_string_lossy().starts_with("linux-probe-v1:") {
        return;
    }
    crate::sandbox::worker::run_linux_containment_child_probe()
        .expect("contained Linux child probe must pass");
    println!("MINI_AGENT_LINUX_CONTAINMENT_PASS");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_containment_preflight_child() {
    if std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER).as_deref()
        != Some(std::ffi::OsStr::new(
            crate::sandbox::worker::LINUX_PREFLIGHT_MARKER_VALUE,
        ))
    {
        return;
    }
    assert!(
        crate::sandbox::worker::standard_streams_are_protocol_pipes(),
        "Linux test-process preflight standard streams must all be protocol pipes"
    );
    crate::sandbox::worker::finalize_internal_worker()
        .expect("Linux test-process containment preflight must pass");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_cpu_limit_probe_child() {
    if std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER).as_deref()
        != Some(std::ffi::OsStr::new("linux-cpu-probe-v1"))
    {
        return;
    }
    crate::sandbox::worker::run_linux_cpu_limit_child_probe()
        .expect("CPU limit probe must be terminated before returning");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_core_limit_probe_child() {
    let Some(marker) = std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER) else {
        return;
    };
    match marker.to_string_lossy().as_ref() {
        "linux-core-probe-v1" => crate::sandbox::worker::run_linux_core_limit_child_probe()
            .expect("core-limit parent probe must verify the sacrificial child"),
        "linux-core-crash-v1" => {
            crate::sandbox::worker::run_linux_core_crash_child_probe()
                .expect("core-limit sacrificial child must be terminated before returning");
        }
        _ => {}
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_descendant_cleanup_probe_child() {
    if std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER).as_deref()
        != Some(std::ffi::OsStr::new("linux-descendant-probe-v1"))
    {
        return;
    }
    let executable = "/mini-agent-worker/mini-agent";
    let _descendant = std::process::Command::new(executable)
        .env_clear()
        .env(
            crate::sandbox::worker::INTERNAL_WORKER_MARKER,
            "linux-sleeper-v1",
        )
        .args([
            "--exact",
            "extras::js::tests::worker_containment::linux_containment_sleeper_child",
            "--nocapture",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("contained cleanup probe must create its controlled descendant");
    std::thread::park_timeout(std::time::Duration::from_secs(30));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_containment_sleeper_child() {
    if std::env::var_os(crate::sandbox::worker::INTERNAL_WORKER_MARKER).as_deref()
        == Some(std::ffi::OsStr::new("linux-sleeper-v1"))
    {
        std::thread::park_timeout(std::time::Duration::from_secs(30));
    }
}
