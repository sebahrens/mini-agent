use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use include_dir::Dir;
use smallvec::SmallVec;

use crate::session::storage;

pub mod agents;
pub(crate) mod commands;
pub(crate) mod instructions;
#[cfg(any(test, feature = "subagents"))]
pub(crate) mod mentions;
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

#[cfg(test)]
thread_local! {
    /// Count of `prompts` directory reads on this thread (test seam).
    pub(crate) static PROMPT_DIR_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn load_dir_files(dir: &Path, ext: &str) -> Vec<(String, String)> {
    #[cfg(test)]
    if dir.file_name().is_some_and(|name| name == "prompts") {
        PROMPT_DIR_READS.with(|reads| reads.set(reads.get() + 1));
    }
    let mut results = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            // Windows can report NotFound for enumeration of an existing
            // non-directory. Suppress only an actually absent optional path.
            let absent = error.kind() == std::io::ErrorKind::NotFound
                && std::fs::symlink_metadata(dir)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if !absent {
                tracing::warn!(path = %dir.display(), %error, "could not read prompt/theme directory");
            }
            return results;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "could not read prompt/theme directory entry");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == ext)
            && let Some(name) = path.file_stem().and_then(|s| s.to_str())
        {
            match std::fs::read_to_string(&path) {
                Ok(content) => results.push((name.to_string(), content)),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "could not read prompt/theme file")
                }
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
    /// Authored root snapshots preserve include origin; diagnostics never reread them.
    pub(crate) instruction_roots: Vec<instructions::InstructionRoot>,
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
    /// Runtime-only discovery metadata, shared by captured context clones.
    /// Session IDs, binding identity and source/schema fingerprints partition
    /// this state; imported history never contains tool activation authority.
    #[cfg(feature = "mcp")]
    pub(crate) mcp_discovery: crate::extras::mcp::deferred::DiscoverySessions,
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
    /// provider cache when its rendered content is byte-identical. Unchanged
    /// memory files (same paths, mtimes and lengths) are not re-read: the
    /// previously rendered block is reused, so a submit only stats them.
    ///
    /// `exclude_compaction` is the session's
    /// [`crate::session::Session::active_compaction_ref`]: that summary is
    /// already replayed as the session's recap, so its daily-log copy is left
    /// out of the block.
    #[cfg(feature = "memory")]
    pub(crate) async fn refresh_memory_if_changed(
        &mut self,
        exclude_compaction: Option<String>,
    ) -> bool {
        match tokio::task::spawn_blocking(move || {
            crate::extras::memory::Mem::open()
                .context_block_excluding_cached(exclude_compaction.as_deref())
        })
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
        let (agents, architecture, instruction_roots) = if no_context_files {
            (None, None, Vec::new())
        } else {
            walk_bound_context_files(workspace)
        };
        context.workspace_root = workspace.root().to_path_buf();
        context.agents = agents;
        context.instruction_roots = instruction_roots;
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
) -> (
    Option<String>,
    Option<String>,
    Vec<instructions::InstructionRoot>,
) {
    let mut agent_parts: SmallVec<[String; 4]> = SmallVec::new();
    #[cfg_attr(not(feature = "archmd"), allow(unused_mut))]
    let mut arch_parts: SmallVec<[String; 4]> = SmallVec::new();
    let mut roots = Vec::new();
    let mut budget = ContextBudget::default();
    let global = storage::agents_path();
    if let Some((content, truncated, binding, relative, identity)) =
        load_captured_global_file(&global)
        && let Some(content) = budget.admit("global AGENTS.md", content, truncated)
    {
        roots.push(
            instructions::InstructionRoot::global(binding, relative, &content)
                .with_identity(identity),
        );
        agent_parts.push(format!("# Global AGENTS.md\n{content}"));
    }
    #[cfg(feature = "archmd")]
    if let Some((content, truncated, _, _, _)) =
        load_captured_global_file(&storage::architecture_path())
        && let Some(content) = budget.admit("global ARCHITECTURE.md", content, truncated)
    {
        arch_parts.push(format!("# Global ARCHITECTURE.md\n{content}"));
    }
    // The retained root is the entire walk. Ambient parents are intentionally
    // excluded, including when their lexical spelling is a symlink ancestor.
    let names = if cfg!(feature = "archmd") {
        &["AGENTS.md", "CLAUDE.md", "ARCHITECTURE.md"][..]
    } else {
        &["AGENTS.md", "CLAUDE.md"][..]
    };
    for (_, name, content, truncated, identity) in
        workspace.read_root_context_files(names, MAX_ANCESTOR_CONTEXT_BYTES)
    {
        let Some(content) = budget.admit(&name, content, truncated) else {
            continue;
        };
        if name == "ARCHITECTURE.md" {
            #[cfg(feature = "archmd")]
            arch_parts.push(format!(
                "# ARCHITECTURE.md ({})\n{}",
                workspace.root().display(),
                content
            ));
        } else {
            let Ok(root) =
                instructions::InstructionRoot::workspace(workspace, PathBuf::from(&name), &content)
            else {
                continue;
            };
            roots.push(root.with_identity(identity));
            agent_parts.push(format!(
                "# {} ({})\n{}",
                name,
                workspace.root().display(),
                content
            ));
        }
    }
    (
        (!agent_parts.is_empty()).then(|| agent_parts.join("\n\n")),
        (!arch_parts.is_empty()).then(|| arch_parts.join("\n\n")),
        roots,
    )
}
fn load_captured_global_file(
    path: &Path,
) -> Option<(
    String,
    bool,
    Arc<crate::paths::WorkspaceBinding>,
    PathBuf,
    crate::fs::CheckedMetadata,
)> {
    let binding = Arc::new(crate::paths::WorkspaceBinding::capture(path.parent()?).ok()?);
    let relative = PathBuf::from(path.file_name()?);
    let file = binding.open_regular_relative(&relative).ok()?;
    let identity = crate::fs::checked_file_metadata(&file).ok()?;
    let (content, truncated) = crate::paths::read_utf8_prefix(file, MAX_ANCESTOR_CONTEXT_BYTES)
        .ok()
        .flatten()?;
    binding.validate().ok()?;
    Some((content, truncated, binding, relative, identity))
}

#[cfg(test)]
pub fn load(no_context_files: bool) -> ContextFiles {
    let workspace_root = std::env::current_dir().ok();
    load_for_workspace(no_context_files, workspace_root.as_deref())
}

/// Load context for one explicitly selected workspace without changing the
/// process working directory. ACP sessions use this entry point so concurrent
/// roots cannot affect one another.
#[cfg(test)]
pub fn load_for_workspace(no_context_files: bool, workspace_root: Option<&Path>) -> ContextFiles {
    load_for_workspace_with(no_context_files, workspace_root, true)
}

/// Startup load against an already captured workspace binding: the context
/// walk, prompt directories and agent definitions are each read once (no
/// cwd-based load followed by a binding-based reload).
pub(crate) fn load_for_binding(
    no_context_files: bool,
    workspace: &crate::paths::WorkspaceBinding,
    read_memory: bool,
) -> ContextFiles {
    #[cfg(not(feature = "memory"))]
    let _ = read_memory;
    if let Err(e) = prompts::ensure_global() {
        tracing::warn!("failed to install default prompts: {e}");
    }
    if let Err(e) = themes::ensure_global() {
        tracing::warn!("failed to install default themes: {e}");
    }
    let (agents, arch_candidate, instruction_roots) = if no_context_files {
        (None, None, Vec::new())
    } else {
        walk_bound_context_files(workspace)
    };
    #[cfg(feature = "archmd")]
    let architecture = arch_candidate;
    #[cfg(not(feature = "archmd"))]
    let _ = arch_candidate;
    #[cfg(feature = "memory")]
    let memory = read_memory
        .then(|| crate::extras::memory::Mem::open().context_block())
        .flatten();
    ContextFiles {
        #[cfg(feature = "mcp")]
        mcp_discovery: Default::default(),
        workspace_root: workspace.root().to_path_buf(),
        agents,
        instruction_roots,
        prompts: prompts::load_for_workspace_binding(workspace),
        current_prompt: None,
        current_prompt_name: None,
        agent_definitions: agents::load_for_workspace_binding(workspace),
        current_agent_name: None,
        current_agent_explicit: false,
        themes: themes::load(),
        current_theme_name: crate::session::storage::load_theme_name(),
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

#[cfg(test)]
fn load_for_workspace_with(
    no_context_files: bool,
    workspace_root: Option<&Path>,
    read_memory: bool,
) -> ContextFiles {
    #[cfg(not(feature = "memory"))]
    let _ = read_memory;
    if let Err(e) = prompts::ensure_global() {
        tracing::warn!("failed to install default prompts: {e}");
    }
    if let Err(e) = themes::ensure_global() {
        tracing::warn!("failed to install default themes: {e}");
    }
    let (agents, arch_candidate, instruction_roots) = if no_context_files {
        (None, None, Vec::new())
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
    let memory = read_memory
        .then(|| crate::extras::memory::Mem::open().context_block())
        .flatten();
    ContextFiles {
        #[cfg(feature = "mcp")]
        mcp_discovery: Default::default(),
        workspace_root: workspace_root
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default(),
        agents,
        instruction_roots,
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

/// Maximum total bytes of context files (global and repository AGENTS.md,
/// CLAUDE.md, ARCHITECTURE.md) to load into the system prompt. Prevents a
/// planted or oversized file from blowing up the context window: a single
/// file larger than what remains is truncated, not loaded whole.
const MAX_ANCESTOR_CONTEXT_BYTES: usize = 524_288;

/// Appended where a context file was cut to fit the budget.
const CONTEXT_TRUNCATION_NOTICE: &str =
    "\n\n[context file truncated: the combined context-file limit was reached]";

/// The running total of context-file bytes admitted into the prompt
/// (mini-agent-ag4ze).
#[derive(Default)]
struct ContextBudget {
    used: usize,
}

impl ContextBudget {
    /// The part of `content` that fits in what remains, with a notice when it
    /// was cut, or `None` when it is empty or nothing remains.
    fn admit(&mut self, label: &str, content: String, truncated: bool) -> Option<String> {
        if content.trim().is_empty() {
            return None;
        }
        let remaining = MAX_ANCESTOR_CONTEXT_BYTES.saturating_sub(self.used);
        if remaining == 0 {
            tracing::warn!(
                "context file {label} skipped: context files exceed {MAX_ANCESTOR_CONTEXT_BYTES} bytes"
            );
            return None;
        }
        if content.len() <= remaining && !truncated {
            self.used += content.len();
            return Some(content);
        }
        let mut end = remaining.min(content.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        tracing::warn!(
            "context file {label} truncated to {end} bytes: context files exceed {MAX_ANCESTOR_CONTEXT_BYTES} bytes"
        );
        self.used = MAX_ANCESTOR_CONTEXT_BYTES;
        let mut kept = content;
        kept.truncate(end);
        kept.push_str(CONTEXT_TRUNCATION_NOTICE);
        Some(kept)
    }
}

/// Reads context only from the explicitly selected workspace root. Global
/// application context is loaded separately; parent directories outside the
/// workspace capability must never influence the prompt.
fn walk_context_files(
    workspace_root: Option<&Path>,
) -> (
    Option<String>,
    Option<String>,
    Vec<instructions::InstructionRoot>,
) {
    if let Some(workspace) =
        workspace_root.and_then(|root| crate::paths::WorkspaceBinding::capture(root).ok())
    {
        return walk_bound_context_files(&workspace);
    }
    let mut roots = Vec::new();
    let mut budget = ContextBudget::default();
    let agents = load_captured_global_file(&storage::agents_path()).and_then(
        |(content, truncated, binding, relative, identity)| {
            budget
                .admit("global AGENTS.md", content, truncated)
                .map(|content| {
                    roots.push(
                        instructions::InstructionRoot::global(binding, relative, &content)
                            .with_identity(identity),
                    );
                    format!("# Global AGENTS.md\n{content}")
                })
        },
    );
    #[cfg(feature = "archmd")]
    let architecture = load_captured_global_file(&storage::architecture_path()).and_then(
        |(content, truncated, _, _, _)| {
            budget
                .admit("global ARCHITECTURE.md", content, truncated)
                .map(|content| format!("# Global ARCHITECTURE.md\n{content}"))
        },
    );
    #[cfg(not(feature = "archmd"))]
    let architecture = None;
    (agents, architecture, roots)
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

        let (agents, _, _) = walk_context_files(Some(&workspace));

        let agents = agents.expect("workspace AGENTS.md should be loaded");
        assert!(agents.contains("WORKSPACE_CONTEXT_MARKER"));
        assert!(!agents.contains("PARENT_CONTEXT_MARKER"));
        std::fs::remove_dir_all(parent).unwrap();
    }

    // mini-agent-ag4ze: one oversized file is cut to the byte cap, and nothing
    // after the cap is admitted.
    #[test]
    fn a_single_oversized_context_file_is_truncated_to_the_cap() {
        let mut budget = ContextBudget::default();
        let huge = "x".repeat(MAX_ANCESTOR_CONTEXT_BYTES + 10);
        let admitted = budget.admit("AGENTS.md", huge, false).unwrap();
        assert!(admitted.ends_with(CONTEXT_TRUNCATION_NOTICE));
        assert_eq!(
            admitted.len(),
            MAX_ANCESTOR_CONTEXT_BYTES + CONTEXT_TRUNCATION_NOTICE.len()
        );
        assert!(budget.admit("CLAUDE.md", "more".into(), false).is_none());

        // A read that was already cut is marked even when it fits exactly.
        let mut budget = ContextBudget::default();
        let admitted = budget.admit("AGENTS.md", "short".into(), true).unwrap();
        assert_eq!(admitted, format!("short{CONTEXT_TRUNCATION_NOTICE}"));

        // Multi-byte text is cut on a character boundary.
        let mut budget = ContextBudget {
            used: MAX_ANCESTOR_CONTEXT_BYTES - 3,
        };
        let admitted = budget.admit("CLAUDE.md", "éé".into(), false).unwrap();
        assert_eq!(admitted, format!("é{CONTEXT_TRUNCATION_NOTICE}"));
    }

    #[test]
    fn an_oversized_workspace_file_is_never_loaded_whole() {
        let workspace =
            std::env::temp_dir().join(format!("mini-agent-context-cap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("AGENTS.md"),
            "y".repeat(2 * MAX_ANCESTOR_CONTEXT_BYTES),
        )
        .unwrap();

        let (agents, _, _) = walk_context_files(Some(&workspace));
        let agents = agents.expect("the file is loaded, truncated");
        assert!(
            agents.len() <= MAX_ANCESTOR_CONTEXT_BYTES + 256,
            "{}",
            agents.len()
        );
        assert!(agents.ends_with(CONTEXT_TRUNCATION_NOTICE));

        let binding = crate::paths::WorkspaceBinding::capture(&workspace).unwrap();
        let (agents, _, _) = walk_bound_context_files(&binding);
        let agents = agents.expect("the bound loader truncates too");
        assert!(
            agents.len() <= MAX_ANCESTOR_CONTEXT_BYTES + 256,
            "{}",
            agents.len()
        );
        assert!(agents.ends_with(CONTEXT_TRUNCATION_NOTICE));
        std::fs::remove_dir_all(workspace).unwrap();
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

#[cfg(test)]
mod file_read_diagnostic_tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let value = tracing::subscriber::with_default(subscriber, run);
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        (value, log)
    }

    #[test]
    fn file_read_diagnostics_report_unreadable_global_and_workspace_files() {
        let root = crate::tests::OwnedTestDirectory::new("prompt-theme-read-diagnostics");
        let _environment = crate::tests::ScopedProcessEnv::set(&[
            (
                "ZS_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "ZS_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "ZS_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "ZS_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ]);
        let paths = crate::paths::process_paths().unwrap();
        let workspace_dir = root.path().join("workspace");
        let project_prompts = workspace_dir.join(".zerostack/prompts");
        for directory in [
            paths.prompts_dir(),
            paths.themes_dir(),
            project_prompts.clone(),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let global_bad = paths.prompts_dir().join("global-invalid.md");
        let theme_bad = paths.themes_dir().join("theme-invalid.json");
        let project_bad = project_prompts.join("project-invalid.md");
        for path in [&global_bad, &theme_bad, &project_bad] {
            std::fs::write(path, [0xff]).unwrap();
        }
        std::fs::write(paths.prompts_dir().join("global-valid.md"), "GLOBAL_VALID").unwrap();
        std::fs::write(paths.themes_dir().join("theme-valid.json"), "{}").unwrap();
        std::fs::write(project_prompts.join("project-valid.md"), "PROJECT_VALID").unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(&workspace_dir).unwrap();
        let ((prompts, themes), log) = capture_logs(|| {
            (
                prompts::load_for_workspace_binding(&workspace),
                themes::load(),
            )
        });
        assert_eq!(
            prompts.get("global-valid").map(String::as_str),
            Some("GLOBAL_VALID")
        );
        assert_eq!(
            prompts.get("project-valid").map(String::as_str),
            Some("PROJECT_VALID")
        );
        assert_eq!(themes.get("theme-valid").map(String::as_str), Some("{}"));
        assert!(!prompts.contains_key("global-invalid"));
        assert!(!prompts.contains_key("project-invalid"));
        assert!(!themes.contains_key("theme-invalid"));
        for path in [&global_bad, &theme_bad, &project_bad] {
            assert!(log.contains(path.to_str().unwrap()), "{log}");
        }
        assert!(log.contains("could not read prompt/theme file"), "{log}");
        assert!(
            log.contains("could not read workspace prompt file"),
            "{log}"
        );
        assert!(log.contains("WARN"), "{log}");
        assert!(!log.contains("GLOBAL_VALID"));
        assert!(!log.contains("PROJECT_VALID"));
    }

    #[test]
    fn file_read_diagnostics_distinguish_absent_directories_from_read_failures() {
        let root = crate::tests::OwnedTestDirectory::new("context-directory-diagnostics");
        let missing = root.path().join("missing");
        let (loaded, log) = capture_logs(|| load_dir_files(&missing, "md"));
        assert!(loaded.is_empty());
        assert!(
            log.is_empty(),
            "a missing optional directory is normal: {log}"
        );
        let not_directory = root.path().join("not-directory");
        std::fs::write(&not_directory, "fixture").unwrap();
        let (loaded, log) = capture_logs(|| load_dir_files(&not_directory, "md"));
        assert!(loaded.is_empty());
        assert!(
            log.contains("could not read prompt/theme directory"),
            "{log}"
        );
        assert!(log.contains(not_directory.to_str().unwrap()), "{log}");
    }

    #[cfg(unix)]
    #[test]
    fn file_read_diagnostics_report_rejected_workspace_symlink_without_reading_target() {
        let root = crate::tests::OwnedTestDirectory::new("workspace-prompt-symlink-diagnostic");
        std::fs::create_dir_all(root.path().join("prompts")).unwrap();
        std::fs::write(root.path().join("target.md"), "TARGET_MUST_NOT_BE_READ").unwrap();
        let link = root.path().join("prompts/link.md");
        std::os::unix::fs::symlink("../target.md", &link).unwrap();
        std::fs::write(root.path().join("prompts/good.md"), "good").unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let (loaded, log) = capture_logs(|| {
            workspace
                .read_relative_dir_files(Path::new("prompts"), "md")
                .unwrap()
        });
        assert_eq!(loaded, vec![("good".into(), "good".into())]);
        assert!(log.contains(link.to_str().unwrap()), "{log}");
        assert!(
            log.contains("could not read workspace prompt file"),
            "{log}"
        );
        assert!(!log.contains("TARGET_MUST_NOT_BE_READ"));
    }
}

#[cfg(test)]
mod startup_binding_load_tests {
    use super::*;

    #[test]
    fn binding_load_reads_prompt_directory_once_and_matches_rebind() {
        let root = crate::tests::OwnedTestDirectory::new("startup-binding-load");
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
        let workspace = crate::paths::WorkspaceBinding::capture(&workspace_root).unwrap();

        PROMPT_DIR_READS.with(|reads| reads.set(0));
        let direct = load_for_binding(true, &workspace, false);
        assert_eq!(PROMPT_DIR_READS.with(|reads| reads.get()), 1);

        let rebound =
            load_for_workspace(true, Some(&workspace_root)).for_workspace_binding(true, &workspace);
        assert_eq!(
            direct
                .prompts
                .keys()
                .collect::<std::collections::BTreeSet<_>>(),
            rebound
                .prompts
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert_eq!(direct.workspace_root, rebound.workspace_root);
        assert_eq!(
            direct
                .agent_definitions
                .keys()
                .collect::<std::collections::BTreeSet<_>>(),
            rebound
                .agent_definitions
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }
}
