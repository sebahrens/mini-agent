//! `sandbox-network` opt-in: the default stays closed and only contained
//! shell commands gain the network when it is explicitly allowed.

use std::path::Path;

use super::*;

fn bwrap_shell_args(sandbox: &Sandbox) -> Vec<String> {
    sandbox
        .build_bwrap_command(
            Path::new("/usr/bin/bwrap"),
            "printf sandboxed",
            Path::new("/workspace"),
            Path::new("/cache/mini-agent"),
            git_metadata::BwrapGitMetadataBinds::none(),
            &read_roots::BwrapReadRoots::none(),
        )
        .unwrap()
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn sandbox_network_defaults_to_deny_and_bwrap_keeps_unshare_net() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    let sandbox = Sandbox::new(true, "bwrap");
    assert_eq!(sandbox.network(), SandboxNetwork::Deny);
    assert_eq!(crate::config::Config::default().sandbox_network, None);
    let args = bwrap_shell_args(&sandbox);
    assert!(args.iter().any(|arg| arg == "--unshare-net"));
    assert!(!args.iter().any(|arg| arg == "/etc/resolv.conf"));
    let matrix = sandbox.capability_matrix_for_policy(SandboxPolicy::RequiredAndAvailable);
    assert_eq!(
        matrix.requested_network_policy,
        BWRAP_REQUESTED_NETWORK_POLICY
    );
}

#[test]
fn sandbox_network_allow_drops_unshare_net_only_for_shell_commands() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    let sandbox = Sandbox::new(true, "bwrap").with_network(SandboxNetwork::Allow);
    let args = bwrap_shell_args(&sandbox);
    assert!(!args.iter().any(|arg| arg == "--unshare-net"));
    // Workspace profile: resolver config becomes visible, read-only.
    assert!(
        args.windows(3)
            .any(|w| w[0] == "--ro-bind-try" && w[1] == "/etc/resolv.conf")
    );
    // Every other namespace stays isolated.
    for flag in [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup",
        "--remount-ro",
        "--new-session",
    ] {
        assert!(args.iter().any(|arg| arg == flag), "missing {flag}");
    }
    let matrix = sandbox.capability_matrix_for_policy(SandboxPolicy::RequiredAndAvailable);
    assert!(matrix.network.contains("sandbox-network = allow"));
    assert!(matrix.network.contains("JS spawn remain denied"));

    // Brokered JS spawn and hooks never take the shell opt-in.
    let source = include_str!("../sandbox.rs");
    let snapshot = source
        .split("fn build_bwrap_snapshot_command(")
        .nth(1)
        .and_then(|rest| rest.split("fn build_bwrap_base_command(").next())
        .unwrap();
    assert!(snapshot.contains("self.profile != SandboxProfile::HostFull,\n            true,\n"));
}

#[test]
fn sandbox_network_seatbelt_profiles_deny_by_default_and_allow_resolution_when_opted_in() {
    let profile = |deny| {
        seatbelt_profile_ladder(
            SandboxProfile::Workspace,
            "/workspace",
            "/cache",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            deny,
        )
    };
    let denied = profile(true);
    assert!(denied.ends_with("(deny network*)"));
    assert!(!denied.contains("(allow network"));
    assert!(!denied.contains("mach-lookup"));
    let allowed = profile(false);
    assert!(allowed.contains("(allow network*)"));
    assert!(allowed.contains("com.apple.dnssd.service"));
    assert!(allowed.contains("/private/etc/resolv.conf"));
    assert!(!allowed.contains("(deny network*)"));
    // Filesystem policy is unchanged by the network opt-in.
    assert_eq!(
        denied.replace("\n(deny network*)", ""),
        allowed.replace(
            &format!("\n{SEATBELT_NETWORK_READS}\n{SEATBELT_ALLOW_NETWORK}"),
            ""
        )
    );
}

#[test]
fn sandbox_network_config_parses_and_rejects_unknown_values() {
    let cfg: crate::config::Config = toml::from_str("sandbox-network = \"allow\"\n").unwrap();
    assert_eq!(cfg.sandbox_network, Some(SandboxNetwork::Allow));
    let cfg: crate::config::Config = toml::from_str("sandbox_network = \"deny\"\n").unwrap();
    assert_eq!(cfg.sandbox_network, Some(SandboxNetwork::Deny));
    assert!(toml::from_str::<crate::config::Config>("sandbox-network = \"open\"\n").is_err());
}

#[test]
fn sandbox_network_status_label_marks_the_opt_in() {
    let allowed = Sandbox::new(true, "bwrap").with_network(SandboxNetwork::Allow);
    assert_eq!(
        crate::ui::statusline::sandbox_label(&allowed),
        Some("sandbox:net")
    );
    assert_eq!(
        crate::ui::statusline::sandbox_label(&Sandbox::new(true, "bwrap")),
        None
    );
}

/// Live check on macOS. Default Seatbelt denies the connection; the opt-in
/// lets `curl` and `git ls-remote` reach a public host. The allowed half
/// needs Internet access and runs only with `MINI_AGENT_NETWORK_TESTS=1`.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_network_seatbelt_live_default_denies_and_opt_in_reaches_the_network() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    if seatbelt_path().is_none() || !seatbelt_exists() {
        skip_unusable_real_sandbox_test("Seatbelt is unavailable for the live network probe");
        return;
    }
    let workspace = Arc::new(
        crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
    );
    let probe = "curl -sS -o /dev/null -w '%{http_code}' --max-time 15 https://example.com";
    let denied = Sandbox::new(true, "seatbelt").with_workspace_binding(workspace.clone());
    let run = denied
        .output_command_with_limits(probe, DEFAULT_COMMAND_LIMITS)
        .await
        .unwrap();
    assert!(
        !run.exit_status.is_some_and(|exit| exit.success()),
        "default sandbox must deny the network: {}",
        String::from_utf8_lossy(&run.stdout)
    );
    if std::env::var_os("MINI_AGENT_NETWORK_TESTS").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let allowed = Sandbox::new(true, "seatbelt")
        .with_network(SandboxNetwork::Allow)
        .with_workspace_binding(workspace);
    for (command, expect) in [
        (probe, "200"),
        (
            "export GIT_CONFIG_GLOBAL=/dev/null GIT_DIR=\"$TMPDIR/net-probe.git\"; git init -q --bare \"$GIT_DIR\" && git ls-remote https://github.com/git/git HEAD | cut -c1-4 | tr -d '\\n' | wc -c | tr -d ' '",
            "4",
        ),
    ] {
        let run = allowed
            .output_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            expect,
            "{command}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}
