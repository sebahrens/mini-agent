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

#[cfg(feature = "js")]
#[tokio::test]
async fn sandbox_network_opt_in_never_reaches_brokered_js_spawn() {
    let sandbox = Sandbox::new(true, "seatbelt").with_network(SandboxNetwork::Allow);
    let owner = crate::extras::js::tool::PermissionBridgeOwner::new(
        None,
        None,
        std::time::Duration::from_secs(1),
    );
    let service = crate::extras::js::host::SpawnEffectService::new(
        sandbox,
        owner.bridge(),
        std::time::Duration::from_secs(1),
    );
    assert_eq!(service.sandbox_network(), SandboxNetwork::Deny);
}

#[test]
fn sandbox_network_allowlist_parses_and_is_not_full_network() {
    let cfg: crate::config::Config = toml::from_str(
        "sandbox-network = \"allowlist\"\nsandbox-network-hosts = [\"registry.npmjs.org\", \"*.github.com\"]\n",
    )
    .unwrap();
    assert_eq!(cfg.sandbox_network, Some(SandboxNetwork::Allowlist));
    assert_eq!(cfg.sandbox_network_hosts.len(), 2);
    assert!(!SandboxNetwork::Allowlist.allows());
    // bwrap cannot enforce the proxy: network stays unshared and the mode is
    // not reported as effective.
    let sandbox = Sandbox::new(true, "bwrap")
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(cfg.sandbox_network_hosts);
    assert!(!sandbox.shell_network_proxied() && !sandbox.shell_network_effective());
    assert!(
        bwrap_shell_args(&sandbox)
            .iter()
            .any(|arg| arg == "--unshare-net")
    );
    // Default closed even with a host list.
    let denied = Sandbox::new(true, "seatbelt").with_network_hosts(vec!["a.example.com".into()]);
    assert!(!denied.shell_network_proxied());
    assert!(denied.network_proxy().unwrap().is_none());
}

/// Live check on macOS: with `allowlist`, the sandbox can reach only the
/// loopback filtering proxy. Direct connections are denied by Seatbelt, an
/// unlisted host gets the proxy's 403, and the proxy environment is set. No
/// Internet access is needed.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_network_allowlist_seatbelt_live_only_the_proxy_is_reachable() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    if seatbelt_path().is_none() || !seatbelt_exists() {
        skip_unusable_real_sandbox_test("Seatbelt is unavailable for the live allowlist probe");
        return;
    }
    let workspace = Arc::new(
        crate::paths::WorkspaceBinding::capture(&std::env::current_dir().unwrap()).unwrap(),
    );
    let sandbox = Sandbox::new(true, "seatbelt")
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(vec!["allowed.example.com".into()])
        .with_workspace_binding(workspace);
    let run = |command: &'static str| {
        let sandbox = sandbox.clone();
        async move {
            let output = sandbox
                .output_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
                .await
                .unwrap();
            (
                output.exit_status.is_some_and(|exit| exit.success()),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        }
    };
    let (_, env, _) = run("printf '%s' \"$HTTPS_PROXY\"").await;
    assert!(
        env.starts_with("http://mini-agent:") && env.contains("@127.0.0.1:"),
        "{env}"
    );
    // Bypassing the proxy: the profile denies the direct connection.
    let (ok, out, err) = run(
        "curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 8 http://93.184.216.34/",
    )
    .await;
    assert!(
        !ok && out.trim() != "200",
        "direct connection must be denied: {out} {err}"
    );
    // Through the proxy, an unlisted host is refused by the proxy itself (the
    // tunnel is rejected with 403), which also proves the proxy is reachable.
    let (ok, _, err) =
        run("curl -sS -o /dev/null --max-time 15 https://unlisted.example.net/").await;
    assert!(!ok && err.contains("403"), "proxy must answer 403: {err}");
    // Loopback is never reachable through the proxy even when listed.
    let (_, code, err) =
        run("curl -sS -o /dev/null -w '%{http_code}' --max-time 15 http://127.0.0.1:9/").await;
    assert_eq!(code.trim(), "403", "{err}");
    // Needs Internet access: a listed host works end to end through the proxy.
    if std::env::var_os("MINI_AGENT_NETWORK_TESTS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let listed = sandbox
            .clone()
            .with_network_hosts(vec!["example.com".into()]);
        let output = listed
            .output_command_with_limits(
                "curl -sS -o /dev/null -w '%{http_code}' --max-time 20 https://example.com",
                DEFAULT_COMMAND_LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "200");
    }
}
