use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use std::io::{self, Read};

use crate::config::{
    Config, EditSystem, QuickModelConfig, StatusLineConfig, StatusLineLine, StatusLineSegment,
};
#[cfg(feature = "mcp")]
use crate::extras::mcp::config::{McpServerConfig, TrustedMcpServer};
use crate::paths::AppPaths;

const PROJECT_CONFIG_TRUST_SCHEMA: u32 = 1;

/// Project-local keys that cannot launch a process, select an executable
/// integration/provider, or weaken an authorization boundary. Every other
/// top-level key, including unknown future keys, requires explicit trust.
const BENIGN_PROJECT_CONFIG_KEYS: &[&str] = &[
    "always_show_welcome",
    "chain",
    "chat_left_margin",
    "colors",
    "compact_enabled",
    "context_window",
    "default_prompt",
    "edit_system",
    "keep_recent_tokens",
    "max_agent_turns",
    "max_bash_output_lines",
    "max_find_results",
    "max_grep_results",
    "max_list_dir_entries",
    "max_read_lines",
    "max_text_file_size",
    "max_tokens",
    "mid_turn_compact_threshold",
    "model",
    "reserve_tokens",
    "retry",
    "show_cost_always",
    "show_reasoning",
    "show_tool_details",
    "statusline",
    "subagent_max_find_results",
    "subagent_max_grep_results",
    "subagent_max_list_dir_entries",
    "subagent_max_read_lines",
    "temperature",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectConfigTrustStore {
    schema: u32,
    #[serde(default)]
    bindings: Vec<ProjectConfigTrustBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectConfigTrustBinding {
    canonical_project: String,
    canonical_config: String,
    config_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectConfigTrustOutcome {
    NoSensitiveKeys,
    AlreadyTrusted,
    Approved,
    SkippedHeadless,
    Declined,
    TrustUnavailable,
}

/// Write `content` to `path` atomically via temp-file + rename.
pub(crate) fn atomic_config_write(path: &Path, content: &str) -> io::Result<()> {
    crate::fs::private_atomic_write_sync(path, content.as_bytes()).map_err(|error| {
        if cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)) {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "configuration file {} is temporarily locked",
                    path.display()
                ),
            )
        } else {
            error
        }
    })
}

#[cfg(all(test, unix))]
pub(crate) fn atomic_config_write_with_failure(
    path: &Path,
    content: &str,
    fail_rename: bool,
) -> io::Result<()> {
    crate::fs::private_atomic_write_with_failure_sync(path, content.as_bytes(), fail_rename)
}

pub(crate) fn read_config_content(path: &Path) -> io::Result<String> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "config file must have a parent directory",
        )
    })?;
    crate::fs::ensure_private_directory(parent)?;
    let mut file = crate::fs::open_private_file(path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

pub(crate) fn parse_config_content(path: &Path, content: &str) -> io::Result<Config> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("toml") => toml::from_str(content)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "config is not valid TOML")),
        _ => serde_yaml_ng::from_str(content).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config is not valid YAML or JSON",
            )
        }),
    }
}

pub(crate) fn serialize_config_content(path: &Path, cfg: &Config) -> io::Result<String> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("toml") => toml::to_string(cfg).map_err(io::Error::other),
        _ => serde_yaml_ng::to_string(cfg).map_err(io::Error::other),
    }
}

fn verify_config_preservation_at(path: &Path) -> io::Result<()> {
    const FIXTURE: &str = r#"
model = "before-check"
temperature = 0.7
future_scalar = "keep-me"
future_array = [1, "two", true]

[future_table]
nested = { flag = true, values = [3, 4] }

[acp_servers.worker]
type = "stdio"
"#;
    const PRESERVED_KEYS: [&str; 4] = [
        "future_scalar",
        "future_array",
        "future_table",
        "acp_servers",
    ];

    atomic_config_write(path, FIXTURE)?;
    let before_content = read_config_content(path)?;
    let before: toml::Value = toml::from_str(&before_content).map_err(io::Error::other)?;
    let mut cfg = parse_config_content(path, &before_content)?;
    cfg.model = Some(CompactString::new("after-check"));
    cfg.temperature = None;

    atomic_config_write(path, &serialize_config_content(path, &cfg)?)?;
    let after_content = read_config_content(path)?;
    let after: toml::Value = toml::from_str(&after_content).map_err(io::Error::other)?;

    let owned_fields_match = after.get("model").and_then(toml::Value::as_str)
        == Some("after-check")
        && after.get("temperature").is_none();
    let preserved_fields_match = PRESERVED_KEYS
        .iter()
        .all(|key| after.get(*key) == before.get(*key));
    let reloaded = parse_config_content(path, &after_content)?;
    let reload_matches =
        reloaded.model.as_deref() == Some("after-check") && reloaded.temperature.is_none();

    if owned_fields_match && preserved_fields_match && reload_matches {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "config preservation check failed",
        ))
    }
}

pub(crate) fn verify_config_preservation(paths: &AppPaths) -> io::Result<()> {
    let path = paths.cache_dir.join(format!(
        ".config-preservation-check-{}-{}.toml",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = verify_config_preservation_at(&path);
    match std::fs::remove_file(&path) {
        Ok(()) => result,
        Err(error) if error.kind() == io::ErrorKind::NotFound => result,
        Err(cleanup_error) => result.and(Err(cleanup_error)),
    }
}

fn path_entry_exists(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Candidate config filenames, in priority order within each search dir.
///
/// * `config.toml` — preferred format, especially for permission rules.
/// * `config.yaml` / `config.yml` — the documented non-TOML format.
/// * `config.json` — legacy fallback. YAML is a strict superset of JSON, so
///   existing JSON configs parse transparently through the YAML reader. This
///   entry exists purely so upgrades do not silently drop a user's config.
const CONFIG_CANDIDATES: [&str; 4] = ["config.toml", "config.yaml", "config.yml", "config.json"];

/// Pick the first existing candidate in `dir`, falling back to the preferred
/// `config.toml` path when none exist (so a fresh install seeds a TOML file).
pub(crate) fn pick_existing(dir: &Path) -> PathBuf {
    for name in CONFIG_CANDIDATES {
        let p = dir.join(name);
        if p.exists() {
            return p;
        }
    }
    dir.join(CONFIG_CANDIDATES[0])
}

fn resolve_config_path() -> PathBuf {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    pick_existing(&paths.config_dir)
}

pub fn config_file_path() -> PathBuf {
    resolve_config_path()
}

fn default_quick_models() -> HashMap<String, QuickModelConfig> {
    let mut map = HashMap::new();
    map.insert(
        "deepseek-v4-flash".to_string(),
        QuickModelConfig {
            provider: CompactString::new("openrouter"),
            model: CompactString::new("deepseek/deepseek-v4-flash"),
            input_token_cost: 0.0983,
            output_token_cost: 0.1966,
            reserve_tokens: None,
            temperature: None,
            extra_body: None,
            context_window: None,
        },
    );
    map.insert(
        "deepseek-v4-pro".to_string(),
        QuickModelConfig {
            provider: CompactString::new("openrouter"),
            model: CompactString::new("deepseek/deepseek-v4-pro"),
            input_token_cost: 0.435,
            output_token_cost: 0.87,
            reserve_tokens: None,
            temperature: None,
            extra_body: None,
            context_window: None,
        },
    );
    map
}

pub fn quick_models_map(cfg: &Config) -> HashMap<String, QuickModelConfig> {
    cfg.quick_models.clone().unwrap_or_default()
}

pub fn save_quick_model(
    name: &str,
    provider: &str,
    model: &str,
    input_token_cost: f64,
    output_token_cost: f64,
) -> std::io::Result<()> {
    let path = resolve_config_path();
    let mut cfg: Config = if path_entry_exists(&path)? {
        parse_config_content(&path, &read_config_content(&path)?)?
    } else {
        Config::default()
    };

    let quick_models = cfg.quick_models.get_or_insert_with(HashMap::new);
    quick_models.insert(
        name.to_string(),
        QuickModelConfig {
            provider: CompactString::new(provider),
            model: CompactString::new(model),
            input_token_cost,
            output_token_cost,
            reserve_tokens: None,
            temperature: None,
            extra_body: None,
            context_window: None,
        },
    );

    atomic_config_write(&path, &serialize_config_content(&path, &cfg)?)?;
    Ok(())
}

fn rich_default_config() -> Config {
    Config {
        quick_models: Some(default_quick_models()),
        provider: Some(CompactString::new("openrouter")),
        model: Some(CompactString::new("deepseek-v4-pro")),
        max_tokens: Some(16384),
        compact_enabled: Some(false),
        max_text_file_size: Some(1_048_576),
        edit_system: Some(EditSystem::Similarity),
        default_permission_mode: Some("standard".to_string()),
        default_prompt: Some(CompactString::new("code")),
        show_tool_details: None,
        chain: Some(crate::config::types::ChainConfig::default()),
        #[cfg(feature = "subagents")]
        subagent_max_read_lines: Some(2000),
        #[cfg(feature = "subagents")]
        subagent_max_grep_results: Some(200),
        #[cfg(feature = "subagents")]
        subagent_max_find_results: Some(200),
        #[cfg(feature = "subagents")]
        task_max_prompts: Some(8),
        #[cfg(feature = "subagents")]
        task_max_concurrency: Some(4),
        #[cfg(feature = "subagents")]
        task_max_output_bytes: Some(256 * 1024),
        #[cfg(feature = "subagents")]
        task_max_cost_units: Some(500_000),
        #[cfg(feature = "subagents")]
        task_timeout_secs: Some(300),
        #[cfg(feature = "advisor")]
        advisor: Some(crate::config::types::AdvisorConfig::default()),
        statusline: Some(StatusLineConfig {
            lines: vec![StatusLineLine {
                segments: vec![
                    StatusLineSegment {
                        item: "cwd".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some("  ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "git_branch".into(),
                        color: Some("grey".into()),
                        left: Some("(".into()),
                        right: Some(")".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" | ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "model".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some("  |  ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "context_used".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some("/".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "context_max".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "context_percentage".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some("  \u{21d1}".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "tokens_input".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" \u{21d3}".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "tokens_output".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "flex_separator".into(),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "loop".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "mode".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "cost".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "btw".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "separator".into(),
                        text: Some(" ".into()),
                        ..Default::default()
                    },
                    StatusLineSegment {
                        item: "prompt".into(),
                        color: Some("grey".into()),
                        ..Default::default()
                    },
                ],
            }],
        }),
        ..Default::default()
    }
}

pub fn load_with_paths(paths: &AppPaths, interactive: bool) -> (Config, bool) {
    let local_config_path = paths.project_config_file();
    load_from_path(
        pick_existing(&paths.config_dir),
        local_config_path.as_deref(),
        &paths.project_config_trust_file(),
        interactive,
    )
}

fn fatal_config_load(message: String) -> ! {
    #[cfg(test)]
    panic!("{message}");

    #[cfg(not(test))]
    {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn load_from_path(
    path: PathBuf,
    local_config_path: Option<&Path>,
    project_trust_path: &Path,
    interactive: bool,
) -> (Config, bool) {
    let is_first_startup = !path_entry_exists(&path).unwrap_or_else(|error| {
        fatal_config_load(format!(
            "error: failed to inspect config path ({}): {}\n\
             Fix the path or remove it to use defaults.",
            path.display(),
            error,
        ))
    });
    #[allow(unused_mut)]
    let mut cfg: Config = if is_first_startup {
        tracing::info!(
            "first startup, writing default config to {}",
            path.display()
        );
        let default = rich_default_config();
        if path.extension().and_then(|e| e.to_str()) == Some("toml")
            && let Ok(content) = toml::to_string(&default)
        {
            atomic_config_write(&path, &content).unwrap_or_else(|error| {
                fatal_config_load(format!(
                    "error: failed to create private config file ({}): {}\n\
                     Fix the path or its permissions, then restart.",
                    path.display(),
                    error,
                ))
            });
        }
        default
    } else {
        let content = read_config_content(&path).unwrap_or_else(|error| {
            fatal_config_load(format!(
                "error: failed to read config file ({}): {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                error,
            ))
        });
        parse_config_content(&path, &content).unwrap_or_else(|error| {
            fatal_config_load(format!(
                "error: {} is not a valid config: {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                error,
            ))
        })
    };

    tracing::debug!(
        "config loaded from {}: {} quick_models, {} custom_providers",
        path.display(),
        cfg.quick_models.as_ref().map(|m| m.len()).unwrap_or(0),
        cfg.custom_providers.as_ref().map(|m| m.len()).unwrap_or(0),
    );

    if let Some(local_config_path) = local_config_path {
        apply_local_override(
            &mut cfg,
            local_config_path,
            project_trust_path,
            interactive,
            &confirm_project_config_trust,
        );
    }

    #[cfg(feature = "mcp")]
    inject_mcp_defaults(&mut cfg);

    (cfg, is_first_startup)
}

fn confirm_project_config_trust(description: &str) -> bool {
    let mut input = String::new();
    eprint!("{description}\nTrust these settings for this exact project config? [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn project_config_binding(path: &Path, content: &str) -> Result<ProjectConfigTrustBinding, String> {
    let project_root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "project config must be inside a project application directory".to_string())?
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize project root: {error}"))?;
    let canonical_config = path
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize project config: {error}"))?;
    let canonical_project = project_root
        .to_str()
        .ok_or_else(|| "canonical project path is not valid UTF-8".to_string())?
        .to_string();
    let canonical_config = canonical_config
        .to_str()
        .ok_or_else(|| "canonical project config path is not valid UTF-8".to_string())?
        .to_string();
    let digest = Sha256::digest(content.as_bytes());
    let config_sha256 = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(ProjectConfigTrustBinding {
        canonical_project,
        canonical_config,
        config_sha256,
    })
}

fn load_project_config_trust(path: &Path) -> std::io::Result<ProjectConfigTrustStore> {
    let mut file = match crate::fs::open_private_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectConfigTrustStore {
                schema: PROJECT_CONFIG_TRUST_SCHEMA,
                bindings: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let store: ProjectConfigTrustStore = serde_json::from_str(&content)
        .map_err(|_| std::io::Error::other("project config trust store is invalid"))?;
    if store.schema != PROJECT_CONFIG_TRUST_SCHEMA {
        return Ok(ProjectConfigTrustStore {
            schema: PROJECT_CONFIG_TRUST_SCHEMA,
            bindings: Vec::new(),
        });
    }
    Ok(store)
}

fn save_project_config_trust(path: &Path, store: &ProjectConfigTrustStore) -> std::io::Result<()> {
    let content = serde_json::to_vec_pretty(store).map_err(std::io::Error::other)?;
    crate::fs::private_atomic_write_sync(path, &content)
}

fn split_project_override(
    local_toml: &str,
) -> Result<(toml::Value, toml::Value, BTreeSet<String>), String> {
    let local: toml::Value =
        toml::from_str(local_toml).map_err(|_| "project config is not valid TOML".to_string())?;
    let table = local
        .as_table()
        .ok_or_else(|| "project config must contain a TOML table".to_string())?;
    let mut benign = toml::map::Map::new();
    let mut sensitive = toml::map::Map::new();
    let mut sensitive_keys = BTreeSet::new();
    for (key, value) in table {
        if BENIGN_PROJECT_CONFIG_KEYS.contains(&key.as_str()) {
            benign.insert(key.clone(), value.clone());
        } else {
            sensitive_keys.insert(key.clone());
            sensitive.insert(key.clone(), value.clone());
        }
    }
    Ok((
        toml::Value::Table(benign),
        toml::Value::Table(sensitive),
        sensitive_keys,
    ))
}

fn merge_config_value(base: &Config, local: toml::Value) -> Result<Config, String> {
    let mut base_toml = toml::Value::try_from(base)
        .map_err(|_| "base config could not be normalized".to_string())?;
    deep_merge_toml(&mut base_toml, local);
    base_toml
        .try_into()
        .map_err(|_| "merged project config has an invalid value".to_string())
}

fn redact_sensitive_value(value: &toml::Value, redact_scalar: bool) -> toml::Value {
    match value {
        toml::Value::Table(table) => {
            let mut redacted = toml::map::Map::new();
            for (key, value) in table {
                let normalized = key.to_ascii_lowercase();
                let redact_child = redact_scalar
                    || matches!(
                        normalized.as_str(),
                        "api_keys" | "env" | "headers" | "password" | "secret" | "token"
                    )
                    || normalized.ends_with("_key")
                    || normalized.ends_with("-key")
                    || normalized.ends_with("_token")
                    || normalized.ends_with("-token");
                redacted.insert(key.clone(), redact_sensitive_value(value, redact_child));
            }
            toml::Value::Table(redacted)
        }
        toml::Value::Array(values) => toml::Value::Array(
            values
                .iter()
                .map(|value| redact_sensitive_value(value, redact_scalar))
                .collect(),
        ),
        _ if redact_scalar => toml::Value::String("<redacted>".to_string()),
        _ => value.clone(),
    }
}

fn sensitive_settings_summary(value: &toml::Value) -> String {
    let redacted = redact_sensitive_value(value, false);
    toml::to_string_pretty(&redacted)
        .unwrap_or_else(|_| "<settings could not be rendered>".to_string())
}

fn apply_local_override_with_confirmation(
    cfg: &mut Config,
    path: &Path,
    trust_store_path: &Path,
    interactive: bool,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<ProjectConfigTrustOutcome, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read project config: {error}"))?;
    let (benign, sensitive, sensitive_keys) = split_project_override(&content)?;
    let benign_cfg = merge_config_value(cfg, benign)?;
    if sensitive_keys.is_empty() {
        *cfg = benign_cfg;
        return Ok(ProjectConfigTrustOutcome::NoSensitiveKeys);
    }

    let binding = match project_config_binding(path, &content) {
        Ok(binding) => binding,
        Err(error) => {
            *cfg = benign_cfg;
            tracing::warn!(
                "project config: sensitive settings are inactive because trust identity is unavailable: {error}"
            );
            return Ok(ProjectConfigTrustOutcome::TrustUnavailable);
        }
    };
    let mut store = match load_project_config_trust(trust_store_path) {
        Ok(store) => store,
        Err(error) => {
            *cfg = benign_cfg;
            tracing::warn!(
                "project config: sensitive settings are inactive because the private trust store is unavailable: {error}"
            );
            return Ok(ProjectConfigTrustOutcome::TrustUnavailable);
        }
    };

    if store.bindings.contains(&binding) {
        *cfg = merge_config_value(&benign_cfg, sensitive)?;
        return Ok(ProjectConfigTrustOutcome::AlreadyTrusted);
    }

    if !interactive {
        *cfg = benign_cfg;
        tracing::warn!(
            project = %binding.canonical_project,
            config = %binding.canonical_config,
            sha256 = %binding.config_sha256,
            keys = ?sensitive_keys,
            "project config: ignored untrusted sensitive settings in headless mode"
        );
        return Ok(ProjectConfigTrustOutcome::SkippedHeadless);
    }

    let keys = sensitive_keys.into_iter().collect::<Vec<_>>().join(", ");
    let settings = sensitive_settings_summary(&sensitive);
    let description = format!(
        "Project-local config requests executable or security-sensitive settings.\n\
         Project: {}\n\
         Config: {}\n\
         SHA-256: {}\n\
         Sensitive keys: {}\n\
         Sensitive settings (secret values redacted):\n{}",
        binding.canonical_project, binding.canonical_config, binding.config_sha256, keys, settings
    );
    if !confirm(&description) {
        *cfg = benign_cfg;
        tracing::warn!("project config: user declined sensitive project-local settings");
        return Ok(ProjectConfigTrustOutcome::Declined);
    }

    let trusted_cfg = merge_config_value(&benign_cfg, sensitive)?;
    store.bindings.retain(|existing| {
        existing.canonical_project != binding.canonical_project
            || existing.canonical_config != binding.canonical_config
    });
    store.bindings.push(binding);
    if let Err(error) = save_project_config_trust(trust_store_path, &store) {
        *cfg = benign_cfg;
        tracing::warn!(
            "project config: sensitive settings are inactive because trust could not be persisted privately: {error}"
        );
        return Ok(ProjectConfigTrustOutcome::TrustUnavailable);
    }
    *cfg = trusted_cfg;
    Ok(ProjectConfigTrustOutcome::Approved)
}

/// Whether `project_config` is bound in the private trust store with its
/// exact current content. Consumers other than config loading (for example
/// project prompt `%%mode=` directives) use this to decide whether repository
/// content may influence the security posture.
pub(crate) fn project_config_is_trusted(project_config: Option<&Path>, trust_store: &Path) -> bool {
    let Some(path) = project_config else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(binding) = project_config_binding(path, &content) else {
        return false;
    };
    let Ok(store) = load_project_config_trust(trust_store) else {
        return false;
    };
    store.bindings.contains(&binding)
}

/// Record trust for `project_config` exactly as an interactive approval would.
#[cfg(test)]
pub(crate) fn trust_project_config(
    project_config: &Path,
    trust_store: &Path,
) -> Result<(), String> {
    let content = std::fs::read_to_string(project_config).map_err(|error| error.to_string())?;
    let binding = project_config_binding(project_config, &content)?;
    let mut store = load_project_config_trust(trust_store).map_err(|error| error.to_string())?;
    store.bindings.push(binding);
    if let Some(parent) = trust_store.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    save_project_config_trust(trust_store, &store).map_err(|error| error.to_string())
}

/// Merge benign `.zerostack/config.toml` keys immediately and keep all other
/// keys inert until trust is bound to the canonical project/config paths and
/// exact config bytes.
fn apply_local_override(
    cfg: &mut Config,
    path: &Path,
    trust_store_path: &Path,
    interactive: bool,
    confirm: &dyn Fn(&str) -> bool,
) {
    if !path.exists() {
        return;
    }
    match apply_local_override_with_confirmation(cfg, path, trust_store_path, interactive, confirm)
    {
        Ok(outcome) => {
            tracing::info!(?outcome, path = %path.display(), "processed project-local config");
            if matches!(
                outcome,
                ProjectConfigTrustOutcome::SkippedHeadless
                    | ProjectConfigTrustOutcome::Declined
                    | ProjectConfigTrustOutcome::TrustUnavailable
            ) {
                eprintln!(
                    "note: project-local executable/security settings are inactive; benign settings were applied from {}",
                    path.display()
                );
            }
        }
        Err(error) => {
            eprintln!(
                "error: {} is not a valid config: {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                error,
            );
            std::process::exit(1);
        }
    }
}

/// Merge a project-local TOML config fragment over `base`: keys present in
/// `local_toml` win, tables (`quick_models`, `mcp_servers`, ...) merge per
/// key, scalars and arrays replace, and absent keys keep the base value.
#[cfg(test)]
pub fn merge_config_override(base: &Config, local_toml: &str) -> Result<Config, String> {
    let local: toml::Value =
        toml::from_str(local_toml).map_err(|_| "project config is not valid TOML".to_string())?;
    merge_config_value(base, local)
}

/// Deep-merge `over` into `base`: objects merge recursively per key, any
/// other value replaces.
fn deep_merge_toml(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge_toml(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

#[cfg(feature = "mcp")]
pub fn inject_mcp_defaults(cfg: &mut Config) {
    let mut servers = cfg.mcp_servers.take().unwrap_or_default();

    if cfg.resolve_enable_exa_mcp() {
        let mut headers = HashMap::new();
        if let Ok(key) = std::env::var("EXA_API_KEY") {
            headers.insert("x-api-key".to_string(), key);
        }
        servers
            .entry("Exa Web Search".to_string())
            .or_insert_with(|| McpServerConfig::built_in(TrustedMcpServer::EXA, headers));
    } else {
        servers.remove("Exa Web Search");
    }

    if cfg.resolve_enable_context7_mcp() {
        let mut headers = HashMap::new();
        if let Ok(key) = std::env::var("CONTEXT7_API_KEY") {
            headers.insert("authorization".to_string(), format!("Bearer {key}"));
        }
        servers
            .entry("Context7".to_string())
            .or_insert_with(|| McpServerConfig::built_in(TrustedMcpServer::CONTEXT7, headers));
    } else {
        servers.remove("Context7");
    }

    if cfg.resolve_enable_grepapp_mcp() {
        let mut headers = HashMap::new();
        if let Ok(key) = std::env::var("GREP_APP_API_KEY") {
            headers.insert("authorization".to_string(), format!("Bearer {key}"));
        }
        servers
            .entry("Grep.app".to_string())
            .or_insert_with(|| McpServerConfig::built_in(TrustedMcpServer::GREP_APP, headers));
    } else {
        servers.remove("Grep.app");
    }

    cfg.mcp_servers = Some(servers);
}

// Both parameters are consumed only by the `mcp` and `lsp` arms below, so a build
// with neither feature legitimately uses neither.
#[cfg_attr(not(any(feature = "mcp", feature = "lsp")), allow(unused_variables))]
fn verification_sensitive_integrations_match(cfg: &Config, active: bool) -> bool {
    #[cfg(feature = "mcp")]
    let mcp_matches = cfg
        .mcp_servers
        .as_ref()
        .is_some_and(|servers| servers.contains_key("sentinel"))
        == active;
    #[cfg(not(feature = "mcp"))]
    let mcp_matches = true;

    #[cfg(feature = "lsp")]
    let lsp_matches = cfg
        .lsp
        .as_ref()
        .is_some_and(|lsp| lsp.servers.contains_key("sentinel"))
        == active;
    #[cfg(not(feature = "lsp"))]
    let lsp_matches = true;

    mcp_matches && lsp_matches
}

/// Installed-binary security check used by release/automation evidence. It
/// exercises the same project-config trust path as startup without requiring
/// a provider credential or launching any configured integration.
pub fn verify_project_config_trust() -> std::io::Result<()> {
    let root = std::env::temp_dir().canonicalize()?.join(format!(
        "mini-agent-project-config-trust-{}",
        uuid::Uuid::new_v4()
    ));
    let project_config = root.join("project/.zerostack/config.toml");
    let trust_store = root.join("state/config/trusted-project-configs.json");
    let result = (|| {
        std::fs::create_dir_all(project_config.parent().expect("config has a parent"))?;
        let exact_project_config = "chat_left_margin = 9\n\
                                    yolo = true\n\
                                    shell = \"untrusted-shell\"\n\
                                    [mcp_servers.sentinel]\n\
                                    command = \"mini-agent-project-config-trust-sentinel\"\n\
                                    [lsp]\n\
                                    enabled = true\n\
                                    [lsp.servers.sentinel]\n\
                                    command = \"mini-agent-project-config-trust-sentinel\"\n";
        std::fs::write(&project_config, exact_project_config)?;

        let mut headless = Config {
            chat_left_margin: Some(1),
            yolo: Some(false),
            shell: Some("trusted-shell".to_string()),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut headless,
            &project_config,
            &trust_store,
            false,
            &|_| panic!("headless project config trust must not prompt"),
        )
        .map_err(std::io::Error::other)?;
        if outcome != ProjectConfigTrustOutcome::SkippedHeadless
            || headless.chat_left_margin != Some(9)
            || headless.yolo != Some(false)
            || headless.shell.as_deref() != Some("trusted-shell")
            || !verification_sensitive_integrations_match(&headless, false)
            || trust_store.exists()
        {
            return Err(std::io::Error::other(
                "untrusted headless project config did not fail closed",
            ));
        }

        let mut approved = Config {
            yolo: Some(false),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut approved,
            &project_config,
            &trust_store,
            true,
            &|description| {
                description.contains("yolo")
                    && description.contains("SHA-256")
                    && description.contains("Project:")
            },
        )
        .map_err(std::io::Error::other)?;
        if outcome != ProjectConfigTrustOutcome::Approved
            || approved.yolo != Some(true)
            || approved.shell.as_deref() != Some("untrusted-shell")
            || !verification_sensitive_integrations_match(&approved, true)
            || !trust_store.is_file()
        {
            return Err(std::io::Error::other(
                "explicit content-bound project config trust was not activated",
            ));
        }

        let mut reused = Config {
            yolo: Some(false),
            shell: Some("trusted-shell".to_string()),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut reused,
            &project_config,
            &trust_store,
            false,
            &|_| panic!("an exact persisted binding must not prompt"),
        )
        .map_err(std::io::Error::other)?;
        if outcome != ProjectConfigTrustOutcome::AlreadyTrusted
            || reused.yolo != Some(true)
            || reused.shell.as_deref() != Some("untrusted-shell")
            || !verification_sensitive_integrations_match(&reused, true)
        {
            return Err(std::io::Error::other(
                "persisted exact project config trust was not reused",
            ));
        }

        let copied_config = root.join("copied-project/.zerostack/config.toml");
        std::fs::create_dir_all(copied_config.parent().expect("config has a parent"))?;
        std::fs::write(&copied_config, exact_project_config)?;
        let mut copied = Config {
            yolo: Some(false),
            shell: Some("trusted-shell".to_string()),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut copied,
            &copied_config,
            &trust_store,
            false,
            &|_| panic!("a copied checkout must not inherit project config trust"),
        )
        .map_err(std::io::Error::other)?;
        if outcome != ProjectConfigTrustOutcome::SkippedHeadless
            || copied.yolo != Some(false)
            || copied.shell.as_deref() != Some("trusted-shell")
            || !verification_sensitive_integrations_match(&copied, false)
        {
            return Err(std::io::Error::other(
                "project config trust crossed canonical project paths",
            ));
        }

        std::fs::write(
            &project_config,
            format!("{exact_project_config}# digest changed\n"),
        )?;
        let mut changed = Config {
            yolo: Some(false),
            shell: Some("trusted-shell".to_string()),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut changed,
            &project_config,
            &trust_store,
            false,
            &|_| panic!("headless changed config must not prompt"),
        )
        .map_err(std::io::Error::other)?;
        if outcome != ProjectConfigTrustOutcome::SkippedHeadless
            || changed.yolo != Some(false)
            || changed.shell.as_deref() != Some("trusted-shell")
            || !verification_sensitive_integrations_match(&changed, false)
        {
            return Err(std::io::Error::other(
                "project config trust survived a content change",
            ));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}

/// Persist only the edits made to an effective (global + project) config.
///
/// Project overrides deliberately remain in the effective values supplied to
/// callers, so serializing `after` wholesale would copy every unchanged local
/// setting into the user's global config. Applying the structural delta to a
/// freshly loaded global config preserves local provenance without requiring
/// each UI caller to understand the merge.
pub fn save_config_changes(before: &Config, after: &Config) -> io::Result<()> {
    save_config_changes_at(&resolve_config_path(), before, after)
}

fn save_config_changes_at(path: &Path, before: &Config, after: &Config) -> io::Result<()> {
    if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
        return save_toml_config_changes_at(path, before, after);
    }

    let global = if path_entry_exists(path)? {
        parse_config_content(path, &read_config_content(path)?)?
    } else {
        Config::default()
    };

    let mut global_value = serde_json::to_value(global).map_err(io::Error::other)?;
    let before_value = serde_json::to_value(before).map_err(io::Error::other)?;
    let after_value = serde_json::to_value(after).map_err(io::Error::other)?;
    apply_json_delta(&mut global_value, &before_value, &after_value);

    let mut updated: Config = serde_json::from_value(global_value).map_err(io::Error::other)?;
    strip_injected_mcp_defaults(&mut updated);
    save_config_at(path, &updated)
}

fn save_toml_config_changes_at(path: &Path, before: &Config, after: &Config) -> io::Result<()> {
    let mut global = if path_entry_exists(path)? {
        let content = read_config_content(path)?;
        // Reject a concurrently corrupted config rather than publishing a
        // syntactically valid delta over values the application cannot load.
        parse_config_content(path, &content)?;
        toml::from_str(&content).map_err(io::Error::other)?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let before = toml::Value::try_from(before).map_err(io::Error::other)?;
    let after = toml::Value::try_from(after).map_err(io::Error::other)?;
    apply_toml_delta(&mut global, &before, &after);

    strip_injected_mcp_defaults_toml(&mut global);
    let _: Config = global.clone().try_into().map_err(io::Error::other)?;
    atomic_config_write(path, &toml::to_string(&global).map_err(io::Error::other)?)?;
    tracing::debug!("config saved to {}", path.display());
    Ok(())
}

fn apply_toml_delta(base: &mut toml::Value, before: &toml::Value, after: &toml::Value) {
    if before == after {
        return;
    }

    let (Some(before), Some(after)) = (before.as_table(), after.as_table()) else {
        *base = after.clone();
        return;
    };
    if !base.is_table() {
        *base = toml::Value::Table(toml::map::Map::new());
    }
    let base = base.as_table_mut().expect("base was initialized as table");
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for key in keys {
        match (before.get(key), after.get(key)) {
            (Some(_), None) => {
                base.remove(key);
            }
            (None, Some(value)) => {
                base.insert(key.clone(), value.clone());
            }
            (Some(old), Some(new)) if old != new => {
                let entry = base
                    .entry(key.clone())
                    .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
                apply_toml_delta(entry, old, new);
            }
            _ => {}
        }
    }
}

fn apply_json_delta(
    base: &mut serde_json::Value,
    before: &serde_json::Value,
    after: &serde_json::Value,
) {
    if before == after {
        return;
    }

    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        *base = after.clone();
        return;
    };
    if !base.is_object() {
        *base = serde_json::Value::Object(serde_json::Map::new());
    }
    let base = base
        .as_object_mut()
        .expect("base was initialized as object");
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for key in keys {
        match (before.get(key), after.get(key)) {
            (Some(_), None) => {
                base.remove(key);
            }
            (None, Some(value)) => {
                base.insert(key.clone(), value.clone());
            }
            (Some(old), Some(new)) if old != new => {
                let entry = base.entry(key.clone()).or_insert(serde_json::Value::Null);
                apply_json_delta(entry, old, new);
            }
            _ => {}
        }
    }
}

fn strip_injected_mcp_defaults(_cfg: &mut Config) {
    #[cfg(feature = "mcp")]
    {
        if let Some(ref mut servers) = _cfg.mcp_servers {
            servers.remove("Exa Web Search");
            servers.remove("Context7");
            servers.remove("Grep.app");
        }
    }
}

fn strip_injected_mcp_defaults_toml(_cfg: &mut toml::Value) {
    #[cfg(feature = "mcp")]
    if let Some(servers) = _cfg
        .as_table_mut()
        .and_then(|root| root.get_mut("mcp_servers"))
        .and_then(toml::Value::as_table_mut)
    {
        servers.remove("Exa Web Search");
        servers.remove("Context7");
        servers.remove("Grep.app");
    }
}

fn save_config_at(path: &Path, cfg: &Config) -> io::Result<()> {
    atomic_config_write(path, &serialize_config_content(path, cfg)?)?;
    tracing::debug!("config saved to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod config_delta_tests {
    use super::{parse_config_content, save_config_changes_at};
    use crate::config::Config;

    #[test]
    fn saving_effective_config_changes_does_not_copy_project_overrides_globally() {
        let root =
            std::env::temp_dir().join(format!("mini-agent-config-delta-{}", uuid::Uuid::new_v4()));
        let path = root.join("config.toml");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &path,
            "model = \"global-model\"\nmax_tokens = 100\nfuture_date = 1979-05-27T07:32:00Z\n",
        )
        .unwrap();

        let before = parse_config_content(
            &path,
            "model = \"project-model\"\nmax_tokens = 100\nchat_left_margin = 7\nfuture_date = 1979-05-27T07:32:00Z\n",
        )
        .unwrap();
        let after = Config {
            max_tokens: Some(200),
            ..before.clone()
        };

        save_config_changes_at(&path, &before, &after).unwrap();
        let saved = parse_config_content(&path, &std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.model.as_deref(), Some("global-model"));
        assert_eq!(saved.max_tokens, Some(200));
        assert_eq!(saved.chat_left_margin, None);
        let raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw["future_date"].is_datetime());

        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod project_config_trust_tests {
    use super::{
        PROJECT_CONFIG_TRUST_SCHEMA, ProjectConfigTrustOutcome, ProjectConfigTrustStore,
        apply_local_override_with_confirmation, save_project_config_trust, split_project_override,
    };
    use crate::config::Config;

    fn fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mini-agent-project-config-trust-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        let config = root.join("project/.zerostack/config.toml");
        let trust = root.join("state/config/trusted-project-configs.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        (root, config, trust)
    }

    #[test]
    fn project_config_trust_headless_keeps_sensitive_keys_inert_and_merges_benign_keys() {
        let (root, config_path, trust_path) = fixture("headless");
        std::fs::write(
            &config_path,
            "chat_left_margin = 7\n\
             yolo = true\n\
             shell = \"untrusted-shell\"\n\
             verify_command = \"untrusted-verifier\"\n\
             [mcp_servers.sentinel]\n\
             command = \"untrusted-mcp-sentinel\"\n\
             [lsp]\n\
             enabled = true\n\
             [lsp.servers.sentinel]\n\
             command = \"untrusted-lsp-sentinel\"\n",
        )
        .unwrap();
        let mut cfg = Config {
            chat_left_margin: Some(1),
            yolo: Some(false),
            shell: Some("trusted-shell".to_string()),
            verify_command: Some("trusted-verifier".into()),
            ..Default::default()
        };

        let outcome = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            false,
            &|_| panic!("headless mode must not prompt"),
        )
        .unwrap();

        assert_eq!(outcome, ProjectConfigTrustOutcome::SkippedHeadless);
        assert_eq!(cfg.chat_left_margin, Some(7));
        assert_eq!(cfg.yolo, Some(false));
        assert_eq!(cfg.shell.as_deref(), Some("trusted-shell"));
        assert_eq!(cfg.verify_command.as_deref(), Some("trusted-verifier"));
        #[cfg(feature = "mcp")]
        assert!(cfg.mcp_servers.is_none());
        #[cfg(feature = "lsp")]
        assert!(cfg.lsp.is_none());
        assert!(!trust_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_trust_approval_is_inspectable_persisted_and_reusable() {
        let (root, config_path, trust_path) = fixture("approval");
        let secret = "must-not-appear-in-confirmation";
        std::fs::write(
            &config_path,
            format!("yolo = true\n[api_keys]\nopenrouter = \"{secret}\"\n"),
        )
        .unwrap();
        let mut cfg = Config {
            yolo: Some(false),
            ..Default::default()
        };
        let displayed = std::cell::RefCell::new(String::new());

        let outcome = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            true,
            &|description| {
                displayed.replace(description.to_string());
                true
            },
        )
        .unwrap();

        assert_eq!(outcome, ProjectConfigTrustOutcome::Approved);
        assert_eq!(cfg.yolo, Some(true));
        let displayed = displayed.borrow();
        let expected_project = root.join("project").display().to_string();
        assert!(displayed.contains(expected_project.as_str()));
        assert!(displayed.contains("api_keys, yolo"));
        assert!(displayed.contains("SHA-256:"));
        assert!(displayed.contains("yolo = true"));
        assert!(displayed.contains("<redacted>"));
        assert!(!displayed.contains(secret));
        assert!(trust_path.is_file());

        let mut fresh = Config {
            yolo: Some(false),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut fresh,
            &config_path,
            &trust_path,
            false,
            &|_| panic!("an exact persisted binding must not prompt"),
        )
        .unwrap();
        assert_eq!(outcome, ProjectConfigTrustOutcome::AlreadyTrusted);
        assert_eq!(fresh.yolo, Some(true));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_trust_content_change_and_denial_are_side_effect_free() {
        let (root, config_path, trust_path) = fixture("content-change");
        std::fs::write(&config_path, "yolo = true\n").unwrap();
        let mut approved = Config::default();
        assert_eq!(
            apply_local_override_with_confirmation(
                &mut approved,
                &config_path,
                &trust_path,
                true,
                &|_| true,
            )
            .unwrap(),
            ProjectConfigTrustOutcome::Approved
        );
        let before_denial = std::fs::read(&trust_path).unwrap();

        std::fs::write(&config_path, "yolo = true\n# changed\n").unwrap();
        let mut changed = Config {
            yolo: Some(false),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut changed,
            &config_path,
            &trust_path,
            true,
            &|_| false,
        )
        .unwrap();

        assert_eq!(outcome, ProjectConfigTrustOutcome::Declined);
        assert_eq!(changed.yolo, Some(false));
        assert_eq!(std::fs::read(&trust_path).unwrap(), before_denial);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_trust_does_not_persist_approval_for_invalid_sensitive_values() {
        let (root, config_path, trust_path) = fixture("invalid-approved");
        std::fs::write(&config_path, "yolo = \"not-a-boolean\"\n").unwrap();
        let mut cfg = Config::default();

        let error = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            true,
            &|_| true,
        )
        .unwrap_err();

        assert!(error.contains("invalid value"));
        assert!(!trust_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_trust_does_not_cross_checkout_paths() {
        let (root, first_config, trust_path) = fixture("path-binding");
        std::fs::write(&first_config, "yolo = true\n").unwrap();
        let mut first = Config::default();
        apply_local_override_with_confirmation(
            &mut first,
            &first_config,
            &trust_path,
            true,
            &|_| true,
        )
        .unwrap();

        let second_config = root.join("copied-project/.zerostack/config.toml");
        std::fs::create_dir_all(second_config.parent().unwrap()).unwrap();
        std::fs::copy(&first_config, &second_config).unwrap();
        let mut copied = Config {
            yolo: Some(false),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut copied,
            &second_config,
            &trust_path,
            false,
            &|_| panic!("headless copied checkout must not prompt"),
        )
        .unwrap();

        assert_eq!(outcome, ProjectConfigTrustOutcome::SkippedHeadless);
        assert_eq!(copied.yolo, Some(false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_trust_schema_change_invalidates_prior_bindings() {
        let (root, config_path, trust_path) = fixture("schema");
        std::fs::write(&config_path, "yolo = true\n").unwrap();
        save_project_config_trust(
            &trust_path,
            &ProjectConfigTrustStore {
                schema: PROJECT_CONFIG_TRUST_SCHEMA + 1,
                bindings: Vec::new(),
            },
        )
        .unwrap();
        let mut cfg = Config {
            yolo: Some(false),
            ..Default::default()
        };

        let outcome = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            false,
            &|_| panic!("headless stale schema must not prompt"),
        )
        .unwrap();

        assert_eq!(outcome, ProjectConfigTrustOutcome::SkippedHeadless);
        assert_eq!(cfg.yolo, Some(false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_trust_classifies_executable_security_and_unknown_keys_as_sensitive() {
        let (_, _, keys) = split_project_override(
            r#"
model = "benign-project-model"
yolo = true
sandbox = false
permission-allow = { bash = ["*"] }
shell = "project-shell"
verify_command = "project-verifier"
enable_skill_proposals = true
custom_providers = {}
mcp_servers = {}
lsp = {}
acp_servers = {}
future_security_switch = true
"#,
        )
        .unwrap();

        assert!(!keys.contains("model"));
        for key in [
            "yolo",
            "sandbox",
            "permission-allow",
            "shell",
            "verify_command",
            "enable_skill_proposals",
            "custom_providers",
            "mcp_servers",
            "lsp",
            "acp_servers",
            "future_security_switch",
        ] {
            assert!(keys.contains(key), "{key} must require project trust");
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_config_trust_symlink_target_change_invalidates_binding() {
        use std::os::unix::fs::symlink;

        let (root, _, trust_path) = fixture("symlink");
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(first.join(".zerostack")).unwrap();
        std::fs::create_dir_all(second.join(".zerostack")).unwrap();
        std::fs::write(first.join(".zerostack/config.toml"), "yolo = true\n").unwrap();
        std::fs::write(second.join(".zerostack/config.toml"), "yolo = true\n").unwrap();
        let linked_project = root.join("linked-project");
        symlink(&first, &linked_project).unwrap();
        let linked_config = linked_project.join(".zerostack/config.toml");

        let mut approved = Config::default();
        apply_local_override_with_confirmation(
            &mut approved,
            &linked_config,
            &trust_path,
            true,
            &|_| true,
        )
        .unwrap();
        std::fs::remove_file(&linked_project).unwrap();
        symlink(&second, &linked_project).unwrap();

        let mut replaced = Config {
            yolo: Some(false),
            ..Default::default()
        };
        let outcome = apply_local_override_with_confirmation(
            &mut replaced,
            &linked_config,
            &trust_path,
            false,
            &|_| panic!("headless replaced symlink must not prompt"),
        )
        .unwrap();
        assert_eq!(outcome, ProjectConfigTrustOutcome::SkippedHeadless);
        assert_eq!(replaced.yolo, Some(false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_config_trust_rejects_symlinked_persistence() {
        use std::os::unix::fs::symlink;

        let (root, config_path, trust_path) = fixture("trust-symlink");
        std::fs::write(&config_path, "yolo = true\n").unwrap();
        std::fs::create_dir_all(trust_path.parent().unwrap()).unwrap();
        let attacker_file = root.join("attacker-controlled.json");
        std::fs::write(
            &attacker_file,
            format!(
                "{{\"schema\":{},\"bindings\":[]}}",
                PROJECT_CONFIG_TRUST_SCHEMA
            ),
        )
        .unwrap();
        symlink(&attacker_file, &trust_path).unwrap();
        let mut cfg = Config {
            yolo: Some(false),
            ..Default::default()
        };

        let outcome = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            true,
            &|_| panic!("unavailable private trust must not prompt or activate"),
        )
        .unwrap();

        assert_eq!(outcome, ProjectConfigTrustOutcome::TrustUnavailable);
        assert_eq!(cfg.yolo, Some(false));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod preservation_check_tests {
    use super::{verify_config_preservation, verify_config_preservation_at};
    use crate::paths::AppPaths;

    #[test]
    fn installed_binary_preservation_check_uses_atomic_round_trip() {
        let directory = std::env::temp_dir().join(format!(
            "mini-agent-config-preservation-check-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("config.toml");

        verify_config_preservation_at(&path).unwrap();

        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved.get("future_scalar").and_then(toml::Value::as_str),
            Some("keep-me")
        );
        assert_eq!(
            saved.get("model").and_then(toml::Value::as_str),
            Some("after-check")
        );
        assert!(saved.get("temperature").is_none());

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn installed_binary_preservation_check_leaves_stale_pid_file_untouched() {
        let directory = std::env::temp_dir().join(format!(
            "mini-agent-config-preservation-cache-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&directory).unwrap();
        let stale_path = directory.join(format!(
            ".config-preservation-check-{}.toml",
            std::process::id()
        ));
        std::fs::write(&stale_path, "stale-user-data").unwrap();
        let paths = AppPaths {
            config_dir: directory.clone(),
            data_dir: directory.clone(),
            local_data_dir: directory.clone(),
            state_dir: directory.clone(),
            cache_dir: directory.clone(),
            credentials_dir: directory.clone(),
            project_dir: None,
        };

        verify_config_preservation(&paths).unwrap();

        assert_eq!(
            std::fs::read_to_string(&stale_path).unwrap(),
            "stale-user-data"
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);

        std::fs::remove_file(stale_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
