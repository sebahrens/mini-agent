use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use include_dir::Dir;
use smallvec::SmallVec;

use crate::session::storage;

pub mod agents;
pub mod prompts;
pub mod themes;

pub(crate) fn load_embedded_files(embedded: &Dir, ext: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    for file in embedded.files() {
        if file.path().extension().is_some_and(|e| e == ext)
            && let Some(name) = file.path().file_stem().and_then(|s| s.to_str())
            && let Some(content) = file.contents_utf8()
        {
            results.push((name.to_string(), content.to_string()));
        }
    }
    results
}

pub(crate) fn load_dir_files(dir: &Path, ext: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    if dir.exists()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == ext)
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                results.push((name.to_string(), content));
            }
        }
    }
    results
}

pub(crate) fn load_dir_files_bounded_status(
    dir: &Path,
    ext: &str,
    max_bytes: usize,
) -> Vec<(String, Result<String, String>)> {
    let mut results = Vec::new();
    if dir.exists()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != ext) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let content = match std::fs::File::open(&path) {
                Ok(file) => match crate::paths::read_utf8_bounded_status(file, max_bytes) {
                    Ok(crate::paths::BoundedUtf8Read::Content(content)) => Ok(content),
                    Ok(crate::paths::BoundedUtf8Read::Oversized) => {
                        Err(format!("exceeds the {max_bytes}-byte limit"))
                    }
                    Ok(crate::paths::BoundedUtf8Read::InvalidUtf8) => {
                        Err("is not valid UTF-8".to_string())
                    }
                    Err(error) => Err(format!("could not be read: {error}")),
                },
                Err(error) => Err(format!("could not be opened: {error}")),
            };
            results.push((name.to_string(), content));
        }
    }
    results
}

#[cfg(test)]
mod bounded_directory_file_tests {
    use super::*;

    #[test]
    fn bounded_directory_loaders_stop_at_limit_plus_one() {
        let root = std::env::temp_dir().join(format!(
            "mini-agent-bounded-directory-files-{}",
            uuid::Uuid::new_v4()
        ));
        let agents = root.join(".zerostack/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("exact.md"), b"12345678").unwrap();
        std::fs::write(agents.join("oversized.md"), b"123456789").unwrap();
        std::fs::write(agents.join("invalid.md"), [0xff, 0xfe]).unwrap();

        let direct_status = load_dir_files_bounded_status(&agents, "md", 8);
        assert_eq!(direct_status.len(), 3);
        assert!(direct_status.contains(&("exact".to_string(), Ok("12345678".to_string()))));
        assert!(direct_status.contains(&(
            "oversized".to_string(),
            Err("exceeds the 8-byte limit".to_string())
        )));
        assert!(
            direct_status.contains(&("invalid".to_string(), Err("is not valid UTF-8".to_string())))
        );

        let workspace = crate::paths::WorkspaceBinding::capture(&root).unwrap();
        let capability_status = workspace
            .read_relative_dir_files_bounded_status(Path::new(".zerostack/agents"), "md", 8)
            .unwrap();
        assert_eq!(capability_status.len(), 3);
        assert!(capability_status.contains(&("exact".to_string(), Ok("12345678".to_string()))));
        assert!(capability_status.contains(&(
            "oversized".to_string(),
            Err("exceeds the 8-byte limit".to_string())
        )));
        assert!(
            capability_status
                .contains(&("invalid".to_string(), Err("is not valid UTF-8".to_string())))
        );

        drop(workspace);
        std::fs::remove_dir_all(root).unwrap();
    }
}

pub(crate) fn copy_embedded_to(embedded: &Dir, dest: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dest)?;
    for file in embedded.files() {
        if let Some(name) = file.path().file_name().and_then(|s| s.to_str()) {
            let dest_path = dest.join(name);
            if let Some(content) = file.contents_utf8() {
                std::fs::write(&dest_path, content)?;
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct ContextFiles {
    /// Workspace used for project context and relative agent tool paths.
    pub workspace_root: PathBuf,
    pub agents: Option<String>,
    pub prompts: HashMap<String, String>,
    pub current_prompt: Option<String>,
    pub current_prompt_name: Option<String>,
    pub agent_definitions: HashMap<String, agents::AgentDefinition>,
    pub current_agent_name: Option<String>,
    /// True when `/agent` (including `/agent default`) is the authority for
    /// the current persona. Prompt reloads must not overwrite that choice.
    pub(crate) current_agent_explicit: bool,
    pub themes: HashMap<String, String>,
    pub current_theme_name: Option<String>,
    pub extra_files: Vec<std::path::PathBuf>,
    /// Preloaded file contents keyed by canonical path. Populated at /add time using
    /// spawn_blocking so agent-build paths never perform synchronous filesystem reads.
    pub extra_file_contents: HashMap<PathBuf, Arc<String>>,
    pub one_shot_restore: Option<ActiveContextSelection>,
    pub chain_declined: Vec<String>,
    #[cfg(feature = "memory")]
    pub memory: Option<String>,
    #[cfg(feature = "archmd")]
    pub architecture: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActiveContextSelection {
    pub(crate) prompt_name: Option<String>,
    pub(crate) agent_name: Option<String>,
    pub(crate) agent_explicit: bool,
}

impl ContextFiles {
    pub(crate) fn active_selection(&self) -> ActiveContextSelection {
        ActiveContextSelection {
            prompt_name: self.current_prompt_name.clone(),
            agent_name: self.current_agent_name.clone(),
            agent_explicit: self.current_agent_explicit,
        }
    }

    /// Select a prompt mode, strip its header directives, and compose its
    /// optional persona with the current main-agent selection. Returns the
    /// requested security-mode directive for the permission layer.
    pub(crate) fn activate_prompt(&mut self, name: &str) -> Option<Option<String>> {
        let content = self.prompts.get(name)?.clone();
        let directives = prompts::parse_directives(&content);
        self.current_prompt = Some(directives.content.to_string());
        self.current_prompt_name = Some(name.to_string());
        if let Some(agent) = directives.agent {
            self.current_agent_explicit = false;
            if agent == "default" {
                self.current_agent_name = None;
            } else if self.agent_definitions.contains_key(agent) {
                self.current_agent_name = Some(agent.to_string());
            } else {
                tracing::warn!(
                    prompt = name,
                    agent,
                    "prompt names an unavailable main-agent persona"
                );
                self.current_agent_name = None;
            }
        }
        Some(directives.mode.map(str::to_string))
    }

    /// Select a persona for the main loop and return its optional default
    /// prompt mode. The caller applies that prompt so model and permission
    /// mappings stay on the same path as an explicit `/prompt` selection.
    pub(crate) fn activate_agent(&mut self, name: &str) -> Option<Option<String>> {
        let mode = self.agent_definitions.get(name)?.mode.clone();
        self.current_agent_name = Some(name.to_string());
        self.current_agent_explicit = true;
        Some(mode)
    }

    pub(crate) fn restore_selection(&mut self, selection: ActiveContextSelection) {
        if let Some(name) = selection.prompt_name {
            let _ = self.activate_prompt(&name);
        } else {
            self.current_prompt = None;
            self.current_prompt_name = None;
        }
        self.current_agent_name = selection
            .agent_name
            .filter(|name| self.agent_definitions.contains_key(name));
        self.current_agent_explicit = selection.agent_explicit;
    }

    #[cfg(feature = "memory")]
    pub(crate) fn replace_memory_if_changed(&mut self, memory: Option<String>) -> bool {
        if self.memory == memory {
            return false;
        }
        self.memory = memory;
        true
    }

    /// Refresh persistent-memory prompt context without destabilizing the
    /// provider cache when its rendered content is byte-identical.
    #[cfg(feature = "memory")]
    pub(crate) async fn refresh_memory_if_changed(&mut self) -> bool {
        match tokio::task::spawn_blocking(|| crate::extras::memory::Mem::open().context_block())
            .await
        {
            Ok(memory) => self.replace_memory_if_changed(memory),
            Err(error) => {
                tracing::warn!(%error, "failed to refresh persistent memory context");
                false
            }
        }
    }

    pub(crate) fn for_workspace_binding(
        &self,
        no_context_files: bool,
        workspace: &crate::paths::WorkspaceBinding,
    ) -> Self {
        let mut context = self.clone();
        let (agents, architecture) = if no_context_files {
            (None, None)
        } else {
            walk_bound_context_files(workspace)
        };
        context.workspace_root = workspace.root().to_path_buf();
        context.agents = agents;
        context.prompts = prompts::load_for_workspace_binding(workspace);
        context.agent_definitions = agents::load_for_workspace_binding(workspace);
        if context
            .current_agent_name
            .as_ref()
            .is_some_and(|name| !context.agent_definitions.contains_key(name))
        {
            context.current_agent_name = None;
        }
        if let Some(name) = &context.current_prompt_name {
            context.current_prompt = context.prompts.get(name).cloned();
        }
        #[cfg(feature = "archmd")]
        {
            context.architecture = architecture;
        }
        #[cfg(not(feature = "archmd"))]
        let _ = architecture;
        context
    }

    pub fn for_workspace(&self, no_context_files: bool, workspace_root: &Path) -> Self {
        let mut context = self.clone();
        let (agents, architecture) = if no_context_files {
            (None, None)
        } else {
            walk_context_files(Some(workspace_root))
        };
        context.workspace_root = workspace_root.to_path_buf();
        context.agents = agents;
        context.prompts = prompts::load_for_workspace(workspace_root);
        let binding = crate::paths::WorkspaceBinding::capture(workspace_root).ok();
        context.agent_definitions = binding
            .as_ref()
            .map(agents::load_for_workspace_binding)
            .unwrap_or_else(agents::load);
        if context
            .current_agent_name
            .as_ref()
            .is_some_and(|name| !context.agent_definitions.contains_key(name))
        {
            context.current_agent_name = None;
        }
        if let Some(name) = &context.current_prompt_name {
            context.current_prompt = context.prompts.get(name).cloned();
        }
        #[cfg(feature = "archmd")]
        {
            context.architecture = architecture;
        }
        #[cfg(not(feature = "archmd"))]
        let _ = architecture;
        context
    }

    #[cfg(feature = "git-worktree")]
    pub fn reload(&mut self) {
        let workspace_root = std::env::current_dir().unwrap_or_default();
        self.reload_from(&workspace_root);
    }

    #[cfg(feature = "git-worktree")]
    pub fn reload_from(&mut self, workspace_root: &Path) {
        self.workspace_root = workspace_root.to_path_buf();
        self.agents = walk_context_files(Some(workspace_root)).0;
        #[cfg(feature = "archmd")]
        {
            self.architecture = walk_context_files(Some(workspace_root)).1;
        }
        self.prompts = prompts::load_for_workspace(workspace_root);
        self.agent_definitions = crate::paths::WorkspaceBinding::capture(workspace_root)
            .ok()
            .as_ref()
            .map(agents::load_for_workspace_binding)
            .unwrap_or_else(agents::load);
        if let Some(name) = &self.current_prompt_name {
            self.current_prompt = self.prompts.get(name).cloned();
        }
        if self
            .current_agent_name
            .as_ref()
            .is_some_and(|name| !self.agent_definitions.contains_key(name))
        {
            self.current_agent_name = None;
        }
        self.themes = themes::load();
        self.current_theme_name = crate::session::storage::load_theme_name();
        #[cfg(feature = "memory")]
        {
            self.memory = crate::extras::memory::Mem::open().context_block();
        }
    }

    #[cfg(feature = "git-worktree")]
    pub(crate) fn reload_from_binding(
        &mut self,
        no_context_files: bool,
        workspace: &crate::paths::WorkspaceBinding,
    ) {
        let mut rebound = self.for_workspace_binding(no_context_files, workspace);
        rebound.themes = themes::load();
        rebound.current_theme_name = crate::session::storage::load_theme_name();
        #[cfg(feature = "memory")]
        {
            rebound.memory = crate::extras::memory::Mem::open().context_block();
        }
        *self = rebound;
    }
}

fn walk_bound_context_files(
    workspace: &crate::paths::WorkspaceBinding,
) -> (Option<String>, Option<String>) {
    let mut agent_parts: SmallVec<[String; 4]> = SmallVec::new();
    #[cfg_attr(not(feature = "archmd"), allow(unused_mut))]
    let mut arch_parts: SmallVec<[String; 4]> = SmallVec::new();
    if let Some(content) = load_file(&storage::agents_path())
        && !content.trim().is_empty()
    {
        agent_parts.push(format!("# Global AGENTS.md\n{content}"));
    }
    #[cfg(feature = "archmd")]
    if let Some(content) = load_file(&storage::architecture_path())
        && !content.trim().is_empty()
    {
        arch_parts.push(format!("# Global ARCHITECTURE.md\n{content}"));
    }
    let names = if cfg!(feature = "archmd") {
        &["AGENTS.md", "CLAUDE.md", "ARCHITECTURE.md"][..]
    } else {
        &["AGENTS.md", "CLAUDE.md"][..]
    };
    let mut total = 0usize;
    for (dir, name, content) in workspace.read_ancestor_files(names) {
        if content.trim().is_empty() || total >= MAX_ANCESTOR_CONTEXT_BYTES {
            continue;
        }
        total = total.saturating_add(content.len());
        if name == "ARCHITECTURE.md" {
            #[cfg(feature = "archmd")]
            arch_parts.push(format!(
                "# ARCHITECTURE.md ({})\n{}",
                dir.display(),
                content
            ));
        } else {
            agent_parts.push(format!("# {} ({})\n{}", name, dir.display(), content));
        }
    }
    let agents = (!agent_parts.is_empty()).then(|| agent_parts.join("\n\n"));
    let architecture = (!arch_parts.is_empty()).then(|| arch_parts.join("\n\n"));
    (agents, architecture)
}

pub fn load(no_context_files: bool) -> ContextFiles {
    let workspace_root = std::env::current_dir().ok();
    load_for_workspace(no_context_files, workspace_root.as_deref())
}

/// Load context for one explicitly selected workspace without changing the
/// process working directory. ACP sessions use this entry point so concurrent
/// roots cannot affect one another.
pub fn load_for_workspace(no_context_files: bool, workspace_root: Option<&Path>) -> ContextFiles {
    if let Err(e) = prompts::ensure_global() {
        tracing::warn!("failed to install default prompts: {e}");
    }
    if let Err(e) = themes::ensure_global() {
        tracing::warn!("failed to install default themes: {e}");
    }
    let (agents, arch_candidate) = if no_context_files {
        (None, None)
    } else {
        walk_context_files(workspace_root)
    };
    #[cfg(feature = "archmd")]
    let architecture = arch_candidate;
    #[cfg(not(feature = "archmd"))]
    let _ = arch_candidate;
    let prompt_map = prompts::load();
    let workspace_binding =
        workspace_root.and_then(|root| crate::paths::WorkspaceBinding::capture(root).ok());
    let agent_definitions = workspace_binding
        .as_ref()
        .map(agents::load_for_workspace_binding)
        .unwrap_or_else(agents::load);
    let theme_map = themes::load();
    let theme_name = crate::session::storage::load_theme_name();
    #[cfg(feature = "memory")]
    let memory = crate::extras::memory::Mem::open().context_block();
    ContextFiles {
        workspace_root: workspace_root
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default(),
        agents,
        prompts: prompt_map,
        current_prompt: None,
        current_prompt_name: None,
        agent_definitions,
        current_agent_name: None,
        current_agent_explicit: false,
        themes: theme_map,
        current_theme_name: theme_name,
        extra_files: Vec::new(),
        extra_file_contents: HashMap::new(),
        one_shot_restore: None,
        chain_declined: Vec::new(),
        #[cfg(feature = "memory")]
        memory,
        #[cfg(feature = "archmd")]
        architecture,
    }
}

fn load_file(path: &PathBuf) -> Option<String> {
    if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    }
}

/// Maximum total bytes of repository context files (AGENTS.md, CLAUDE.md,
/// ARCHITECTURE.md) to load into the system prompt. Prevents a planted or
/// oversized file from blowing up the context window.
const MAX_ANCESTOR_CONTEXT_BYTES: usize = 524_288;

/// Reads context only from the explicitly selected workspace root. Global
/// application context is loaded separately; parent directories outside the
/// workspace capability must never influence the prompt.
fn walk_context_files(workspace_root: Option<&Path>) -> (Option<String>, Option<String>) {
    let mut agent_parts: SmallVec<[String; 4]> = SmallVec::new();
    #[cfg_attr(not(feature = "archmd"), allow(unused_mut))]
    let mut arch_parts: SmallVec<[String; 4]> = SmallVec::new();
    let mut total_bytes: usize = 0;

    let global_agents = storage::agents_path();
    if let Some(content) = load_file(&global_agents)
        && !content.trim().is_empty()
    {
        total_bytes += content.len();
        agent_parts.push(format!("# Global AGENTS.md\n{}", content));
    }

    #[cfg(feature = "archmd")]
    {
        let global_arch = storage::architecture_path();
        if let Some(content) = load_file(&global_arch)
            && !content.trim().is_empty()
        {
            total_bytes += content.len();
            arch_parts.push(format!("# Global ARCHITECTURE.md\n{}", content));
        }
    }

    if let Some(cwd) = workspace_root {
        let mut current = Some(cwd);
        while let Some(dir) = current {
            if total_bytes >= MAX_ANCESTOR_CONTEXT_BYTES {
                tracing::warn!(
                    "ancestor context files exceed {} bytes, stopping traversal",
                    MAX_ANCESTOR_CONTEXT_BYTES
                );
                break;
            }
            for name in &["AGENTS.md", "CLAUDE.md"] {
                let path = dir.join(name);
                if let Some(content) = load_file(&path)
                    && !content.trim().is_empty()
                {
                    total_bytes += content.len();
                    agent_parts.push(format!("# {} ({})\n{}", name, dir.display(), content));
                }
            }
            #[cfg(feature = "archmd")]
            {
                let path = dir.join("ARCHITECTURE.md");
                if let Some(content) = load_file(&path)
                    && !content.trim().is_empty()
                {
                    total_bytes += content.len();
                    arch_parts.push(format!(
                        "# ARCHITECTURE.md ({})\n{}",
                        dir.display(),
                        content
                    ));
                }
            }
            current = None;
        }
    }

    let agents = if agent_parts.is_empty() {
        None
    } else {
        Some(agent_parts.join("\n\n"))
    };
    let architecture = if arch_parts.is_empty() {
        None
    } else {
        Some(arch_parts.join("\n\n"))
    };
    (agents, architecture)
}

#[cfg(test)]
mod repository_context_tests {
    use super::*;

    #[test]
    fn unbound_loader_does_not_read_context_above_selected_workspace() {
        let parent = std::env::temp_dir().join(format!(
            "mini-agent-context-boundary-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = parent.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(parent.join("AGENTS.md"), "PARENT_CONTEXT_MARKER").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "WORKSPACE_CONTEXT_MARKER").unwrap();

        let (agents, _) = walk_context_files(Some(&workspace));

        let agents = agents.expect("workspace AGENTS.md should be loaded");
        assert!(agents.contains("WORKSPACE_CONTEXT_MARKER"));
        assert!(!agents.contains("PARENT_CONTEXT_MARKER"));
        std::fs::remove_dir_all(parent).unwrap();
    }
}

/// Read the architecture document for an explicit workspace.
///
/// There is deliberately no process-CWD variant: workspace authority is passed
/// explicitly so a worktree switch never depends on the process working
/// directory (see `PermissionChecker::rebind_working_dir`).
#[cfg(feature = "archmd")]
pub(crate) fn load_architecture_from(workspace_root: &Path) -> Option<String> {
    walk_context_files(Some(workspace_root)).1
}
