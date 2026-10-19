//! Wiring of the documented designer -> implementer -> reviewer pipeline
//! (`docs/agent/SUBAGENTS.md`, "Designer -> implementer -> reviewer
//! pipeline"). The config and personas come from
//! `tests/fixtures/persona-pipeline/`; nothing here touches the network.
//!
//! A full three-stage run with mock completion models is not covered:
//! `AnyClient` is a closed enum of real provider clients, so a per-provider
//! mock would need a new production seam.

use std::collections::HashMap;
use std::ffi::OsString;

use super::{ResolvedSpecialization, resolve_persona_runtime, resolve_specialization};
use crate::context::agents::AgentTool;
use crate::provider::AnyClient;

const CONFIG: &str = include_str!("../../../../tests/fixtures/persona-pipeline/config.toml");
const DESIGNER: &str =
    include_str!("../../../../tests/fixtures/persona-pipeline/agents/designer.md");
const IMPLEMENTER: &str =
    include_str!("../../../../tests/fixtures/persona-pipeline/agents/implementer.md");
const REVIEWER: &str =
    include_str!("../../../../tests/fixtures/persona-pipeline/agents/reviewer.md");

const VENDOR_OPENAI_KEY: &str = "sk-vendor-openai-must-never-reach-glm";
const GLM_KEY: &str = "glm-gateway-key";

fn fixture_config() -> crate::config::Config {
    toml::from_str(CONFIG).expect("pipeline fixture config parses")
}

/// Isolated persona directories plus process env: the pipeline's own key
/// variables are set, and a real-looking vendor OpenAI key sits beside them so
/// any leak into the isolated `glm` provider would be observable.
struct Pipeline {
    _root: crate::tests::OwnedTestDirectory,
    _env: crate::tests::ScopedProcessEnv,
    workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
}

impl Pipeline {
    fn new(label: &str, glm_key: Option<&str>) -> Self {
        let root = crate::tests::OwnedTestDirectory::new(label);
        let env = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            ("ANTHROPIC_API_KEY", Some(OsString::from("anthropic-key"))),
            ("ANTHROPIC_AUTH_TOKEN", None),
            ("OPENROUTER_API_KEY", Some(OsString::from("openrouter-key"))),
            ("OPENAI_API_KEY", Some(OsString::from(VENDOR_OPENAI_KEY))),
            ("EXAMPLE_GLM_API_KEY", glm_key.map(OsString::from)),
        ]);
        let paths = crate::paths::process_paths().unwrap();
        crate::paths::ensure_private_directory(&paths.agents_dir()).unwrap();
        for (name, body) in [
            ("designer", DESIGNER),
            ("implementer", IMPLEMENTER),
            ("reviewer", REVIEWER),
        ] {
            crate::fs::private_atomic_write_sync(
                &paths.agents_dir().join(format!("{name}.md")),
                body.as_bytes(),
            )
            .unwrap();
        }
        let workspace =
            std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(root.path()).unwrap());
        Self {
            _root: root,
            _env: env,
            workspace,
        }
    }

    fn specialization(&self, agent_type: &str) -> ResolvedSpecialization {
        resolve_specialization(Some(agent_type), Some(&self.workspace))
            .unwrap()
            .unwrap()
    }

    /// The parent after `/agent designer`: the opus alias on Anthropic.
    fn parent_client(&self, config: &crate::config::Config) -> AnyClient {
        crate::provider::create_client(
            "anthropic",
            None,
            config.custom_providers_map(),
            config.api_keys.as_ref(),
        )
        .unwrap()
    }
}

#[test]
fn implementer_runs_on_the_glm_custom_provider_in_write_mode() {
    let pipeline = Pipeline::new("pipeline-implementer", Some(GLM_KEY));
    let config = fixture_config();
    let parent = pipeline.parent_client(&config);
    assert_eq!(parent.provider_name(), "anthropic");

    let runtime = resolve_persona_runtime(
        parent,
        "anthropic",
        "anthropic",
        "example-opus-model".into(),
        20,
        None,
        &config,
        Some(&pipeline.specialization("implementer")),
    )
    .unwrap();

    // A separate client for the OpenAI-compatible `glm` endpoint, not the
    // parent's Anthropic client.
    assert_eq!(runtime.provider_name, "glm");
    assert_eq!(runtime.client.provider_name(), "openai");
    assert_eq!(runtime.model_name, "example-glm-model");
    assert!(runtime.execution.write_mode);
    assert_eq!(runtime.max_turns, 20);
    assert_eq!(
        runtime.execution.tools,
        Some(vec![
            AgentTool::Read,
            AgentTool::Grep,
            AgentTool::FindFiles,
            AgentTool::Edit,
            AgentTool::Write,
            AgentTool::Shell,
        ])
    );
    assert!(
        config
            .subagents
            .as_ref()
            .is_some_and(|policy| policy.allow_write),
        "the fixture opts in to write children"
    );
}

#[test]
fn reviewer_runs_read_only_on_the_review_alias() {
    let pipeline = Pipeline::new("pipeline-reviewer", Some(GLM_KEY));
    let config = fixture_config();

    let runtime = resolve_persona_runtime(
        pipeline.parent_client(&config),
        "anthropic",
        "anthropic",
        "example-opus-model".into(),
        20,
        None,
        &config,
        Some(&pipeline.specialization("reviewer")),
    )
    .unwrap();

    assert_eq!(runtime.provider_name, "openrouter");
    assert_eq!(runtime.client.provider_name(), "openrouter");
    assert_eq!(runtime.model_name, "example-vendor/example-reviewer-model");
    assert!(!runtime.execution.write_mode);
    assert_eq!(
        runtime.execution.tools,
        Some(vec![AgentTool::Read, AgentTool::Grep, AgentTool::FindFiles])
    );
    assert_eq!(runtime.max_turns, 14);
}

#[test]
fn designer_stays_a_standard_main_loop_persona_not_a_write_child() {
    let pipeline = Pipeline::new("pipeline-designer", None);
    let designer = pipeline.specialization("designer");
    // `mode: design` names a prompt for the main loop; it must not read as
    // `mode: write`, which would make the designer a write-capable child.
    assert!(!designer.write_mode);
    let definition =
        crate::context::agents::lookup_for_workspace("designer", Some(&pipeline.workspace))
            .unwrap();
    assert_eq!(definition.mode.as_deref(), Some("design"));
}

#[test]
fn raw_model_id_persona_stays_on_the_parent_provider() {
    let pipeline = Pipeline::new("pipeline-raw-model", Some(GLM_KEY));
    let config = fixture_config();
    let mut raw = pipeline.specialization("reviewer");
    raw.model = Some("example-vendor/raw-model-id".into());

    let runtime = resolve_persona_runtime(
        pipeline.parent_client(&config),
        "anthropic",
        "anthropic",
        "example-opus-model".into(),
        20,
        None,
        &config,
        Some(&raw),
    )
    .unwrap();

    assert_eq!(runtime.provider_name, "anthropic");
    assert_eq!(runtime.client.provider_name(), "anthropic");
    assert_eq!(runtime.model_name, "example-vendor/raw-model-id");
}

#[test]
fn glm_provider_never_receives_a_vendor_credential() {
    let config = fixture_config();
    let custom = config.custom_providers_map();
    let mut api_keys = HashMap::new();
    api_keys.insert("openai".to_string(), VENDOR_OPENAI_KEY.to_string());
    let env_with = |glm: Option<&'static str>| {
        move |name: &str| match (name, glm) {
            ("OPENAI_API_KEY", _) => Ok(VENDOR_OPENAI_KEY.to_string()),
            ("EXAMPLE_GLM_API_KEY", Some(key)) => Ok(key.to_string()),
            _ => Err(std::env::VarError::NotPresent),
        }
    };

    // Only the vendor key exists: creating the glm client must fail rather
    // than fall back to it, and the error must not echo the vendor key.
    let error = crate::provider::create_client_with_env(
        "glm",
        None,
        custom,
        Some(&api_keys),
        env_with(None),
    )
    .err()
    .expect("glm must not inherit the vendor OpenAI key")
    .to_string();
    assert!(error.contains("glm"), "{error}");
    assert!(!error.contains(VENDOR_OPENAI_KEY), "{error}");

    // The credential actually selected is the provider's own api_key_env.
    let provider = crate::provider::resolve_provider_config("glm", custom).unwrap();
    assert_eq!(provider.credential_isolation, None, "name-based isolation");
    let resolver = crate::auth::AuthResolver::new(provider.kind)
        .with_env_override(provider.api_key_env.as_deref())
        .with_config_keys(Some(&api_keys))
        .with_custom_provider_name(Some("glm"))
        .with_credential_isolation(provider.credential_isolation);
    let credential = resolver
        .resolve_credential_with_env(env_with(Some(GLM_KEY)))
        .unwrap();
    assert_eq!(credential.secret(), GLM_KEY);
    assert!(
        crate::provider::create_client_with_env(
            "glm",
            None,
            custom,
            Some(&api_keys),
            env_with(Some(GLM_KEY)),
        )
        .is_ok()
    );
}

#[test]
fn implementer_resolution_fails_closed_without_the_glm_key() {
    // The vendor OpenAI key is in the environment but EXAMPLE_GLM_API_KEY is
    // not: the task must not start a child on the gateway with that key.
    let pipeline = Pipeline::new("pipeline-missing-glm-key", None);
    let config = fixture_config();

    let error = resolve_persona_runtime(
        pipeline.parent_client(&config),
        "anthropic",
        "anthropic",
        "example-opus-model".into(),
        20,
        None,
        &config,
        Some(&pipeline.specialization("implementer")),
    )
    .err()
    .expect("missing glm key must fail the persona runtime")
    .to_string();
    assert!(
        error.contains("could not initialize provider 'glm'"),
        "{error}"
    );
    assert!(!error.contains(VENDOR_OPENAI_KEY), "{error}");
}

/// Builds persona clients with an injected environment and records the CLI key
/// each cross-provider build was handed (mini-agent-zmt7r).
struct RecordingFactory {
    keys: std::cell::RefCell<Vec<(String, Option<String>)>>,
}

impl RecordingFactory {
    fn new() -> Self {
        Self {
            keys: Default::default(),
        }
    }

    fn build(
        &self,
        glm: Option<&'static str>,
    ) -> impl Fn(&str, Option<&str>, &crate::config::Config) -> anyhow::Result<AnyClient> + '_ {
        move |provider, key, config| {
            self.keys
                .borrow_mut()
                .push((provider.to_owned(), key.map(str::to_owned)));
            crate::provider::create_client_with_env(
                provider,
                key,
                config.custom_providers_map(),
                config.api_keys.as_ref(),
                move |name| match (name, glm) {
                    ("OPENAI_API_KEY", _) => Ok(VENDOR_OPENAI_KEY.to_string()),
                    ("OPENROUTER_API_KEY", _) => Ok("openrouter-key".to_string()),
                    ("ANTHROPIC_API_KEY", _) => Ok("anthropic-key".to_string()),
                    ("EXAMPLE_GLM_API_KEY", Some(value)) => Ok(value.to_string()),
                    _ => Err(std::env::VarError::NotPresent),
                },
            )
        }
    }
}

fn cli_key_parent(config: &crate::config::Config) -> AnyClient {
    crate::provider::create_client_with_env(
        "anthropic",
        Some(CLI_KEY),
        config.custom_providers_map(),
        config.api_keys.as_ref(),
        |_| Err(std::env::VarError::NotPresent),
    )
    .unwrap()
}

const CLI_KEY: &str = "sk-ant-cli-key-must-never-reach-glm";

/// A persona pinned to a quick-model alias, built directly so these tests
/// need neither persona files nor process environment.
fn alias_persona(alias: &str) -> ResolvedSpecialization {
    ResolvedSpecialization {
        write_mode: false,
        isolation_worktree: false,
        prompt: "persona".into(),
        result_notice: None,
        source: "test".into(),
        tools: None,
        required_tools: Vec::new(),
        model: Some(alias.into()),
        turn_budget: None,
    }
}

#[test]
fn cli_key_for_the_session_provider_never_reaches_a_custom_provider() {
    let implementer = alias_persona("glm");
    let config = fixture_config();
    let factory = RecordingFactory::new();

    let runtime = super::resolve_persona_runtime_with(
        cli_key_parent(&config),
        "anthropic",
        "anthropic",
        "example-opus-model".into(),
        20,
        Some(CLI_KEY),
        &config,
        Some(&implementer),
        &factory.build(Some(GLM_KEY)),
    )
    .unwrap();

    assert_eq!(runtime.provider_name, "glm");
    assert_eq!(
        *factory.keys.borrow(),
        vec![("glm".to_owned(), None)],
        "the glm client resolves its own api_key_env, never the CLI key"
    );
}

#[test]
fn custom_provider_without_its_own_credential_fails_closed_despite_a_cli_key() {
    let implementer = alias_persona("glm");
    let config = fixture_config();
    let factory = RecordingFactory::new();

    let error = super::resolve_persona_runtime_with(
        cli_key_parent(&config),
        "anthropic",
        "anthropic",
        "example-opus-model".into(),
        20,
        Some(CLI_KEY),
        &config,
        Some(&implementer),
        &factory.build(None),
    )
    .err()
    .expect("glm without its own key must not fall back to the CLI key")
    .to_string();

    assert!(
        error.contains("could not initialize provider 'glm'"),
        "{error}"
    );
    assert!(!error.contains(CLI_KEY), "{error}");
    assert!(!error.contains(VENDOR_OPENAI_KEY), "{error}");
}

#[test]
fn cli_key_still_reaches_a_persona_on_the_session_provider() {
    // Session provider openrouter (child currently elsewhere): a persona alias
    // back on the session provider gets the CLI key; the key is scoped to it.
    let reviewer = alias_persona("review");
    let config = fixture_config();
    let factory = RecordingFactory::new();

    let runtime = super::resolve_persona_runtime_with(
        cli_key_parent(&config),
        "anthropic",
        "openrouter",
        "example-opus-model".into(),
        20,
        Some(CLI_KEY),
        &config,
        Some(&reviewer),
        &factory.build(None),
    )
    .unwrap();

    assert_eq!(runtime.provider_name, "openrouter");
    assert_eq!(
        *factory.keys.borrow(),
        vec![("openrouter".to_owned(), Some(CLI_KEY.to_owned()))]
    );
}
