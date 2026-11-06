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
        settings: None,
        startup_timeout_ms: None,
        request_timeout_ms: None,
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
        ("java", server("jdtls", &[], &[".java"])),
        ("csharp", server("csharp-ls", &[], &[".cs"])),
        (
            "ruby",
            server("ruby-lsp", &[], &[".rb", ".rake", ".gemspec"]),
        ),
        ("php", server("intelephense", &["--stdio"], &[".php"])),
        (
            "kotlin",
            server("kotlin-language-server", &[], &[".kt", ".kts"]),
        ),
        ("swift", server("sourcekit-lsp", &[], &[".swift"])),
        ("zig", server("zls", &[], &[".zig"])),
    ]
}

/// Built-ins whose servers evaluate repository build definitions or run
/// repository-selected code to resolve a project, so opening an untrusted
/// repository and editing one file would run repository code outside the shell
/// sandbox: Gradle, MSBuild, Bundler, SwiftPM and `build.zig` (java, csharp,
/// ruby, kotlin, swift, zig); `build.rs` and proc macros (rust); the
/// interpreter and plugins a repository's `pyrightconfig.json`/`tsconfig.json`
/// select (python, typescript); and the toolchain a repository's `go.mod`
/// selects (go). They start only in a project whose config is trusted; a
/// same-named `[lsp.servers.<name>]` entry is the user's explicit choice and
/// is never withheld.
pub const BUILD_EVALUATING_BUILTINS: &[&str] = &[
    "java",
    "csharp",
    "ruby",
    "kotlin",
    "swift",
    "zig",
    "rust",
    "python",
    "typescript",
    "go",
];

/// A built-in left out of the registered set because the project is untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WithheldBuiltin {
    pub name: String,
    pub extensions: Vec<String>,
}

impl WithheldBuiltin {
    /// One-line reason and remedy, shown by `/doctor` and the `lsp` tool.
    pub(crate) fn explanation(&self) -> String {
        format!(
            "built-in '{0}' language server is withheld: it runs repository build/config code and this project's .mini-agent/config.toml is missing or not trusted. Trust the project config, or define [lsp.servers.{0}] in your own config to enable it",
            self.name
        )
    }
}

/// Splits the build-evaluating built-ins of `servers` off unless `trusted()`
/// says the project is trusted. `trusted` runs only when one of them is
/// present.
pub(crate) fn split_untrusted_builtins(
    servers: Vec<(String, LspServerConfig)>,
    user: &HashMap<String, LspServerConfig>,
    trusted: impl FnOnce() -> bool,
) -> (Vec<(String, LspServerConfig)>, Vec<WithheldBuiltin>) {
    let gated = |name: &str| BUILD_EVALUATING_BUILTINS.contains(&name) && !user.contains_key(name);
    if !servers.iter().any(|(name, _)| gated(name)) || trusted() {
        return (servers, Vec::new());
    }
    let (withheld, kept): (Vec<_>, Vec<_>) = servers.into_iter().partition(|(name, _)| gated(name));
    let withheld = withheld
        .into_iter()
        .map(|(name, cfg)| {
            tracing::warn!(
                "lsp[{name}]: built-in server not started because it evaluates project build files and this project's config is not trusted; trust the project config or define [lsp.servers.{name}] yourself"
            );
            WithheldBuiltin {
                name,
                extensions: cfg.extensions.iter().map(|e| e.to_string()).collect(),
            }
        })
        .collect();
    (kept, withheld)
}

/// Drops the build-evaluating built-ins of `servers` unless trusted.
#[cfg(test)]
pub(crate) fn withhold_untrusted_builtins(
    servers: Vec<(String, LspServerConfig)>,
    user: &HashMap<String, LspServerConfig>,
    trusted: impl FnOnce() -> bool,
) -> Vec<(String, LspServerConfig)> {
    split_untrusted_builtins(servers, user, trusted).0
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
mod language_coverage_tests {
    use super::*;

    #[test]
    fn builtins_cover_jvm_dotnet_ruby_php_kotlin_swift_and_zig() {
        let servers = resolve_servers(&HashMap::new());
        for (file, expected) in [
            ("A.java", "jdtls"),
            ("a.cs", "csharp-ls"),
            ("a.rb", "ruby-lsp"),
            ("Rakefile.rake", "ruby-lsp"),
            ("x.gemspec", "ruby-lsp"),
            ("a.php", "intelephense"),
            ("a.kt", "kotlin-language-server"),
            ("build.gradle.kts", "kotlin-language-server"),
            ("a.swift", "sourcekit-lsp"),
            ("a.zig", "zls"),
            ("a.rs", "rust-analyzer"),
        ] {
            let (_, server) = server_for_path(&servers, std::path::Path::new(file))
                .unwrap_or_else(|| panic!("no server for {file}"));
            assert_eq!(server.command, expected, "{file}");
        }
    }

    fn fixture(name: &str, extension: &str) -> (String, LspServerConfig) {
        (name.to_string(), server(name, &[], &[extension]))
    }

    #[test]
    fn build_evaluating_builtins_are_withheld_unless_trusted_or_user_defined() {
        let all = resolve_servers(&HashMap::new());
        let kept = |servers: Vec<(String, LspServerConfig)>| {
            servers
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };
        let untrusted = kept(withhold_untrusted_builtins(
            all.clone(),
            &HashMap::new(),
            || false,
        ));
        for gated in BUILD_EVALUATING_BUILTINS {
            assert!(
                !untrusted.iter().any(|n| n == gated),
                "{gated} must be withheld"
            );
        }
        for plain in ["clangd", "bash", "lua", "php"] {
            assert!(untrusted.iter().any(|n| n == plain), "{plain} stays");
        }
        let trusted = kept(withhold_untrusted_builtins(
            all.clone(),
            &HashMap::new(),
            || true,
        ));
        assert_eq!(trusted.len(), all.len());
        // A user-defined entry of the same name is explicit and never withheld.
        let mut user = HashMap::new();
        user.insert(
            "kotlin".to_string(),
            server("my-kotlin-ls", &[], &[".kt"]).clone(),
        );
        let resolved = resolve_servers(&user);
        let with_override = kept(withhold_untrusted_builtins(resolved, &user, || false));
        assert!(with_override.iter().any(|n| n == "kotlin"));
        assert!(!with_override.iter().any(|n| n == "java"));
        // Trust is not even evaluated when no gated server is present.
        let only_plain = vec![fixture("clangd", ".c")];
        let out = withhold_untrusted_builtins(only_plain, &HashMap::new(), || {
            panic!("trust must not be consulted")
        });
        assert_eq!(out.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_planted_binaries_are_not_resolved_for_the_new_builtins() {
        use crate::extras::executable_search::PlantedExecutables;
        let fixture = PlantedExecutables::new("lsp-builtins");
        for name in ["jdtls", "zls", "kotlin-language-server", "sourcekit-lsp"] {
            PlantedExecutables::plant(&fixture.workspace, name);
        }
        for search in fixture.hostile_search_paths() {
            let found = available_servers(&HashMap::new(), Some(search.as_os_str()));
            for gated in BUILD_EVALUATING_BUILTINS {
                assert!(
                    found.iter().all(|(n, _)| n != gated),
                    "{gated} resolved from a relative PATH entry"
                );
            }
        }
    }

    #[test]
    fn new_builtins_are_inactive_without_their_binary_and_can_be_disabled() {
        let empty = std::ffi::OsString::new();
        let none = available_servers(&HashMap::new(), Some(empty.as_os_str()));
        for name in ["java", "csharp", "ruby", "php", "kotlin", "swift", "zig"] {
            assert!(none.iter().all(|(n, _)| n != name), "{name} needs PATH");
        }
        let mut user = HashMap::new();
        user.insert(
            "zig".to_string(),
            LspServerConfig {
                disabled: true,
                ..Default::default()
            },
        );
        assert!(server_for_path(&resolve_servers(&user), std::path::Path::new("a.zig")).is_none());
    }
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
