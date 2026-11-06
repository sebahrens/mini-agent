//! Built-in language-server defaults and resolution against user
//! `[lsp.servers]` config. Servers are PATH binaries only — mini-agent never
//! auto-installs anything; a missing binary is skipped at spawn time.

use std::collections::HashMap;

use compact_str::CompactString;

use crate::config::types::LspServerConfig;

/// Parent variables every built-in server receives. With the environment
/// cleared, servers that shell out to a toolchain (gopls to `go`, pyright to
/// the active virtualenv, rust-analyzer to cargo/rustup) otherwise cannot find
/// their caches, homes or workspaces. Names only: nothing credential-shaped
/// (tokens, API keys, proxy URLs) is listed, and a user override of
/// `inherit_env` replaces this list.
pub(crate) const BUILTIN_INHERIT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "LOCALAPPDATA",
    "APPDATA",
    "TMPDIR",
    "TEMP",
    "TMP",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "GOPATH",
    "GOROOT",
    "GOCACHE",
    "GOMODCACHE",
    "GOFLAGS",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "VIRTUAL_ENV",
    "JAVA_HOME",
];

fn server(command: &str, args: &[&str], extensions: &[&str]) -> LspServerConfig {
    LspServerConfig {
        command: CompactString::from(command),
        args: args.iter().map(|s| CompactString::from(*s)).collect(),
        extensions: extensions.iter().map(|s| CompactString::from(*s)).collect(),
        env: HashMap::new(),
        inherit_env: BUILTIN_INHERIT_ENV
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        sandbox: None,
        network: Default::default(),
        initialization: None,
        disabled: false,
        wait_ms: None,
    }
}

/// Defaults applied when `[lsp]` is enabled. Each is used only if its binary
/// is found on PATH at spawn time.
pub fn builtin_servers() -> Vec<(&'static str, LspServerConfig)> {
    vec![
        ("rust", server("rust-analyzer", &[], &[".rs"])),
        ("go", server("gopls", &[], &[".go"])),
        (
            "typescript",
            server(
                "typescript-language-server",
                &["--stdio"],
                &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
            ),
        ),
        (
            "python",
            server("pyright-langserver", &["--stdio"], &[".py", ".pyi"]),
        ),
        (
            "clangd",
            server(
                "clangd",
                &[],
                &[".c", ".h", ".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx"],
            ),
        ),
        (
            "bash",
            server(
                "bash-language-server",
                &["start"],
                &[".sh", ".bash", ".zsh"],
            ),
        ),
        ("lua", server("lua-language-server", &[], &[".lua"])),
    ]
}

/// Merges user config over the built-ins: a same-named entry replaces the
/// built-in, `disabled = true` removes it, new names add custom servers.
/// Entries without a command are dropped (they can only ever disable).
/// User servers take precedence, ordered by name, followed by unoverridden
/// built-ins in declaration order.
pub fn resolve_servers(user: &HashMap<String, LspServerConfig>) -> Vec<(String, LspServerConfig)> {
    let mut configured: Vec<_> = user.iter().collect();
    configured.sort_unstable_by_key(|(name, _)| *name);
    let mut servers: Vec<_> = configured
        .into_iter()
        .filter(|(_, cfg)| !cfg.disabled && !cfg.command.is_empty())
        .map(|(name, cfg)| (name.clone(), cfg.clone()))
        .collect();
    servers.extend(
        builtin_servers()
            .into_iter()
            .filter(|(name, _)| !user.contains_key(*name))
            .map(|(name, cfg)| (name.to_string(), cfg)),
    );
    servers
}

/// Metadata-only eligibility under the launcher's existing absolute-PATH rule.
/// This never starts a server or proves its later handshake will succeed.
pub(crate) fn available_servers(
    user: &HashMap<String, LspServerConfig>,
    search: Option<&std::ffi::OsStr>,
) -> Vec<(String, LspServerConfig)> {
    let bounded = search
        .filter(|value| value.len() <= 32 * 1024)
        .map(|value| {
            std::env::split_paths(value)
                .filter(|path| path.is_absolute())
                .take(128)
                .collect::<Vec<_>>()
        })
        .and_then(|roots| std::env::join_paths(roots).ok());
    resolve_servers(user)
        .into_iter()
        .filter(|(_, server)| {
            server.command.len() <= 4096
                && crate::extras::executable_search::resolve_service_executable(
                    server.command.as_str(),
                    bounded.as_deref(),
                    None,
                )
                .is_ok()
        })
        .collect()
}

/// First server claiming this path's extension (case-insensitive, dot
/// included in configured extensions).
pub fn server_for_path<'a>(
    servers: &'a [(String, LspServerConfig)],
    path: &std::path::Path,
) -> Option<&'a (String, LspServerConfig)> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let dot_ext = format!(".{ext}");
    servers
        .iter()
        .find(|(_, cfg)| cfg.extensions.iter().any(|e| e.as_str() == dot_ext))
}

#[cfg(test)]
mod inherit_env_tests {
    use super::*;

    #[test]
    fn builtins_inherit_home_and_toolchain_variables_but_no_secrets() {
        for (name, cfg) in builtin_servers() {
            for required in [
                "PATH",
                "HOME",
                "GOCACHE",
                "GOPATH",
                "CARGO_HOME",
                "VIRTUAL_ENV",
            ] {
                assert!(
                    cfg.inherit_env.iter().any(|v| v == required),
                    "{name} must inherit {required}"
                );
            }
            for forbidden in [
                "OPENROUTER_API_KEY",
                "GITHUB_TOKEN",
                "AWS_SECRET_ACCESS_KEY",
                "HTTPS_PROXY",
                "GOPROXY",
            ] {
                assert!(
                    !cfg.inherit_env.iter().any(|v| v == forbidden),
                    "{name} must not inherit {forbidden}"
                );
            }
        }
    }

    #[test]
    fn builtin_environment_delegates_set_variables_and_skips_unset_ones() {
        let _env = crate::tests::ScopedProcessEnv::set(&[
            ("GOCACHE", Some("/cache/go".into())),
            ("GOMODCACHE", None),
            ("OPENROUTER_API_KEY", Some("sk-secret".into())),
        ]);
        let (_, cfg) = builtin_servers()
            .into_iter()
            .find(|(name, _)| *name == "go")
            .unwrap();
        let delegated =
            crate::extras::lsp::client::delegated_environment_for_test(&cfg.inherit_env, &cfg.env)
                .unwrap();
        let get = |name: &str| {
            delegated
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        assert_eq!(get("GOCACHE").as_deref(), Some("/cache/go"));
        assert_eq!(get("GOMODCACHE"), None);
        assert_eq!(get("OPENROUTER_API_KEY"), None);
    }
}
