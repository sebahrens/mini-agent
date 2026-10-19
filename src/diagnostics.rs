//! Bounded operator diagnostics over captured state, without provider calls.
use std::fmt::Write as _;
use std::path::Path;

use crate::auth::{AuthResolver, CredentialSource};
use crate::{cli::Cli, config::Config, paths::AppPaths, sandbox::Sandbox, session::Session};

const MAX_REPORT_BYTES: usize = 64 * 1024;
const MAX_LABEL_CHARS: usize = 192;
const MAX_DIRECTORY_ENTRIES: usize = 128;
#[cfg(feature = "lsp")]
const MAX_LSP_SERVERS: usize = 32;

/// Captured local source metadata only; no credential or registration escapes
/// this operator-only classification.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CredentialReport {
    Source(CredentialSource),
    ChatGpt(crate::auth::LoginRecordState),
    Copilot(crate::auth::LoginRecordState),
    Missing,
    InvalidRoute,
}
impl CredentialReport {
    pub(crate) fn setup_label(&self) -> &'static str {
        use crate::auth::LoginRecordState as S;
        match self {
            Self::Source(CredentialSource::NotRequired) => "key: not required",
            Self::Source(CredentialSource::AwsDefaultChain) => "auth: AWS SDK chain",
            Self::Source(CredentialSource::GoogleAdc) => "auth: Google ADC",
            Self::Source(_) => "key: set",
            Self::ChatGpt(S::Stored) => "auth: saved ChatGPT login",
            Self::ChatGpt(S::SignInRequired) => "auth: ChatGPT sign-in required",
            Self::ChatGpt(_) => "auth: ChatGPT record unavailable",
            Self::Copilot(S::Stored) => "auth: saved Copilot login",
            Self::Copilot(S::Absent | S::SignInRequired) => "auth: Copilot sign-in required",
            Self::Copilot(S::Unavailable) => "auth: Copilot record unavailable",
            Self::Missing => "key: not set",
            Self::InvalidRoute => "auth: invalid route",
        }
    }
    fn doctor_label(&self) -> String {
        use crate::auth::LoginRecordState as S;
        match self {
            Self::Source(CredentialSource::Cli) => "CLI --api-key (value omitted)".into(),
            Self::Source(CredentialSource::Environment(name)) => {
                format!("environment {name} (value omitted)")
            }
            Self::Source(CredentialSource::Config(name)) => {
                format!("config api_keys.{name} (value omitted)")
            }
            Self::Source(CredentialSource::NotRequired) => "not required".into(),
            Self::Source(CredentialSource::AwsDefaultChain) => {
                "AWS SDK credential chain (resolved on request; values omitted)".into()
            }
            Self::Source(CredentialSource::GoogleAdc) => {
                "Google Application Default Credentials (resolved on request; values omitted)"
                    .into()
            }
            Self::ChatGpt(S::Stored) => {
                "saved ChatGPT registration (resolved on request; values omitted)".into()
            }
            Self::ChatGpt(S::SignInRequired) => {
                "saved ChatGPT registration; sign-in required (values omitted)".into()
            }
            Self::ChatGpt(_) => {
                "ChatGPT login record unavailable (read-only inspection; values omitted)".into()
            }
            Self::Copilot(S::Stored) => {
                "saved Copilot login (resolved on request; values omitted)".into()
            }
            Self::Copilot(S::Absent | S::SignInRequired) => {
                "missing; run /login copilot or configure this route's credentials".into()
            }
            Self::Copilot(S::Unavailable) => {
                "Copilot login record unavailable (read-only inspection; values omitted)".into()
            }
            Self::Source(CredentialSource::CopilotLogin) => {
                unreachable!("unresolved login marker is inspected before reporting")
            }
            Self::Missing => "missing; configure this route's credentials".into(),
            Self::InvalidRoute => "provider configuration invalid (values omitted)".into(),
        }
    }
}

pub(crate) fn provider_credential_with_env<F>(
    cfg: &Config,
    name: &str,
    cli_key: Option<&str>,
    credentials_dir: &Path,
    get_env: F,
) -> CredentialReport
where
    F: Fn(&str) -> Result<String, std::env::VarError>,
{
    use crate::auth::LoginRecordState;
    let custom = cfg.custom_providers_map();
    let Ok(route) = crate::provider::resolve_provider_config(name, custom) else {
        return CredentialReport::InvalidRoute;
    };
    if crate::provider::chatgpt_login_route_with_env(name, cli_key, custom, &get_env)
        .unwrap_or(false)
    {
        let state = crate::auth::oauth::diagnostic_state_at(credentials_dir);
        if state != LoginRecordState::Absent {
            return CredentialReport::ChatGpt(state);
        }
    }
    let source = AuthResolver::new(route.kind)
        .with_cli_key(cli_key)
        .with_env_override(route.api_key_env.as_deref())
        .with_config_keys(cfg.api_keys.as_ref())
        .with_custom_provider_name(Some(name))
        .with_credential_isolation(route.credential_isolation)
        .credential_source_with_env(&get_env);
    match source {
        Ok(CredentialSource::CopilotLogin) => {
            CredentialReport::Copilot(crate::auth::copilot::diagnostic_state_at(
                credentials_dir,
                &get_env("COPILOT_GITHUB_HOST").unwrap_or_else(|_| "github.com".into()),
            ))
        }
        Ok(source) => CredentialReport::Source(source),
        Err(_) => CredentialReport::Missing,
    }
}

#[derive(Default)]
pub(crate) struct DoctorReport {
    text: String,
    truncated: bool,
}

fn label(value: &str) -> String {
    let mut chars = value.chars();
    let short: String = chars.by_ref().take(MAX_LABEL_CHARS).collect();
    let mut safe: String = crate::ui::events::sanitize_for_review(&short)
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        safe.push('…');
    }
    safe
}

impl DoctorReport {
    pub(crate) fn section(&mut self, name: &str) {
        if self.text.len() + name.len() + 4 <= MAX_REPORT_BYTES {
            let _ = writeln!(self.text, "\n{name}:");
        } else {
            self.truncated = true;
        }
    }

    pub(crate) fn row(&mut self, key: &str, value: impl AsRef<str>) {
        let value = label(value.as_ref());
        if self.text.len() + key.len() + value.len() + 6 <= MAX_REPORT_BYTES {
            let _ = writeln!(self.text, "  {key}: {value}");
        } else {
            self.truncated = true;
        }
    }

    pub(crate) fn render(mut self) -> String {
        if self.truncated {
            let mut end = self.text.len().min(MAX_REPORT_BYTES - 40);
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.truncate(end);
            self.text.push_str("\n[diagnostic report limit reached]\n");
        }
        self.text
    }

    pub(crate) fn capture(
        cli: &Cli,
        cfg: &Config,
        paths: &AppPaths,
        session: Option<&Session>,
        sandbox: Option<&Sandbox>,
        scan_storage: bool,
    ) -> Self {
        Self::capture_with_mcp_count(cli, cfg, paths, session, sandbox, scan_storage, None)
    }

    /// A live frontend can supply its retained effective MCP catalogue count.
    /// This changes only diagnostic metadata, not the captured config authority.
    pub(crate) fn capture_with_mcp_count(
        cli: &Cli,
        cfg: &Config,
        paths: &AppPaths,
        session: Option<&Session>,
        sandbox: Option<&Sandbox>,
        scan_storage: bool,
        mcp_configured_count: Option<usize>,
    ) -> Self {
        #[cfg(not(feature = "mcp"))]
        let _ = mcp_configured_count;
        let mut report = Self::default();
        report.text.push_str("mini-agent doctor\n");
        report.section("Build");
        report.row("version", env!("CARGO_PKG_VERSION"));
        report.row("fingerprint", env!("MINI_AGENT_BUILD_FINGERPRINT"));
        report.row("target", env!("MINI_AGENT_BUILD_TARGET"));
        report.section("Configuration");
        if let Some(path) = &cfg.provenance.global_file {
            report.row("global file", path.to_string_lossy());
        } else {
            report.row("global file", "load provenance unavailable");
        }
        report.row(
            "global status",
            cfg.provenance
                .global_status
                .unwrap_or("provenance unavailable"),
        );
        if let Some(path) = &cfg.provenance.project_file {
            report.row("project file", path.to_string_lossy());
            report.row(
                "project status",
                cfg.provenance.project_trust.unwrap_or("unknown"),
            );
        } else {
            report.row("project file", "no applied project layer");
        }
        report.row("load warnings", cfg.load_warnings.len().to_string());
        report.section("Config key sources (values omitted; merged tables can use both layers)");
        for key in cfg
            .provenance
            .global_keys
            .union(&cfg.provenance.project_keys)
        {
            let source = match (
                cfg.provenance.global_keys.contains(key),
                cfg.provenance.project_keys.contains(key),
            ) {
                (true, true) => "global + project",
                (true, false) => "global",
                _ => "project",
            };
            report.row(key, source);
        }
        let (provider, model) = session
            .map(|session| (session.provider.clone(), session.model.clone()))
            .unwrap_or_else(|| cli.resolve_provider_and_model(cfg));
        report.section("Provider and model");
        report.row("provider", &provider);
        report.row("model", &model);
        report.row(
            "selection",
            if session.is_some() {
                "current session"
            } else {
                "resolved diagnostic settings; config errors may prevent startup"
            },
        );
        let credential = provider_credential_with_env(
            cfg,
            &provider,
            cli.api_key.as_deref(),
            &paths.credentials_dir,
            |name| std::env::var(name),
        )
        .doctor_label();
        report.row("credential source", credential);
        let window = session
            .map(|session| session.context_window)
            .unwrap_or_else(|| {
                cfg.resolve_context_window(&provider, &model, crate::config::quick_models_map(cfg))
            });
        report.row("context window", window.to_string());
        let (prices, source) = session
            .map(|session| {
                (
                    session.known_input_output_prices(),
                    "captured session rates",
                )
            })
            .unwrap_or_else(|| {
                crate::startup::diagnostic_startup_pricing(cli, cfg, &provider, &model)
            });
        report.row("price source", source);
        report.row(
            "USD per million tokens",
            prices
                .map(|(input, output)| format!("input {input}; output {output}"))
                .unwrap_or_else(|| "unknown".into()),
        );
        report.row(
            "catalogue date",
            "not recorded in embedded snapshot; build fingerprint identifies it",
        );

        report.section("Sandbox");
        let static_sandbox;
        let sandbox = match sandbox {
            Some(sandbox) => sandbox,
            None => {
                static_sandbox = Sandbox::new(
                    cli.resolve_sandbox(cfg) && cli.general_sandbox_is_eligible(cfg),
                    &cli.resolve_sandbox_backend(cfg),
                );
                &static_sandbox
            }
        };
        match sandbox.cached_capability_matrix() {
            Some(matrix) => {
                report.row("backend", matrix.backend);
                report.row("cached status", matrix.status);
                report.row("filesystem reads", matrix.filesystem_reads);
                report.row("filesystem writes", matrix.filesystem_writes);
                report.row("network", matrix.network);
                if let Some(reason) = sandbox.cached_unavailable_diagnostic() {
                    report.row("reason", reason);
                }
            }
            None => {
                report.row("requested backend", cli.resolve_sandbox_backend(cfg));
                report.row("status", "not probed; this command does not run preflight");
            }
        }
        report.section("JavaScript worker (last process build report)");
        if let Some(status) = crate::provider::diagnostic_js_runtime_report() {
            for (name, state) in [
                ("JavaScript", status.javascript),
                ("learned skills", status.learned_skills),
            ] {
                use crate::provider::RuntimeAvailability;
                let value = match state {
                    RuntimeAvailability::Available => "recorded available".into(),
                    RuntimeAvailability::NotCompiled => "not compiled".into(),
                    RuntimeAvailability::Unavailable { reason } => {
                        format!("not live: {}", label(&reason))
                    }
                };
                report.row(name, value);
            }
        } else {
            report.row("status", "recorded metadata busy or unavailable");
        }
        report.section("Integrations");
        report.row("MCP compiled", cfg!(feature = "mcp").to_string());
        #[cfg(feature = "mcp")]
        report.row(
            "MCP configured servers",
            mcp_configured_count
                .unwrap_or_else(|| cfg.mcp_servers.as_ref().map_or(0, |servers| servers.len()))
                .to_string(),
        );
        report.row("LSP compiled", cfg!(feature = "lsp").to_string());
        #[cfg(feature = "lsp")]
        if let Some(lsp) = cfg.resolve_lsp() {
            let automatic = lsp.enabled == crate::config::LspActivation::Auto;
            report.row(
                "LSP",
                if automatic {
                    "auto; server processes are not started for diagnostics"
                } else {
                    "enabled; server processes are not started for diagnostics"
                },
            );
            let servers = crate::extras::lsp::registry::resolve_servers(&lsp.servers);
            if scan_storage {
                let search = std::env::var_os("PATH").filter(|path| path.len() <= 32 * 1024);
                let roots: Vec<_> = search
                    .as_deref()
                    .map(std::env::split_paths)
                    .into_iter()
                    .flatten()
                    .filter(|p| p.is_absolute())
                    .take(128)
                    .collect();
                let bounded_search = std::env::join_paths(roots).ok();
                let checked = bounded_lsp_checks(&servers, |server| {
                    server.command.len() <= 4096
                        && crate::extras::executable_search::resolve_service_executable(
                            server.command.as_str(),
                            bounded_search.as_deref(),
                            None,
                        )
                        .is_ok()
                });
                if automatic {
                    let available = checked.iter().filter(|(_, found)| *found).count();
                    report.row(
                        "LSP effective servers",
                        if checked.len() < servers.len() {
                            format!(
                                "{available}+ (partial; {} of {} candidates checked)",
                                checked.len(),
                                servers.len()
                            )
                        } else {
                            available.to_string()
                        },
                    );
                } else {
                    report.row("LSP effective servers", servers.len().to_string());
                }
                for (name, found) in checked {
                    report.row(
                        "LSP executable",
                        format!(
                            "{}: {}",
                            label(&name),
                            if found {
                                "found; metadata only, launch/handshake not verified"
                            } else {
                                "not found in bounded absolute PATH lookup"
                            }
                        ),
                    );
                }
                report.row("LSP lookup bounds", "at most 32 servers and 128 PATH roots; no argument or environment values shown");
            } else {
                report.row(
                    if automatic {
                        "LSP candidates (availability not rechecked)"
                    } else {
                        "LSP effective servers"
                    },
                    servers.len().to_string(),
                );
                report.row(
                    "LSP executable lookup",
                    "not performed in TUI; --doctor checks bounded PATH metadata",
                );
            }
        } else {
            report.row("LSP", "disabled");
        }
        report.row("hooks compiled", cfg!(feature = "hooks").to_string());
        #[cfg(feature = "hooks")]
        match crate::extras::hooks::diagnostic_hook_metadata() {
            Ok(Some((events, handlers, once))) => {
                report.row("hook dispatcher", format!("loaded: {events} events, {handlers} handlers; executable pins not rechecked"));
                report.row(
                    "once hooks completed",
                    once.map(|n| n.to_string())
                        .unwrap_or_else(|| "metadata busy".into()),
                );
            }
            Ok(None) => report.row("hook dispatcher", "not initialized"),
            Err(reason) => report.row("hook dispatcher", reason),
        }
        report.section("Directories");
        for (name, path) in [
            ("config", &paths.config_dir),
            ("data", &paths.data_dir),
            ("state", &paths.state_dir),
            ("cache", &paths.cache_dir),
        ] {
            report.row(name, path.to_string_lossy());
            if scan_storage {
                report.row("immediate file bytes", directory_bytes(path));
            }
        }
        let sessions = paths.sessions_dir();
        report.row("sessions", sessions.to_string_lossy());
        if scan_storage {
            report.row("immediate session file bytes", directory_bytes(&sessions));
        } else {
            report.row(
                "disk size",
                "not scanned in TUI; --doctor inspects bounded immediate files",
            );
        }
        report
    }
}

#[cfg(feature = "lsp")]
fn bounded_lsp_checks(
    servers: &[(String, crate::config::types::LspServerConfig)],
    mut lookup: impl FnMut(&crate::config::types::LspServerConfig) -> bool,
) -> Vec<(String, bool)> {
    // Bound BEFORE the first filesystem lookup, not just when rendering.
    servers
        .iter()
        .take(MAX_LSP_SERVERS)
        .map(|(name, server)| (name.clone(), lookup(server)))
        .collect()
}

/// Metadata only, bounded to direct regular entries. Never follow links or
/// read content; an authority change discards the aggregate before display.
fn directory_bytes(path: &Path) -> String {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return "missing or unreadable".into();
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return "not a regular directory".into();
    }
    let Ok(binding) = crate::paths::WorkspaceBinding::capture(path) else {
        return "directory authority unavailable".into();
    };
    let Ok(entries) = std::fs::read_dir(binding.root()) else {
        return "directory unreadable".into();
    };
    let mut bytes = 0u64;
    let mut partial = false;
    for (index, entry) in entries.enumerate() {
        if index == MAX_DIRECTORY_ENTRIES {
            partial = true;
            break;
        }
        let Ok(entry) = entry else {
            partial = true;
            continue;
        };
        let Ok(file) = binding.open_regular_relative(Path::new(&entry.file_name())) else {
            partial = true;
            continue;
        };
        match file.metadata() {
            Ok(meta) => bytes = bytes.saturating_add(meta.len()),
            Err(_) => partial = true,
        }
    }
    if binding.validate().is_err() {
        return "directory changed during inspection".into();
    }
    format!(
        "{bytes}{} (direct files only; at most {MAX_DIRECTORY_ENTRIES} entries)",
        if partial { "+, partial" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_labels_and_report_are_utf8_bounded_and_terminal_safe() {
        let mut report = DoctorReport::default();
        for _ in 0..1000 {
            report.row("label", format!("\u{1b}[31m\n{}", "界".repeat(400)));
        }
        let rendered = report.render();
        assert!(rendered.len() <= MAX_REPORT_BYTES);
        assert!(rendered.contains("diagnostic report limit reached"));
        assert!(rendered.contains('…'));
        assert!(!rendered.contains('\u{1b}') && !rendered.contains("\n界"));
    }

    #[test]
    fn doctor_disabled_and_unsupported_sandbox_metadata_never_require_a_probe() {
        let disabled = Sandbox::new(false, "none");
        assert_eq!(
            disabled.cached_capability_matrix().unwrap().status,
            "disabled"
        );
        let unsupported = Sandbox::new(true, "unrecognized-doctor-backend");
        assert_eq!(
            unsupported.cached_capability_matrix().unwrap().status,
            "requested-but-unavailable; subprocess launch denied"
        );
    }

    #[test]
    fn doctor_captured_session_metadata_omits_credentials_history_and_config_values() {
        let root = crate::tests::OwnedTestDirectory::new("doctor-report");
        let paths = AppPaths {
            config_dir: root.path().join("config"),
            data_dir: root.path().join("data"),
            local_data_dir: root.path().join("local"),
            state_dir: root.path().join("state"),
            cache_dir: root.path().join("cache"),
            credentials_dir: root.path().join("credentials"),
            project_dir: Some(root.path().to_owned()),
        };
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "api_keys":{"openai":"PRIVATE_CONFIG_KEY"},
            "system_prompt":"PRIVATE_PROMPT_BODY"
        }))
        .unwrap();
        let cli = Cli {
            api_key: Some("PRIVATE_CLI_KEY".into()),
            no_sandbox: true,
            ..Default::default()
        };
        let mut session = Session::new(
            "openai",
            "doctor-model",
            12345,
            root.path().to_str().unwrap(),
        );
        session.add_message(crate::session::MessageRole::User, "PRIVATE_HISTORY_BODY");
        let before = serde_json::to_vec(&session).unwrap();
        let report =
            DoctorReport::capture(&cli, &cfg, &paths, Some(&session), None, false).render();
        for section in [
            "Build:",
            "Configuration:",
            "Provider and model:",
            "Sandbox:",
            "JavaScript worker",
            "Integrations:",
            "Directories:",
        ] {
            assert!(report.contains(section), "{section}: {report}");
        }
        assert!(report.contains("CLI --api-key (value omitted)"));
        assert!(report.contains("captured session rates") && report.contains("12345"));
        assert!(!report.contains("PRIVATE_"));
        assert_eq!(serde_json::to_vec(&session).unwrap(), before);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(feature = "lsp")]
    #[test]
    fn lsp44_doctor_bounds_lookup_before_probe_and_checks_each_candidate_once() {
        let servers: Vec<_> = (0..MAX_LSP_SERVERS + 5)
            .map(|i| {
                (
                    format!("server-{i}"),
                    crate::config::types::LspServerConfig::default(),
                )
            })
            .collect();
        let mut probes = 0;
        let checked = bounded_lsp_checks(&servers, |_| {
            probes += 1;
            true
        });
        assert_eq!(probes, MAX_LSP_SERVERS);
        assert_eq!(checked.len(), MAX_LSP_SERVERS);
        assert_eq!(
            checked.last().unwrap().0,
            format!("server-{}", MAX_LSP_SERVERS - 1)
        );
        assert!(checked.iter().all(|(_, found)| *found));
    }

    #[cfg(all(unix, feature = "lsp"))]
    #[test]
    fn lsp44_doctor_auto_reports_effective_servers_without_executing_and_busy_view_does_not_probe()
    {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::tests::OwnedTestDirectory::new("lsp44-doctor");
        let binary = root.path().join("PRIVATE_COMMAND_NAME");
        let marker = root.path().join("executed");
        std::fs::write(&binary, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = AppPaths {
            config_dir: root.path().join("config"),
            data_dir: root.path().join("data"),
            local_data_dir: root.path().join("local"),
            state_dir: root.path().join("state"),
            cache_dir: root.path().join("cache"),
            credentials_dir: root.path().join("credentials"),
            project_dir: Some(root.path().to_owned()),
        };
        let mut lsp = crate::config::types::LspConfig::default();
        for (name, _) in crate::extras::lsp::registry::builtin_servers() {
            lsp.servers.insert(
                name.into(),
                crate::config::types::LspServerConfig {
                    disabled: true,
                    ..Default::default()
                },
            );
        }
        lsp.servers.insert(
            "fixture".into(),
            crate::config::types::LspServerConfig {
                command: binary.to_str().unwrap().into(),
                ..Default::default()
            },
        );
        let mut cfg = Config {
            lsp: Some(lsp),
            ..Default::default()
        };
        let cli = Cli {
            no_sandbox: true,
            ..Default::default()
        };
        let capture = |cfg: &Config, scan| {
            DoctorReport::capture(&cli, cfg, &paths, None, None, scan).render()
        };
        let report = capture(&cfg, true);
        assert!(report.contains("LSP effective servers: 1"), "{report}");
        assert!(!report.contains("PRIVATE_COMMAND_NAME"));
        assert!(!marker.exists());
        let mut large = cfg.clone();
        for index in 0..33 {
            large.lsp.as_mut().unwrap().servers.insert(
                format!("extra-{index:02}"),
                crate::config::types::LspServerConfig {
                    command: binary.to_str().unwrap().into(),
                    ..Default::default()
                },
            );
        }
        let limited = capture(&large, true);
        assert!(
            limited.contains("LSP effective servers: 32+ (partial; 32 of 34 candidates checked)"),
            "{limited}"
        );
        assert_eq!(limited.matches("LSP executable:").count(), MAX_LSP_SERVERS);
        assert!(!marker.exists());
        std::fs::remove_file(&binary).unwrap();
        let busy = capture(&cfg, false);
        assert!(
            busy.contains("LSP candidates (availability not rechecked): 1"),
            "{busy}"
        );
        let missing = capture(&cfg, true);
        assert!(missing.contains("LSP effective servers: 0"), "{missing}");
        cfg.lsp.as_mut().unwrap().enabled = false.into();
        assert!(capture(&cfg, true).contains("LSP: disabled"));
        assert!(!marker.exists());
    }

    #[test]
    fn doctor_directory_scan_is_shallow_bounded_and_does_not_disclose_file_names_or_contents() {
        let root = crate::tests::OwnedTestDirectory::new("doctor-directory");
        std::fs::write(
            root.path().join("PRIVATE_FILE_NAME"),
            b"PRIVATE_FILE_CONTENTS",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/ignored"), vec![0; 4096]).unwrap();
        let report = directory_bytes(root.path());
        assert!(report.starts_with("21+, partial"), "{report}");
        assert!(!report.contains("PRIVATE"));
        for index in 0..MAX_DIRECTORY_ENTRIES + 1 {
            std::fs::write(root.path().join(index.to_string()), []).unwrap();
        }
        assert!(directory_bytes(root.path()).contains("partial"));
        assert_eq!(
            std::fs::read(root.path().join("PRIVATE_FILE_NAME")).unwrap(),
            b"PRIVATE_FILE_CONTENTS"
        );
    }

    #[cfg(unix)]
    #[test]
    fn doctor_directory_scan_rejects_links_and_nonregular_entries_without_opening_contents() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("doctor-directory-special");
        let directory = root.path().join("scanned");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(root.path().join("outside"), [0; 4096]).unwrap();
        symlink(root.path().join("outside"), directory.join("linked")).unwrap();
        let _gate = crate::tests::process_gate::ProcessGate::new(&directory.join("fifo")).unwrap();
        assert!(directory_bytes(&directory).starts_with("0+, partial"));
        symlink(&directory, root.path().join("directory-link")).unwrap();
        assert_eq!(
            directory_bytes(&root.path().join("directory-link")),
            "not a regular directory"
        );
    }
}
