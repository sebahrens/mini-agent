use std::collections::HashMap;

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/agents");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    Embedded,
    UserGlobal,
    ProjectOverride,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub prompt: String,
    pub source: AgentDefinitionSource,
}

fn merge_definitions(
    agents: &mut HashMap<String, AgentDefinition>,
    definitions: impl IntoIterator<Item = (String, String)>,
    source: AgentDefinitionSource,
) {
    for (name, prompt) in definitions {
        agents.insert(name, AgentDefinition { prompt, source });
    }
}

/// Load all agent type definitions. Priority (highest wins):
///   project override (.zerostack/agents/<name>.md)
///   → user global (data_dir/agents/<name>.md)
///   → compiled-in default (data/agents/<name>.md)
pub fn load() -> HashMap<String, AgentDefinition> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    let mut agents = HashMap::new();

    merge_definitions(
        &mut agents,
        crate::context::load_embedded_files(&EMBEDDED, "md"),
        AgentDefinitionSource::Embedded,
    );
    merge_definitions(
        &mut agents,
        crate::context::load_dir_files(&paths.agents_dir(), "md"),
        AgentDefinitionSource::UserGlobal,
    );
    if let Some(project_dir) = paths.project_agents_dir() {
        merge_definitions(
            &mut agents,
            crate::context::load_dir_files(&project_dir, "md"),
            AgentDefinitionSource::ProjectOverride,
        );
    }
    agents
}

/// Look up the system prompt and its provenance for a named agent type. Returns
/// `None` when the name is not registered so callers can fall back to the
/// default explore prompt.
pub fn lookup(name: &str) -> Option<AgentDefinition> {
    load().remove(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded_prompt(name: &str) -> String {
        crate::context::load_embedded_files(&EMBEDDED, "md")
            .into_iter()
            .find_map(|(candidate, prompt)| (candidate == name).then_some(prompt))
            .unwrap_or_else(|| panic!("missing embedded specialist {name}"))
    }

    #[test]
    fn project_definition_wins_and_retains_override_provenance() {
        let mut agents = HashMap::new();
        merge_definitions(
            &mut agents,
            [("review".to_string(), "embedded".to_string())],
            AgentDefinitionSource::Embedded,
        );
        merge_definitions(
            &mut agents,
            [("review".to_string(), "project".to_string())],
            AgentDefinitionSource::ProjectOverride,
        );

        let resolved = agents.remove("review").unwrap();
        assert_eq!(resolved.prompt, "project");
        assert_eq!(resolved.source, AgentDefinitionSource::ProjectOverride);
    }

    #[test]
    fn embedded_specialists_respect_read_only_execution_contracts() {
        let azure = embedded_prompt("azure-cloud-architect");
        assert!(azure.contains("stated and verified constraints support that decision"));
        assert!(azure.contains("**Constraints assumed**"));

        let informatica = embedded_prompt("informatica-mapplet-to-fabric-sql");
        assert!(informatica.contains("**Queries not executed**"));
        assert!(informatica.contains("caller or operator to run"));

        let security = embedded_prompt("rust-security-review");
        assert!(security.contains("Recommend that the calling agent run `cargo deny check`"));
        assert!(security.contains("do not claim that command was executed"));

        let concurrency = embedded_prompt("rust-async-concurrency");
        assert!(concurrency.contains("hand the exact `cargo expand` command back"));
        assert!(concurrency.contains("state that it was not executed"));

        let unsafe_audit = embedded_prompt("rust-unsafe-code-audit");
        assert!(unsafe_audit.contains("recommend that the calling agent add and run"));
        assert!(unsafe_audit.contains("do not claim to have compiled or executed it"));
    }
}
