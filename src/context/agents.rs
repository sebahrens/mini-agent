use std::collections::HashMap;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/agents");
const MAX_AGENT_PROMPT_BYTES: usize = 256 * 1024;
const MAX_AGENT_DESCRIPTION_CHARS: usize = 160;
const MAX_AGENT_MODEL_CHARS: usize = 256;
const MAX_PROJECT_NOTES_BYTES: usize = 64 * 1024;
const MAX_PROJECT_AGENT_ENTRIES: usize = 1_024;
const MAX_PROJECT_AGENT_FILES: usize = 128;
// Low to high priority: retain the legacy directory's override precedence.
const PROJECT_AGENT_LAYERS: [&str; 3] =
    [".claude/agents", ".mini-agent/agents", ".zerostack/agents"];
const MAX_DECLARED_TOOLS: usize = 128;
const MAX_TOOL_NAME_CHARS: usize = 64;
const MAX_NOTICE_NAMES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentTool {
    Read,
    Grep,
    FindFiles,
    ListDir,
    Write,
    Edit,
    Shell,
    #[cfg(feature = "js")]
    Js,
    #[cfg(feature = "skills")]
    SkillsSearch,
    #[cfg(feature = "memory")]
    MemoryRead,
    #[cfg(feature = "memory")]
    MemorySearch,
}

impl AgentTool {
    fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "read" => Some(Self::Read),
            "grep" => Some(Self::Grep),
            "glob" | "find" | "find_files" | "findfiles" => Some(Self::FindFiles),
            "list" | "list_dir" | "listdir" => Some(Self::ListDir),
            "write" => Some(Self::Write),
            "edit" => Some(Self::Edit),
            "bash" | "shell" => Some(Self::Shell),
            #[cfg(feature = "js")]
            "js" | "javascript" => Some(Self::Js),
            #[cfg(feature = "skills")]
            "skills_search" | "skillssearch" => Some(Self::SkillsSearch),
            #[cfg(feature = "memory")]
            "memory_read" | "memoryread" => Some(Self::MemoryRead),
            #[cfg(feature = "memory")]
            "memory_search" | "memorysearch" => Some(Self::MemorySearch),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Grep => "grep",
            Self::FindFiles => "find_files",
            Self::ListDir => "list_dir",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Shell => "shell",
            #[cfg(feature = "js")]
            Self::Js => "js",
            #[cfg(feature = "skills")]
            Self::SkillsSearch => "skills_search",
            #[cfg(feature = "memory")]
            Self::MemoryRead => "memory_read",
            #[cfg(feature = "memory")]
            Self::MemorySearch => "memory_search",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentTurnBudget {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AgentMetadata {
    mode: Option<String>,
    description: Option<String>,
    tools: Option<Vec<AgentTool>>,
    required_tools: Vec<String>,
    tool_narrowing_notice: Option<String>,
    model: Option<String>,
    turn_budget: Option<AgentTurnBudget>,
    /// The turn budget came from the deprecated `effort` alias.
    deprecated_effort_alias: bool,
    unknown_keys: Vec<String>,
}

type NormalizedAgentDefinition = (String, String, AgentMetadata);

pub(crate) fn valid_agent_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn normalize_agent_definition(
    name: String,
    prompt: String,
) -> Result<NormalizedAgentDefinition, &'static str> {
    if !valid_agent_name(&name) {
        return Err("filename stem is not a valid agent type");
    }
    if prompt.len() > MAX_AGENT_PROMPT_BYTES {
        return Err("definition exceeds the 256 KiB limit");
    }
    let prompt = prompt.strip_prefix('\u{feff}').unwrap_or(&prompt);
    let Some(first_line_end) = prompt.find('\n') else {
        return (!prompt.trim().is_empty() && prompt.trim_end_matches('\r') != "---")
            .then(|| (name, prompt.to_string(), AgentMetadata::default()))
            .ok_or_else(|| {
                if prompt.trim_end_matches('\r') == "---" {
                    "frontmatter is unterminated"
                } else {
                    "prompt body is empty"
                }
            });
    };
    if prompt[..first_line_end].trim_end_matches('\r') != "---" {
        return (!prompt.trim().is_empty())
            .then(|| (name, prompt.to_string(), AgentMetadata::default()))
            .ok_or("prompt body is empty");
    }

    let yaml_start = first_line_end + 1;
    let mut offset = yaml_start;
    let mut yaml_end = None;
    let mut body_start = None;
    for line in prompt[yaml_start..].split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            yaml_end = Some(offset);
            body_start = Some(offset + line.len());
            break;
        }
        offset += line.len();
    }
    let (Some(yaml_end), Some(body_start)) = (yaml_end, body_start) else {
        return Err("frontmatter is unterminated");
    };
    let metadata: serde_yaml_ng::Value = serde_yaml_ng::from_str(&prompt[yaml_start..yaml_end])
        .map_err(|_| "frontmatter is not valid YAML")?;
    let mapping = metadata
        .as_mapping()
        .ok_or("frontmatter must be a YAML mapping")?;
    if let Some(declared_name) = mapping.get(serde_yaml_ng::Value::String("name".into()))
        && declared_name.as_str() != Some(&name)
    {
        return Err("frontmatter name does not match the filename stem");
    }
    let mode = mapping
        .get(serde_yaml_ng::Value::String("mode".into()))
        .map(|value| {
            let mode = value.as_str().ok_or("frontmatter mode must be a string")?;
            valid_agent_name(mode)
                .then(|| mode.to_string())
                .ok_or("frontmatter mode is not a valid prompt name")
        })
        .transpose()?;
    let description = mapping
        .get(serde_yaml_ng::Value::String("description".into()))
        .map(|value| {
            let description = value
                .as_str()
                .ok_or("frontmatter description must be a string")?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if description.is_empty() {
                return Err("frontmatter description must not be empty");
            }
            if description.chars().count() > MAX_AGENT_DESCRIPTION_CHARS {
                return Err("frontmatter description exceeds the 160 character limit");
            }
            Ok(description)
        })
        .transpose()?;
    let parsed_tools = mapping
        .get(serde_yaml_ng::Value::String("tools".into()))
        .map(|value| parse_agent_tools(value, mode.as_deref() == Some("write")))
        .transpose()?;
    let (tools, tool_narrowing_notice) = match parsed_tools {
        Some((tools, notice)) => (Some(tools), notice),
        None => (None, None),
    };
    let required_tools = mapping
        .get(serde_yaml_ng::Value::String("required_tools".into()))
        .map(parse_required_agent_tools)
        .transpose()?
        .unwrap_or_default();
    let model = mapping
        .get(serde_yaml_ng::Value::String("model".into()))
        .map(|value| {
            let model = value
                .as_str()
                .ok_or("frontmatter model must be a string")?
                .trim();
            if model.is_empty() {
                return Err("frontmatter model must not be empty");
            }
            if model.chars().count() > MAX_AGENT_MODEL_CHARS || model.chars().any(char::is_control)
            {
                return Err("frontmatter model is invalid or exceeds the 256 character limit");
            }
            Ok(model.to_string())
        })
        .transpose()?;
    let parse_turn_budget = |key: &str| {
        mapping
            .get(serde_yaml_ng::Value::String(key.into()))
            .map(|value| match value.as_str() {
                Some("low") => Ok(AgentTurnBudget::Low),
                Some("medium") => Ok(AgentTurnBudget::Medium),
                Some("high") => Ok(AgentTurnBudget::High),
                Some(_) if key == "effort" => {
                    Err("frontmatter effort (deprecated alias of turn_budget) must be low, medium, or high")
                }
                Some(_) => Err("frontmatter turn_budget must be low, medium, or high"),
                None if key == "effort" => {
                    Err("frontmatter effort (deprecated alias of turn_budget) must be a string")
                }
                None => Err("frontmatter turn_budget must be a string"),
            })
            .transpose()
    };
    let turn_budget = parse_turn_budget("turn_budget")?;
    let deprecated_effort = parse_turn_budget("effort")?;
    if turn_budget.is_some() && deprecated_effort.is_some() {
        return Err(
            "frontmatter sets both turn_budget and its deprecated alias effort; keep only turn_budget",
        );
    }
    let deprecated_effort_alias = deprecated_effort.is_some();
    let turn_budget = turn_budget.or(deprecated_effort);
    let known_keys = [
        "name",
        "mode",
        "description",
        "tools",
        "required_tools",
        "model",
        "turn_budget",
        "effort",
    ];
    let mut unknown_keys = mapping
        .keys()
        .filter_map(serde_yaml_ng::Value::as_str)
        .filter(|key| !known_keys.contains(key))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if mapping.keys().any(|key| key.as_str().is_none()) {
        return Err("frontmatter keys must be strings");
    }
    unknown_keys.sort_unstable();
    let body = prompt[body_start..].trim_start_matches(['\r', '\n']);
    (!body.trim().is_empty())
        .then(|| {
            (
                name,
                body.to_string(),
                AgentMetadata {
                    mode,
                    description,
                    tools,
                    required_tools,
                    tool_narrowing_notice,
                    model,
                    turn_budget,
                    deprecated_effort_alias,
                    unknown_keys,
                },
            )
        })
        .ok_or("prompt body is empty")
}

fn parse_required_agent_tools(value: &serde_yaml_ng::Value) -> Result<Vec<String>, &'static str> {
    let names = if let Some(list) = value.as_sequence() {
        list.iter()
            .map(|name| {
                name.as_str()
                    .ok_or("required_tools entries must be strings")
            })
            .collect::<Result<Vec<_>, _>>()?
    } else if let Some(list) = value.as_str() {
        list.split(',').collect()
    } else {
        return Err("required_tools must be a comma-separated string or string list");
    };
    if names.len() > MAX_DECLARED_TOOLS {
        return Err("required_tools exceeds the 128 entry limit");
    }
    let mut required = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty()
            || name.len() > MAX_TOOL_NAME_CHARS
            || !name.is_ascii()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        {
            return Err("required_tools names must be ASCII tool identifiers of at most 64 bytes");
        }
        let name =
            AgentTool::parse(name).map_or_else(|| name.to_string(), |tool| tool.name().to_string());
        if !required.contains(&name) {
            required.push(name);
        }
    }
    Ok(required)
}

/// Compatibility for the installed code-indexer persona, which predates
/// required_tools metadata. Recognize its explicit Tools-section CLI contract,
/// never a prose mention or an example elsewhere in a persona.
#[cfg(any(test, feature = "subagents"))]
fn requires_indexer_cli(prompt: &str) -> bool {
    let normalized = prompt.replace("\r\n", "\n");
    let Some(section) = normalized.strip_prefix("## Tools\n").or_else(|| {
        normalized
            .split_once("\n## Tools\n")
            .map(|(_, section)| section)
    }) else {
        return false;
    };
    let section = section.split("\n## ").next().unwrap_or(section);
    let Some(code) = section
        .trim_start()
        .strip_prefix("All queries use a single CLI script:\n\n```bash\n")
    else {
        return false;
    };
    let Some((code, _)) = code.split_once("\n```") else {
        return false;
    };
    code.lines()
        .any(|line| line.trim() == "CLI=\".claude/code-indexer_state/code_indexer_cli.py\"")
        && code
            .lines()
            .any(|line| line.trim_start().starts_with("python3 $CLI batch "))
}

fn parse_agent_tools(
    value: &serde_yaml_ng::Value,
    write_mode: bool,
) -> Result<(Vec<AgentTool>, Option<String>), &'static str> {
    let names = if let Some(list) = value.as_sequence() {
        list.iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .ok_or("frontmatter tools entries must be strings")
            })
            .collect::<Result<Vec<_>, _>>()?
    } else if let Some(list) = value.as_str() {
        list.split(',').map(str::trim).collect()
    } else {
        return Err("frontmatter tools must be a comma-separated string or string list");
    };

    if names.len() > MAX_DECLARED_TOOLS {
        return Err("frontmatter tools exceeds the 128 entry limit");
    }
    let mut tools = Vec::new();
    let mut ignored = Vec::new();
    for name in names {
        if name.is_empty() {
            return Err("frontmatter tools contains an empty tool name");
        }
        if name.chars().count() > MAX_TOOL_NAME_CHARS
            || !name.is_ascii()
            || name.chars().any(char::is_control)
        {
            return Err("frontmatter tool names must be printable ASCII and at most 64 characters");
        }
        if let Some(tool) = AgentTool::parse(name).filter(|tool| {
            write_mode || !matches!(tool, AgentTool::Write | AgentTool::Edit | AgentTool::Shell)
        }) {
            if !tools.contains(&tool) {
                tools.push(tool);
            }
        } else if !ignored.contains(&name) {
            ignored.push(name);
        }
    }
    let notice = (!ignored.is_empty()).then(|| {
        let names = ignored.iter().take(MAX_NOTICE_NAMES).copied().collect::<Vec<_>>().join(", ");
        let more = if ignored.len() > MAX_NOTICE_NAMES {
            format!(" (and {} more)", ignored.len() - MAX_NOTICE_NAMES)
        } else { String::new() };
        if write_mode {
            format!("tools {names}{more} ignored: write personas support only installed tools")
        } else {
            format!("tools {names}{more} ignored: subagents are read-only and support only installed read tools")
        }
    });
    Ok((tools, notice))
}

#[cfg(test)]
pub(crate) fn parse_for_test(name: &str, content: &str) -> AgentDefinition {
    let mut definitions = HashMap::new();
    merge_definitions(
        &mut definitions,
        [normalize_agent_definition(name.to_string(), content.to_string()).unwrap()],
        AgentDefinitionSource::Embedded,
    );
    definitions.remove(name).unwrap()
}

/// Tool narrowings declared by the compiled-in personas.
#[cfg(all(test, feature = "subagents"))]
pub(crate) fn embedded_tool_narrowings_for_test() -> Vec<Option<Vec<AgentTool>>> {
    normalize_agent_definitions(crate::context::load_embedded_files(&EMBEDDED, "md"))
        .into_iter()
        .map(|(_, _, metadata)| metadata.tools)
        .collect()
}

fn normalize_agent_definitions(
    definitions: impl IntoIterator<Item = (String, String)>,
) -> Vec<NormalizedAgentDefinition> {
    definitions
        .into_iter()
        .filter_map(|(name, prompt)| normalize_agent_definition(name, prompt).ok())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentDefinitionSource {
    Embedded,
    UserGlobal,
    ProjectOverride { directory: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub prompt: String,
    /// Optional prompt mode selected when this persona becomes the main agent.
    pub mode: Option<String>,
    /// Optional bounded description used by the task tool's `agent_type` schema.
    pub(crate) description: Option<String>,
    /// Optional narrowing of the read-only tools installed in specialist children.
    pub(crate) tools: Option<Vec<AgentTool>>,
    pub(crate) required_tools: Vec<String>,
    /// Optional raw model id or configured quick-model alias for specialist children.
    pub(crate) model: Option<String>,
    /// Optional `turn_budget` tier (formerly `effort`) that narrows the global
    /// child turn cap. Unrelated to the model's `[reasoning] effort`.
    pub(crate) turn_budget: Option<AgentTurnBudget>,
    pub source: AgentDefinitionSource,
    project_notes_path: Option<PathBuf>,
    ignored_definition_notices: Vec<String>,
}

fn merge_definitions(
    agents: &mut HashMap<String, AgentDefinition>,
    definitions: impl IntoIterator<Item = NormalizedAgentDefinition>,
    source: AgentDefinitionSource,
) {
    for (name, prompt, metadata) in definitions {
        warn_metadata(&name, &source, &metadata);
        let notices = metadata_notices(&metadata);
        agents.insert(
            name,
            AgentDefinition {
                prompt,
                mode: metadata.mode,
                description: metadata.description,
                tools: metadata.tools,
                required_tools: metadata.required_tools,
                model: metadata.model,
                turn_budget: metadata.turn_budget,
                source: source.clone(),
                project_notes_path: None,
                ignored_definition_notices: notices,
            },
        );
    }
}

impl AgentDefinition {
    #[cfg(any(test, feature = "subagents"))]
    pub(crate) fn required_tool_names(&self) -> Vec<String> {
        let mut tools = self.required_tools.clone();
        if requires_indexer_cli(&self.prompt) && !tools.iter().any(|tool| tool == "shell") {
            tools.push("shell".to_string());
        }
        tools
    }

    pub(crate) fn list_entry(&self, name: &str) -> String {
        let notice = self
            .ignored_definition_notices
            .iter()
            .find(|notice| notice.starts_with("[specialist metadata: tools "));
        match notice {
            Some(notice) => format!("  {name} — {notice}"),
            None => format!("  {name}"),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(prompt: &str, mode: Option<&str>) -> Self {
        Self {
            prompt: prompt.to_string(),
            mode: mode.map(str::to_string),
            description: None,
            tools: None,
            required_tools: Vec::new(),
            model: None,
            turn_budget: None,
            source: AgentDefinitionSource::Embedded,
            project_notes_path: None,
            ignored_definition_notices: Vec::new(),
        }
    }

    #[cfg(any(test, feature = "subagents"))]
    pub fn project_override_path(&self, name: &str) -> Option<PathBuf> {
        match &self.source {
            AgentDefinitionSource::ProjectOverride { directory } => {
                Some(directory.join(format!("{name}.md")))
            }
            AgentDefinitionSource::Embedded | AgentDefinitionSource::UserGlobal => None,
        }
    }

    pub(crate) fn source_description(&self, name: &str) -> String {
        let source = match &self.source {
            AgentDefinitionSource::Embedded => "compiled-in default".to_string(),
            AgentDefinitionSource::UserGlobal => "user-global configuration".to_string(),
            AgentDefinitionSource::ProjectOverride { directory } => format!(
                "trusted project override {}",
                directory.join(format!("{name}.md")).display()
            ),
        };
        match &self.project_notes_path {
            Some(path) => format!("{source} with trusted project notes {}", path.display()),
            None => source,
        }
    }

    #[cfg(any(test, feature = "subagents"))]
    pub(crate) fn result_notice(&self, name: &str) -> Option<String> {
        let mut notices = self.ignored_definition_notices.clone();
        if let Some(path) = self.project_override_path(name) {
            notices.push(format!(
                "[specialist source: project override {}]",
                path.display()
            ));
        }
        if let Some(path) = &self.project_notes_path {
            notices.push(format!(
                "[specialist notes: trusted project file {}]",
                path.display()
            ));
        }
        (!notices.is_empty()).then(|| notices.join("\n"))
    }

    #[cfg(any(test, feature = "subagents"))]
    fn one_line_description(&self) -> String {
        if let Some(description) = &self.description {
            return description.clone();
        }
        let first_paragraph = self
            .prompt
            .split("\n\n")
            .find(|paragraph| !paragraph.trim().is_empty())
            .unwrap_or("Specialized read-only investigation")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let sentence_end = first_paragraph
            .find(". ")
            .map(|index| index + 1)
            .unwrap_or(first_paragraph.len());
        let sentence = &first_paragraph[..sentence_end];
        let mut chars = sentence.chars();
        let description = chars
            .by_ref()
            .take(MAX_AGENT_DESCRIPTION_CHARS)
            .collect::<String>();
        if chars.next().is_some() {
            format!("{}…", description.trim_end())
        } else {
            description
        }
    }
}

fn metadata_notices(metadata: &AgentMetadata) -> Vec<String> {
    let mut notices = unknown_key_notices(&metadata.unknown_keys);
    if let Some(notice) = &metadata.tool_narrowing_notice {
        notices.push(format!("[specialist metadata: {notice}]"));
    }
    notices
}

fn unknown_key_notices(keys: &[String]) -> Vec<String> {
    if keys.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "[specialist metadata: unknown frontmatter keys ignored: {}]",
            keys.join(", ")
        )]
    }
}

fn warn_metadata(name: &str, source: &AgentDefinitionSource, metadata: &AgentMetadata) {
    if let Some(notice) = &metadata.tool_narrowing_notice {
        tracing::warn!(
            agent_type = name,
            notice,
            "narrowing specialist tools to supported read-only set"
        );
    }
    if metadata.unknown_keys.is_empty() && !metadata.deprecated_effort_alias {
        return;
    }
    let source = match source {
        AgentDefinitionSource::Embedded => "compiled-in default".to_string(),
        AgentDefinitionSource::UserGlobal => "user-global configuration".to_string(),
        AgentDefinitionSource::ProjectOverride { directory } => directory.display().to_string(),
    };
    if !metadata.unknown_keys.is_empty() {
        tracing::warn!(
            agent_type = name,
            source = %source,
            unknown_keys = %metadata.unknown_keys.join(", "),
            "ignoring unknown specialist frontmatter keys"
        );
    }
    if metadata.deprecated_effort_alias {
        tracing::warn!(
            agent_type = name,
            source = %source,
            "specialist frontmatter `effort` is deprecated; rename it to `turn_budget` \
             (it scales the child turn cap, not model reasoning effort)"
        );
    }
}

fn project_definitions_trusted(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> bool {
    crate::config::load::project_config_is_trusted_in_workspace(paths, workspace)
}

fn merge_project_definitions(
    agents: &mut HashMap<String, AgentDefinition>,
    definitions: impl IntoIterator<Item = (String, Result<String, String>)>,
    directory: PathBuf,
    trusted: bool,
) {
    if !trusted {
        return;
    }
    let mut agent_definitions = Vec::new();
    let mut project_notes = None;
    for (name, content) in definitions {
        if name == ".notes" {
            project_notes = Some(content);
        } else {
            agent_definitions.push((name, content));
        }
    }
    merge_external_definitions(
        agents,
        agent_definitions,
        &directory,
        AgentDefinitionSource::ProjectOverride {
            directory: directory.clone(),
        },
        "project",
    );
    if let Some(notes) = project_notes {
        append_project_notes(agents, notes, &directory.join(".notes.md"));
    }
}

fn append_project_notes(
    agents: &mut HashMap<String, AgentDefinition>,
    content: Result<String, String>,
    path: &Path,
) {
    let notes = match content {
        Ok(notes) if notes.len() <= MAX_PROJECT_NOTES_BYTES => notes,
        Ok(_) => {
            let reason = format!("exceeds the {MAX_PROJECT_NOTES_BYTES}-byte limit");
            warn_and_record_ignored_notes(agents, path, &reason);
            return;
        }
        Err(reason) => {
            warn_and_record_ignored_notes(agents, path, &reason);
            return;
        }
    };
    let notes = notes.strip_prefix('\u{feff}').unwrap_or(&notes).trim();
    if notes.is_empty() {
        warn_and_record_ignored_notes(agents, path, "file is empty");
        return;
    }

    let addition = format!("\n\n---\n\n## Project notes\n\n{notes}");
    for (name, definition) in agents.iter_mut() {
        if definition.prompt.len().saturating_add(addition.len()) > MAX_AGENT_PROMPT_BYTES {
            let reason = "combined persona and project notes exceed the 256 KiB prompt limit";
            tracing::warn!(
                agent_type = name,
                definition = %path.display(),
                reason,
                "ignoring project notes for specialist"
            );
            definition.ignored_definition_notices.push(format!(
                "[specialist notes ignored: {} ({reason})]",
                path.display()
            ));
            continue;
        }
        definition.prompt.push_str(&addition);
        definition.project_notes_path = Some(path.to_path_buf());
    }
}

fn warn_and_record_ignored_notes(
    agents: &mut HashMap<String, AgentDefinition>,
    path: &Path,
    reason: &str,
) {
    tracing::warn!(
        definition = %path.display(),
        reason,
        "ignoring invalid project specialist notes"
    );
    for definition in agents.values_mut() {
        definition.ignored_definition_notices.push(format!(
            "[specialist notes ignored: {} ({reason})]",
            path.display()
        ));
    }
}

fn merge_external_definitions(
    agents: &mut HashMap<String, AgentDefinition>,
    definitions: impl IntoIterator<Item = (String, Result<String, String>)>,
    directory: &Path,
    source: AgentDefinitionSource,
    source_label: &str,
) {
    for (name, content) in definitions {
        let path = directory.join(format!("{name}.md"));
        let normalized = content.and_then(|prompt| {
            normalize_agent_definition(name.clone(), prompt).map_err(str::to_string)
        });
        match normalized {
            Ok((name, prompt, metadata)) => {
                warn_metadata(&name, &source, &metadata);
                let notices = metadata_notices(&metadata);
                agents.insert(
                    name,
                    AgentDefinition {
                        prompt,
                        mode: metadata.mode,
                        description: metadata.description,
                        tools: metadata.tools,
                        required_tools: metadata.required_tools,
                        model: metadata.model,
                        turn_budget: metadata.turn_budget,
                        source: source.clone(),
                        project_notes_path: None,
                        ignored_definition_notices: notices,
                    },
                );
            }
            Err(reason) => {
                tracing::warn!(
                    agent_type = name,
                    definition = %path.display(),
                    source = source_label,
                    reason,
                    "ignoring invalid specialist definition"
                );
                if let Some(fallback) = agents.get_mut(&name) {
                    let fallback_source = fallback.source_description(&name);
                    fallback.ignored_definition_notices.push(format!(
                        "[specialist source: {source_label} definition ignored: {} ({reason}); using {fallback_source}]",
                        path.display()
                    ));
                }
            }
        }
    }
}

fn load_base(paths: &crate::paths::AppPaths) -> HashMap<String, AgentDefinition> {
    let mut agents = HashMap::new();
    merge_definitions(
        &mut agents,
        normalize_agent_definitions(crate::context::load_embedded_files(&EMBEDDED, "md")),
        AgentDefinitionSource::Embedded,
    );
    merge_external_definitions(
        &mut agents,
        crate::context::load_dir_files_bounded_status(
            &paths.agents_dir(),
            "md",
            MAX_AGENT_PROMPT_BYTES,
        ),
        &paths.agents_dir(),
        AgentDefinitionSource::UserGlobal,
        "user",
    );
    agents
}

/// Load all agent type definitions. Priority (highest wins):
///   project override (.zerostack/agents → .mini-agent/agents → .claude/agents)
///   → user global (data_dir/agents/<name>.md)
///   → compiled-in default (data/agents/<name>.md)
pub fn load() -> HashMap<String, AgentDefinition> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    if let Some(root) = paths.project_dir.as_deref().and_then(Path::parent)
        && let Ok(workspace) = crate::paths::WorkspaceBinding::capture(root)
    {
        return (*load_for_paths_and_workspace(&paths, &workspace)).clone();
    }
    load_base(&paths)
}

/// Load agent definitions while resolving project overrides through the same
/// captured workspace capability used by the session's filesystem tools.
/// Global definitions remain process-scoped; only project-owned definitions
/// are rebound per session.
pub(crate) fn load_for_workspace_binding(
    workspace: &crate::paths::WorkspaceBinding,
) -> HashMap<String, AgentDefinition> {
    (*shared_for_workspace_binding(workspace)).clone()
}

/// The cached persona map for `workspace`, shared without cloning prompts.
fn shared_for_workspace_binding(
    workspace: &crate::paths::WorkspaceBinding,
) -> std::sync::Arc<HashMap<String, AgentDefinition>> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    load_for_paths_and_workspace(&paths, workspace)
}

/// Bounded number of (agents directory, workspace root) pairs kept.
const MAX_PERSONA_CACHE_ENTRIES: usize = 16;

type PersonaCacheKey = (PathBuf, PathBuf);
type PersonaCacheEntry = (
    PersonaFingerprint,
    std::sync::Arc<HashMap<String, AgentDefinition>>,
);

static PERSONA_CACHE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PersonaCacheKey, PersonaCacheEntry>>,
> = std::sync::LazyLock::new(Default::default);

/// Stat identity of one directory or entry. Unix records device, inode, link
/// count, mode, size and nanosecond mtime/ctime, so an in-place edit that
/// keeps the size still changes ctime; Windows records attributes, size,
/// creation and last-write times.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StatStamp {
    kind: u8,
    words: [i64; 9],
}

macro_rules! stat_stamp {
    ($metadata:expr, $ext:path) => {{
        #[allow(unused_imports)]
        use $ext;
        let metadata = $metadata;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            3
        } else if file_type.is_dir() {
            2
        } else if file_type.is_file() {
            1
        } else {
            0
        };
        #[cfg(unix)]
        let words = [
            metadata.dev() as i64,
            metadata.ino() as i64,
            metadata.nlink() as i64,
            i64::from(metadata.mode()),
            metadata.size() as i64,
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ];
        #[cfg(windows)]
        let words = [
            i64::from(metadata.file_attributes()),
            metadata.file_size() as i64,
            metadata.creation_time() as i64,
            metadata.last_write_time() as i64,
            0,
            0,
            0,
            0,
            0,
        ];
        #[cfg(not(any(unix, windows)))]
        let words = [metadata.len() as i64, 0, 0, 0, 0, 0, 0, 0, 0];
        StatStamp { kind, words }
    }};
}

fn std_stamp(metadata: &std::fs::Metadata) -> StatStamp {
    #[cfg(unix)]
    {
        stat_stamp!(metadata, std::os::unix::fs::MetadataExt)
    }
    #[cfg(windows)]
    {
        stat_stamp!(metadata, std::os::windows::fs::MetadataExt)
    }
    #[cfg(not(any(unix, windows)))]
    {
        stat_stamp!(metadata, std::fs::Metadata)
    }
}

fn cap_stamp(metadata: &cap_std::fs::Metadata) -> StatStamp {
    #[cfg(any(unix, windows))]
    {
        stat_stamp!(metadata, cap_std::fs::MetadataExt)
    }
    #[cfg(not(any(unix, windows)))]
    {
        stat_stamp!(metadata, cap_std::fs::Metadata)
    }
}

/// One persona layer's identity as seen through the same path discipline as
/// its loader. A layer that cannot be stamped reliably makes the whole
/// fingerprint uncacheable (`None`), which always reloads.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LayerStamp {
    /// The loader ignores this layer (missing or not openable through the
    /// checked no-follow path); the error kind distinguishes the cases.
    Unavailable(std::io::ErrorKind),
    /// Over the entry limit: the loader fails the whole layer closed.
    Overflow(StatStamp),
    Listed {
        directory: StatStamp,
        entries: Vec<(std::ffi::OsString, StatStamp)>,
    },
}

/// Everything `load_uncached` reads: project trust (re-evaluated on every
/// call) and, for every persona layer, the directory identity plus each
/// definition entry's name and stat identity. Embedded personas are static.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PersonaFingerprint {
    trusted: bool,
    user: LayerStamp,
    project: Vec<LayerStamp>,
}

fn user_layer_stamp(dir: &Path) -> Option<LayerStamp> {
    // Mirrors `load_dir_files_bounded_status`, which follows links.
    let directory = match std::fs::metadata(dir) {
        Ok(metadata) => std_stamp(&metadata),
        Err(error) => return Some(LayerStamp::Unavailable(error.kind())),
    };
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let name = path.file_name()?.to_os_string();
        // The loader opens through links, so stamp the link and its target.
        let link = std_stamp(&std::fs::symlink_metadata(&path).ok()?);
        let target = std_stamp(&std::fs::metadata(&path).ok()?);
        entries.push((name.clone(), link));
        entries.push((name, target));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Some(LayerStamp::Listed { directory, entries })
}

fn project_layer_stamp(
    workspace: &crate::paths::WorkspaceBinding,
    layer: &str,
) -> Option<LayerStamp> {
    // Opened through the same checked, no-follow capability path as
    // `read_relative_definition_layer`; a swapped-in link fails here too.
    let directory = match workspace.open_dir_relative(Path::new(layer)) {
        Ok(directory) => directory,
        Err(error) => return Some(LayerStamp::Unavailable(error.kind())),
    };
    let stamp = cap_stamp(&directory.dir_metadata().ok()?);
    let mut entries = Vec::new();
    for (index, entry) in directory.read_dir(".").ok()?.enumerate() {
        if index >= MAX_PROJECT_AGENT_ENTRIES {
            return Some(LayerStamp::Overflow(stamp));
        }
        let name = entry.ok()?.file_name();
        if Path::new(&name).extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let metadata = directory.symlink_metadata(&name).ok()?;
        entries.push((name, cap_stamp(&metadata)));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Some(LayerStamp::Listed {
        directory: stamp,
        entries,
    })
}

fn persona_fingerprint(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
    trusted: bool,
) -> Option<PersonaFingerprint> {
    let user = user_layer_stamp(&paths.agents_dir())?;
    let project = if trusted {
        PROJECT_AGENT_LAYERS
            .into_iter()
            .map(|layer| project_layer_stamp(workspace, layer))
            .collect::<Option<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Some(PersonaFingerprint {
        trusted,
        user,
        project,
    })
}

#[cfg(test)]
thread_local! {
    static UNCACHED_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Load (or reuse) the persona map for `workspace`. Trust is re-evaluated on
/// every call; the cached map is reused only when the trust state and the
/// stat identity of every layer directory and definition entry equal those
/// captured before the cached load. Any mismatch, or a layer that cannot be
/// stamped, reloads through the checked loader.
fn load_for_paths_and_workspace(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> std::sync::Arc<HashMap<String, AgentDefinition>> {
    let trusted = paths
        .with_workspace_root(workspace.root())
        .map(|paths| project_definitions_trusted(&paths, workspace))
        .unwrap_or(false);
    // Stamp before loading: an edit racing the load changes the stamp the
    // next call sees, so it can never be masked by the stored fingerprint.
    let fingerprint = persona_fingerprint(paths, workspace, trusted);
    let key = (paths.agents_dir(), workspace.root().to_path_buf());
    if let Some(fingerprint) = &fingerprint
        && let Some((cached, agents)) = PERSONA_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        && cached == fingerprint
    {
        return std::sync::Arc::clone(agents);
    }
    let agents = std::sync::Arc::new(load_uncached(paths, workspace, trusted));
    let mut cache = PERSONA_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match fingerprint {
        Some(fingerprint) => {
            if cache.len() >= MAX_PERSONA_CACHE_ENTRIES && !cache.contains_key(&key) {
                cache.clear();
            }
            cache.insert(key, (fingerprint, std::sync::Arc::clone(&agents)));
        }
        None => {
            cache.remove(&key);
        }
    }
    agents
}

fn load_uncached(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
    trusted: bool,
) -> HashMap<String, AgentDefinition> {
    #[cfg(test)]
    UNCACHED_LOADS.with(|loads| loads.set(loads.get() + 1));
    let mut agents = load_base(paths);
    if trusted {
        for layer in PROJECT_AGENT_LAYERS {
            let definitions = workspace.read_relative_definition_layer(
                Path::new(layer),
                "md",
                MAX_AGENT_PROMPT_BYTES,
                MAX_PROJECT_AGENT_ENTRIES,
                MAX_PROJECT_AGENT_FILES,
            );
            match definitions {
                Ok(definitions) if layer == ".zerostack/agents" => {
                    merge_project_definitions(
                        &mut agents,
                        definitions,
                        workspace.root().join(layer),
                        true,
                    );
                }
                Ok(definitions) => merge_external_definitions(
                    &mut agents,
                    definitions.into_iter().filter(|(name, _)| name != ".notes"),
                    &workspace.root().join(layer),
                    AgentDefinitionSource::ProjectOverride {
                        directory: workspace.root().join(layer),
                    },
                    "project",
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(layer, %error, "ignoring unavailable project specialist layer")
                }
            }
        }
    }
    agents
}

/// Look up the system prompt and its provenance for a named agent type.
#[cfg(any(test, feature = "subagents"))]
pub fn lookup(name: &str) -> Option<AgentDefinition> {
    load().remove(name)
}

#[cfg(any(test, feature = "subagents"))]
pub(crate) fn available_names_for_workspace(
    workspace: Option<&crate::paths::WorkspaceBinding>,
) -> Vec<String> {
    let mut names: Vec<_> = match workspace {
        Some(workspace) => shared_for_workspace_binding(workspace)
            .keys()
            .cloned()
            .collect(),
        None => load().into_keys().collect(),
    };
    names.sort_unstable();
    names
}

/// [`available_names_for_workspace`] against explicit application paths, so
/// tests can change project trust without touching the process paths.
#[cfg(test)]
pub(crate) fn available_names_for_paths_and_workspace(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> Vec<String> {
    let mut names: Vec<_> = load_for_paths_and_workspace(paths, workspace)
        .keys()
        .cloned()
        .collect();
    names.sort_unstable();
    names
}

#[cfg(feature = "subagents")]
pub(crate) fn available_schema_entries_for_workspace(
    workspace: Option<&crate::paths::WorkspaceBinding>,
) -> Vec<(String, String)> {
    let agents = match workspace {
        Some(workspace) => shared_for_workspace_binding(workspace),
        None => std::sync::Arc::new(load()),
    };
    let mut entries = agents
        .iter()
        .map(|(name, definition)| {
            let name = name.clone();
            let mut description = definition.one_line_description();
            let required = definition.required_tool_names();
            if !required.is_empty() {
                description.push_str(&format!(
                    " [requires installed child tools: {}]",
                    required
                        .iter()
                        .take(MAX_NOTICE_NAMES)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            (name, description)
        })
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    entries
}

#[cfg(any(test, feature = "subagents"))]
pub(crate) fn lookup_for_workspace(
    name: &str,
    workspace: Option<&crate::paths::WorkspaceBinding>,
) -> Option<AgentDefinition> {
    match workspace {
        Some(workspace) => shared_for_workspace_binding(workspace).get(name).cloned(),
        None => lookup(name),
    }
}

/// Describe the highest-precedence file that may define `name` without
/// reading or parsing any persona contents. Permission prompts use this before
/// a task delegation is authorized; full definition loading happens only
/// after approval.
#[cfg(feature = "subagents")]
pub(crate) fn source_hint_for_workspace(
    name: &str,
    workspace: Option<&crate::paths::WorkspaceBinding>,
) -> String {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    if !valid_agent_name(name) {
        return "unresolved agent type".to_string();
    }
    if let Some(workspace) = workspace {
        let trusted = paths
            .with_workspace_root(workspace.root())
            .map(|paths| project_definitions_trusted(&paths, workspace))
            .unwrap_or(false);
        if trusted {
            for layer in PROJECT_AGENT_LAYERS.into_iter().rev() {
                let relative = Path::new(layer).join(format!("{name}.md"));
                if workspace.has_relative_regular_file(&relative) {
                    return format!(
                        "trusted project override {}",
                        workspace.root().join(relative).display()
                    );
                }
            }
        }
    }
    let user_path = paths.agents_dir().join(format!("{name}.md"));
    if user_path.is_file() {
        return format!("user-global configuration {}", user_path.display());
    }
    if EMBEDDED.get_file(format!("{name}.md")).is_some() {
        return "compiled-in default".to_string();
    }
    "unresolved agent type".to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn isolated_paths(root: &Path) -> crate::paths::AppPaths {
        crate::paths::AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: Some(root.join(".zerostack")),
        }
    }

    // Exact Tools-section excerpt from code-indexer 0.1.6's
    // agents/code-indexer-subcall.md; this is an executable requirement,
    // not an incidental use of the words Bash, Agent, or code-indexer.
    pub(crate) const INDEXER_CLI_PERSONA: &str = "# Code Indexer Exploration Agent\n\n\
        ## Tools\n\nAll queries use a single CLI script:\n\n```bash\n\
        CLI=\".claude/code-indexer_state/code_indexer_cli.py\"\n\
        python3 $CLI batch --commands \"search symbol_name\"\n```\n\n\
        ## Workflow\nReturn actual code extracts with file:line evidence.";

    #[test]
    fn persona_required_tools_are_explicit_bounded_and_separate_from_narrowing() {
        let explicit = parse_for_test(
            "review",
            "---\ntools: [Read]\nrequired_tools: [Read, Bash, Agent, mcp__indexer__search]\n---\nReview source.",
        );
        assert_eq!(explicit.tools, Some(vec![AgentTool::Read]));
        assert_eq!(
            explicit.required_tool_names(),
            ["read", "shell", "Agent", "mcp__indexer__search"]
        );
        assert_eq!(
            parse_for_test("indexer", INDEXER_CLI_PERSONA).required_tool_names(),
            ["shell"]
        );
        assert_eq!(
            parse_for_test("indexer", &INDEXER_CLI_PERSONA.replace('\n', "\r\n"))
                .required_tool_names(),
            ["shell"]
        );
        for body in [
            "Review Bash, Agent, and code-indexer usage. Do not run shell commands.",
            "## Examples\nAll queries use a single CLI script:\n\n```bash\nCLI=\".claude/code-indexer_state/code_indexer_cli.py\"\npython3 $CLI batch --commands x\n```",
            "## Tools\nUse Read and Grep. A CLI example is available in code-indexer docs.",
        ] {
            assert!(
                parse_for_test("review", body)
                    .required_tool_names()
                    .is_empty()
            );
        }
        for invalid in [
            "[Read, 5]",
            "true",
            "['']",
            "['bad name']",
            "['Read\nBash']",
        ] {
            assert!(
                normalize_agent_definition(
                    "review".into(),
                    format!("---\nrequired_tools: {invalid}\n---\nBODY")
                )
                .is_err(),
                "{invalid}"
            );
        }
    }

    fn uncached_loads() -> usize {
        UNCACHED_LOADS.with(std::cell::Cell::get)
    }

    /// Overwrite `path` in place (same inode, no truncation) with `content`.
    fn rewrite_in_place(path: &Path, content: &str) {
        use std::io::{Seek, Write};
        let before = std::fs::metadata(path).unwrap().len();
        assert_eq!(before, content.len() as u64, "edit must keep the size");
        let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn persona_cache_reuses_unchanged_layers_and_reloads_on_edit_or_trust_change() {
        let fixture = crate::tests::OwnedTestDirectory::new("persona-cache");
        let root = fixture.path();
        let paths = isolated_paths(root);
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(paths.agents_dir()).unwrap();
        let user = paths.agents_dir().join("helper.md");
        std::fs::write(&user, "user helper alpha").unwrap();
        let project = root.join(".zerostack/agents/editor.md");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "---\nmode: write\n---\nproject editor").unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root).unwrap();

        // Two delegations with unchanged files parse once.
        let start = uncached_loads();
        let first = load_for_paths_and_workspace(&paths, &workspace);
        let second = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(
            uncached_loads(),
            start + 1,
            "unchanged layers must not reparse"
        );
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(first["editor"].mode.as_deref(), Some("write"));

        // A same-size in-place edit of a project persona invalidates.
        rewrite_in_place(&project, "---\nmode: reads\n---\nproject editor");
        let edited = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(uncached_loads(), start + 2);
        assert_eq!(edited["editor"].mode.as_deref(), Some("reads"));
        load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(uncached_loads(), start + 2);

        // So does a same-size in-place edit of a user-global persona.
        rewrite_in_place(&user, "user helper omega");
        let user_edited = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(uncached_loads(), start + 3);
        assert_eq!(user_edited["helper"].prompt, "user helper omega");

        // Adding a persona file invalidates.
        std::fs::write(root.join(".zerostack/agents/added.md"), "added").unwrap();
        assert!(load_for_paths_and_workspace(&paths, &workspace).contains_key("added"));
        assert_eq!(uncached_loads(), start + 4);

        // Trust revocation (the trusted config bytes change) invalidates and
        // drops every project persona; nothing project-owned stays cached.
        std::fs::write(&config, "default_prompt = \"review\"\n").unwrap();
        let revoked = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(uncached_loads(), start + 5);
        assert!(!revoked.contains_key("editor"));
        assert!(!revoked.contains_key("added"));
        assert_eq!(revoked["helper"].prompt, "user helper omega");
    }

    #[cfg(unix)]
    #[test]
    fn persona_cache_reloads_when_a_layer_component_becomes_a_link() {
        let fixture = crate::tests::OwnedTestDirectory::new("persona-cache-link");
        let root = fixture.path();
        let paths = isolated_paths(root);
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(root.join(".claude/agents")).unwrap();
        std::fs::write(root.join(".claude/agents/linked.md"), "linked persona").unwrap();
        std::fs::create_dir_all(root.join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root).unwrap();
        assert!(load_for_paths_and_workspace(&paths, &workspace).contains_key("linked"));
        // Same directory inode and entries, now reached through a link: the
        // checked loader refuses it, so the cache must not serve it.
        std::fs::rename(root.join(".claude"), root.join("moved")).unwrap();
        std::os::unix::fs::symlink(root.join("moved"), root.join(".claude")).unwrap();
        assert!(!load_for_paths_and_workspace(&paths, &workspace).contains_key("linked"));
    }

    #[test]
    fn project_agent_layers_require_current_trust_and_preserve_legacy_precedence() {
        let fixture = crate::tests::OwnedTestDirectory::new("project-agent-layers");
        let root = fixture.path();
        let paths = isolated_paths(root);
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(paths.agents_dir()).unwrap();
        std::fs::write(paths.agents_dir().join("review.md"), "user-global fallback").unwrap();
        for layer in PROJECT_AGENT_LAYERS {
            std::fs::create_dir_all(root.join(layer)).unwrap();
            std::fs::write(
                root.join(layer).join("review.md"),
                format!("persona {layer}"),
            )
            .unwrap();
        }
        std::fs::write(root.join(".claude/agents/claude-only.md"), "Claude persona").unwrap();
        std::fs::write(root.join(".mini-agent/agents/mini-only.md"), "mini persona").unwrap();
        std::fs::write(
            root.join(".zerostack/agents/.notes.md"),
            "legacy shared notes",
        )
        .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root).unwrap();
        let untrusted = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(untrusted["review"].prompt, "user-global fallback");
        assert!(!untrusted.contains_key("claude-only"));
        assert!(!untrusted.contains_key("mini-only"));
        std::fs::create_dir_all(root.join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let trusted = load_for_paths_and_workspace(&paths, &workspace);
        assert!(
            trusted["review"]
                .prompt
                .starts_with("persona .zerostack/agents")
        );
        assert_eq!(
            trusted["review"].project_override_path("review"),
            Some(root.join(".zerostack/agents/review.md"))
        );
        for name in ["claude-only", "mini-only", "review"] {
            assert!(trusted[name].prompt.ends_with("legacy shared notes"));
        }
        std::fs::remove_file(root.join(".zerostack/agents/review.md")).unwrap();
        let next = load_for_paths_and_workspace(&paths, &workspace);
        assert!(
            next["review"]
                .prompt
                .starts_with("persona .mini-agent/agents")
        );
        std::fs::write(
            root.join(".mini-agent/agents/review.md"),
            "---\nname: wrong\n---\ninvalid",
        )
        .unwrap();
        let fallback = load_for_paths_and_workspace(&paths, &workspace);
        assert!(
            fallback["review"]
                .prompt
                .starts_with("persona .claude/agents")
        );
        assert!(
            fallback["review"]
                .result_notice("review")
                .unwrap()
                .contains("definition ignored")
        );
        // Trust remains bound to exactly the original config bytes.
        std::fs::write(&config, "default_prompt = \"review\"\n").unwrap();
        assert_eq!(
            load_for_paths_and_workspace(&paths, &workspace)["review"].prompt,
            "user-global fallback"
        );
    }

    #[test]
    fn imported_agent_tools_keep_persona_with_a_bounded_visible_notice() {
        let definition = parse_for_test(
            "review",
            "---\nname: review\ntools: [Read, Edit, Bash, Read, Glob, Write]\n---\nPERSONA",
        );
        assert_eq!(definition.prompt, "PERSONA");
        assert_eq!(
            definition.tools,
            Some(vec![AgentTool::Read, AgentTool::FindFiles])
        );
        let entry = definition.list_entry("review");
        assert!(entry.contains("Edit, Bash, Write ignored"), "{entry}");
        assert!(
            definition
                .result_notice("review")
                .unwrap()
                .contains("subagents are read-only")
        );
        let all_ignored = parse_for_test(
            "review",
            "---\ntools: [Bash, Edit, Write, Task, Fetch, mcp_tool]\n---\nPERSONA",
        );
        assert_eq!(all_ignored.tools, Some(Vec::new()));
        let many = (0..MAX_DECLARED_TOOLS)
            .map(|n| format!("{n:03}{}", "x".repeat(61)))
            .collect::<Vec<_>>()
            .join(", ");
        let many = parse_for_test("review", &format!("---\ntools: {many}\n---\nPERSONA"));
        let notice = many.list_entry("review");
        assert!(notice.contains("and 120 more"));
        assert!(!notice.contains("008xxxxxxxx"));
        assert!(notice.len() < 700);
        assert_eq!(notice.lines().count(), 1);
        for invalid in [
            "[Read, 123]".to_string(),
            "Read,,Write".to_string(),
            format!("[{}]", "X".repeat(65)),
            format!("[{}]", vec!["Read"; 129].join(",")),
            "[\"Bash\\nWrite\"]".to_string(),
            "[工具]".to_string(),
        ] {
            assert!(
                normalize_agent_definition(
                    "review".into(),
                    format!("---\ntools: {invalid}\n---\nBODY")
                )
                .is_err(),
                "{invalid}"
            );
        }
        #[cfg(not(feature = "js"))]
        assert_eq!(
            parse_for_test("review", "---\ntools: [JavaScript]\n---\nBODY").tools,
            Some(vec![])
        );
    }

    #[test]
    fn project_agent_layer_limits_fail_closed_before_any_partial_merge() {
        let fixture = crate::tests::OwnedTestDirectory::new("project-agent-layer-limits");
        let root = fixture.path();
        let paths = isolated_paths(root);
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(root.join(".claude/agents")).unwrap();
        std::fs::write(root.join(".claude/agents/review.md"), "valid lower layer").unwrap();
        std::fs::create_dir_all(root.join(".mini-agent/agents")).unwrap();
        for n in 0..=MAX_PROJECT_AGENT_FILES {
            std::fs::write(
                root.join(".mini-agent/agents")
                    .join(format!("agent-{n}.md")),
                "BODY",
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root).unwrap();
        let resolved = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(resolved["review"].prompt, "valid lower layer");
        assert!(!resolved.keys().any(|name| name.starts_with("agent-")));
        assert!(
            workspace
                .read_relative_definition_layer(Path::new(".claude/agents"), "md", 8, 0, 1)
                .is_err()
        );
        let bounded = workspace
            .read_relative_definition_layer(Path::new(".claude/agents"), "md", 8, 1, 1)
            .unwrap();
        assert!(bounded[0].1.as_ref().unwrap_err().contains("8-byte limit"));
    }

    #[cfg(unix)]
    #[test]
    fn project_agent_layers_never_follow_private_file_or_directory_links() {
        use std::os::unix::fs::symlink;
        let fixture = crate::tests::OwnedTestDirectory::new("project-agent-layer-links");
        let root = fixture.path();
        let paths = isolated_paths(root);
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(root.join(".claude/agents")).unwrap();
        std::fs::create_dir_all(root.join("private-personas")).unwrap();
        std::fs::write(root.join("private-personas/review.md"), "PRIVATE PERSONA").unwrap();
        symlink(
            root.join("private-personas/review.md"),
            root.join(".claude/agents/review.md"),
        )
        .unwrap();
        symlink(root.join("private-personas"), root.join(".mini-agent")).unwrap();
        std::fs::create_dir_all(root.join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root).unwrap();
        let agents = load_for_paths_and_workspace(&paths, &workspace);
        assert!(!agents.contains_key("review"));
        assert!(!workspace.has_relative_regular_file(Path::new(".claude/agents/review.md")));
        assert!(
            workspace
                .read_relative_definition_layer(Path::new(".mini-agent/agents"), "md", 100, 10, 10)
                .is_err()
        );
        let fifo = root.join(".claude/agents/fifo.md");
        // Drop both release-channel handles: a blocking read-open would now hang.
        drop(crate::tests::process_gate::ProcessGate::new(&fifo).unwrap());
        assert!(!workspace.has_relative_regular_file(Path::new(".claude/agents/fifo.md")));
        let entries = workspace
            .read_relative_definition_layer(Path::new(".claude/agents"), "md", 100, 10, 10)
            .unwrap();
        assert!(entries.iter().all(|(_, result)| result.is_err()));
    }

    #[test]
    fn real_project_trust_binding_controls_persona_override_round_trip() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mini-agent-persona-trust-{}", uuid::Uuid::new_v4()));
        let paths = crate::paths::AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: Some(root.join(crate::product::LEGACY_PROJECT_DIRECTORY)),
        };
        crate::paths::prepare_storage_roots(&paths).unwrap();
        let agent_dir = root.join(".zerostack/agents");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("rust-security-review.md"),
            "trusted project persona",
        )
        .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(&root).unwrap();

        let untrusted = load_for_paths_and_workspace(&paths, &workspace);
        assert_ne!(
            untrusted["rust-security-review"].prompt,
            "trusted project persona"
        );

        std::fs::create_dir_all(root.join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let trusted = load_for_paths_and_workspace(&paths, &workspace);
        assert_eq!(
            trusted["rust-security-review"].prompt,
            "trusted project persona"
        );
        assert!(matches!(
            trusted["rust-security-review"].source,
            AgentDefinitionSource::ProjectOverride { .. }
        ));

        drop(workspace);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn embedded_personas_are_bounded_task_scoped_and_caveats_first() {
        const SHIPPED_PERSONA_TOKEN_BUDGET: u64 = 2_000;

        for (name, prompt) in crate::context::load_embedded_files(&EMBEDDED, "md") {
            let estimated = crate::session::Session::estimate_tokens(&prompt);
            assert!(
                estimated <= SHIPPED_PERSONA_TOKEN_BUDGET,
                "embedded persona {name} costs about {estimated} tokens"
            );
            let caveats = prompt
                .find("## Caveats first")
                .unwrap_or_else(|| panic!("embedded persona {name} must lead with caveats"));
            assert!(
                caveats < 1_000,
                "embedded persona {name} buries caveats after {caveats} bytes"
            );
            assert!(
                prompt.contains("delegated objective"),
                "embedded persona {name} must keep its checklist task-scoped"
            );
        }
    }

    #[test]
    fn custom_agent_frontmatter_is_validated_and_not_injected() {
        let normalized = normalize_agent_definition(
            "review".into(),
            "---\nname: review\ndescription: metadata only\n---\n\nTrusted body\n".into(),
        )
        .unwrap();
        assert_eq!(normalized.0, "review");
        assert_eq!(normalized.1, "Trusted body\n");
        assert_eq!(normalized.2.description.as_deref(), Some("metadata only"));
        assert!(normalize_agent_definition("README".into(), "body".into()).is_err());
        assert!(
            normalize_agent_definition("review".into(), "---\nname: different\n---\nbody".into())
                .is_err()
        );
        assert!(
            normalize_agent_definition("review".into(), "---\ninvalid: [\n---\nbody".into())
                .is_err()
        );
        assert!(normalize_agent_definition("review".into(), "---".into()).is_err());
        assert!(normalize_agent_definition("review".into(), " \r\n".into()).is_err());
        assert_eq!(
            normalize_agent_definition(
                "review".into(),
                "\u{feff}---\nname: review\n---\nbody".into()
            )
            .unwrap()
            .1,
            "body"
        );
        let with_mode = normalize_agent_definition(
            "review".into(),
            "---\nname: review\nmode: review-security\n---\nbody".into(),
        )
        .unwrap();
        assert_eq!(with_mode.2.mode.as_deref(), Some("review-security"));
        assert!(
            normalize_agent_definition(
                "review".into(),
                "---\nname: review\nmode: ../../escape\n---\nbody".into(),
            )
            .is_err()
        );

        let configured = normalize_agent_definition(
            "review".into(),
            "---\nname: review\ndescription: Focused review\ntools: Read, Grep, Glob\nmodel: fast\nturn_budget: medium\nfuture-key: ignored\n---\nbody".into(),
        )
        .unwrap();
        assert_eq!(configured.2.description.as_deref(), Some("Focused review"));
        assert_eq!(
            configured.2.tools,
            Some(vec![AgentTool::Read, AgentTool::Grep, AgentTool::FindFiles])
        );
        assert_eq!(configured.2.model.as_deref(), Some("fast"));
        assert_eq!(configured.2.turn_budget, Some(AgentTurnBudget::Medium));
        assert_eq!(configured.2.unknown_keys, vec!["future-key"]);
        #[cfg(feature = "skills")]
        assert_eq!(
            normalize_agent_definition(
                "review".into(),
                "---\nname: review\ntools: SkillsSearch\n---\nbody".into(),
            )
            .unwrap()
            .2
            .tools,
            Some(vec![AgentTool::SkillsSearch])
        );
        #[cfg(feature = "js")]
        assert_eq!(
            normalize_agent_definition(
                "review".into(),
                "---\nname: review\ntools: JavaScript\n---\nbody".into(),
            )
            .unwrap()
            .2
            .tools,
            Some(vec![AgentTool::Js])
        );
        let imported = normalize_agent_definition(
            "review".into(),
            "---\nname: review\ntools: Read, Bash\n---\nbody".into(),
        )
        .unwrap();
        assert_eq!(imported.2.tools, Some(vec![AgentTool::Read]));
        assert!(
            imported
                .2
                .tool_narrowing_notice
                .unwrap()
                .contains("Bash ignored")
        );
        assert!(
            normalize_agent_definition(
                "review".into(),
                "---\nname: review\nmode: [review]\n---\nbody".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn persona_turn_budget_accepts_deprecated_effort_alias_but_not_both() {
        let parse = |frontmatter: &str| {
            normalize_agent_definition(
                "review".into(),
                format!("---\nname: review\n{frontmatter}\n---\nbody"),
            )
        };

        let current = parse("turn_budget: low").unwrap().2;
        assert_eq!(current.turn_budget, Some(AgentTurnBudget::Low));
        assert!(!current.deprecated_effort_alias);
        assert!(current.unknown_keys.is_empty());

        let legacy = parse("effort: high").unwrap().2;
        assert_eq!(legacy.turn_budget, Some(AgentTurnBudget::High));
        assert!(legacy.deprecated_effort_alias);
        assert!(legacy.unknown_keys.is_empty());

        let both = parse("turn_budget: low\neffort: low").unwrap_err();
        assert!(both.contains("both turn_budget"), "{both}");
        assert!(
            parse("turn_budget: maximal")
                .unwrap_err()
                .contains("turn_budget must be low, medium, or high")
        );
        assert!(
            parse("effort: 3")
                .unwrap_err()
                .contains("deprecated alias of turn_budget")
        );
    }

    #[test]
    fn only_trusted_project_definition_wins_and_retains_override_provenance() {
        let mut agents = HashMap::new();
        merge_definitions(
            &mut agents,
            [(
                "review".to_string(),
                "embedded".to_string(),
                AgentMetadata::default(),
            )],
            AgentDefinitionSource::Embedded,
        );
        merge_project_definitions(
            &mut agents,
            [("review".to_string(), Ok("project".to_string()))],
            PathBuf::from("/workspace/.zerostack/agents"),
            false,
        );

        let ignored = agents.get("review").unwrap();
        assert_eq!(ignored.prompt, "embedded");
        assert_eq!(ignored.source, AgentDefinitionSource::Embedded);

        merge_project_definitions(
            &mut agents,
            [("review".to_string(), Ok("project".to_string()))],
            PathBuf::from("/workspace/.zerostack/agents"),
            true,
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
        assert_eq!(
            resolved.source_description("review"),
            "trusted project override /workspace/.zerostack/agents/review.md"
        );
    }

    #[test]
    fn trusted_project_notes_append_without_replacing_personas() {
        let mut agents = HashMap::new();
        merge_definitions(
            &mut agents,
            [(
                "review".to_string(),
                "generic persona".to_string(),
                AgentMetadata::default(),
            )],
            AgentDefinitionSource::Embedded,
        );
        let directory = PathBuf::from("/workspace/.zerostack/agents");

        merge_project_definitions(
            &mut agents,
            [(
                ".notes".to_string(),
                Ok("Repository-specific review guidance".to_string()),
            )],
            directory.clone(),
            true,
        );

        let definition = agents.get("review").unwrap();
        assert!(definition.prompt.starts_with("generic persona"));
        assert!(definition.prompt.contains("## Project notes"));
        assert!(
            definition
                .prompt
                .ends_with("Repository-specific review guidance")
        );
        assert_eq!(definition.source, AgentDefinitionSource::Embedded);
        assert_eq!(
            definition.source_description("review"),
            "compiled-in default with trusted project notes /workspace/.zerostack/agents/.notes.md"
        );
        assert!(
            definition
                .result_notice("review")
                .unwrap()
                .contains("[specialist notes: trusted project file")
        );

        let original = definition.prompt.clone();
        merge_project_definitions(
            &mut agents,
            [(
                ".notes".to_string(),
                Ok("untrusted replacement".to_string()),
            )],
            directory,
            false,
        );
        assert_eq!(agents["review"].prompt, original);
    }

    #[test]
    fn oversized_project_notes_are_ignored_with_a_visible_notice() {
        let mut agents = HashMap::new();
        merge_definitions(
            &mut agents,
            [(
                "review".to_string(),
                "generic persona".to_string(),
                AgentMetadata::default(),
            )],
            AgentDefinitionSource::Embedded,
        );
        merge_project_definitions(
            &mut agents,
            [(
                ".notes".to_string(),
                Ok("x".repeat(MAX_PROJECT_NOTES_BYTES + 1)),
            )],
            PathBuf::from("/workspace/.zerostack/agents"),
            true,
        );

        let definition = &agents["review"];
        assert_eq!(definition.prompt, "generic persona");
        assert!(definition.project_notes_path.is_none());
        assert!(
            definition
                .result_notice("review")
                .unwrap()
                .contains("specialist notes ignored")
        );
    }

    #[test]
    fn permission_source_descriptions_cover_non_project_layers() {
        let embedded = AgentDefinition {
            prompt: "embedded".into(),
            mode: None,
            description: None,
            tools: None,
            required_tools: Vec::new(),
            model: None,
            turn_budget: None,
            source: AgentDefinitionSource::Embedded,
            project_notes_path: None,
            ignored_definition_notices: Vec::new(),
        };
        let user = AgentDefinition {
            prompt: "user".into(),
            mode: None,
            description: None,
            tools: None,
            required_tools: Vec::new(),
            model: None,
            turn_budget: None,
            source: AgentDefinitionSource::UserGlobal,
            project_notes_path: None,
            ignored_definition_notices: Vec::new(),
        };

        assert_eq!(embedded.source_description("review"), "compiled-in default");
        assert_eq!(
            user.source_description("review"),
            "user-global configuration"
        );
        let long = AgentDefinition {
            prompt: format!("You are a specialist with {}", "detail ".repeat(80)),
            mode: None,
            description: None,
            tools: None,
            required_tools: Vec::new(),
            model: None,
            turn_budget: None,
            source: AgentDefinitionSource::Embedded,
            project_notes_path: None,
            ignored_definition_notices: Vec::new(),
        };
        let description = long.one_line_description();
        assert!(description.ends_with('…'));
        assert!(description.chars().count() <= MAX_AGENT_DESCRIPTION_CHARS + 1);
    }

    #[test]
    fn invalid_user_definition_keeps_fallback_and_records_a_result_notice() {
        let mut agents = HashMap::new();
        merge_definitions(
            &mut agents,
            [(
                "review".to_string(),
                "embedded".to_string(),
                AgentMetadata::default(),
            )],
            AgentDefinitionSource::Embedded,
        );

        merge_external_definitions(
            &mut agents,
            [(
                "review".to_string(),
                Ok("---\nname: wrong\n---\nbody".into()),
            )],
            Path::new("/config/agents"),
            AgentDefinitionSource::UserGlobal,
            "user",
        );

        let resolved = agents.get("review").unwrap();
        assert_eq!(resolved.prompt, "embedded");
        assert_eq!(resolved.source, AgentDefinitionSource::Embedded);
        let notice = resolved.result_notice("review").unwrap();
        assert!(notice.starts_with("[specialist source: user definition ignored:"));
        assert!(notice.contains("/config/agents/review.md"));
        assert!(notice.contains("frontmatter name does not match"));
        assert!(notice.contains("using compiled-in default"));

        merge_project_definitions(
            &mut agents,
            [("review".to_string(), Ok("valid project".into()))],
            PathBuf::from("/workspace/.zerostack/agents"),
            true,
        );
        let project = agents.get("review").unwrap();
        assert_eq!(project.prompt, "valid project");
        let project_notice = project.result_notice("review").unwrap();
        assert!(project_notice.starts_with("[specialist source: project override"));
        assert!(!project_notice.contains("definition ignored"));
    }

    #[test]
    fn unknown_frontmatter_keys_are_visible_but_do_not_drop_the_definition() {
        let mut agents = HashMap::new();
        merge_external_definitions(
            &mut agents,
            [(
                "review".to_string(),
                Ok("---\nname: review\ndescription: Explicit schema text\nfuture-option: true\n---\nbody".into()),
            )],
            Path::new("/config/agents"),
            AgentDefinitionSource::UserGlobal,
            "user",
        );

        let definition = agents.get("review").unwrap();
        assert_eq!(definition.prompt, "body");
        assert_eq!(definition.one_line_description(), "Explicit schema text");
        assert_eq!(
            definition.result_notice("review").as_deref(),
            Some("[specialist metadata: unknown frontmatter keys ignored: future-option]")
        );
    }

    #[test]
    fn unreadable_project_definition_keeps_fallback_and_records_a_result_notice() {
        let mut agents = HashMap::new();
        merge_definitions(
            &mut agents,
            [(
                "review".to_string(),
                "embedded".to_string(),
                AgentMetadata::default(),
            )],
            AgentDefinitionSource::Embedded,
        );

        merge_project_definitions(
            &mut agents,
            [("review".to_string(), Err("exceeds the 8-byte limit".into()))],
            PathBuf::from("/workspace/.zerostack/agents"),
            true,
        );

        let resolved = agents.get("review").unwrap();
        assert_eq!(resolved.prompt, "embedded");
        let notice = resolved.result_notice("review").unwrap();
        assert!(notice.starts_with("[specialist source: project definition ignored:"));
        assert!(notice.contains("/workspace/.zerostack/agents/review.md"));
        assert!(notice.contains("exceeds the 8-byte limit"));
        assert!(notice.contains("using compiled-in default"));
    }

    #[test]
    fn untrusted_workspace_definitions_are_never_resolved() {
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
        assert!(lookup_for_workspace("review", Some(&first_binding)).is_none());
        assert!(lookup_for_workspace("review", Some(&second_binding)).is_none());
        assert!(
            !available_names_for_workspace(Some(&first_binding))
                .iter()
                .any(|name| name == "review")
        );

        drop(first_binding);
        drop(second_binding);
        std::fs::remove_dir_all(container).unwrap();
    }

    #[test]
    fn embedded_specialists_respect_read_only_execution_contracts() {
        for (name, prompt) in crate::context::load_embedded_files(&EMBEDDED, "md") {
            assert!(prompt.contains("read-only"), "{name} must stay read-only");
            for repository_marker in [
                "mini-agent",
                "zerostack",
                "Phase 6",
                "src/extras/",
                "editors/vscode",
                "ACP stdio",
                "JsTool",
            ] {
                assert!(
                    !prompt.contains(repository_marker),
                    "embedded persona {name} contains repository-specific marker {repository_marker}"
                );
            }
            assert!(
                prompt.find("## Caveats first").unwrap()
                    < prompt.find("## Return contract").unwrap(),
                "{name} must put caveats before its deliverable"
            );
        }
    }
}
