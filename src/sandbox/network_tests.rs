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
    // bwrap enforces the allowlist through a private proxy socket; the
    // namespace stays unshared and this is not full network access.
    let sandbox = Sandbox::new(true, "bwrap")
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(cfg.sandbox_network_hosts);
    assert!(sandbox.shell_network_proxied() && !sandbox.shell_network_effective());
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

#[cfg(unix)]
fn allowlist_bwrap_args(hosts: &[&str]) -> Vec<String> {
    let sandbox = Sandbox::new(true, "bwrap")
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(hosts.iter().map(|host| host.to_string()).collect());
    bwrap_shell_args(&sandbox)
}

/// The bwrap allowlist keeps the empty network namespace and adds exactly two
/// things: the proxy's private Unix socket and the forwarder that relays it to
/// the namespace's own loopback. The command runs behind the forwarder.
#[cfg(unix)]
#[test]
fn sandbox_network_allowlist_bwrap_keeps_the_empty_namespace_and_only_adds_the_proxy_socket() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    let args = allowlist_bwrap_args(&["registry.npmjs.org"]);
    assert!(args.iter().any(|arg| arg == "--unshare-net"));
    assert!(!args.iter().any(|arg| arg == "--share-net"));
    // Namespace bind of the proxy socket (a socket, nothing else from the
    // host's /tmp) and the read-only helper executable.
    let socket_bind = args
        .windows(3)
        .find(|w| w[0] == "--bind" && w[2] == BWRAP_FORWARDER_SOCKET)
        .expect("proxy socket is bound into the namespace");
    assert!(
        socket_bind[1].ends_with("/proxy.sock") && socket_bind[1].contains("mini-agent-net-"),
        "{socket_bind:?}"
    );
    assert!(
        args.windows(3)
            .any(|w| w[0] == "--ro-bind" && w[2] == BWRAP_FORWARDER_EXECUTABLE)
    );
    // Proxy variables point at the in-namespace forwarder, with a credential.
    let routed = LAST_BWRAP_ENV.with(|last| last.borrow().clone());
    let proxy_url = routed
        .iter()
        .find(|(key, _)| key == "HTTPS_PROXY")
        .expect("proxy variable is exported")
        .1
        .to_string_lossy()
        .into_owned();
    assert!(
        proxy_url.starts_with("http://mini-agent:")
            && proxy_url.ends_with(&format!("@127.0.0.1:{}", net_proxy::FORWARDER_PORT)),
        "{proxy_url:?}"
    );
    // The token must not be readable from the world-readable bwrap cmdline.
    // (bwrap only runs on Linux; other hosts spell the arguments out.)
    if cfg!(target_os = "linux") {
        assert!(
            !args.iter().any(|arg| arg.contains("mini-agent:")),
            "{args:?}"
        );
    }
    // The command runs behind the forwarder, after the bwrap options.
    let separator = args.iter().position(|arg| arg == "--").unwrap();
    assert_eq!(args[separator + 1], BWRAP_FORWARDER_EXECUTABLE);
    assert_eq!(args[separator + 2], net_forwarder::FORWARDER_ARG);
    assert_eq!(args[separator + 3], BWRAP_FORWARDER_SOCKET);
    assert_eq!(args[separator + 5], "--");
    assert!(args.last().unwrap().contains("printf sandboxed"));
    // CA/resolver config is visible for TLS verification inside the sandbox.
    assert!(
        args.windows(3)
            .any(|w| w[0] == "--ro-bind-try" && w[1] == "/etc/ssl")
    );
}

/// Default closed: neither `deny` nor `allow` nor an unset mode with a host
/// list ever mounts the proxy socket or routes the command through the
/// forwarder.
#[cfg(unix)]
#[test]
fn sandbox_network_allowlist_bwrap_is_default_closed() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    let hosts = vec!["registry.npmjs.org".to_string()];
    for network in [SandboxNetwork::Deny, SandboxNetwork::Allow] {
        let sandbox = Sandbox::new(true, "bwrap")
            .with_network(network)
            .with_network_hosts(hosts.clone());
        assert!(!sandbox.shell_network_proxied());
        let args = bwrap_shell_args(&sandbox);
        assert!(!args.iter().any(|arg| arg.contains("mini-agent-net")));
        assert!(!args.iter().any(|arg| arg == net_forwarder::FORWARDER_ARG));
        assert!(!args.iter().any(|arg| arg.ends_with("_PROXY")));
    }
    // A disabled sandbox never starts a proxy either.
    let off = Sandbox::new(false, "bwrap")
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(hosts);
    assert!(!off.shell_network_proxied());
    assert!(off.network_proxy().unwrap().is_none());
}

/// Hooks, workspace services and brokered JS spawn never get the proxy: the
/// snapshot (JS spawn) builder has no forwarder and keeps `--unshare-net`.
#[cfg(unix)]
#[test]
fn sandbox_network_allowlist_bwrap_does_not_reach_hooks_or_js_spawn() {
    let source = include_str!("../sandbox.rs");
    let hook = source
        .split("fn build_hook_bwrap_command(")
        .nth(1)
        .and_then(|rest| rest.split("\n    fn ").next())
        .unwrap();
    assert!(!hook.contains("network_proxy") && !hook.contains("bwrap_forwarder_launcher"));
    let snapshot = source
        .split("fn build_bwrap_snapshot_command(")
        .nth(1)
        .and_then(|rest| rest.split("fn build_bwrap_base_command(").next())
        .unwrap();
    assert!(!snapshot.contains("network_proxy") && !snapshot.contains("bwrap_forwarder_launcher"));
}

/// Native Linux control: inside a real bubblewrap namespace the command has
/// no route except the filtering proxy. Needs a built `mini-agent` binary next
/// to the test binary (it acts as the forwarder helper); run on a suitable
/// Linux runner like the other native controls.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires native Linux namespaces, bubblewrap and a built mini-agent binary"]
async fn sandbox_network_allowlist_bwrap_native_only_the_proxy_is_reachable() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    super::linux_workspace_tests::required_backend();
    let forwarder = std::env::current_exe()
        .unwrap()
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("mini-agent");
    assert!(
        forwarder.exists(),
        "native control requires the built binary at {}",
        forwarder.display()
    );
    let root = std::env::temp_dir().join(format!("mini-agent-net-native-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let cache = root.join("cache");
    crate::fs::ensure_private_directory(&cache).unwrap();
    let sandbox = super::linux_workspace_tests::bound(&root.join("ws"), &cache)
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(vec!["allowed.example.com".into()])
        .with_net_forwarder_exe(forwarder);
    let run = |script: String| {
        let sandbox = sandbox.clone();
        async move {
            let output = sandbox
                .output_command_with_limits(&script, DEFAULT_COMMAND_LIMITS)
                .await
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    };
    // Raw talk to the forwarder: no credential -> the proxy answers 407, a
    // listed name that resolves to loopback -> 403, an unlisted name -> 403.
    // Everything else (a direct route to the Internet) must not exist.
    let probe = |request: &str| {
        format!(
            "exec 3<>/dev/tcp/127.0.0.1/{port} || {{ echo NOFORWARDER; exit 0; }}; printf '{request}' >&3; head -n1 <&3",
            port = net_proxy::FORWARDER_PORT
        )
    };
    let bash = |script: String| format!("bash -c '{}'", script.replace('\'', "'\\''"));
    let out = run(bash(probe(
        "CONNECT allowed.example.com:443 HTTP/1.1\\r\\n\\r\\n",
    )))
    .await;
    assert!(
        out.contains(" 407 "),
        "forwarder must reach the proxy: {out}"
    );
    let out = run(bash(
        "exec 3<>/dev/tcp/93.184.216.34/80 && echo DIRECT || echo NOROUTE".into(),
    ))
    .await;
    assert!(out.contains("NOROUTE"), "no direct route may exist: {out}");
    let out = run("printf '%s' \"$HTTPS_PROXY\"".into()).await;
    assert!(out.contains("@127.0.0.1:3128"), "{out}");
    let _ = std::fs::remove_dir_all(root);
}

/// `/proc/<pid>/cmdline` of the long-lived bwrap process is world-readable:
/// opt-in secrets and the proxy token must reach the sandbox another way.
#[cfg(target_os = "linux")]
#[test]
fn bwrap_argv_never_carries_environment_values() {
    let _process_env = crate::tests::ScopedProcessEnv::set(&[]);
    let sandbox = Sandbox::new(true, "bwrap")
        .with_network(SandboxNetwork::Allowlist)
        .with_network_hosts(vec!["registry.npmjs.org".to_string()])
        .with_shell_env(ShellEnvPolicy {
            set: [(
                "GH_TOKEN".to_string(),
                "adversarial-secret-value".to_string(),
            )]
            .into(),
            allow_secrets: true,
            ..ShellEnvPolicy::default()
        });
    let args = bwrap_shell_args(&sandbox);
    let routed = LAST_BWRAP_ENV.with(|last| last.borrow().clone());
    assert!(
        routed
            .iter()
            .any(|(key, value)| key == "GH_TOKEN" && value == "adversarial-secret-value"),
        "the secret must still reach the sandbox: {routed:?}"
    );
    assert!(
        !args.iter().any(|arg| arg == "--setenv"
            || arg.contains("adversarial-secret-value")
            || arg.contains("mini-agent:")),
        "{args:?}"
    );
    assert!(args.windows(2).any(|w| w[0] == "--args"));
}
