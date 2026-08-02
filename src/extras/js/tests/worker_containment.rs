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
        "--close-fds",
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
    assert!(source.contains("is_trusted_system_path"));
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
    assert!(source.contains("process_descendants"));
    assert!(source.contains("contained descendant survived parent teardown"));
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
