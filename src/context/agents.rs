use std::collections::HashMap;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/agents");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    Embedded,
    UserGlobal,
    ProjectOverride { directory: PathBuf },
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
        agents.insert(
            name,
            AgentDefinition {
                prompt,
                source: source.clone(),
            },
        );
    }
}

impl AgentDefinition {
    pub fn project_override_path(&self, name: &str) -> Option<PathBuf> {
        match &self.source {
            AgentDefinitionSource::ProjectOverride { directory } => {
                Some(directory.join(format!("{name}.md")))
            }
            AgentDefinitionSource::Embedded | AgentDefinitionSource::UserGlobal => None,
        }
    }
}

fn load_base(paths: &crate::paths::AppPaths) -> HashMap<String, AgentDefinition> {
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
    agents
}

/// Load all agent type definitions. Priority (highest wins):
///   project override (.zerostack/agents/<name>.md)
///   → user global (data_dir/agents/<name>.md)
///   → compiled-in default (data/agents/<name>.md)
pub fn load() -> HashMap<String, AgentDefinition> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    let mut agents = load_base(&paths);
    if let Some(project_dir) = paths.project_agents_dir() {
        merge_definitions(
            &mut agents,
            crate::context::load_dir_files(&project_dir, "md"),
            AgentDefinitionSource::ProjectOverride {
                directory: project_dir,
            },
        );
    }
    agents
}

/// Load agent definitions while resolving project overrides through the same
/// captured workspace capability used by the session's filesystem tools.
/// Global definitions remain process-scoped; only project-owned definitions
/// are rebound per session.
pub(crate) fn load_for_workspace_binding(
    workspace: &crate::paths::WorkspaceBinding,
) -> HashMap<String, AgentDefinition> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    let mut agents = load_base(&paths);
    let project_dir = workspace.root().join(".zerostack/agents");
    if let Ok(definitions) = workspace.read_relative_dir_files(Path::new(".zerostack/agents"), "md")
    {
        merge_definitions(
            &mut agents,
            definitions,
            AgentDefinitionSource::ProjectOverride {
                directory: project_dir,
            },
        );
    }
    agents
}

/// Look up the system prompt and its provenance for a named agent type.
pub fn lookup(name: &str) -> Option<AgentDefinition> {
    load().remove(name)
}

pub(crate) fn available_names_for_workspace(
    workspace: Option<&crate::paths::WorkspaceBinding>,
) -> Vec<String> {
    let mut names: Vec<_> = match workspace {
        Some(workspace) => load_for_workspace_binding(workspace),
        None => load(),
    }
    .into_keys()
    .collect();
    names.sort_unstable();
    names
}

pub(crate) fn lookup_for_workspace(
    name: &str,
    workspace: Option<&crate::paths::WorkspaceBinding>,
) -> Option<AgentDefinition> {
    match workspace {
        Some(workspace) => load_for_workspace_binding(workspace).remove(name),
        None => lookup(name),
    }
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
            AgentDefinitionSource::ProjectOverride {
                directory: PathBuf::from("/workspace/.zerostack/agents"),
            },
        );

        let resolved = agents.remove("review").unwrap();
        assert_eq!(resolved.prompt, "project");
        assert_eq!(
            resolved.source,
            AgentDefinitionSource::ProjectOverride {
                directory: PathBuf::from("/workspace/.zerostack/agents")
            }
        );
        assert_eq!(
            resolved.project_override_path("review"),
            Some(PathBuf::from("/workspace/.zerostack/agents/review.md"))
        );
    }

    #[test]
    fn workspace_bound_definitions_do_not_cross_sessions() {
        let container = std::env::temp_dir().join(format!(
            "mini-agent-agent-definitions-{}",
            uuid::Uuid::new_v4()
        ));
        let first = container.join("first");
        let second = container.join("second");
        for (workspace, prompt) in [(&first, "FIRST_WORKSPACE"), (&second, "SECOND_WORKSPACE")] {
            std::fs::create_dir_all(workspace.join(".zerostack/agents")).unwrap();
            std::fs::write(workspace.join(".zerostack/agents/review.md"), prompt).unwrap();
        }

        let first_binding = crate::paths::WorkspaceBinding::capture(&first).unwrap();
        let second_binding = crate::paths::WorkspaceBinding::capture(&second).unwrap();
        let first_definition = lookup_for_workspace("review", Some(&first_binding)).unwrap();
        let second_definition = lookup_for_workspace("review", Some(&second_binding)).unwrap();

        assert_eq!(first_definition.prompt, "FIRST_WORKSPACE");
        assert_eq!(second_definition.prompt, "SECOND_WORKSPACE");
        assert_eq!(
            first_definition.project_override_path("review"),
            Some(first_binding.root().join(".zerostack/agents/review.md"))
        );
        assert_eq!(
            second_definition.project_override_path("review"),
            Some(second_binding.root().join(".zerostack/agents/review.md"))
        );

        drop(first_binding);
        drop(second_binding);
        std::fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn embedded_specialists_respect_read_only_execution_contracts() {
        let azure = embedded_prompt("azure-cloud-architect");
        assert!(azure.contains("stated and verified constraints support that decision"));
        assert!(azure.contains("**Constraints assumed**"));
        assert!(
            azure.find("**Unknown constraints").unwrap() < azure.find("**Architecture**").unwrap()
        );

        let informatica = embedded_prompt("informatica-mapplet-to-fabric-sql");
        assert!(informatica.contains("**Queries not executed**"));
        assert!(informatica.contains("caller or operator to run"));
        assert!(informatica.contains("Documentation snapshot: **2026-08-31**"));
        assert!(informatica.contains("design hypothesis"));
        assert!(
            informatica
                .find("**Assumptions requiring human confirmation**")
                .unwrap()
                < informatica.find("**The T-SQL**").unwrap()
        );

        let security = embedded_prompt("rust-security-review");
        assert!(security.contains("Recommend that the calling agent run `cargo deny check`"));
        assert!(security.contains("do not claim that command was executed"));
        assert!(security.contains("enumerate every call path into hook subprocess execution"));
        assert!(security.contains("newly added caller is gated"));

        let concurrency = embedded_prompt("rust-async-concurrency");
        assert!(concurrency.contains("read-only source investigation"));
        assert!(concurrency.contains("never assume a runtime flavor"));
        assert!(concurrency.contains("source inspection alone cannot answer"));

        let unsafe_audit = embedded_prompt("rust-unsafe-code-audit");
        assert!(unsafe_audit.contains("recommend that the calling agent add and run"));
        assert!(unsafe_audit.contains("do not claim to have compiled or executed it"));
        assert!(unsafe_audit.contains("canonical Phase 6 security invariants"));

        let vscode = embedded_prompt("vscode-extension-developer");
        assert!(vscode.contains("Treat `editors/vscode/` as the only stable location"));
        assert!(vscode.contains("grepping for `workspace.isTrusted`"));
        assert!(!vscode.contains("editors/vscode/src/extension.ts"));

        let rust_maintainer = embedded_prompt("rust-maintainer");
        assert!(rust_maintainer.contains("delegate Tokio cancel-safety"));
        assert!(rust_maintainer.contains("Derive every command from the repository's actual"));
        assert!(rust_maintainer.contains("State explicitly which checks you cannot run"));
        assert!(rust_maintainer.contains("Do not claim to have compiled, tested, or executed"));
        assert!(
            rust_maintainer
                .find("Caveats and unverified assumptions")
                .unwrap()
                < rust_maintainer
                    .find("Lifecycle investigation method")
                    .unwrap()
        );

        let python_maintainer = embedded_prompt("python-maintainer");
        assert!(python_maintainer.contains("Derive every command from the repository's actual"));
        assert!(python_maintainer.contains("Do not claim to have executed code"));
        assert!(python_maintainer.contains("You do not assume any specific framework"));
        assert!(
            python_maintainer
                .contains("Derive the interpreter and tool invocation from what you find")
        );
        assert!(
            python_maintainer
                .find("Caveats and unverified assumptions")
                .unwrap()
                < python_maintainer
                    .find("Lifecycle investigation method")
                    .unwrap()
        );

        let node_ts_maintainer = embedded_prompt("node-typescript-maintainer");
        assert!(node_ts_maintainer.contains("hand those off explicitly"));
        assert!(node_ts_maintainer.contains("Do not claim to have executed code"));
        assert!(node_ts_maintainer.contains("You do not assume npm, ESM, or React"));
        assert!(
            node_ts_maintainer
                .contains("Derive the package manager and script invocations from what you find")
        );
        assert!(
            node_ts_maintainer
                .find("Caveats and unverified assumptions")
                .unwrap()
                < node_ts_maintainer
                    .find("Lifecycle investigation method")
                    .unwrap()
        );
    }
}
