use crate::config::Config;
use crate::config::types::CustomProviderConfig;
use compact_str::CompactString;
use std::collections::HashMap;

fn custom_provider(provider_type: &str) -> CustomProviderConfig {
    CustomProviderConfig {
        provider_type: CompactString::new(provider_type),
        base_url: "https://gateway.example.com".to_string(),
        api_key_env: None,
        danger_accept_invalid_certs: None,
        api_style: None,
        headers: HashMap::new(),
        timeout_secs: None,
        model: None,
    }
}

#[test]
fn is_anthropic_native_builtin_providers() {
    let cfg = Config::default();
    assert!(cfg.is_anthropic_native("anthropic"));
    assert!(cfg.is_anthropic_native("Anthropic")); // case-insensitive
    for p in ["openai", "gemini", "google", "openrouter", "ollama"] {
        assert!(!cfg.is_anthropic_native(p), "{p} is not anthropic-native");
    }
}

#[test]
fn is_anthropic_native_resolves_custom_provider_type() {
    // A custom gateway named anything but routing through the Anthropic-native
    // protocol must be treated as anthropic-native (so cache fields are added),
    // while an OpenAI-style gateway must not.
    let mut providers = HashMap::new();
    providers.insert("my-claude-proxy".to_string(), custom_provider("anthropic"));
    providers.insert("my-oai-gateway".to_string(), custom_provider("openai"));
    let cfg = Config {
        custom_providers: Some(providers),
        ..Config::default()
    };
    assert!(cfg.is_anthropic_native("my-claude-proxy"));
    assert!(!cfg.is_anthropic_native("my-oai-gateway"));
    // Unknown name with no custom entry falls back to the literal kind.
    assert!(!cfg.is_anthropic_native("totally-unknown"));
}

#[test]
fn mid_turn_threshold_unset_by_default() {
    let cfg = Config::default();
    assert_eq!(cfg.resolve_mid_turn_compact_threshold(), None);
}

#[test]
fn mid_turn_threshold_valid_value_passes_through() {
    let cfg = Config {
        mid_turn_compact_threshold: Some(0.80),
        ..Config::default()
    };
    assert_eq!(cfg.resolve_mid_turn_compact_threshold(), Some(0.80));
}

#[test]
fn mid_turn_threshold_upper_bound_inclusive() {
    let cfg = Config {
        mid_turn_compact_threshold: Some(1.0),
        ..Config::default()
    };
    assert_eq!(cfg.resolve_mid_turn_compact_threshold(), Some(1.0));
}

#[test]
fn mid_turn_threshold_out_of_range_treated_as_unset() {
    // Zero would compact constantly; negatives and >1 are nonsense. All map to
    // "unset" so a misconfigured value silently disables the feature rather
    // than wedging the agent.
    for bad in [0.0, -0.1, 1.5, 2.0] {
        let cfg = Config {
            mid_turn_compact_threshold: Some(bad),
            ..Config::default()
        };
        assert_eq!(
            cfg.resolve_mid_turn_compact_threshold(),
            None,
            "threshold {bad} should be treated as unset"
        );
    }
}

#[test]
fn compact_enabled_default_false() {
    assert!(!Config::default().resolve_compact_enabled());
}

#[test]
fn show_reasoning_defaults_off() {
    assert!(!Config::default().resolve_show_reasoning());
}

#[test]
fn show_reasoning_can_be_disabled() {
    let cfg = Config {
        show_reasoning: Some(false),
        ..Config::default()
    };
    assert!(!cfg.resolve_show_reasoning());
}

#[test]
fn context_exhausted_report_math() {
    // window 20000, threshold 0.80 -> ceiling 16000.
    // prompt 18000 -> 90% of window, overflow 18000 - 16000 = 2000.
    let lines = crate::ui::context_exhausted_report(18_000, 0.80, 20_000, 8_192, 6_000);
    let joined = lines.join("\n");
    assert!(
        joined.contains("context window .............. 20000 tokens"),
        "{joined}"
    );
    assert!(joined.contains("16000 tokens  (80% of window)"), "{joined}");
    assert!(joined.contains("18000 tokens  (90% of window)"), "{joined}");
    assert!(
        joined.contains("overflow above ceiling ...... 2000 tokens"),
        "{joined}"
    );
    assert!(
        joined.contains("reserved for response ....... 8192 tokens"),
        "{joined}"
    );
    assert!(
        joined.contains("kept-recent budget .......... 6000 tokens"),
        "{joined}"
    );
    // Guidance references the actual pressure and the floor the KV cache must hold.
    assert!(
        joined.contains("raise mid_turn_compact_threshold above 90%"),
        "{joined}"
    );
    assert!(joined.contains("hold 18000+ tokens"), "{joined}");
}

#[test]
fn catalog_context_window_reads_known_model() {
    // deepseek-v4-pro is a 1M-context model in the baked openrouter catalog.
    assert_eq!(
        Config::catalog_context_window("openrouter", "deepseek/deepseek-v4-pro"),
        Some(1_048_576)
    );
}

#[test]
fn catalog_context_window_none_for_unknown() {
    assert!(Config::catalog_context_window("openrouter", "no/such-model").is_none());
    // Providers without a baked catalog (custom gateways, ollama) return None.
    assert!(Config::catalog_context_window("ollama", "llama3.1").is_none());
}

#[test]
fn resolve_context_window_prefers_config_pin_over_catalog() {
    let cfg: Config = serde_json::from_str(r#"{ "context_window": 128000 }"#).unwrap();
    let qm = std::collections::HashMap::new();
    assert_eq!(
        cfg.resolve_context_window("openrouter", "deepseek/deepseek-v4-pro", &qm),
        128_000
    );
    // Without a pin, the catalog's 1M wins.
    let cfg = Config::default();
    assert_eq!(
        cfg.resolve_context_window("openrouter", "deepseek/deepseek-v4-pro", &qm),
        1_048_576
    );
}

#[test]
fn resolve_context_window_from_quick_model() {
    let mut qm = std::collections::HashMap::new();
    qm.insert(
        "test".to_string(),
        crate::config::types::QuickModelConfig {
            provider: compact_str::CompactString::new("openrouter"),
            model: compact_str::CompactString::new("deepseek/deepseek-chat"),
            input_token_cost: 0.0,
            output_token_cost: 0.0,
            reserve_tokens: None,
            temperature: None,
            extra_body: None,
            context_window: Some(64_000),
        },
    );
    let cfg = Config::default();
    // Quick model's 64k wins over the catalog's 128k for deepseek-chat.
    assert_eq!(
        cfg.resolve_context_window("openrouter", "deepseek/deepseek-chat", &qm),
        64_000
    );
    // Global config pin still wins over quick model.
    let cfg: Config = serde_json::from_str(r#"{ "context_window": 32000 }"#).unwrap();
    assert_eq!(
        cfg.resolve_context_window("openrouter", "deepseek/deepseek-chat", &qm),
        32_000
    );
    // Quick model with context_window: None falls through to catalog (128k).
    qm.get_mut("test").unwrap().context_window = None;
    let cfg = Config::default();
    let cw = cfg.resolve_context_window("openrouter", "deepseek/deepseek-chat", &qm);
    assert_eq!(cw, 128_000);
}

// ── YAML config reader (replaces the former JSON reader) ───────────────
//
// The on-disk config may be TOML or YAML. YAML is a strict superset of JSON,
// so legacy `config.json` files parse transparently through the YAML reader.
// These tests pin that contract: YAML parsing, the JSON-superset guarantee,
// round-tripping of `serde_json::Value` fields (extra_body / permission), and
// the filename resolution priority.

#[test]
fn yaml_reader_parses_config() {
    let yaml = r#"provider: openrouter
model: deepseek/deepseek-v4-flash
max_tokens: 16384
temperature: 0.7
context_window: 128000
compact_enabled: true
default_prompt: code
show_tool_details: 3
permission-modes: ["guarded", "standard", "yolo"]
mid_turn_compact_threshold: 0.80
quick_models:
  fast:
    provider: openai
    model: gpt-4o-mini
custom_providers:
  local-vllm:
    provider_type: openai
    base_url: http://localhost:8000/v1
    api_key_env: VLLM_API_KEY
permission:
  '*': ask
  read: allow
  bash:
    'cargo test': allow
    'rm **': deny
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.provider.as_deref(), Some("openrouter"));
    assert_eq!(cfg.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
    assert_eq!(cfg.max_tokens, Some(16384));
    assert_eq!(cfg.temperature, Some(0.7));
    assert_eq!(cfg.context_window, Some(128000));
    assert_eq!(cfg.compact_enabled, Some(true));
    assert_eq!(cfg.default_prompt.as_deref(), Some("code"));
    assert_eq!(cfg.mid_turn_compact_threshold, Some(0.80));
    match cfg.show_tool_details {
        Some(crate::config::ShowToolDetails::Lines(3)) => {}
        other => panic!("unexpected show_tool_details: {other:?}"),
    }
    assert_eq!(
        cfg.permission_modes.as_deref(),
        Some(
            &[
                "guarded".to_string(),
                "standard".to_string(),
                "yolo".to_string()
            ][..]
        )
    );
    let qm = cfg.quick_models.expect("quick_models");
    let fast = qm.get("fast").expect("fast model");
    assert_eq!(fast.provider.as_str(), "openai");
    assert_eq!(fast.model.as_str(), "gpt-4o-mini");
    let cps = cfg.custom_providers.expect("custom_providers");
    let vllm = cps.get("local-vllm").expect("local-vllm provider");
    assert_eq!(vllm.base_url, "http://localhost:8000/v1");
    assert_eq!(vllm.api_key_env.as_deref(), Some("VLLM_API_KEY"));
    assert_eq!(
        cfg.permission,
        Some(serde_json::json!({
            "*": "ask",
            "read": "allow",
            "bash": { "cargo test": "allow", "rm **": "deny" }
        }))
    );
}

#[test]
fn yaml_reader_accepts_plain_json_superset() {
    // YAML is a superset of JSON: a plain JSON config must parse through the
    // YAML reader identically to the equivalent YAML.
    let json = r#"{
      "provider": "openrouter",
      "model": "deepseek/deepseek-v4-flash",
      "max_tokens": 16384,
      "compact_enabled": true,
      "quick_models": {
        "fast": { "provider": "openai", "model": "gpt-4o-mini" }
      },
      "permission": { "*": "ask", "read": "allow" }
    }"#;
    let from_json: Config = serde_yaml_ng::from_str(json).unwrap();

    let yaml = r#"provider: openrouter
model: deepseek/deepseek-v4-flash
max_tokens: 16384
compact_enabled: true
quick_models:
  fast:
    provider: openai
    model: gpt-4o-mini
permission:
  '*': ask
  read: allow
"#;
    let from_yaml: Config = serde_yaml_ng::from_str(yaml).unwrap();

    assert_eq!(from_json.provider, from_yaml.provider);
    assert_eq!(from_json.model, from_yaml.model);
    assert_eq!(from_json.max_tokens, from_yaml.max_tokens);
    assert_eq!(from_json.compact_enabled, from_yaml.compact_enabled);
    let jf = from_json
        .quick_models
        .as_ref()
        .and_then(|m| m.get("fast"))
        .expect("json fast model");
    let yf = from_yaml
        .quick_models
        .as_ref()
        .and_then(|m| m.get("fast"))
        .expect("yaml fast model");
    assert_eq!(jf.provider.as_str(), yf.provider.as_str());
    assert_eq!(jf.model.as_str(), yf.model.as_str());
    assert_eq!(from_json.permission, from_yaml.permission);
}

#[test]
fn yaml_round_trips_serde_json_value_fields() {
    // `extra_body` and `permission` are typed as `serde_json::Value`; ensure
    // they survive a YAML serialize/deserialize round trip intact.
    let cfg = Config {
        provider: Some(CompactString::new("openrouter")),
        extra_body: Some(serde_json::json!({ "plugins": { "preset": "quality" } })),
        permission: Some(serde_json::json!({ "*": "ask", "read": "allow" })),
        ..Config::default()
    };
    let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
    let back: Config = serde_yaml_ng::from_str(&yaml).unwrap();
    assert_eq!(back.provider, cfg.provider);
    assert_eq!(back.extra_body, cfg.extra_body);
    assert_eq!(back.permission, cfg.permission);
}

#[test]
fn malformed_permission_objects_report_the_field_and_tool_path() {
    let cfg = Config {
        permission: Some(serde_json::json!({
            "read": {"src/**": ["allow"]}
        })),
        ..Config::default()
    };

    let error = cfg.build_permission_config().unwrap_err().to_string();
    assert!(error.contains("permission"), "{error}");
    assert!(error.contains("read"), "{error}");
    assert!(error.contains("src/**"), "{error}");
}

#[test]
fn malformed_external_directory_objects_report_the_pattern_path() {
    let cfg = Config {
        permission: Some(serde_json::json!({
            "external_directory": {"/private/**": ["allow"]}
        })),
        ..Config::default()
    };

    let error = cfg.build_permission_config().unwrap_err().to_string();
    assert!(error.contains("permission"), "{error}");
    assert!(error.contains("external_directory"), "{error}");
    assert!(error.contains("/private/**"), "{error}");
}

#[test]
fn unknown_permission_tool_is_rejected_instead_of_discarded() {
    for field in ["permission", "permission-regex"] {
        let mut cfg = Config::default();
        let malformed = serde_json::json!({
            "writ": {"secrets/**": "deny"}
        });
        if field == "permission" {
            cfg.permission = Some(malformed);
        } else {
            cfg.permission_regex = Some(malformed);
        }

        let error = cfg.build_permission_config().unwrap_err().to_string();
        assert!(error.contains(field), "{error}");
        assert!(error.contains("writ"), "{error}");
        assert!(error.contains("unsupported permission tool"), "{error}");
    }
}

#[test]
fn yaml_round_trips_scalar_and_nested_fields() {
    let cfg = Config {
        provider: Some(CompactString::new("openrouter")),
        model: Some(CompactString::new("deepseek/deepseek-v4-flash")),
        max_tokens: Some(16384),
        context_window: Some(128000),
        compact_enabled: Some(true),
        default_prompt: Some(CompactString::new("code")),
        ..Config::default()
    };
    let yaml = serde_yaml_ng::to_string(&cfg).unwrap();
    // The emitter produces block-style YAML, not JSON flow braces.
    assert!(!yaml.trim_start().starts_with('{'));
    let back: Config = serde_yaml_ng::from_str(&yaml).unwrap();
    assert_eq!(back.provider, cfg.provider);
    assert_eq!(back.model, cfg.model);
    assert_eq!(back.max_tokens, cfg.max_tokens);
    assert_eq!(back.context_window, cfg.context_window);
    assert_eq!(back.compact_enabled, cfg.compact_enabled);
    assert_eq!(back.default_prompt, cfg.default_prompt);
}

#[test]
fn config_cross_feature_round_trip() {
    use std::path::Path;

    use crate::config::load::{parse_config_content, serialize_config_content};

    let fixture = r#"
model = "original-model"
temperature = 0.7
future_scalar = "keep-me"
future_integer = 9223372036854775000
future_datetime = 2026-07-30T12:34:56Z
future_array = [1, "two", true]
enable-exa-mcp = false
enable-context7-mcp = true
enable-grepapp-mcp = false
wt-auto-merge = true
wt-base-dir = "/tmp/worktrees"
wt-force = true
task_max_turns = 17
task_max_prompts = 5
task_max_concurrency = 2
task_max_output_bytes = 4096
task_max_cost_units = 12345
task_timeout_secs = 90
task_enabled = true
subagent_model = "sub-model"
subagent_provider = "sub-provider"
acp_host = "127.0.0.1"
acp_port = 7243

[future_table]
nested = { flag = true, values = [3, 4] }

[mcp_servers.audit]
command = "printf"
args = ["ready"]
env = { MODE = "audit" }

[acp_servers.worker]
type = "stdio"

[lsp]
enabled = true

[lsp.servers.rust]
command = "rust-analyzer"
args = []
extensions = [".rs"]
env = {}
inherit_env = []
network = "inherit"
disabled = false

[advisor]
enabled = true
model = "advisor-model"
max_uses = 4
human_handoff = false
advisor_kilobytes_limit = 128
"#;
    let path = Path::new("config.toml");
    let before: toml::Value = toml::from_str(fixture).unwrap();
    let mut cfg = parse_config_content(path, fixture).unwrap();

    cfg.model = Some(CompactString::new("updated-model"));
    cfg.temperature = None;

    let serialized = serialize_config_content(path, &cfg).unwrap();
    let after: toml::Value = toml::from_str(&serialized).unwrap();

    assert_eq!(
        after.get("model").and_then(toml::Value::as_str),
        Some("updated-model")
    );
    assert!(
        after.get("temperature").is_none(),
        "deleting an owned field must not resurrect its preserved input value"
    );
    for key in [
        "future_scalar",
        "future_integer",
        "future_datetime",
        "future_array",
        "future_table",
        "enable-exa-mcp",
        "enable-context7-mcp",
        "enable-grepapp-mcp",
        "wt-auto-merge",
        "wt-base-dir",
        "wt-force",
        "task_max_turns",
        "task_max_prompts",
        "task_max_concurrency",
        "task_max_output_bytes",
        "task_max_cost_units",
        "task_timeout_secs",
        "task_enabled",
        "subagent_model",
        "subagent_provider",
        "mcp_servers",
        "acp_servers",
        "acp_host",
        "acp_port",
        "lsp",
        "advisor",
    ] {
        assert_eq!(
            after.get(key),
            before.get(key),
            "cross-feature value changed for {key}"
        );
    }

    let reloaded = parse_config_content(path, &serialized).unwrap();
    assert_eq!(reloaded.model.as_deref(), Some("updated-model"));
    assert_eq!(reloaded.temperature, None);
}

#[test]
fn malformed_owned_config_field_still_fails_closed() {
    use std::path::Path;

    let error = crate::config::load::parse_config_content(
        Path::new("config.toml"),
        "max_tokens = \"many\"",
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

// `pick_existing` is pure (no env/global state), so this priority test is
// hermetic and safe to run in parallel with everything else.
#[test]
fn config_candidate_priority_toml_yaml_yml_legacy_json() {
    use crate::config::load::pick_existing;

    let dir = std::env::temp_dir().join(format!("zs_cfgtest_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let name = |p: std::path::PathBuf| p.file_name().unwrap().to_str().unwrap().to_string();

    // Nothing exists yet -> defaults to the preferred config.toml path.
    assert_eq!(name(pick_existing(&dir)), "config.toml");

    // Legacy config.json is still discovered (parsed via the YAML reader, since
    // YAML is a superset of JSON).
    std::fs::write(dir.join("config.json"), "{}").unwrap();
    assert_eq!(name(pick_existing(&dir)), "config.json");

    // .yml outranks legacy .json.
    std::fs::write(dir.join("config.yml"), "").unwrap();
    assert_eq!(name(pick_existing(&dir)), "config.yml");
    let _ = std::fs::remove_file(dir.join("config.yml"));

    // .yaml outranks legacy .json.
    std::fs::write(dir.join("config.yaml"), "").unwrap();
    assert_eq!(name(pick_existing(&dir)), "config.yaml");

    // .yaml also outranks .yml when both exist.
    std::fs::write(dir.join("config.yml"), "").unwrap();
    assert_eq!(name(pick_existing(&dir)), "config.yaml");

    // .toml outranks every other candidate.
    std::fs::write(dir.join("config.toml"), "").unwrap();
    assert_eq!(name(pick_existing(&dir)), "config.toml");

    let _ = std::fs::remove_dir_all(&dir);
}

use crate::config::merge_config_override;

#[test]
fn local_override_replaces_scalar() {
    let base = Config {
        model: Some(CompactString::new("global-model")),
        ..Config::default()
    };
    let merged = merge_config_override(&base, "model = \"local-model\"").unwrap();
    assert_eq!(merged.model.as_deref(), Some("local-model"));
}

#[test]
fn local_override_keeps_unset_keys() {
    let base = Config {
        model: Some(CompactString::new("global-model")),
        temperature: Some(0.7),
        ..Config::default()
    };
    let merged = merge_config_override(&base, "temperature = 0.2").unwrap();
    assert_eq!(merged.model.as_deref(), Some("global-model"));
    assert_eq!(merged.temperature, Some(0.2));
}

#[test]
fn local_override_merges_maps_per_key() {
    let mut keys = HashMap::new();
    keys.insert("openai".to_string(), "sk-global".to_string());
    keys.insert("gemini".to_string(), "gm-global".to_string());
    let base = Config {
        api_keys: Some(keys),
        ..Config::default()
    };
    let local = r#"
[api_keys]
gemini = "gm-local"
anthropic = "sk-ant-local"
"#;
    let merged = merge_config_override(&base, local).unwrap();
    let keys = merged.api_keys.unwrap();
    // Untouched key kept, existing key replaced, new key added.
    assert_eq!(keys.get("openai").map(String::as_str), Some("sk-global"));
    assert_eq!(keys.get("gemini").map(String::as_str), Some("gm-local"));
    assert_eq!(
        keys.get("anthropic").map(String::as_str),
        Some("sk-ant-local")
    );
}

#[test]
fn local_override_arrays_replace() {
    let base = Config {
        permission_modes: Some(vec!["a".to_string(), "b".to_string()]),
        ..Config::default()
    };
    let merged = merge_config_override(&base, "permission-modes = [\"c\"]").unwrap();
    assert_eq!(merged.permission_modes, Some(vec!["c".to_string()]));
}

#[test]
fn local_override_partial_retry() {
    // `retry` is a non-Option struct: a local `[retry]` table must merge per
    // key over the (default-filled) base rather than replace it wholesale.
    let base = Config::default();
    let merged = merge_config_override(&base, "[retry]\nmax_attempts = 9").unwrap();
    assert_eq!(merged.retry.max_attempts, 9);
    assert_eq!(merged.retry.initial_backoff_ms, 500);
    assert_eq!(merged.retry.max_backoff_ms, 10_000);
}

#[test]
fn local_override_empty_keeps_base() {
    let base = Config {
        model: Some(CompactString::new("global-model")),
        ..Config::default()
    };
    let merged = merge_config_override(&base, "").unwrap();
    assert_eq!(merged.model.as_deref(), Some("global-model"));
}

#[test]
fn local_override_invalid_toml_errors() {
    assert!(merge_config_override(&Config::default(), "not [valid").is_err());
}

#[test]
fn local_override_wrong_type_errors() {
    let err = merge_config_override(&Config::default(), "max_tokens = \"lots\"").unwrap_err();
    assert!(!err.is_empty());
}

#[cfg(feature = "mcp")]
#[test]
fn local_override_merges_mcp_servers() {
    use crate::extras::mcp::config::McpServerConfig;
    let mut servers = HashMap::new();
    servers.insert(
        "global-srv".to_string(),
        McpServerConfig::Url {
            url: "https://global.example.com/mcp".to_string(),
            headers: HashMap::new(),
            oauth: None,
        },
    );
    let base = Config {
        mcp_servers: Some(servers),
        ..Config::default()
    };
    let local = r#"
[mcp_servers.local-fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
"#;
    let merged = merge_config_override(&base, local).unwrap();
    let servers = merged.mcp_servers.unwrap();
    assert_eq!(servers.len(), 2);
    assert!(matches!(
        servers.get("global-srv"),
        Some(McpServerConfig::Url { .. })
    ));
    assert!(matches!(
        servers.get("local-fs"),
        Some(McpServerConfig::Command { .. })
    ));
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_read_only_exemption_trust_is_not_deserialized_or_inferred_from_endpoint() {
    use crate::extras::mcp::config::{McpServerConfig, TrustedMcpServer};

    let custom: McpServerConfig =
        serde_json::from_str(r#"{"url":"https://mcp.context7.com/mcp","headers":{}}"#).unwrap();
    assert_eq!(custom.trusted_identity(), None);

    let built_in = McpServerConfig::built_in(TrustedMcpServer::CONTEXT7, HashMap::new());
    assert_eq!(
        built_in.trusted_identity(),
        Some(TrustedMcpServer::CONTEXT7)
    );
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_read_only_exemption_custom_named_like_builtin_stays_untrusted() {
    use crate::extras::mcp::config::McpServerConfig;

    let custom: McpServerConfig =
        serde_json::from_str(r#"{"url":"https://custom.example.com/mcp"}"#).unwrap();
    let mut cfg = Config {
        enable_exa_mcp: Some(false),
        enable_context7_mcp: Some(true),
        enable_grepapp_mcp: Some(false),
        mcp_servers: Some(HashMap::from([("Context7".to_string(), custom)])),
        ..Config::default()
    };

    crate::config::inject_mcp_defaults(&mut cfg);

    let context7 = cfg.mcp_servers.unwrap().remove("Context7").unwrap();
    assert_eq!(context7.trusted_identity(), None);
}

// --- default_permission_mode validation (mini-agent-dobf) ---

mod default_permission_mode {
    use crate::cli::Cli;
    use crate::config::Config;
    use crate::permission::{SecurityMode, resolve_execution_authority};
    use crate::sandbox::SandboxPolicy;

    fn resolve(mode: &str) -> Result<SecurityMode, String> {
        let cfg = Config {
            default_permission_mode: Some(mode.to_string()),
            ..Config::default()
        };
        resolve_execution_authority(&Cli::default(), &cfg, SandboxPolicy::Disabled, "unused")
            .map(|authority| authority.mode)
            .map_err(|error| error.to_string())
    }

    #[test]
    fn every_documented_mode_is_accepted() {
        assert_eq!(resolve("standard"), Ok(SecurityMode::Standard));
        assert_eq!(resolve("accept"), Ok(SecurityMode::Standard));
        assert_eq!(resolve("restrictive"), Ok(SecurityMode::Restrictive));
        assert_eq!(resolve("readonly"), Ok(SecurityMode::ReadOnly));
        assert_eq!(resolve("planwrite"), Ok(SecurityMode::PlanWrite));
        assert_eq!(resolve("guarded"), Ok(SecurityMode::Guarded));
        assert_eq!(resolve("yolo"), Ok(SecurityMode::Yolo));
    }

    #[test]
    fn unknown_mode_is_rejected_with_the_accepted_values() {
        let error = resolve("bogus").expect_err("unknown modes must not degrade to standard");
        assert!(error.contains("default_permission_mode"), "{error}");
        assert!(error.contains("bogus"), "{error}");
        for accepted in [
            "standard",
            "restrictive",
            "readonly",
            "planwrite",
            "guarded",
            "yolo",
        ] {
            assert!(error.contains(accepted), "{error} lacks {accepted}");
        }
    }

    #[test]
    fn explicit_mode_flags_still_outrank_an_invalid_default() {
        // A CLI flag selects the mode outright; the config value is still
        // validated so a typo never lingers unnoticed.
        let cfg = Config {
            default_permission_mode: Some("bogus".to_string()),
            ..Config::default()
        };
        let cli = Cli {
            guarded: true,
            ..Cli::default()
        };
        assert!(
            resolve_execution_authority(&cli, &cfg, SandboxPolicy::Disabled, "unused").is_err()
        );
    }
}
