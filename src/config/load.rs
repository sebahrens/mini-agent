use std::collections::HashMap;
use std::path::{Path, PathBuf};

use compact_str::CompactString;

use std::io::{self, Read};

use crate::config::{
    Config, EditSystem, QuickModelConfig, StatusLineConfig, StatusLineLine, StatusLineSegment,
};
#[cfg(feature = "mcp")]
use crate::extras::mcp::config::{McpServerConfig, TrustedMcpServer};
use crate::paths::AppPaths;

/// Write `content` to `path` atomically via temp-file + rename.
pub(crate) fn atomic_config_write(path: &Path, content: &str) -> io::Result<()> {
    crate::fs::private_atomic_write_sync(path, content.as_bytes())
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

pub fn load_with_paths(paths: &AppPaths) -> (Config, bool) {
    let local_config_path = paths.project_config_file();
    load_from_path(
        pick_existing(&paths.config_dir),
        local_config_path.as_deref(),
    )
}

fn load_from_path(path: PathBuf, local_config_path: Option<&Path>) -> (Config, bool) {
    let is_first_startup = !path_entry_exists(&path).unwrap_or_else(|error| {
        eprintln!(
            "error: failed to inspect config path ({}): {}\n\
             Fix the path or remove it to use defaults.",
            path.display(),
            error,
        );
        std::process::exit(1);
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
                eprintln!(
                    "error: failed to create private config file ({}): {}\n\
                     Fix the path or its permissions, then restart.",
                    path.display(),
                    error,
                );
                std::process::exit(1);
            });
        }
        default
    } else {
        let content = read_config_content(&path).unwrap_or_else(|e| {
            eprintln!(
                "error: failed to read config file ({}): {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                e,
            );
            std::process::exit(1);
        });
        parse_config_content(&path, &content).unwrap_or_else(|error| {
            eprintln!(
                "error: {} is not a valid config: {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                error,
            );
            std::process::exit(1);
        })
    };

    tracing::debug!(
        "config loaded from {}: {} quick_models, {} custom_providers",
        path.display(),
        cfg.quick_models.as_ref().map(|m| m.len()).unwrap_or(0),
        cfg.custom_providers.as_ref().map(|m| m.len()).unwrap_or(0),
    );

    if let Some(local_config_path) = local_config_path {
        apply_local_override(&mut cfg, local_config_path);
    }

    #[cfg(feature = "mcp")]
    inject_mcp_defaults(&mut cfg);

    (cfg, is_first_startup)
}

/// Merge `.zerostack/config.toml` over the global config when it exists.
/// The local file is trusted exactly like the global one — it can set any
/// key, including `yolo` or permission rules — so a startup note is printed
/// whenever an override is applied.
fn apply_local_override(cfg: &mut Config, path: &Path) {
    if !path.exists() {
        return;
    }
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!(
            "error: failed to read project config ({}): {}",
            path.display(),
            e,
        );
        std::process::exit(1);
    });
    match merge_config_override(cfg, &content) {
        Ok(merged) => {
            tracing::info!(
                "applied project-local config override from {}",
                path.display()
            );
            eprintln!(
                "note: applied project-local config override from {}",
                path.display()
            );
            *cfg = merged;
        }
        Err(e) => {
            eprintln!(
                "error: {} is not a valid config: {}\n\
                 Fix the file or remove it to use defaults.",
                path.display(),
                e,
            );
            std::process::exit(1);
        }
    }
}

/// Merge a project-local TOML config fragment over `base`: keys present in
/// `local_toml` win, tables (`quick_models`, `mcp_servers`, ...) merge per
/// key, scalars and arrays replace, and absent keys keep the base value.
pub fn merge_config_override(base: &Config, local_toml: &str) -> Result<Config, String> {
    let local: toml::Value =
        toml::from_str(local_toml).map_err(|_| "project config is not valid TOML".to_string())?;
    // `Config` skips `None` fields when serializing, so the base TOML holds
    // exactly the keys that are set and the local TOML exactly the keys the
    // project file sets.
    let mut base_toml = toml::Value::try_from(base)
        .map_err(|_| "base config could not be normalized".to_string())?;
    deep_merge_toml(&mut base_toml, local);
    base_toml
        .try_into()
        .map_err(|_| "merged project config has an invalid value".to_string())
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

pub fn save_config(cfg: &Config) -> io::Result<()> {
    #[cfg_attr(not(feature = "mcp"), allow(unused_mut))]
    let mut cfg = cfg.clone();
    #[cfg(feature = "mcp")]
    {
        if let Some(ref mut servers) = cfg.mcp_servers {
            servers.remove("Exa Web Search");
            servers.remove("Context7");
            servers.remove("Grep.app");
        }
    }
    let path = resolve_config_path();
    atomic_config_write(&path, &serialize_config_content(&path, &cfg)?)?;
    tracing::debug!("config saved to {}", path.display());
    Ok(())
}
