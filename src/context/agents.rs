use std::collections::HashMap;

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/agents");

/// Load all agent type definitions. Priority (highest wins):
///   project override (.zerostack/agents/<name>.md)
///   → user global (data_dir/agents/<name>.md)
///   → compiled-in default (data/agents/<name>.md)
pub fn load() -> HashMap<String, String> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    let mut agents: HashMap<String, String> = HashMap::new();

    for (name, content) in crate::context::load_embedded_files(&EMBEDDED, "md") {
        agents.entry(name).or_insert(content);
    }
    for (name, content) in crate::context::load_dir_files(&paths.agents_dir(), "md") {
        agents.insert(name, content);
    }
    if let Some(project_dir) = paths.project_agents_dir() {
        for (name, content) in crate::context::load_dir_files(&project_dir, "md") {
            agents.insert(name, content);
        }
    }
    agents
}

/// Look up the system prompt for a named agent type. Returns `None` when the
/// name is not registered so callers can fall back to the default explore prompt.
pub fn lookup(name: &str) -> Option<String> {
    load().remove(name)
}
