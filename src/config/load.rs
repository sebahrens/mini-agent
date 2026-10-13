use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use std::io;

use crate::config::keys;
use crate::config::{
    Config, EditSystem, QuickModelConfig, StatusLineConfig, StatusLineLine, StatusLineSegment,
};
#[cfg(feature = "mcp")]
use crate::extras::mcp::config::{McpServerConfig, TrustedMcpServer};
use crate::paths::AppPaths;

const PROJECT_CONFIG_TRUST_SCHEMA: u32 = 1;
const MAX_PROJECT_CONFIG_BYTES: usize = 256 * 1024;
const MAX_PRIVATE_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_PROJECT_TRUST_BYTES: usize = 1024 * 1024;

/// Project-local keys that cannot launch a process, select an executable
/// integration/provider, or weaken an authorization boundary. Every other
/// top-level key, including unknown future keys, requires explicit trust.
const BENIGN_PROJECT_CONFIG_KEYS: &[&str] = &[
    "always_show_welcome",
    // Goal bounds only limit how long the harness keeps working and who
    // reviews a completion claim; nothing here widens what may be executed.
    // The command-bearing keys are separate and stay sensitive.
    "goal",
    "chain",
    "chat_left_margin",
    "colors",
    "compact_enabled",
    "context_window",
    "default_prompt",
    "edit_system",
    "keep_recent_tokens",
    "keep_recent_tool_results",
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
    "reasoning_display",
    "show_tool_details",
    "diff_context_lines",
    "max_diff_lines",
    "statusline",
    "subagent_max_find_results",
    "subagent_max_grep_results",
    "subagent_max_list_dir_entries",
    "subagent_max_read_lines",
    "temperature",
];

/// Top-level `extra_body` keys the OpenAI Responses request body actually
/// honors.
///
/// rig deserializes `extra_body` into its `AdditionalParameters`, which has no
/// `deny_unknown_fields`, so anything outside this set is dropped without a
/// word — a Chat-Completions habit like `reasoning_effort` or
/// `max_completion_tokens` simply never reaches the provider. `stream` is
/// listed because rig consumes and removes it before deserializing.
const RESPONSES_EXTRA_BODY_KEYS: &[&str] = &[
    "background",
    "include",
    "metadata",
    "parallel_tool_calls",
    "previous_response_id",
    "prompt_cache_key",
    "prompt_cache_retention",
    "reasoning",
    "service_tier",
    "store",
    "stream",
    "text",
    "top_p",
    "truncation",
    "user",
];

/// The complete set of `include` values rig's closed `Include` enum can
/// deserialize. Anything else turns *every* completion request into a
/// `RequestError`, so it is rejected at load with the accepted list named.
const RESPONSES_INCLUDE_VALUES: &[&str] = &[
    "code_interpreter_call.outputs",
    "computer_call.output.image_url",
    "file_search_call.results",
    "message.input_image.image_url",
    "reasoning.encrypted_content",
];

/// Whether `provider` resolves to the OpenAI family *and* to the Responses
/// API, which is the only combination where `extra_body` is interpreted by
/// rig's closed `AdditionalParameters` schema.
fn provider_uses_responses_api(cfg: &Config, provider: &str) -> bool {
    let customs = cfg.custom_providers.clone().unwrap_or_default();
    let Ok(resolved) = crate::provider::resolve_provider_config(provider, &customs) else {
        return false;
    };
    if resolved.kind != crate::auth::ProviderKind::OpenAI {
        return false;
    }
    crate::provider::resolve_api_style(resolved.base_url.as_deref(), customs.get(provider))
        == crate::config::ApiStyle::Responses
}

/// Validates one `extra_body` value against the Responses request schema.
///
/// Warnings are returned for keys rig would silently drop and for a stripped
/// `previous_response_id` (this agent always sends the full input for a turn,
/// so a globally pinned id would resend the whole conversation *and* ask the
/// provider to prepend a stored one). An `include` value outside rig's closed
/// enum is a hard error: left in place it would fail every request with an
/// opaque `RequestError`.
pub(crate) fn validate_responses_extra_body(
    label: &str,
    extra: &mut serde_json::Value,
) -> Result<Vec<String>, String> {
    if !extra.is_object() {
        let shape = match extra {
            serde_json::Value::Array(_) => "an array",
            serde_json::Value::Null => "an empty value",
            _ => "a scalar",
        };
        return Err(format!(
            "{label} must be a table of Responses request-body keys, not {shape}"
        ));
    }
    let object = extra
        .as_object_mut()
        .expect("extra_body shape checked immediately above");

    if let Some(include) = object.get("include") {
        let serde_json::Value::Array(values) = include else {
            return Err(format!(
                "{label}.include must be an array of strings; accepted values are {}",
                RESPONSES_INCLUDE_VALUES.join(", ")
            ));
        };
        for value in values {
            let accepted = value
                .as_str()
                .is_some_and(|value| RESPONSES_INCLUDE_VALUES.contains(&value));
            if !accepted {
                return Err(format!(
                    "{label}.include contains an unsupported value {value}; \
                     the OpenAI Responses API client accepts only {}",
                    RESPONSES_INCLUDE_VALUES.join(", ")
                ));
            }
        }
    }

    let mut warnings = Vec::new();
    if object.remove("previous_response_id").is_some() {
        warnings.push(format!(
            "{label}.previous_response_id was ignored: this agent replays the full input \
             every turn, so pinning a stored response id would duplicate the conversation"
        ));
    }
    let unknown: Vec<String> = object
        .keys()
        .filter(|key| !RESPONSES_EXTRA_BODY_KEYS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        warnings.push(format!(
            "{label} has key(s) the OpenAI Responses API request ignores: {}. \
             Chat-Completions keys such as reasoning_effort are not read on this path; \
             use the [reasoning] config section instead",
            unknown.join(", ")
        ));
    }
    Ok(warnings)
}

/// Validates every configured `extra_body` whose provider resolves to the
/// OpenAI Responses API, stripping `previous_response_id` and warning about
/// keys that would be dropped in silence.
pub(crate) fn validate_extra_body(cfg: &mut Config) -> Result<Vec<String>, String> {
    let global_is_responses = cfg
        .provider
        .clone()
        .is_some_and(|provider| provider_uses_responses_api(cfg, &provider));
    let responses_quick_models: Vec<String> = cfg
        .quick_models
        .iter()
        .flatten()
        .filter(|(_, model)| provider_uses_responses_api(cfg, &model.provider))
        .map(|(name, _)| name.clone())
        .collect();

    let mut warnings = Vec::new();
    if global_is_responses && let Some(extra) = cfg.extra_body.as_mut() {
        warnings.extend(validate_responses_extra_body("extra_body", extra)?);
    }
    if let Some(quick_models) = cfg.quick_models.as_mut() {
        for name in responses_quick_models {
            let Some(model) = quick_models.get_mut(&name) else {
                continue;
            };
            let Some(extra) = model.extra_body.as_mut() else {
                continue;
            };
            warnings.extend(validate_responses_extra_body(
                &format!("quick_models.{name}.extra_body"),
                extra,
            )?);
        }
    }
    Ok(warnings)
}

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
    read_private_content(path, MAX_PRIVATE_CONFIG_BYTES, false)
}

pub(crate) fn parse_config_content(path: &Path, content: &str) -> io::Result<Config> {
    let mut cfg: Config = match path.extension().and_then(|extension| extension.to_str()) {
        Some("toml") => toml::from_str(content).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "config is not valid TOML ({})",
                    keys::describe_toml_error(content, &error)
                ),
            )
        }),
        _ => serde_yaml_ng::from_str(content).map_err(|error| {
            let message = match error.location() {
                Some(location) => format!(
                    "config is not valid YAML or JSON (line {}, column {})",
                    location.line(),
                    location.column()
                ),
                None => "config is not valid YAML or JSON".to_string(),
            };
            io::Error::new(io::ErrorKind::InvalidData, message)
        }),
    }?;
    cfg.provenance.global_file = Some(path.to_path_buf());
    cfg.provenance.global_status = Some("loaded");
    let spellings: Vec<String> =
        if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
            toml::from_str::<toml::Value>(content)
                .ok()
                .and_then(|value| {
                    value
                        .as_table()
                        .map(|table| table.keys().cloned().collect())
                })
                .unwrap_or_default()
        } else {
            serde_yaml_ng::from_str::<serde_yaml_ng::Value>(content)
                .ok()
                .and_then(|value| {
                    value.as_mapping().map(|table| {
                        table
                            .keys()
                            .filter_map(|key| key.as_str().map(str::to_owned))
                            .collect()
                    })
                })
                .unwrap_or_default()
        };
    cfg.provenance.global_keys = spellings
        .iter()
        .filter_map(|spelling| keys::lookup(spelling).map(|key| key.name))
        .collect();
    Ok(cfg)
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
            input_token_cost: Some(0.0983),
            output_token_cost: Some(0.1966),
            cache_read_token_cost: None,
            cache_write_token_cost: None,
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
            input_token_cost: Some(0.435),
            output_token_cost: Some(0.87),
            cache_read_token_cost: None,
            cache_write_token_cost: None,
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
    input_token_cost: Option<f64>,
    output_token_cost: Option<f64>,
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
            cache_read_token_cost: None,
            cache_write_token_cost: None,
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
        keep_recent_tool_results: Some(crate::session::DEFAULT_KEEP_RECENT_TOOL_RESULTS),
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

/// Inspect configuration without creating files, repairing permissions,
/// prompting for trust, or exiting on errors. Diagnostics contain only fixed
/// error classes: parser excerpts and credential values must not escape.
pub(crate) fn load_for_diagnostics(paths: &AppPaths) -> Config {
    let path = pick_existing(&paths.config_dir);
    let mut cfg = match read_private_content_mode(&path, MAX_PRIVATE_CONFIG_BYTES, true, false) {
        Ok(content) => match parse_config_content(&path, &content) {
            Ok(cfg) => cfg,
            Err(_) => {
                let mut cfg = Config::default();
                cfg.provenance.global_status = Some("invalid; defaults shown");
                cfg.load_warnings
                    .push("global config could not be parsed (values omitted)".into());
                cfg
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut cfg = rich_default_config();
            cfg.provenance.global_status = Some("missing; compiled defaults shown");
            cfg
        }
        Err(_) => {
            let mut cfg = Config::default();
            cfg.provenance.global_status = Some("unreadable or not private; defaults shown");
            cfg.load_warnings
                .push("global config cannot be inspected without permission repair".into());
            cfg
        }
    };
    cfg.provenance.global_file = Some(path);
    let unknown = keys::unknown_key_warnings("global config", cfg.preserved.keys());
    cfg.load_warnings.extend(std::iter::repeat_n(
        "global config has an unknown or unavailable key (name/value omitted)".into(),
        unknown.len(),
    ));
    let clamped = cfg.retry.clamp_to_limits();
    cfg.load_warnings.extend(
        clamped
            .into_iter()
            .map(|_| "global retry policy was clamped".into()),
    );
    let user_retry = cfg.retry.clone();
    if let Some(project) = paths.project_config_file() {
        match std::fs::symlink_metadata(&project) {
            Ok(_) => {
                if apply_local_override_with_confirmation_mode(
                    &mut cfg,
                    &project,
                    &paths.project_config_trust_file(),
                    false,
                    &|_| false,
                    false,
                )
                .is_err()
                {
                    cfg.provenance.project_file = Some(project);
                    cfg.provenance.project_trust = Some("unreadable or invalid; inactive");
                    cfg.load_warnings
                        .push("project config could not be inspected (values omitted)".into());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => cfg
                .load_warnings
                .push("project config path is unavailable".into()),
        }
    }
    let capped = cfg.retry.cap_at(&user_retry);
    cfg.load_warnings.extend(
        capped
            .into_iter()
            .map(|_| "project retry policy was capped".into()),
    );
    let shell_warnings = cfg.shell_exact_allow_warnings();
    cfg.load_warnings.extend(std::iter::repeat_n(
        "exact shell allow rule requires review (pattern omitted)".into(),
        shell_warnings.len(),
    ));
    #[cfg(feature = "mcp")]
    inject_mcp_defaults(&mut cfg);
    match validate_extra_body(&mut cfg) {
        Ok(warnings) => cfg.load_warnings.extend(std::iter::repeat_n(
            "provider request setting was ignored (name/value omitted)".into(),
            warnings.len(),
        )),
        Err(_) => cfg
            .load_warnings
            .push("provider request settings are invalid (values omitted)".into()),
    }
    cfg
}

#[cfg(test)]
mod doctor_config_tests {
    use super::*;

    fn paths(root: &Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: Some(root.join("workspace/.zerostack")),
        }
    }

    #[test]
    fn doctor_missing_configuration_does_not_create_paths_or_default_files() {
        let root = crate::tests::OwnedTestDirectory::new("doctor-config-missing");
        let cfg = load_for_diagnostics(&paths(root.path()));
        assert_eq!(
            cfg.provenance.global_status,
            Some("missing; compiled defaults shown")
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert!(cfg.provenance.global_keys.is_empty());
    }

    #[test]
    fn doctor_config_warning_count_matches_real_loading_without_disclosing_authored_keys() {
        let root = crate::tests::OwnedTestDirectory::new("doctor-config-warnings");
        let paths = paths(root.path());
        std::fs::create_dir(root.path().join("workspace")).unwrap();
        let path = paths.config_dir.join("config.toml");
        crate::fs::private_atomic_create_sync(&path, b"provider = 'ollama'\nPRIVATE_UNKNOWN_KEY = 'PRIVATE_VALUE'\n[retry]\nmax_attempts = 1000\n").unwrap();
        let diagnosed = load_for_diagnostics(&paths);
        let normal = load_with_paths(&paths, false).0;
        assert_eq!(diagnosed.load_warnings.len(), normal.load_warnings.len());
        assert_eq!(diagnosed.load_warnings.len(), 2);
        assert_eq!(
            diagnosed.retry.max_attempts,
            crate::retry::MAX_RETRY_ATTEMPTS
        );
        assert!(!format!("{:?}", diagnosed.load_warnings).contains("PRIVATE_"));
        let project = paths.project_config_file().unwrap();
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(
            &project,
            "context_window=10000\nPRIVATE_PROJECT_UNKNOWN='PRIVATE_PROJECT_VALUE'\n",
        )
        .unwrap();
        let diagnosed = load_for_diagnostics(&paths);
        let normal = load_with_paths(&paths, false).0;
        assert_eq!(diagnosed.load_warnings.len(), normal.load_warnings.len());
        assert_eq!(diagnosed.load_warnings.len(), 3);
        assert!(!format!("{:?}", diagnosed.load_warnings).contains("PRIVATE_"));
    }

    #[test]
    fn doctor_invalid_config_omits_parser_values_and_preserves_bytes() {
        let root = crate::tests::OwnedTestDirectory::new("doctor-config-invalid");
        let paths = paths(root.path());
        let path = paths.config_dir.join("config.toml");
        let bytes = b"model = 'PRIVATE_PARSE_VALUE' trailing invalid\n";
        crate::fs::private_atomic_create_sync(&path, bytes).unwrap();
        let cfg = load_for_diagnostics(&paths);
        assert_eq!(
            cfg.provenance.global_status,
            Some("invalid; defaults shown")
        );
        assert!(!format!("{:?}", cfg.load_warnings).contains("PRIVATE_PARSE_VALUE"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(&paths.config_dir).unwrap().count(), 1);
        assert!(!paths.state_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn doctor_private_permission_refusal_does_not_repair_file_or_directory() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = crate::tests::OwnedTestDirectory::new("doctor-config-mode");
        let paths = paths(root.path());
        let path = paths.config_dir.join("config.toml");
        crate::fs::private_atomic_create_sync(&path, b"model = 'PRIVATE_MODEL'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&paths.config_dir, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let cfg = load_for_diagnostics(&paths);
        assert_eq!(
            cfg.provenance.global_status,
            Some("unreadable or not private; defaults shown")
        );
        assert!(cfg.model.is_none());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        assert_eq!(
            std::fs::metadata(&paths.config_dir).unwrap().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn doctor_project_layers_preserve_actual_trust_and_omit_unknown_names() {
        let root = crate::tests::OwnedTestDirectory::new("doctor-config-trust");
        let paths = paths(root.path());
        let global = paths.config_dir.join("config.toml");
        crate::fs::private_atomic_create_sync(
            &global,
            b"provider = 'anthropic'\nmodel = 'global-model'\n",
        )
        .unwrap();
        let project = paths.project_config_file().unwrap();
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "model = 'project-model'\nprovider = 'ollama'\nPRIVATE_UNKNOWN_NAME = 'PRIVATE_UNKNOWN_VALUE'\n").unwrap();
        let trust = paths.project_config_trust_file();
        let untrusted = load_for_diagnostics(&paths);
        assert_eq!(untrusted.provider.as_deref(), Some("anthropic"));
        assert_eq!(untrusted.model.as_deref(), Some("project-model"));
        assert!(untrusted.provenance.project_keys.contains("model"));
        assert!(!untrusted.provenance.project_keys.contains("provider"));
        assert!(!trust.exists());
        let mut normal = parse_config_content(&global, "provider = 'anthropic'\n").unwrap();
        apply_local_override_with_confirmation(&mut normal, &project, &trust, true, &|_| true)
            .unwrap();
        let before = std::fs::read(&trust).unwrap();
        let trusted = load_for_diagnostics(&paths);
        assert_eq!(trusted.provider.as_deref(), Some("ollama"));
        assert_eq!(
            trusted.provenance.project_trust,
            Some("content-bound trusted")
        );
        assert!(trusted.provenance.project_keys.contains("provider"));
        assert!(!format!("{:?}", trusted.provenance).contains("PRIVATE_UNKNOWN_NAME"));
        assert_eq!(std::fs::read(&trust).unwrap(), before);
        let serializable = Config {
            #[cfg(feature = "mcp")]
            mcp_servers: None,
            ..trusted.clone()
        };
        let encoded = serde_json::to_string(&serializable).unwrap();
        assert!(!encoded.contains("provenance") && !encoded.contains("global_status"));
        std::fs::write(&project, "model = 'revision'\nprovider = 'openai'\n").unwrap();
        assert_eq!(
            load_for_diagnostics(&paths).provider.as_deref(),
            Some("anthropic")
        );
        assert_eq!(std::fs::read(&trust).unwrap(), before);
    }
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
    if is_first_startup {
        cfg.provenance.global_file = Some(path.clone());
        cfg.provenance.global_status = Some(
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                "initialized from compiled defaults"
            } else {
                "missing; compiled defaults shown"
            },
        );
    }

    tracing::debug!(
        "config loaded from {}: {} quick_models, {} custom_providers",
        path.display(),
        cfg.quick_models.as_ref().map(|m| m.len()).unwrap_or(0),
        cfg.custom_providers.as_ref().map(|m| m.len()).unwrap_or(0),
    );

    let mut warnings = Vec::new();
    emit_config_warnings(
        &mut warnings,
        keys::unknown_key_warnings(
            &format!("global config {}", path.display()),
            cfg.preserved.keys(),
        ),
    );
    let clamped = cfg.retry.clamp_to_limits();
    emit_config_warnings(
        &mut warnings,
        clamped_retry_warnings(&cfg, clamped, "global config", &path),
    );
    #[cfg(feature = "mcp")]
    {
        if let Some(servers) = &cfg.mcp_servers {
            for name in servers.keys() {
                cfg.mcp_origins.entry(name.clone()).or_default().push(
                    crate::extras::mcp::project::ServerOrigin {
                        source: path.display().to_string(),
                        selected: true,
                        inactive: None,
                        digest: None,
                    },
                );
            }
        }
        if let Some(root) = local_config_path
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            match crate::paths::WorkspaceBinding::capture(root) {
                Ok(workspace) => {
                    if let Err(error) = crate::extras::mcp::project::load_in_workspace(
                        &mut cfg,
                        &workspace,
                        project_trust_path,
                        interactive,
                        &confirm_project_config_trust,
                    ) {
                        emit_config_warnings(&mut warnings, vec![error]);
                    }
                }
                Err(_) => emit_config_warnings(
                    &mut warnings,
                    vec!["project MCP workspace identity is unavailable".into()],
                ),
            }
        }
    }
    if let Some(local_config_path) = local_config_path {
        if local_config_path.exists() {
            emit_config_warnings(
                &mut warnings,
                project_config_unknown_key_warnings(local_config_path),
            );
        }
        let user_retry = cfg.retry.clone();
        apply_local_override(
            &mut cfg,
            local_config_path,
            project_trust_path,
            interactive,
            &confirm_project_config_trust,
        );
        // A repository may only tighten the user's retry policy.
        let clamped = cfg.retry.cap_at(&user_retry);
        emit_config_warnings(
            &mut warnings,
            clamped_retry_warnings(
                &cfg,
                clamped,
                "project config (may only tighten the global retry policy)",
                local_config_path,
            ),
        );
    }

    emit_config_warnings(&mut warnings, cfg.shell_exact_allow_warnings());

    #[cfg(feature = "mcp")]
    inject_mcp_defaults(&mut cfg);

    match validate_extra_body(&mut cfg) {
        Ok(body_warnings) => emit_config_warnings(&mut warnings, body_warnings),
        Err(error) => fatal_config_load(format!(
            "error: {} configures an unusable OpenAI Responses request body: {}\n\
             Fix the value or remove it.",
            path.display(),
            error,
        )),
    }

    cfg.load_warnings = warnings;

    (cfg, is_first_startup)
}

/// Print each new load warning to stderr and the log, and keep it for
/// `--print-config`.
fn emit_config_warnings(warnings: &mut Vec<String>, new: Vec<String>) {
    for warning in new {
        tracing::warn!("config: {warning}");
        eprintln!("warning: {warning}");
        warnings.push(warning);
    }
}

/// Describe each `[retry]` field that load-time bounding changed, naming the
/// file it came from and the value now in effect.
fn clamped_retry_warnings(
    cfg: &Config,
    fields: Vec<&'static str>,
    source: &str,
    path: &Path,
) -> Vec<String> {
    fields
        .into_iter()
        .map(|field| {
            let value = match field {
                "max_attempts" => cfg.retry.max_attempts as u64,
                "initial_backoff_ms" => cfg.retry.initial_backoff_ms,
                _ => cfg.retry.max_backoff_ms,
            };
            format!(
                "{source} {}: retry.{field} is outside the allowed range and was clamped to {value}",
                path.display()
            )
        })
        .collect()
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

#[cfg(test)]
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

/// Read one bounded private file while retaining its parent and exact inode.
/// The ordinary private opener still owns UID, permission repair, DACL and
/// descriptor-identity checks; it cannot authorize a different captured file.
fn read_private_content(
    path: &Path,
    max_bytes: usize,
    require_private_mode: bool,
) -> io::Result<String> {
    read_private_content_mode(path, max_bytes, require_private_mode, true)
}

fn read_private_content_mode(
    path: &Path,
    max_bytes: usize,
    require_private_mode: bool,
    repair: bool,
) -> io::Result<String> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("private file has no parent"))?;
    if std::fs::symlink_metadata(parent)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory is a link",
        ));
    }
    if repair {
        crate::fs::ensure_private_directory(parent)?;
    }
    let directory = crate::paths::WorkspaceBinding::capture(parent)?;
    let name = Path::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("private file has no name"))?,
    );
    directory.validate().map_err(io::Error::other)?;
    let file = directory.open_regular_relative(name)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        if metadata.uid() != crate::fs::private_current_uid()
            || metadata.nlink() != 1
            || (require_private_mode && metadata.mode() & 0o077 != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private file authority is invalid",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = require_private_mode;
    #[cfg(windows)]
    if crate::fs::windows_file_link_count(&file)? != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private file is hardlinked",
        ));
    }
    let private = if repair {
        crate::fs::open_private_file(path)?
    } else {
        crate::fs::open_private_file_checked(path, crate::fs::PrivateFileAccess::Read)?
    };
    if !repair {
        crate::fs::validate_private_file(&private)?;
    }
    crate::fs::ensure_same_file(
        path,
        &crate::fs::checked_file_metadata(&file)?,
        &crate::fs::checked_file_metadata(&private)?,
    )?;
    let content = bounded_config_text(private, max_bytes)?;
    let current = directory.open_regular_relative(name)?;
    crate::fs::ensure_same_file(
        path,
        &crate::fs::checked_file_metadata(&file)?,
        &crate::fs::checked_file_metadata(&current)?,
    )?;
    directory.validate().map_err(io::Error::other)?;
    if parent.canonicalize()? != directory.root()
        || std::fs::symlink_metadata(parent)?.file_type().is_symlink()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory changed during read",
        ));
    }
    Ok(content)
}

fn bounded_config_text(file: std::fs::File, max_bytes: usize) -> io::Result<String> {
    match crate::paths::read_utf8_bounded_status(file, max_bytes)? {
        crate::paths::BoundedUtf8Read::Content(content) => Ok(content),
        crate::paths::BoundedUtf8Read::Oversized => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("configuration exceeds {max_bytes} bytes"),
        )),
        crate::paths::BoundedUtf8Read::InvalidUtf8 => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration is not valid UTF-8",
        )),
    }
}

fn read_project_config_text_in_workspace(
    workspace: &crate::paths::WorkspaceBinding,
    relative: &Path,
) -> io::Result<String> {
    workspace.validate().map_err(io::Error::other)?;
    let file = workspace.open_regular_relative(relative)?;
    let identity = crate::fs::checked_file_metadata(&file)?;
    let content = bounded_config_text(file, MAX_PROJECT_CONFIG_BYTES)?;
    let path = workspace.root().join(relative);
    let canonical_config = path.canonicalize()?;
    let current = workspace.open_regular_relative(relative)?;
    crate::fs::ensure_same_file(
        &path,
        &identity,
        &crate::fs::checked_file_metadata(&current)?,
    )?;
    workspace.validate().map_err(io::Error::other)?;
    if canonical_config != path {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "project config identity changed",
        ));
    }
    Ok(content)
}

fn checked_project_config_binding(
    workspace: &crate::paths::WorkspaceBinding,
    relative: &Path,
    content: &str,
) -> Result<ProjectConfigTrustBinding, String> {
    let path = workspace.root().join(relative);
    let canonical_config = path
        .canonicalize()
        .map_err(|_| "project file is unavailable")?;
    let binding = ProjectConfigTrustBinding {
        canonical_project: workspace
            .root()
            .to_str()
            .ok_or("project path is not UTF-8")?
            .to_owned(),
        canonical_config: canonical_config
            .to_str()
            .ok_or("project file path is not UTF-8")?
            .to_owned(),
        config_sha256: crate::hex::encode_lower(Sha256::digest(content.as_bytes())),
    };
    workspace.validate()?;
    if Path::new(&binding.canonical_project) != workspace.root()
        || Path::new(&binding.canonical_config) != workspace.root().join(relative)
    {
        return Err("project config trust identity changed".to_string());
    }
    Ok(binding)
}

fn read_project_config_in_workspace(
    workspace: &crate::paths::WorkspaceBinding,
    relative: &Path,
) -> io::Result<(String, ProjectConfigTrustBinding)> {
    let content = read_project_config_text_in_workspace(workspace, relative)?;
    let binding =
        checked_project_config_binding(workspace, relative, &content).map_err(io::Error::other)?;
    Ok((content, binding))
}

fn capture_project_config(path: &Path) -> io::Result<(crate::paths::WorkspaceBinding, PathBuf)> {
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("project config has no root"))?;
    let workspace = crate::paths::WorkspaceBinding::capture(root)?;
    let relative = Path::new(
        path.parent()
            .and_then(Path::file_name)
            .ok_or_else(|| io::Error::other("project config has no directory"))?,
    )
    .join(
        path.file_name()
            .ok_or_else(|| io::Error::other("project config has no name"))?,
    );
    Ok((workspace, relative))
}

fn captured_project_config_content(
    path: &Path,
) -> io::Result<(crate::paths::WorkspaceBinding, PathBuf, String)> {
    let (workspace, relative) = capture_project_config(path)?;
    let content = read_project_config_text_in_workspace(&workspace, &relative)?;
    Ok((workspace, relative, content))
}

fn read_project_config_content(path: &Path) -> io::Result<String> {
    captured_project_config_content(path).map(|(_, _, content)| content)
}

#[cfg(test)]
fn read_project_config(path: &Path) -> io::Result<(String, ProjectConfigTrustBinding)> {
    let (workspace, relative) = capture_project_config(path)?;
    read_project_config_in_workspace(&workspace, &relative)
}

#[cfg(any(test, feature = "mcp"))]
fn load_project_config_trust(path: &Path) -> std::io::Result<ProjectConfigTrustStore> {
    load_project_config_trust_mode(path, true)
}

fn load_project_config_trust_mode(
    path: &Path,
    repair: bool,
) -> std::io::Result<ProjectConfigTrustStore> {
    let content = match read_private_content_mode(path, MAX_PROJECT_TRUST_BYTES, true, repair) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProjectConfigTrustStore {
                schema: PROJECT_CONFIG_TRUST_SCHEMA,
                bindings: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };
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

/// Fields inside an otherwise benign table that are not themselves benign.
///
/// `[goal]` is benign because every bound in it only limits how long the
/// harness keeps working; none can widen what runs. `judge` is not a bound.
/// Naming a `quick_models` entry decides which endpoint a bounded slice of the
/// conversation is sent to, and `off` removes the review of completion claims
/// altogether, so an untrusted project file gets neither for free — for the
/// same reason `goal_judge_model` and `goal_checks` are sensitive keys.
const SENSITIVE_FIELDS_IN_BENIGN_TABLES: &[(&str, &str)] = &[("goal", "judge")];

/// Whether a raw project `statusline` table shows a segment that drives the
/// background host `git status` refresh. Such a statusline is sensitive: the
/// refresh runs Git on the host against repository content the model can
/// author, so an untrusted clone must not be able to switch it on.
fn statusline_value_needs_git_status(value: &toml::Value) -> bool {
    value
        .get("lines")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|line| line.get("segments").and_then(toml::Value::as_array))
        .flatten()
        .filter_map(|segment| segment.get("item").and_then(toml::Value::as_str))
        .any(|item| crate::config::STATUSLINE_GIT_STATUS_ITEMS.contains(&item))
}

fn split_project_override(
    local_toml: &str,
) -> Result<(toml::Value, toml::Value, BTreeSet<String>), String> {
    let local = parse_project_toml(local_toml)?;
    let table = local
        .as_table()
        .ok_or_else(|| "project config must contain a TOML table".to_string())?;
    let mut benign = toml::map::Map::new();
    let mut sensitive = toml::map::Map::new();
    let mut sensitive_keys = BTreeSet::new();
    for (key, value) in table {
        if key == "permission_mode_cycle" {
            tracing::warn!("project config: permission_mode_cycle is global-only and was ignored");
            continue;
        }
        if !BENIGN_PROJECT_CONFIG_KEYS.contains(&key.as_str())
            || (key == "statusline" && statusline_value_needs_git_status(value))
        {
            sensitive_keys.insert(key.clone());
            sensitive.insert(key.clone(), value.clone());
            continue;
        }

        // A benign table may still carry one field that is not. Split those
        // out rather than classifying the whole table as sensitive: the bounds
        // beside them are exactly what a project should be able to set.
        let mut value = value.clone();
        for (table_key, field) in SENSITIVE_FIELDS_IN_BENIGN_TABLES {
            if key != table_key {
                continue;
            }
            let Some(held) = value.as_table_mut().and_then(|table| table.remove(*field)) else {
                continue;
            };
            let mut carved = toml::map::Map::new();
            carved.insert((*field).to_string(), held);
            sensitive_keys.insert(format!("{key}.{field}"));
            sensitive.insert(key.clone(), toml::Value::Table(carved));
        }
        benign.insert(key.clone(), value);
    }
    Ok((
        toml::Value::Table(benign),
        toml::Value::Table(sensitive),
        sensitive_keys,
    ))
}

fn merge_config_value(base: &Config, mut local: toml::Value) -> Result<Config, String> {
    // Only the owner CLI/global config sets financial policy. Strip
    // ignored project caps before recording diagnostic provenance.
    if let Some(table) = local.as_table_mut() {
        table.remove("max_budget_usd");
    }
    let mut provenance = base.provenance.clone();
    if let Some(table) = local.as_table() {
        provenance.project_keys.extend(
            table
                .iter()
                .filter(|(_, value)| !value.as_table().is_some_and(|table| table.is_empty()))
                .filter_map(|(spelling, _)| keys::lookup(spelling).map(|key| key.name)),
        );
        for (spelling, value) in table {
            if !value.is_table()
                && let Some(key) = keys::lookup(spelling)
            {
                provenance.global_keys.remove(key.name);
            }
        }
    }
    let mut base_toml = toml::Value::try_from(base)
        .map_err(|_| "base config could not be normalized".to_string())?;
    deep_merge_toml(&mut base_toml, local);
    let mut merged: Config = base_toml
        .try_into()
        .map_err(|_| "merged project config has an invalid value".to_string())?;
    merged.provenance = provenance;
    merged.load_warnings = base.load_warnings.clone();
    #[cfg(feature = "mcp")]
    {
        merged.mcp_origins = base.mcp_origins.clone();
    }
    Ok(merged)
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

#[cfg(feature = "mcp")]
fn record_project_mcp_origins(
    cfg: &mut Config,
    value: &toml::Value,
    relative: &Path,
    digest: &str,
    trusted: bool,
) {
    let Some(servers) = value.get("mcp_servers").and_then(toml::Value::as_table) else {
        return;
    };
    let source = relative
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    for name in servers.keys() {
        let origins = cfg.mcp_origins.entry(name.clone()).or_default();
        origins.retain(|origin| origin.source != source);
        if trusted {
            for origin in origins.iter_mut() {
                origin.selected = false;
            }
        }
        origins.push(crate::extras::mcp::project::ServerOrigin {
            source: source.clone(),
            selected: trusted,
            inactive: (!trusted).then(|| "project content is not trusted".into()),
            digest: Some(digest.into()),
        });
    }
}

fn apply_local_override_with_confirmation(
    cfg: &mut Config,
    path: &Path,
    trust_store_path: &Path,
    interactive: bool,
    confirm: &dyn Fn(&str) -> bool,
) -> Result<ProjectConfigTrustOutcome, String> {
    apply_local_override_with_confirmation_mode(
        cfg,
        path,
        trust_store_path,
        interactive,
        confirm,
        true,
    )
}

fn apply_local_override_with_confirmation_mode(
    cfg: &mut Config,
    path: &Path,
    trust_store_path: &Path,
    interactive: bool,
    confirm: &dyn Fn(&str) -> bool,
    repair: bool,
) -> Result<ProjectConfigTrustOutcome, String> {
    let (workspace, relative, content) = captured_project_config_content(path)
        .map_err(|error| format!("failed to read project config: {error}"))?;
    cfg.provenance.project_file = Some(path.to_path_buf());
    cfg.provenance.project_trust = Some("sensitive keys inactive");
    let (benign, sensitive, sensitive_keys) = split_project_override(&content)?;
    if !repair {
        let local = parse_project_toml(&content)?;
        let warnings = keys::unknown_key_warnings(
            "project config",
            local
                .as_table()
                .into_iter()
                .flat_map(|table| table.keys().map(String::as_str)),
        );
        cfg.load_warnings.extend(std::iter::repeat_n(
            "project config has an unknown or unavailable key (name/value omitted)".into(),
            warnings.len(),
        ));
    }
    #[allow(unused_mut)]
    let mut benign_cfg = merge_config_value(cfg, benign)?;
    #[cfg(feature = "mcp")]
    record_project_mcp_origins(
        &mut benign_cfg,
        &sensitive,
        &relative,
        &crate::extras::mcp::project::digest(&content),
        false,
    );
    if sensitive_keys.is_empty() {
        *cfg = benign_cfg;
        cfg.provenance.project_trust = Some("benign keys only");
        return Ok(ProjectConfigTrustOutcome::NoSensitiveKeys);
    }

    let binding = match checked_project_config_binding(&workspace, &relative, &content) {
        Ok(binding) => binding,
        Err(error) => {
            *cfg = benign_cfg;
            tracing::warn!(
                "project config: sensitive settings are inactive because trust identity is unavailable: {error}"
            );
            return Ok(ProjectConfigTrustOutcome::TrustUnavailable);
        }
    };
    let mut store = match load_project_config_trust_mode(trust_store_path, repair) {
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
        #[allow(unused_mut)]
        let mut trusted = merge_config_value(&benign_cfg, sensitive.clone())?;
        #[cfg(feature = "mcp")]
        record_project_mcp_origins(
            &mut trusted,
            &sensitive,
            &relative,
            &binding.config_sha256,
            true,
        );
        *cfg = trusted;
        cfg.provenance.project_trust = Some("content-bound trusted");
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

    #[allow(unused_mut)]
    let mut trusted_cfg = merge_config_value(&benign_cfg, sensitive.clone())?;
    #[cfg(feature = "mcp")]
    record_project_mcp_origins(
        &mut trusted_cfg,
        &sensitive,
        &relative,
        &binding.config_sha256,
        true,
    );
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
    cfg.provenance.project_trust = Some("content-bound approved");
    Ok(ProjectConfigTrustOutcome::Approved)
}

/// Whether `project_config` is bound in the private trust store with its
/// exact current content. Consumers other than config loading (for example
/// project prompt `%%mode=` directives) use this to decide whether repository
/// content may influence the security posture.
#[cfg(test)]
pub(crate) fn project_config_is_trusted(project_config: Option<&Path>, trust_store: &Path) -> bool {
    let Some(path) = project_config else {
        return false;
    };
    let Ok((_, binding)) = read_project_config(path) else {
        return false;
    };
    let Ok(store) = load_project_config_trust(trust_store) else {
        return false;
    };
    store.bindings.contains(&binding)
}

/// Existing project/config/SHA trust authority shared by templates, prompts
/// and specialist definitions, using the caller's retained workspace.
pub(crate) fn project_config_is_trusted_in_workspace(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> bool {
    project_config_is_trusted_in_workspace_mode(paths, workspace, true)
}

#[cfg(feature = "mcp")]
pub(crate) fn project_config_is_trusted_in_workspace_readonly(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> bool {
    project_config_is_trusted_in_workspace_mode(paths, workspace, false)
}

fn project_config_is_trusted_in_workspace_mode(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
    repair: bool,
) -> bool {
    let Ok((_, binding)) =
        read_project_config_in_workspace(workspace, Path::new(".zerostack/config.toml"))
    else {
        return false;
    };
    let Ok(store) = load_project_config_trust_mode(&paths.project_config_trust_file(), repair)
    else {
        return false;
    };
    workspace.validate().is_ok() && store.bindings.contains(&binding)
}

/// Record trust for `project_config` exactly as an interactive approval would.
#[cfg(test)]
pub(crate) fn trust_project_config(
    project_config: &Path,
    trust_store: &Path,
) -> Result<(), String> {
    let (_, binding) = read_project_config(project_config).map_err(|error| error.to_string())?;
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
    merge_config_value(base, parse_project_toml(local_toml)?)
}

/// Parse a project-local TOML config with its top-level keys rewritten to
/// their canonical spellings, so trust classification and merging see
/// `permission_deny` exactly as they see `permission-deny`.
fn parse_project_toml(local_toml: &str) -> Result<toml::Value, String> {
    let mut local: toml::Value = toml::from_str(local_toml).map_err(|error| {
        format!(
            "project config is not valid TOML ({})",
            keys::describe_toml_error(local_toml, &error)
        )
    })?;
    if let Some(table) = local.as_table_mut() {
        keys::canonicalize_top_level_keys(table)?;
    }
    Ok(local)
}

/// Unknown-key warnings for the project-local config at `path`. Unreadable or
/// invalid files yield none: loading them reports its own error.
fn project_config_unknown_key_warnings(path: &Path) -> Vec<String> {
    let Ok(content) = read_project_config_content(path) else {
        return Vec::new();
    };
    let Ok(local) = parse_project_toml(&content) else {
        return Vec::new();
    };
    let source = format!("project config {}", path.display());
    match local.clone().try_into::<Config>() {
        Ok(cfg) => keys::unknown_key_warnings(&source, cfg.preserved.keys()),
        Err(_) => local
            .as_table()
            .map(|table| keys::unknown_key_warnings(&source, table.keys().map(String::as_str)))
            .unwrap_or_default(),
    }
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
        remove_built_in(&mut servers, "Exa Web Search");
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
        remove_built_in(&mut servers, "Context7");
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
        remove_built_in(&mut servers, "Grep.app");
    }

    for (name, server) in &servers {
        if server.trusted_identity().is_some() {
            let origins = cfg.mcp_origins.entry(name.clone()).or_default();
            origins.retain(|origin| origin.source != "built-in");
            for origin in origins.iter_mut() {
                origin.selected = false;
            }
            origins.push(crate::extras::mcp::project::ServerOrigin {
                source: "built-in".into(),
                selected: true,
                inactive: None,
                digest: None,
            });
        }
    }
    cfg.mcp_servers = Some(servers);
}

/// Remove `name` only when it is the injected built-in. A user-defined server
/// that shares the name is the user's configuration (mini-agent-ydfkp).
#[cfg(feature = "mcp")]
fn remove_built_in(servers: &mut HashMap<String, McpServerConfig>, name: &str) {
    if servers
        .get(name)
        .is_some_and(|server| server.trusted_identity().is_some())
    {
        servers.remove(name);
    }
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

    let mut before = before.clone();
    strip_injected_mcp_defaults(&mut before);
    let mut after = after.clone();
    strip_injected_mcp_defaults(&mut after);
    let mut global_value = serde_json::to_value(global).map_err(io::Error::other)?;
    let before_value = serde_json::to_value(before).map_err(io::Error::other)?;
    let after_value = serde_json::to_value(after).map_err(io::Error::other)?;
    apply_json_delta(&mut global_value, &before_value, &after_value);

    let updated: Config = serde_json::from_value(global_value).map_err(io::Error::other)?;
    save_config_at(path, &updated)
}

fn save_toml_config_changes_at(path: &Path, before: &Config, after: &Config) -> io::Result<()> {
    let mut global = if path_entry_exists(path)? {
        let content = read_config_content(path)?;
        // Reject a concurrently corrupted config rather than publishing a
        // syntactically valid delta over values the application cannot load.
        parse_config_content(path, &content)?;
        let mut global: toml::Value = toml::from_str(&content).map_err(io::Error::other)?;
        // Rewrite alias spellings (`permission_deny`) to the canonical key the
        // delta below is expressed in, so a change never leaves both.
        if let Some(table) = global.as_table_mut() {
            keys::canonicalize_top_level_keys(table)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
        global
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    // Injected built-ins are runtime state, never configuration: compare the
    // configurations without them. The file itself can only hold user-defined
    // servers, whatever they are named.
    let mut before = before.clone();
    strip_injected_mcp_defaults(&mut before);
    let mut after = after.clone();
    strip_injected_mcp_defaults(&mut after);
    let before = toml::Value::try_from(before).map_err(io::Error::other)?;
    let after = toml::Value::try_from(after).map_err(io::Error::other)?;
    apply_toml_delta(&mut global, &before, &after);

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

/// Drop injected built-in MCP servers (never user-defined ones, whatever
/// their name) so they are neither persisted nor diffed as config changes.
fn strip_injected_mcp_defaults(_cfg: &mut Config) {
    #[cfg(feature = "mcp")]
    if let Some(ref mut servers) = _cfg.mcp_servers {
        servers.retain(|_, server| server.trusted_identity().is_none());
        if servers.is_empty() {
            _cfg.mcp_servers = None;
        }
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
    fn reasoning_preference_cycles_persist_without_copying_effective_project_values() {
        use crate::config::ReasoningDisplay;
        let root = crate::tests::OwnedTestDirectory::new("reasoning-preference-delta");
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "model = 'global-model'\n# preserved comment\nfuture_key = 7\n",
        )
        .unwrap();
        let startup = Config {
            model: Some("project-model".into()),
            chat_left_margin: Some(7),
            ..Config::default()
        };
        let mut current = startup.resolve_reasoning_display();
        for _ in 0..6 {
            let mut before = startup.clone();
            before.reasoning_display = Some(current);
            let mut after = before.clone();
            current = current.next();
            after.reasoning_display = Some(current);
            save_config_changes_at(&path, &before, &after).unwrap();
            let raw = std::fs::read_to_string(&path).unwrap();
            let saved = parse_config_content(&path, &raw).unwrap();
            assert_eq!(saved.resolve_reasoning_display(), current);
            assert_eq!(saved.model.as_deref(), Some("global-model"));
            assert_eq!(saved.chat_left_margin, None);
            let values: toml::Value = toml::from_str(&raw).unwrap();
            assert_eq!(values["future_key"].as_integer(), Some(7));
        }
        assert_eq!(current, ReasoningDisplay::Collapsed);
        // A failed persistence attempt cannot be mistaken for a durable preference.
        assert!(
            save_config_changes_at(
                root.path(),
                &startup,
                &Config {
                    reasoning_display: Some(ReasoningDisplay::Full),
                    ..startup.clone()
                }
            )
            .is_err()
        );
    }

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

    // mini-agent-ydfkp: a user-defined server that happens to share a
    // built-in's name is the user's; only injected built-ins are transient.
    #[cfg(feature = "mcp")]
    #[test]
    fn saving_keeps_user_mcp_servers_named_like_built_ins() {
        for extension in ["toml", "json"] {
            let root = std::env::temp_dir()
                .join(format!("mini-agent-config-mcp-{}", uuid::Uuid::new_v4()));
            let path = root.join(format!("config.{extension}"));
            std::fs::create_dir_all(&root).unwrap();
            let content = if extension == "toml" {
                "enable-context7-mcp = false\nenable-exa-mcp = true\nenable-grepapp-mcp = true\n\n\
                 [mcp_servers.Context7]\nurl = \"https://custom.example.com/mcp\"\n\n\
                 [mcp_servers.\"Exa Web Search\"]\ncommand = \"my-exa\"\n"
                    .to_string()
            } else {
                serde_json::json!({
                    "enable-context7-mcp": false,
                    "enable-exa-mcp": true,
                    "enable-grepapp-mcp": true,
                    "mcp_servers": {
                        "Context7": {"url": "https://custom.example.com/mcp"},
                        "Exa Web Search": {"command": "my-exa"},
                    }
                })
                .to_string()
            };
            std::fs::write(&path, &content).unwrap();

            let mut before = parse_config_content(&path, &content).unwrap();
            // Loading injects built-ins; neither toggle may drop a user entry.
            crate::config::inject_mcp_defaults(&mut before);
            let servers = before.mcp_servers.as_ref().unwrap();
            assert!(servers["Context7"].trusted_identity().is_none());
            assert!(servers["Exa Web Search"].trusted_identity().is_none());
            assert!(
                servers["Grep.app"].trusted_identity().is_some(),
                "an enabled built-in without a user entry is injected"
            );

            let after = Config {
                max_tokens: Some(321),
                ..before.clone()
            };
            save_config_changes_at(&path, &before, &after).unwrap();
            let saved_text = std::fs::read_to_string(&path).unwrap();
            let saved = parse_config_content(&path, &saved_text).unwrap();
            assert_eq!(saved.max_tokens, Some(321), "{extension}");
            let servers = saved.mcp_servers.expect("user servers survive the save");
            assert!(
                servers.contains_key("Context7"),
                "{extension}: {saved_text}"
            );
            assert!(
                servers.contains_key("Exa Web Search"),
                "{extension}: {saved_text}"
            );
            assert!(
                !servers.contains_key("Grep.app"),
                "{extension}: an injected built-in is never persisted: {saved_text}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
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
    fn usd_budget_project_layers_cannot_raise_lower_or_set_global_cap_even_when_trusted() {
        let root = crate::tests::OwnedTestDirectory::new("project-budget-global-only");
        let path = root.path().join("project/.zerostack/config.toml");
        let trust = root
            .path()
            .join("state/config/trusted-project-configs.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for global in [None, Some(0.5)] {
            for project in ["max_budget_usd = 0", "max_budget_usd = 1000"] {
                for interactive in [false, true] {
                    std::fs::write(&path, format!("{project}\nchat_left_margin=7\nyolo=true\n"))
                        .unwrap();
                    let mut cfg = Config {
                        max_budget_usd: global,
                        ..Default::default()
                    };
                    apply_local_override_with_confirmation(
                        &mut cfg,
                        &path,
                        &trust,
                        interactive,
                        &|_| true,
                    )
                    .unwrap();
                    assert_eq!(cfg.max_budget_usd, global);
                    assert_eq!(cfg.chat_left_margin, Some(7));
                }
            }
        }
    }

    #[test]
    fn permission_mode_cycle_is_global_only_even_for_approved_project_config() {
        let root = crate::tests::OwnedTestDirectory::new("project-mode-cycle-global-only");
        let path = root.path().join("project/.zerostack/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "permission_mode_cycle = [\"yolo\"]\nyolo = true\n").unwrap();
        let trust = root
            .path()
            .join("state/config/trusted-project-configs.json");
        let mut cfg = Config {
            permission_mode_cycle: Some(vec!["readonly".into(), "guarded".into()]),
            ..Default::default()
        };
        apply_local_override_with_confirmation(&mut cfg, &path, &trust, true, &|_| true).unwrap();
        assert_eq!(cfg.yolo, Some(true));
        assert_eq!(
            cfg.permission_mode_cycle,
            Some(vec!["readonly".into(), "guarded".into()])
        );
        let (benign, sensitive, keys) =
            split_project_override("permission_mode_cycle = []\n").unwrap();
        assert!(
            benign.as_table().unwrap().is_empty()
                && sensitive.as_table().unwrap().is_empty()
                && keys.is_empty()
        );
    }

    #[test]
    fn project_update_check_cannot_opt_in_to_network_without_trust() {
        let root = crate::tests::OwnedTestDirectory::new("project-update-check-trust");
        let path = root.path().join("project/.zerostack/config.toml");
        let trust = root
            .path()
            .join("state/config/trusted-project-configs.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "update_check = true\n").unwrap();
        let mut cfg = Config::default();
        let outcome =
            apply_local_override_with_confirmation(&mut cfg, &path, &trust, false, &|_| {
                panic!("headless must not prompt")
            })
            .unwrap();
        assert_eq!(outcome, ProjectConfigTrustOutcome::SkippedHeadless);
        assert!(cfg.update_check.is_none());
        apply_local_override_with_confirmation(&mut cfg, &path, &trust, true, &|description| {
            assert!(description.contains("update_check"));
            true
        })
        .unwrap();
        assert_eq!(cfg.update_check, Some(true));
    }

    #[test]
    fn project_shell_deadline_requires_content_bound_trust() {
        let root = crate::tests::OwnedTestDirectory::new("project-shell-deadline-trust");
        let config_path = root.path().join("project/.zerostack/config.toml");
        let trust_path = root
            .path()
            .join("state/config/trusted-project-configs.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "shell_timeout_secs = 600\n").unwrap();
        let mut cfg = Config {
            shell_timeout_secs: Some(45),
            ..Default::default()
        };
        let skipped = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            false,
            &|_| panic!("headless mode must not prompt"),
        )
        .unwrap();
        assert_eq!(skipped, ProjectConfigTrustOutcome::SkippedHeadless);
        assert_eq!(cfg.resolve_shell_timeout_secs(), 45);

        apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            true,
            &|request| {
                assert!(request.contains("shell_timeout_secs"), "{request}");
                true
            },
        )
        .unwrap();
        assert_eq!(cfg.resolve_shell_timeout_secs(), 600);

        std::fs::write(&config_path, "shell_timeout_secs = 120\n").unwrap();
        cfg.shell_timeout_secs = Some(45);
        let changed = apply_local_override_with_confirmation(
            &mut cfg,
            &config_path,
            &trust_path,
            false,
            &|_| panic!("changed headless project must not prompt"),
        )
        .unwrap();
        assert_eq!(changed, ProjectConfigTrustOutcome::SkippedHeadless);
        assert_eq!(cfg.resolve_shell_timeout_secs(), 45);
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
             goal_checks = [\"untrusted-goal-check\"]\n\
             goal_judge_model = \"untrusted-judge\"\n\
             session_title_model = \"untrusted-titler\"\n\
             [goal]\n\
             max_rounds = 9\n\
             judge = \"untrusted-quick-model\"\n\
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
        // Titling a session sends its first exchange to the named endpoint, so
        // an untrusted project cannot choose it (mini-agent-3wsib).
        assert!(cfg.session_title_model.is_none());
        // Goal checks carry the authority of whoever configured them, so an
        // untrusted project config cannot introduce one. The bounds table next
        // to it is benign and does apply: it only limits how long the harness
        // keeps working.
        #[cfg(feature = "goal")]
        {
            assert!(
                cfg.goal_checks.is_none(),
                "an untrusted project config must not supply a goal check"
            );
            assert!(
                cfg.goal_judge_model.is_none(),
                "nor name the model that reviews completion claims"
            );
            // `[goal].judge` is carved out of its own table: choosing which
            // configured endpoint sees the transcript, or switching the review
            // off, is not a bound.
            assert!(
                cfg.goal.as_ref().and_then(|g| g.judge.as_deref()).is_none(),
                "nor choose the judge from inside the bounds table"
            );
            assert_eq!(cfg.goal.as_ref().and_then(|g| g.max_rounds), Some(9));
        }
        #[cfg(feature = "mcp")]
        assert!(cfg.mcp_servers.is_none());
        #[cfg(feature = "lsp")]
        assert!(cfg.lsp.is_none());
        assert!(!trust_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_shell_prefix_authority_requires_trust_for_both_key_spellings() {
        use crate::permission::SecurityMode;
        use crate::permission::checker::{CheckResult, PermissionChecker};

        for key in ["permission-allow-prefix", "permission_allow_prefix"] {
            let fixture = crate::tests::OwnedTestDirectory::new("project-shell-prefix-trust");
            let root = fixture.path().to_path_buf();
            let config_path = root.join("project/.zerostack/config.toml");
            let trust_path = root.join("state/config/trusted-project-configs.json");
            std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            let global = root.join("global/config.toml");
            std::fs::create_dir_all(global.parent().unwrap()).unwrap();
            let base_toml = "permission-allow-prefix = { bash = [\"git status\"] }\n";
            std::fs::write(&global, base_toml).unwrap();
            let local_toml = format!("chat_left_margin = 7\n{key} = {{ bash = [\"git push\"] }}\n");
            std::fs::write(&config_path, &local_toml).unwrap();
            let (_, _, sensitive) = split_project_override(&local_toml).unwrap();
            assert!(sensitive.contains("permission-allow-prefix"));

            let permission = |cfg: &Config, command: &str| {
                PermissionChecker::new(
                    &cfg.build_permission_config().unwrap(),
                    SecurityMode::Standard,
                    Some(root.join("project")),
                    Some(vec!["standard".to_string()]),
                )
                .unwrap()
                .check("shell", command)
            };
            let (headless, first_startup) =
                super::load_from_path(global, Some(&config_path), &trust_path, false);
            assert!(!first_startup);
            assert_eq!(headless.chat_left_margin, Some(7));
            assert_eq!(
                permission(&headless, "git status --short"),
                CheckResult::Allowed
            );
            assert_eq!(
                permission(&headless, "git push origin main"),
                CheckResult::Ask
            );
            assert!(!trust_path.exists());

            let mut declined: Config = toml::from_str(base_toml).unwrap();
            let outcome = apply_local_override_with_confirmation(
                &mut declined,
                &config_path,
                &trust_path,
                true,
                &|description| {
                    assert!(description.contains("permission-allow-prefix"));
                    false
                },
            )
            .unwrap();
            assert_eq!(outcome, ProjectConfigTrustOutcome::Declined);
            assert_eq!(
                permission(&declined, "git push origin main"),
                CheckResult::Ask
            );
            assert!(!trust_path.exists());

            // The ordinary merge accepts an explicitly authorized override;
            // the project loader must make that trust decision first.
            let base: Config = toml::from_str(base_toml).unwrap();
            let merged = super::merge_config_override(&base, &local_toml).unwrap();
            assert_eq!(
                permission(&merged, "git push origin main"),
                CheckResult::Allowed
            );
            let mut approved = base;
            let outcome = apply_local_override_with_confirmation(
                &mut approved,
                &config_path,
                &trust_path,
                true,
                &|_| true,
            )
            .unwrap();
            assert_eq!(outcome, ProjectConfigTrustOutcome::Approved);
            assert_eq!(
                permission(&approved, "git push origin main"),
                CheckResult::Allowed
            );
            assert!(trust_path.exists());
        }
    }

    #[test]
    fn headless_load_clamps_extreme_project_retry_to_the_user_policy() {
        let (root, config_path, trust_path) = fixture("retry-headless");
        let global = root.join("global/config.toml");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(&global, "model = \"global-model\"\n").unwrap();
        // `yolo` makes this an untrusted file with sensitive keys, so the
        // headless path (no prompt, benign subset only) is what runs.
        std::fs::write(
            &config_path,
            "yolo = true\n\
             [retry]\n\
             max_attempts = 4000000000\n\
             initial_backoff_ms = 9000000000000000\n\
             max_backoff_ms = 0\n",
        )
        .unwrap();

        let (cfg, first_startup) =
            super::load_from_path(global.clone(), Some(&config_path), &trust_path, false);

        assert!(!first_startup);
        assert_eq!(cfg.yolo, None, "sensitive key stays inert headless");
        let defaults = crate::retry::RetryConfig::default();
        assert_eq!(cfg.retry.max_attempts, defaults.max_attempts);
        assert_eq!(cfg.retry.max_backoff_ms, 0);
        assert_eq!(cfg.retry.initial_backoff_ms, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_clamps_global_retry_and_lets_project_tighten_within_it() {
        let (root, config_path, trust_path) = fixture("retry-global");
        let global = root.join("global/config.toml");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(
            &global,
            "[retry]\n\
             max_attempts = 50\n\
             initial_backoff_ms = 1000\n\
             max_backoff_ms = 120000\n",
        )
        .unwrap();
        std::fs::write(&config_path, "[retry]\nmax_attempts = 8\n").unwrap();

        let (cfg, _) = super::load_from_path(global, Some(&config_path), &trust_path, false);

        assert_eq!(cfg.retry.max_attempts, 8);
        assert_eq!(cfg.retry.initial_backoff_ms, 1000);
        assert_eq!(cfg.retry.max_backoff_ms, crate::retry::MAX_RETRY_BACKOFF_MS);
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

    /// mini-agent-93gx4: a cloned repository's statusline must not switch on
    /// the background host `git status` refresh without trust.
    #[test]
    fn untrusted_project_statusline_with_git_status_does_not_set_needs_git_status() {
        let (root, config_path, trust_path) = fixture("statusline-git");
        std::fs::write(
            &config_path,
            "chat_left_margin = 5\n\
             [[statusline.lines]]\n\
             segments = [{ item = \"model\" }, { item = \"git_status\" }]\n",
        )
        .unwrap();

        for (interactive, expected) in [
            (false, ProjectConfigTrustOutcome::SkippedHeadless),
            (true, ProjectConfigTrustOutcome::Declined),
        ] {
            let mut cfg = Config::default();
            let outcome = apply_local_override_with_confirmation(
                &mut cfg,
                &config_path,
                &trust_path,
                interactive,
                &|description| {
                    assert!(description.contains("statusline"), "{description}");
                    false
                },
            )
            .unwrap();
            assert_eq!(outcome, expected);
            assert_eq!(cfg.chat_left_margin, Some(5), "benign keys still apply");
            assert!(
                !cfg.statusline
                    .as_ref()
                    .is_some_and(crate::config::StatusLineConfig::needs_git_status),
                "untrusted statusline must not enable the host git refresh"
            );
        }

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
        assert!(
            approved
                .statusline
                .as_ref()
                .is_some_and(crate::config::StatusLineConfig::needs_git_status),
            "a trusted project statusline keeps its git segments"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_statusline_without_git_status_segments_stays_benign() {
        let (_root, _, sensitive) = split_project_override(
            "[[statusline.lines]]\n\
             segments = [{ item = \"model\" }, { item = \"git_branch\" }]\n",
        )
        .unwrap();
        assert!(sensitive.is_empty(), "{sensitive:?}");
        let (_, _, sensitive) = split_project_override(
            "[[statusline.lines]]\n\
             segments = [{ item = \"git_changes\" }]\n",
        )
        .unwrap();
        assert!(sensitive.contains("statusline"), "{sensitive:?}");
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
        std::fs::create_dir_all(first.join(crate::product::LEGACY_PROJECT_DIRECTORY)).unwrap();
        std::fs::create_dir_all(second.join(crate::product::LEGACY_PROJECT_DIRECTORY)).unwrap();
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

#[cfg(test)]
mod config_key_tests {
    use super::{
        load_from_path, merge_config_override, parse_config_content, save_config_changes_at,
        split_project_override,
    };
    use crate::config::Config;
    use std::path::Path;

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mini-agent-config-keys-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn parse_toml(content: &str) -> Config {
        parse_config_content(Path::new("config.toml"), content).unwrap()
    }

    /// Map equality is order-insensitive, unlike serialized text of the
    /// `HashMap`-backed permission tables.
    fn normalized(cfg: &Config) -> serde_json::Value {
        serde_json::to_value(cfg).unwrap()
    }

    /// The snake_case spelling of each kebab-case key (and the kebab spelling
    /// of the snake-case permission keys next to them) configures exactly
    /// what the canonical spelling does, in TOML and YAML alike.
    #[test]
    fn snake_and_kebab_spellings_deserialize_identically() {
        let mut pairs = vec![
            (
                "sandbox-backend = \"seatbelt\"",
                "sandbox_backend = \"seatbelt\"",
            ),
            (
                "permission-deny = { bash = [\"rm **\"], write = [\"/etc/**\"] }",
                "permission_deny = { bash = [\"rm **\"], write = [\"/etc/**\"] }",
            ),
            (
                "permission-modes = [\"guarded\"]",
                "permission_modes = [\"guarded\"]",
            ),
            (
                "permission-allow = { read = [\"src/**\"] }",
                "permission_allow = { read = [\"src/**\"] }",
            ),
            (
                "default_permission_mode = \"readonly\"",
                "default-permission-mode = \"readonly\"",
            ),
            ("allow_all_mcp_calls = false", "allow-all-mcp-calls = false"),
        ];
        let js_pair = (
            "js-fetch-origins = [\"https://docs.rs\"]",
            "js_fetch_origins = [\"https://docs.rs\"]",
        );
        if cfg!(feature = "js") {
            pairs.push(js_pair);
        }
        for (canonical, alias) in pairs {
            let expected = parse_toml(canonical);
            let actual = parse_toml(alias);
            assert!(
                actual.preserved.keys().next().is_none(),
                "`{alias}` must be owned, not preserved"
            );
            assert_eq!(normalized(&actual), normalized(&expected), "{alias}");
        }

        let restricted = parse_toml("sandbox_backend = \"seatbelt\"\npermission_modes = []");
        assert_eq!(restricted.sandbox_backend.as_deref(), Some("seatbelt"));
        assert_eq!(restricted.permission_modes, Some(Vec::new()));
        #[cfg(feature = "js")]
        assert_eq!(
            parse_toml(js_pair.1).js_fetch_origins,
            Some(vec!["https://docs.rs".to_string()])
        );
        // A build without `js` keeps either spelling without owning it and
        // without calling it unknown.
        #[cfg(not(feature = "js"))]
        for spelling in [js_pair.0, js_pair.1] {
            let cfg = parse_toml(spelling);
            let keys: Vec<&str> = cfg.preserved.keys().collect();
            assert!(
                crate::config::keys::unknown_key_warnings("test", keys).is_empty(),
                "{spelling}"
            );
        }

        let yaml = parse_config_content(
            Path::new("config.yaml"),
            "permission_deny:\n  bash: [\"rm **\"]\nsandbox_backend: seatbelt\n",
        )
        .unwrap();
        assert_eq!(
            normalized(&yaml),
            normalized(&parse_toml(
                "permission-deny = { bash = [\"rm **\"] }\nsandbox-backend = \"seatbelt\""
            ))
        );
    }

    #[test]
    fn both_spellings_of_one_key_are_rejected_not_silently_merged() {
        let error = parse_config_content(
            Path::new("config.toml"),
            "permission-deny = { bash = [\"rm **\"] }\npermission_deny = {}\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("line"), "{error}");
        assert!(
            merge_config_override(
                &Config::default(),
                "sandbox-backend = \"bwrap\"\nsandbox_backend = \"seatbelt\""
            )
            .is_err()
        );
    }

    /// Unknown keys warn with the nearest known key; a key owned only by
    /// builds with other Cargo features (here `mcp_servers`, `acp_host` and
    /// the JS keys) never warns, whichever feature row runs this test.
    #[test]
    fn load_warns_once_per_authored_shell_allow_metachar_entry_without_granting_it() {
        let fixture = crate::tests::OwnedTestDirectory::new("shell-allow-warning");
        let root = fixture.path().to_path_buf();
        let global = root.join("config.toml");
        std::fs::write(&global, r#"permission-allow = { bash = ["git status*", "cargo test?", "echo [secret-canary]", "git diff"] }
permission-allow-prefix = { shell = ["git status"] }
"#).unwrap();
        let (cfg, _) = load_from_path(global, None, &root.join("trust.json"), false);
        assert_eq!(cfg.load_warnings.len(), 3);
        for warning in &cfg.load_warnings {
            assert!(warning.contains("exact complete script"));
            assert!(warning.contains("permission-allow-prefix"));
            assert!(!warning.contains("secret-canary"));
        }
        let mut checker = crate::permission::checker::PermissionChecker::new(
            &cfg.build_permission_config().unwrap(),
            crate::permission::SecurityMode::Standard,
            Some(root.clone()),
            None,
        )
        .unwrap();
        assert_eq!(
            checker.check("shell", "cargo test --lib"),
            crate::permission::checker::CheckResult::Ask
        );
        assert_eq!(
            checker.check("shell", "git status --short"),
            crate::permission::checker::CheckResult::Allowed
        );
    }

    #[test]
    fn load_warns_about_unknown_keys_only() {
        let root = scratch("unknown");
        let global = root.join("global/config.toml");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(
            &global,
            "model = \"m\"\n\
             sandbx = true\n\
             acp_host = \"127.0.0.1\"\n\
             js_fetch_origins = [\"https://docs.rs\"]\n\
             enable-exa-mcp = false\n\
             [mcp_servers.audit]\n\
             command = \"printf\"\n",
        )
        .unwrap();
        let project = root.join("project/.zerostack/config.toml");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(
            &project,
            "chat_left_margin = 2\npermision_deny = { bash = [\"rm **\"] }\n",
        )
        .unwrap();
        let trust = root.join("state/trust.json");

        let (cfg, _) = load_from_path(global.clone(), Some(&project), &trust, false);

        assert_eq!(cfg.load_warnings.len(), 2, "{:?}", cfg.load_warnings);
        let global_warning = &cfg.load_warnings[0];
        assert!(global_warning.contains("`sandbx`"), "{global_warning}");
        assert!(global_warning.contains("`sandbox`"), "{global_warning}");
        assert!(global_warning.contains(&global.display().to_string()));
        let project_warning = &cfg.load_warnings[1];
        assert!(
            project_warning.contains("`permision_deny`"),
            "{project_warning}"
        );
        assert!(
            project_warning.contains("`permission-deny`"),
            "{project_warning}"
        );
        for warning in &cfg.load_warnings {
            for other_build_key in ["mcp_servers", "acp_host", "js_fetch_origins", "enable-exa"] {
                assert!(!warning.contains(other_build_key), "{warning}");
            }
        }
        assert_eq!(cfg.chat_left_margin, Some(2));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toml_parse_errors_name_line_and_column_without_echoing_values() {
        let error =
            parse_config_content(Path::new("config.toml"), "model = \"m\"\nmax_tokens = \n")
                .unwrap_err()
                .to_string();
        assert!(error.contains("line 2, column"), "{error}");

        let error = parse_config_content(
            Path::new("config.toml"),
            "model = \"m\"\n\nmax_tokens = \"sk-secret-value\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("line 3, column 14"), "{error}");
        assert!(!error.contains("sk-secret-value"), "{error}");

        let error = split_project_override("a = 1\nb = [\n").unwrap_err();
        assert!(error.contains("line 2"), "{error}");
    }

    /// Project-local configs are classified and merged by canonical key, so a
    /// snake spelling is exactly as sensitive as the kebab one and merges
    /// into the base value instead of colliding with it.
    #[test]
    fn project_aliases_are_classified_and_merged_canonically() {
        let (_, _, sensitive) =
            split_project_override("permission_deny = { bash = [\"rm **\"] }\n").unwrap();
        assert!(sensitive.contains("permission-deny"), "{sensitive:?}");

        let base = parse_toml("permission-deny = { write = [\"/etc/**\"] }");
        let merged =
            merge_config_override(&base, "permission_deny = { bash = [\"rm **\"] }").unwrap();
        let deny = merged.permission_deny.unwrap();
        assert_eq!(deny["bash"], vec!["rm **".to_string()]);
        assert_eq!(deny["write"], vec!["/etc/**".to_string()]);
    }

    #[test]
    fn saving_rewrites_alias_spellings_to_the_canonical_key() {
        let root = scratch("save");
        let path = root.join("config.toml");
        std::fs::write(&path, "sandbox_backend = \"bwrap\"\nmax_tokens = 1\n").unwrap();
        let before = parse_toml(&std::fs::read_to_string(&path).unwrap());
        let after = Config {
            sandbox_backend: Some("seatbelt".to_string()),
            ..before.clone()
        };

        save_config_changes_at(&path, &before, &after).unwrap();

        let raw: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["sandbox-backend"].as_str(), Some("seatbelt"));
        assert!(raw.get("sandbox_backend").is_none());
        assert_eq!(raw["max_tokens"].as_integer(), Some(1));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
pub(crate) mod bounded_project_trust_tests {
    use super::*;

    fn paths(root: &Path) -> AppPaths {
        AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: Some(root.join(".zerostack")),
        }
    }

    #[test]
    fn bounded_trust_preserves_exact_schema_hash_and_sensitive_application() {
        let root = crate::tests::OwnedTestDirectory::new("bounded-trust-positive");
        let paths = paths(root.path());
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(root.path().join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        let content = "model='project-model'\nshell_timeout_secs=9\n";
        std::fs::write(&config, content).unwrap();
        trust_project_config(&config, &paths.project_config_trust_file()).unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        assert!(project_config_is_trusted(
            Some(&config),
            &paths.project_config_trust_file()
        ));
        assert!(project_config_is_trusted_in_workspace(&paths, &workspace));
        let expected = project_config_binding(&config, content).unwrap();
        let stored = load_project_config_trust(&paths.project_config_trust_file()).unwrap();
        assert_eq!(stored.schema, 1);
        assert!(stored.bindings.contains(&expected));
        let mut cfg = Config::default();
        assert_eq!(
            apply_local_override_with_confirmation(
                &mut cfg,
                &config,
                &paths.project_config_trust_file(),
                false,
                &|_| panic!("already trusted must not prompt")
            )
            .unwrap(),
            ProjectConfigTrustOutcome::AlreadyTrusted
        );
        assert_eq!(cfg.model.as_deref(), Some("project-model"));
        assert_eq!(cfg.shell_timeout_secs, Some(9));
        std::fs::write(&config, "model='changed'\nshell_timeout_secs=1\n").unwrap();
        assert!(!project_config_is_trusted_in_workspace(&paths, &workspace));
    }

    #[test]
    fn bounded_config_and_private_state_reject_size_utf8_links_and_keep_mode_repair() {
        let root = crate::tests::OwnedTestDirectory::new("bounded-trust-limits");
        let paths = paths(root.path());
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(root.path().join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, vec![b'x'; MAX_PROJECT_CONFIG_BYTES + 1]).unwrap();
        let mut cfg = Config::default();
        let before = serde_json::to_value(&cfg).unwrap();
        assert!(
            apply_local_override_with_confirmation(
                &mut cfg,
                &config,
                &paths.project_config_trust_file(),
                true,
                &|_| panic!("bad data must not prompt")
            )
            .is_err()
        );
        assert_eq!(serde_json::to_value(&cfg).unwrap(), before);
        std::fs::write(&config, [0xff]).unwrap();
        assert!(read_project_config(&config).is_err());
        let private = paths.config_dir.join("config.toml");
        std::fs::write(&private, "model='owned'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(read_config_content(&private).unwrap(), "model='owned'\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&private, vec![b'x'; MAX_PRIVATE_CONFIG_BYTES + 1]).unwrap();
        assert!(read_config_content(&private).is_err());
        std::fs::write(&private, [0xff]).unwrap();
        assert!(read_config_content(&private).is_err());
        let store = paths.project_config_trust_file();
        crate::fs::ensure_private_directory(store.parent().unwrap()).unwrap();
        std::fs::write(&store, vec![b'x'; MAX_PROJECT_TRUST_BYTES + 1]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(load_project_config_trust(&store).is_err());
        std::fs::write(&store, [0xff]).unwrap();
        assert!(load_project_config_trust(&store).is_err());
        std::fs::write(&store, b"{\"schema\":1,\"bindings\":[]}").unwrap();
        std::fs::hard_link(&store, root.path().join("alias")).unwrap();
        assert!(load_project_config_trust(&store).is_err());
        assert!(crate::fs::open_private_file(&store).is_err());
        std::fs::remove_file(root.path().join("alias")).unwrap();
        assert!(
            load_project_config_trust(&store)
                .unwrap()
                .bindings
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn captured_prompt_and_specialist_consumers_never_rebind_replaced_workspace() {
        let root = crate::tests::OwnedTestDirectory::new("bounded-trust-consumers");
        let workspace_root = root.path().join("workspace");
        std::fs::create_dir(&workspace_root).unwrap();
        let environment = [
            "ZS_CONFIG_DIR",
            "ZS_DATA_DIR",
            "ZS_LOCAL_DATA_DIR",
            "ZS_STATE_DIR",
            "ZS_CACHE_DIR",
            "ZS_CREDENTIALS_DIR",
        ]
        .map(|name| (name, Some(root.path().join(name).into_os_string())));
        let _environment = crate::tests::ScopedProcessEnv::set(&environment);
        let process = crate::paths::process_paths().unwrap();
        crate::paths::prepare_storage_roots(&process).unwrap();
        let paths = process.with_workspace_root(&workspace_root).unwrap();
        std::fs::create_dir_all(workspace_root.join(".zerostack/prompts")).unwrap();
        std::fs::create_dir_all(workspace_root.join(".zerostack/agents")).unwrap();
        std::fs::write(
            paths.project_config_file().unwrap(),
            "default_prompt='code'\n",
        )
        .unwrap();
        std::fs::write(
            workspace_root.join(".zerostack/prompts/owned.md"),
            "%%mode=yolo\nOWNED_PROMPT",
        )
        .unwrap();
        std::fs::write(
            workspace_root.join(".zerostack/agents/owned.md"),
            "---\ndescription: Owned specialist\n---\nOWNED_AGENT",
        )
        .unwrap();
        trust_project_config(
            &paths.project_config_file().unwrap(),
            &paths.project_config_trust_file(),
        )
        .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(&workspace_root).unwrap();
        let prompts = crate::context::prompts::load_for_workspace_binding(&workspace);
        assert_eq!(
            crate::context::prompts::parse_directives(&prompts["owned"]).mode,
            Some("yolo")
        );
        assert!(
            crate::context::agents::load_for_workspace_binding(&workspace).contains_key("owned")
        );
        std::fs::rename(&workspace_root, root.path().join("retained")).unwrap();
        std::fs::create_dir_all(workspace_root.join(".zerostack/prompts")).unwrap();
        std::fs::create_dir_all(workspace_root.join(".zerostack/agents")).unwrap();
        std::fs::write(
            paths.project_config_file().unwrap(),
            "default_prompt='code'\n",
        )
        .unwrap();
        std::fs::write(
            workspace_root.join(".zerostack/prompts/owned.md"),
            "%%mode=yolo\nREPLACEMENT_PROMPT",
        )
        .unwrap();
        std::fs::write(
            workspace_root.join(".zerostack/agents/owned.md"),
            "REPLACEMENT_AGENT",
        )
        .unwrap();
        trust_project_config(
            &paths.project_config_file().unwrap(),
            &paths.project_config_trust_file(),
        )
        .unwrap();
        assert!(!project_config_is_trusted_in_workspace(&paths, &workspace));
        assert!(
            !crate::context::prompts::load_for_workspace_binding(&workspace).contains_key("owned")
        );
        assert!(
            !crate::context::agents::load_for_workspace_binding(&workspace).contains_key("owned")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_project_identity_keeps_benign_keys_without_sensitive_authority() {
        use std::os::unix::ffi::OsStringExt;
        let root = crate::tests::OwnedTestDirectory::new("bounded-project-nonutf8");
        let project = root
            .path()
            .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        std::fs::create_dir_all(project.join(".zerostack")).unwrap();
        let config = project.join(".zerostack/config.toml");
        std::fs::write(&config, "model='benign'\nshell_timeout_secs=9\n").unwrap();
        let mut cfg = Config::default();
        assert_eq!(
            apply_local_override_with_confirmation(
                &mut cfg,
                &config,
                &root.path().join("trust.json"),
                true,
                &|_| panic!("unrepresentable identity must not prompt")
            )
            .unwrap(),
            ProjectConfigTrustOutcome::TrustUnavailable
        );
        assert_eq!(cfg.model.as_deref(), Some("benign"));
        assert_eq!(cfg.shell_timeout_secs, None);
    }

    #[cfg(unix)]
    #[test]
    fn private_read_rejects_parent_and_symlink_replacement_after_capture() {
        let root = crate::tests::OwnedTestDirectory::new("bounded-private-read-race");
        let parent = root.path().join("private");
        crate::fs::ensure_private_directory(&parent).unwrap();
        let path = parent.join("config.toml");
        std::fs::write(&path, "model='selected'\n").unwrap();
        let source = parent.clone();
        let retained = root.path().join("retained");
        let result = crate::fs::with_private_open_test_hook(
            move |stage| {
                if stage == "initial_open" {
                    std::fs::rename(&source, &retained).unwrap();
                    crate::fs::ensure_private_directory(&source).unwrap();
                    std::fs::write(source.join("config.toml"), "model='replacement'\n").unwrap();
                }
            },
            || read_config_content(&path),
        );
        assert!(
            result.is_err(),
            "replacement parent must not supply config bytes"
        );
        let replacement = root.path().join("other.toml");
        std::fs::write(&replacement, "model='external'\n").unwrap();
        let swap = path.clone();
        let result = crate::fs::with_private_open_test_hook(
            move |stage| {
                if stage == "initial_open" {
                    std::fs::remove_file(&swap).unwrap();
                    std::os::unix::fs::symlink(&replacement, &swap).unwrap();
                }
            },
            || read_config_content(&path),
        );
        assert!(result.is_err(), "symlink swap must not supply config bytes");
    }

    /// Called by the existing bounded child fixture. A blocking regression
    /// aborts only its disposable child; the parent always kills and reaps it.
    #[cfg(unix)]
    pub(crate) fn assert_fifo_readers_and_both_private_open_races(
        root: &Path,
        paths: &AppPaths,
        workspace: &crate::paths::WorkspaceBinding,
    ) {
        let config = paths.project_config_file().unwrap();
        assert!(!project_config_is_trusted(
            Some(&config),
            &paths.project_config_trust_file()
        ));
        assert!(project_config_unknown_key_warnings(&config).is_empty());
        let mut cfg = Config::default();
        assert!(
            apply_local_override_with_confirmation(
                &mut cfg,
                &config,
                &paths.project_config_trust_file(),
                true,
                &|_| panic!("FIFO must not prompt")
            )
            .is_err()
        );
        assert!(!project_config_is_trusted_in_workspace(paths, workspace));
        for stage in ["initial_open", "revalidation_open"] {
            let path = root.join(format!("private-race-{stage}"));
            std::fs::write(&path, b"owned private bytes").unwrap();
            let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let marker = changed.clone();
            let swap = path.clone();
            let result = crate::fs::with_private_open_test_hook(
                move |seen| {
                    if seen == stage {
                        eprintln!("PRIVATE_RACE_STAGE_REACHED={stage}");
                        std::fs::remove_file(&swap).unwrap();
                        drop(crate::tests::process_gate::ProcessGate::new(&swap).unwrap());
                        marker.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                },
                || crate::fs::open_private_file(&path),
            );
            assert!(changed.load(std::sync::atomic::Ordering::SeqCst));
            assert!(result.is_err(), "{stage} FIFO must reject without blocking");
        }
        let environment = [
            ("ZS_CONFIG_DIR", &paths.config_dir),
            ("ZS_DATA_DIR", &paths.data_dir),
            ("ZS_LOCAL_DATA_DIR", &paths.local_data_dir),
            ("ZS_STATE_DIR", &paths.state_dir),
            ("ZS_CACHE_DIR", &paths.cache_dir),
            ("ZS_CREDENTIALS_DIR", &paths.credentials_dir),
        ]
        .map(|(name, path)| (name, Some(path.clone().into_os_string())));
        let _environment = crate::tests::ScopedProcessEnv::set(&environment);
        std::fs::create_dir_all(root.join(".zerostack/prompts")).unwrap();
        let prompt = root.join(".zerostack/prompts/fifo.md");
        drop(crate::tests::process_gate::ProcessGate::new(&prompt).unwrap());
        assert!(
            !crate::context::prompts::load_for_workspace_binding(workspace).contains_key("fifo")
        );
        assert!(
            !crate::context::agents::load_for_workspace_binding(workspace).contains_key("fifo")
        );
        let store = paths.project_config_trust_file();
        std::fs::remove_file(&store).unwrap();
        drop(crate::tests::process_gate::ProcessGate::new(&store).unwrap());
        assert!(load_project_config_trust(&store).is_err());
    }
}

#[cfg(feature = "mcp")]
pub(crate) fn read_project_mcp_text(
    workspace: &crate::paths::WorkspaceBinding,
) -> io::Result<String> {
    crate::extras::mcp::project::FileSnapshot::read(workspace, Path::new(".mcp.json")).and_then(
        |snapshot| {
            if !snapshot.exists() {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "project MCP file is absent or empty",
                ))
            } else {
                Ok(snapshot.content)
            }
        },
    )
}

#[cfg(feature = "mcp")]
pub(crate) fn project_mcp_is_trusted(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
    content: &str,
) -> bool {
    project_mcp_is_trusted_at(&paths.project_config_trust_file(), workspace, content)
}

#[cfg(feature = "mcp")]
pub(crate) fn project_mcp_is_trusted_at(
    trust_path: &Path,
    workspace: &crate::paths::WorkspaceBinding,
    content: &str,
) -> bool {
    let Ok(binding) = checked_project_config_binding(workspace, Path::new(".mcp.json"), content)
    else {
        return false;
    };
    let Ok(store) = load_project_config_trust_mode(trust_path, false) else {
        return false;
    };
    workspace.validate().is_ok() && store.bindings.contains(&binding)
}

#[cfg(feature = "mcp")]
pub(crate) fn approve_project_mcp(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
    expected_digest: &str,
) -> io::Result<()> {
    approve_project_mcp_at(
        &paths.project_config_trust_file(),
        workspace,
        expected_digest,
    )
}

#[cfg(feature = "mcp")]
pub(crate) fn approve_project_mcp_at(
    trust_path: &Path,
    workspace: &crate::paths::WorkspaceBinding,
    expected_digest: &str,
) -> io::Result<()> {
    let content = read_project_mcp_text(workspace)?;
    let binding = checked_project_config_binding(workspace, Path::new(".mcp.json"), &content)
        .map_err(io::Error::other)?;
    if binding.config_sha256 != expected_digest {
        return Err(io::Error::other(
            "project MCP definitions changed during approval",
        ));
    }
    let mut store = load_project_config_trust(trust_path)?;
    store.bindings.retain(|old| {
        old.canonical_project != binding.canonical_project
            || old.canonical_config != binding.canonical_config
    });
    store.bindings.push(binding);
    workspace.validate().map_err(io::Error::other)?;
    save_project_config_trust(trust_path, &store)
}

/// Persist one explicit MCP edit as a private structural file delta. Runtime
/// project/default values and unavailable feature keys never get serialized.
#[cfg(feature = "mcp")]
pub(crate) fn save_mcp_server_at(
    path: &Path,
    name: &str,
    server: Option<&crate::extras::mcp::config::McpServerConfig>,
) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("global config has no parent"))?;
    let workspace = crate::paths::WorkspaceBinding::capture(parent).map_err(io::Error::other)?;
    let relative = Path::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("global config has no name"))?,
    );
    let snapshot = crate::extras::mcp::project::FileSnapshot::read_private(&workspace, relative)?;
    if snapshot.exists() {
        // Validate, never repair, the exact existing private global file before
        // trusting it as the basis of a changed-only structural publication.
        let file = crate::fs::open_private_file_checked(path, crate::fs::PrivateFileAccess::Read)?;
        crate::fs::validate_private_file(&file)?;
        parse_config_content(path, &snapshot.content)?;
    }
    let changed = if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
        let mut document: toml::Value = if snapshot.exists() {
            toml::from_str(&snapshot.content).map_err(io::Error::other)?
        } else {
            toml::Value::Table(Default::default())
        };
        let object = document
            .as_table_mut()
            .ok_or_else(|| io::Error::other("global config is not a table"))?;
        // Parsing above validates aliases on a copy. A structural MCP edit
        // must retain every unrelated key's original spelling and value.
        let servers = object
            .entry("mcp_servers")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or_else(|| io::Error::other("mcp_servers is not a table"))?;
        if let Some(server) = server {
            if servers.contains_key(name) {
                return Err(io::Error::other("MCP name already exists in global config"));
            }
            servers.insert(
                name.to_owned(),
                toml::Value::try_from(server).map_err(io::Error::other)?,
            );
        } else if servers.remove(name).is_none() {
            return Err(io::Error::other("MCP name is not in global config"));
        }
        toml::to_string(&document).map_err(io::Error::other)?
    } else {
        let mut document: serde_yaml_ng::Value = if snapshot.exists() {
            serde_yaml_ng::from_str(&snapshot.content).map_err(io::Error::other)?
        } else {
            serde_yaml_ng::Value::Mapping(Default::default())
        };
        let object = document
            .as_mapping_mut()
            .ok_or_else(|| io::Error::other("global config is not an object"))?;
        let servers = object
            .entry(serde_yaml_ng::Value::String("mcp_servers".into()))
            .or_insert_with(|| serde_yaml_ng::Value::Mapping(Default::default()))
            .as_mapping_mut()
            .ok_or_else(|| io::Error::other("mcp_servers is not an object"))?;
        let key = serde_yaml_ng::Value::String(name.into());
        if let Some(server) = server {
            if servers.contains_key(&key) {
                return Err(io::Error::other("MCP name already exists in global config"));
            }
            servers.insert(
                key,
                serde_yaml_ng::to_value(server).map_err(io::Error::other)?,
            );
        } else if servers.remove(&key).is_none() {
            return Err(io::Error::other("MCP name is not in global config"));
        }
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            serde_json::to_string_pretty(&document).map_err(io::Error::other)?
        } else {
            serde_yaml_ng::to_string(&document).map_err(io::Error::other)?
        }
    };
    parse_config_content(path, &changed)?;
    snapshot.publish(&workspace, &changed)?;
    Ok(path.to_owned())
}

#[cfg(all(test, feature = "mcp"))]
mod mcp_delta_tests {
    use super::*;
    #[test]
    fn mcp_add_remove_preserves_unavailable_fields_unknown_nested_keys_and_no_project_copy() {
        let root = crate::tests::OwnedTestDirectory::new("mcp-global-delta");
        for (file, original) in [
            (
                "config.toml",
                "model='original'\nauto_update_prompts=false\nfuture_feature={keep=['unknown', '雪']}\n[mcp_servers.existing]\ncommand='runner'\nfuture_server_key=17\n",
            ),
            (
                "config.json",
                r#"{"model":"original","auto_update_prompts":false,"future_feature":{"keep":["unknown","雪"]},"mcp_servers":{"existing":{"command":"runner","future_server_key":17}}}"#,
            ),
            (
                "config.yaml",
                "model: original\nauto_update_prompts: false\nfuture_feature: {keep: [unknown, 雪]}\nmcp_servers:\n  existing:\n    command: runner\n    future_server_key: 17\n",
            ),
        ] {
            let path = root.path().join(file);
            crate::fs::private_atomic_create_sync(&path, original.as_bytes()).unwrap();
            let server = crate::extras::mcp::config::McpServerConfig::Url {
                url: "http://127.0.0.1:3000/mcp".into(),
                allow_local: true,
                headers: Default::default(),
                oauth: None,
            };
            save_mcp_server_at(&path, "local", Some(&server)).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                text.contains("future_feature")
                    && text.contains("future_server_key")
                    && text.contains("original")
                    && text.contains("unknown")
            );
            let cfg = parse_config_content(&path, &text).unwrap();
            assert!(text.contains("auto_update_prompts"));
            assert!(!text.contains("auto-update-prompts"));
            if file == "config.json" {
                assert!(
                    serde_json::from_str::<serde_json::Value>(&text).is_ok(),
                    "a structural JSON edit must preserve JSON format"
                );
            }
            assert!(cfg.mcp_servers.unwrap().contains_key("local"));
            assert!(save_mcp_server_at(&path, "local", Some(&server)).is_err());
            save_mcp_server_at(&path, "local", None).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("auto_update_prompts"));
            assert!(!text.contains("auto-update-prompts"));
            assert!(!text.contains("127.0.0.1") && text.contains("future_server_key"));
            assert_eq!(
                parse_config_content(&path, &text).unwrap().model.as_deref(),
                Some("original")
            );
            assert!(save_mcp_server_at(&path, "missing", None).is_err());
        }
    }
}

/// Side-effect-free MCP catalogue refresh for an explicit UI operation. It
/// never prompts, writes defaults, starts services, or reloads provider policy.
#[cfg(feature = "mcp")]
pub(crate) fn mcp_config_in_workspace_at(
    workspace: &crate::paths::WorkspaceBinding,
    path: &Path,
) -> io::Result<Config> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("global config has no parent"))?;
    let binding = crate::paths::WorkspaceBinding::capture(parent).map_err(io::Error::other)?;
    let relative = Path::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("global config has no name"))?,
    );
    let snapshot = crate::extras::mcp::project::FileSnapshot::read_private(&binding, relative)?;
    let mut cfg = if snapshot.exists() {
        parse_config_content(path, &snapshot.content)?
    } else {
        Config::default()
    };
    if let Some(servers) = &cfg.mcp_servers {
        for name in servers.keys() {
            cfg.mcp_origins.entry(name.clone()).or_default().push(
                crate::extras::mcp::project::ServerOrigin {
                    source: path.display().to_string(),
                    selected: true,
                    inactive: None,
                    digest: None,
                },
            );
        }
    }
    let paths =
        AppPaths::from_process(Some(workspace.root().to_path_buf())).map_err(io::Error::other)?;
    let trust = paths.project_config_trust_file();
    crate::extras::mcp::project::load_in_workspace(&mut cfg, workspace, &trust, false, &|_| false)
        .map_err(io::Error::other)?;
    match read_project_config_in_workspace(workspace, Path::new(".zerostack/config.toml")) {
        Ok((content, binding)) => {
            let local = parse_project_toml(&content).map_err(io::Error::other)?;
            let trusted = load_project_config_trust_mode(&trust, false)?
                .bindings
                .contains(&binding);
            if trusted {
                cfg = merge_config_value(&cfg, local.clone()).map_err(io::Error::other)?;
            }
            record_project_mcp_origins(
                &mut cfg,
                &local,
                Path::new(".zerostack/config.toml"),
                &binding.config_sha256,
                trusted,
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(io::Error::other(
                "project TOML MCP authority is unavailable",
            ));
        }
    }
    inject_mcp_defaults(&mut cfg);
    workspace.validate().map_err(io::Error::other)?;
    Ok(cfg)
}
