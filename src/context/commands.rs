//! Authored slash templates. Authority is parsed before literal substitution;
//! arguments and included bytes can never create directives or more includes.
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_TEMPLATE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_EXPANDED_BYTES: usize = 256 * 1024;
const MAX_INCLUDE_BYTES: usize = 64 * 1024;
const MAX_INCLUDES: usize = 8;
const MAX_ENTRIES: usize = 1024;
const MAX_FILES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Text(String),
    Arguments,
    Argument(usize),
    Include(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandTemplate {
    pub(crate) description: String,
    /// Existing prompt name, selected for this turn only.
    pub(crate) mode: Option<String>,
    /// Existing quick-model alias or raw id; normal model persistence applies.
    pub(crate) model: Option<String>,
    pub(crate) security_mode: Option<String>,
    segments: Vec<Segment>,
}

#[derive(Debug)]
pub(crate) struct DeferredCustomCommand {
    pub(crate) template: CommandTemplate,
    pub(crate) arguments: String,
}
impl std::fmt::Display for DeferredCustomCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("custom prompt template submission")
    }
}
impl std::error::Error for DeferredCustomCommand {}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

fn bounded_field(value: &serde_yaml_ng::Value, max: usize) -> Result<String, String> {
    let text = value
        .as_str()
        .ok_or("frontmatter fields must be strings")?
        .trim();
    if text.is_empty()
        || text.len() > max
        || text
            .chars()
            .any(crate::ui::events::needs_terminal_sanitizing)
    {
        return Err("frontmatter field is empty, oversized, or contains display controls".into());
    }
    Ok(text.to_string())
}

impl CommandTemplate {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_TEMPLATE_BYTES {
            return Err("template exceeds 64 KiB".into());
        }
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut body = text;
        let mut description = String::new();
        let mut mode = None;
        let mut model = None;
        if text
            .lines()
            .next()
            .is_some_and(|line| line.trim_end_matches('\r') == "---")
        {
            let first = text.find('\n').ok_or("unterminated frontmatter")? + 1;
            let mut offset = first;
            let mut end = None;
            for line in text[first..].split_inclusive('\n') {
                if line.trim_end_matches(['\r', '\n']) == "---" {
                    end = Some((offset, offset + line.len()));
                    break;
                }
                offset += line.len();
            }
            let (end, start) = end.ok_or("unterminated frontmatter")?;
            let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text[first..end])
                .map_err(|_| "invalid frontmatter YAML")?;
            let mapping = yaml.as_mapping().ok_or("frontmatter must be a mapping")?;
            for (key, value) in mapping {
                match key.as_str() {
                    Some("description") => description = bounded_field(value, 512)?,
                    Some("mode") => {
                        let value = bounded_field(value, 64)?;
                        if !valid_name(&value) {
                            return Err("mode must name an existing prompt".into());
                        }
                        mode = Some(value);
                    }
                    Some("model") => model = Some(bounded_field(value, 256)?),
                    _ => {
                        return Err(
                            "unsupported frontmatter field (only description, mode, model)".into(),
                        );
                    }
                }
            }
            body = &text[start..];
        }
        let directives = super::prompts::parse_directives(body);
        if directives.agent.is_some() || directives.grant.is_some() {
            return Err("custom commands cannot declare agent or grant directives".into());
        }
        if !directives.warnings.is_empty() {
            return Err("invalid or duplicate template directive".into());
        }
        let security_mode = directives.mode.map(str::to_string);
        if security_mode.as_deref().is_some_and(|value| {
            value != "last_user_mode" && crate::permission::SecurityMode::from_str(value).is_none()
        }) {
            return Err("unknown authored %%mode security directive".into());
        }
        if directives.content.trim().is_empty() {
            return Err("template body is empty".into());
        }
        let segments = parse_segments(directives.content)?;
        Ok(Self {
            description,
            mode,
            model,
            security_mode,
            segments,
        })
    }

    /// Includes are authorized against the pre-command read policy. Ask is
    /// fail-closed: slash dispatch cannot block the UI's permission event loop.
    pub(crate) fn expand(
        &self,
        arguments: &str,
        workspace: &crate::paths::WorkspaceBinding,
        permission: &Option<crate::permission::checker::PermCheck>,
    ) -> Result<String, String> {
        if arguments.len() > MAX_TEMPLATE_BYTES {
            return Err("command arguments exceed 64 KiB".into());
        }
        let words = crate::ui::slash::split_args(arguments)?;
        let mut output = String::new();
        for segment in &self.segments {
            let value = match segment {
                Segment::Text(text) => text.clone(),
                Segment::Arguments => arguments.to_string(),
                Segment::Argument(index) => words.get(index - 1).cloned().unwrap_or_default(),
                Segment::Include(path) => {
                    workspace.validate()?;
                    let identity = workspace
                        .logical_relative_path(Path::new(path))
                        .map_err(|_| "include must stay in the workspace")?;
                    if permission.as_ref().is_some_and(|permission| {
                        !permission
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .allows_mutation_preimage(&identity.to_string_lossy(), true)
                    }) {
                        return Err("template include requires existing read permission (denied or approval required)".into());
                    }
                    let file = workspace.open_regular_relative(Path::new(path))
                        .map_err(|_| "template include must be a readable regular workspace file without links")?;
                    let content = crate::paths::read_utf8_bounded_status(file, MAX_INCLUDE_BYTES)
                        .map_err(|_| "could not read template include")?;
                    let crate::paths::BoundedUtf8Read::Content(content) = content else {
                        return Err("template include must be UTF-8 and at most 64 KiB".into());
                    };
                    // Explicit data fences; nothing from an included file is
                    // parsed as a mode, model, placeholder or further include.
                    let content = content.replace("</template_file>", "<\u{2060}/template_file>");
                    format!(
                        "<template_file path={}>\n{content}\n</template_file>",
                        serde_json::to_string(path).unwrap()
                    )
                }
            };
            if output.len().saturating_add(value.len()) > MAX_EXPANDED_BYTES {
                return Err("expanded template exceeds 256 KiB".into());
            }
            output.push_str(&value);
        }
        if output.trim().is_empty() {
            return Err("expanded template is empty".into());
        }
        Ok(output)
    }
}

fn parse_segments(body: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut at = 0;
    let mut includes = 0;
    while at < body.len() {
        let rest = &body[at..];
        let mut replacement = None;
        if rest.starts_with("$ARGUMENTS")
            && rest[10..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
        {
            replacement = Some((10, Segment::Arguments));
        } else if let Some(number) = rest.strip_prefix('$') {
            let digits = number.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                let index: usize = number[..digits]
                    .parse()
                    .map_err(|_| "argument number is too large")?;
                if index > 128 {
                    return Err("argument placeholder exceeds $128".into());
                }
                if index > 0 {
                    replacement = Some((digits + 1, Segment::Argument(index)));
                }
            }
        }
        if rest.starts_with('@')
            && (at == 0
                || body[..at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace))
        {
            let (used, path) = if let Some(braced) = rest.strip_prefix("@{") {
                let end = braced
                    .find('}')
                    .ok_or("unterminated authored file include")?;
                (end + 3, &braced[..end])
            } else {
                let length = rest[1..]
                    .find(char::is_whitespace)
                    .unwrap_or(rest.len() - 1);
                (length + 1, &rest[1..1 + length])
            };
            let path_obj = Path::new(path);
            if path.is_empty()
                || path_obj.is_absolute()
                || path.contains('\\')
                || path_obj
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(
                    "authored file include must be a relative workspace path without traversal"
                        .into(),
                );
            }
            includes += 1;
            if includes > MAX_INCLUDES {
                return Err("template has more than eight file includes".into());
            }
            replacement = Some((used, Segment::Include(path.into())));
        }
        if let Some((used, segment)) = replacement {
            if !literal.is_empty() {
                segments.push(Segment::Text(std::mem::take(&mut literal)));
            }
            segments.push(segment);
            at += used;
        } else {
            let c = rest.chars().next().unwrap();
            literal.push(c);
            at += c.len_utf8();
        }
    }
    if !literal.is_empty() {
        segments.push(Segment::Text(literal));
    }
    Ok(segments)
}

pub(crate) fn load_for_workspace(
    workspace: &crate::paths::WorkspaceBinding,
) -> BTreeMap<String, CommandTemplate> {
    let Ok(paths) = crate::paths::process_paths() else {
        return BTreeMap::new();
    };
    load_for_paths(&paths, workspace)
}

pub(crate) fn load_for_paths(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> BTreeMap<String, CommandTemplate> {
    let trusted = crate::config::load::project_config_is_trusted_in_workspace(paths, workspace);
    load_layers(paths, workspace, trusted)
}

/// Template layer directories, global first, in merge order.
const PROJECT_LAYERS: [&str; 2] = [".claude/commands", ".mini-agent/commands"];

/// Cheap identity of one layer directory: its own and its target's
/// metadata plus each entry's name, size and modification time (bounded by
/// the loader's entry limit). Adding, removing, renaming or editing a
/// template changes it without reading any template; a reload then re-runs
/// every check of the full loader.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirStamp {
    link: (Option<std::time::SystemTime>, u64, bool),
    target: Option<(Option<std::time::SystemTime>, u64)>,
    entries: Vec<(std::ffi::OsString, Option<std::time::SystemTime>, u64)>,
}

fn dir_stamp(path: &Path) -> Option<DirStamp> {
    let link = std::fs::symlink_metadata(path).ok()?;
    let target = std::fs::metadata(path).ok();
    let mut entries: Vec<_> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .take(MAX_ENTRIES + 1)
        .filter_map(Result::ok)
        .map(|entry| {
            let meta = entry.metadata().ok();
            (
                entry.file_name(),
                meta.as_ref().and_then(|meta| meta.modified().ok()),
                meta.map_or(0, |meta| meta.len()),
            )
        })
        .collect();
    entries.sort_unstable();
    Some(DirStamp {
        link: (link.modified().ok(), link.len(), link.is_symlink()),
        target: target.map(|meta| (meta.modified().ok(), meta.len())),
        entries,
    })
}

/// What a cached template map was loaded from. Project trust is part of the
/// key and re-evaluated on every lookup, so revoking trust drops project
/// templates at the next `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandsStamp {
    config_dir: PathBuf,
    workspace: PathBuf,
    trusted: bool,
    layers: Vec<Option<DirStamp>>,
}

fn commands_stamp(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> CommandsStamp {
    let trusted = crate::config::load::project_config_is_trusted_in_workspace(paths, workspace);
    let mut layers = vec![dir_stamp(&paths.config_dir.join("commands"))];
    if trusted {
        layers.extend(
            PROJECT_LAYERS
                .iter()
                .map(|layer| dir_stamp(&workspace.root().join(layer))),
        );
    }
    CommandsStamp {
        config_dir: paths.config_dir.clone(),
        workspace: workspace.root().to_path_buf(),
        trusted,
        layers,
    }
}

/// Templates loaded by [`load_cached`], with the layer stamp they match.
#[derive(Default)]
pub(crate) struct CommandCache {
    loaded: Option<(CommandsStamp, BTreeMap<String, CommandTemplate>)>,
}

/// The workspace's templates for completion (names and descriptions), read
/// from disk only when a layer directory changed since the previous call.
/// Each call costs a few `stat`s, the directory listings and the trust
/// check instead of reading and parsing every template. Execution paths keep
/// using [`load_for_workspace`].
pub(crate) fn load_cached<'a>(
    cache: &'a mut CommandCache,
    workspace: &crate::paths::WorkspaceBinding,
) -> Option<&'a BTreeMap<String, CommandTemplate>> {
    let Ok(paths) = crate::paths::process_paths() else {
        cache.loaded = None;
        return None;
    };
    Some(load_cached_for_paths(cache, &paths, workspace))
}

/// `(name, description)` of every template, from the stamped cache (the
/// `/help` listing); empty when the process paths cannot be resolved.
pub(crate) fn described_cached(
    cache: &mut CommandCache,
    workspace: &crate::paths::WorkspaceBinding,
) -> Vec<(String, String)> {
    load_cached(cache, workspace)
        .map(described)
        .unwrap_or_default()
}

fn described(commands: &BTreeMap<String, CommandTemplate>) -> Vec<(String, String)> {
    commands
        .iter()
        .map(|(name, template)| (name.clone(), template.description.clone()))
        .collect()
}

fn load_cached_for_paths<'a>(
    cache: &'a mut CommandCache,
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
) -> &'a BTreeMap<String, CommandTemplate> {
    // Stamp before loading: a change made while loading leaves a stale
    // stamp, so the next call reloads.
    let stamp = commands_stamp(paths, workspace);
    if cache
        .loaded
        .as_ref()
        .is_none_or(|(cached, _)| *cached != stamp)
    {
        let commands = load_layers(paths, workspace, stamp.trusted);
        cache.loaded = Some((stamp, commands));
    }
    &cache.loaded.as_ref().expect("loaded above").1
}

fn load_layers(
    paths: &crate::paths::AppPaths,
    workspace: &crate::paths::WorkspaceBinding,
    trusted: bool,
) -> BTreeMap<String, CommandTemplate> {
    let mut commands = BTreeMap::new();
    // Global definitions are only read through the private app root. Missing
    // command directories are optional and never created while listing.
    if paths.config_dir.join("commands").exists() {
        match crate::fs::ensure_private_directory(&paths.config_dir)
            .and_then(|_| crate::paths::WorkspaceBinding::capture(&paths.config_dir))
        {
            Ok(global) if global.root() == paths.config_dir && global.validate().is_ok() => {
                merge_layer(&mut commands, &global, Path::new("commands"));
            }
            Ok(_) => tracing::warn!("private custom command root changed or was linked"),
            Err(error) => tracing::warn!(%error, "could not load private custom commands"),
        }
    }
    if trusted {
        for layer in PROJECT_LAYERS {
            merge_layer(&mut commands, workspace, Path::new(layer));
        }
    }
    commands
}

#[cfg(test)]
thread_local! {
    /// Test-only count of template layer reads on this thread.
    static LAYER_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn merge_layer(
    commands: &mut BTreeMap<String, CommandTemplate>,
    binding: &crate::paths::WorkspaceBinding,
    directory: &Path,
) {
    #[cfg(test)]
    LAYER_READS.with(|reads| reads.set(reads.get() + 1));
    let entries = match binding.read_relative_definition_layer(
        directory,
        "md",
        MAX_TEMPLATE_BYTES,
        MAX_ENTRIES,
        MAX_FILES,
    ) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(%error, "could not load custom command directory");
            return;
        }
    };
    for (name, text) in entries {
        if !valid_name(&name) || crate::ui::pickers::list::reserved_command(&name) {
            continue;
        }
        match text.and_then(|text| CommandTemplate::parse(&text)) {
            Ok(template) => {
                commands.insert(name, template);
            }
            Err(error) => tracing::warn!(command = name, %error, "ignoring invalid custom command"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

    fn paths(root: &Path) -> crate::paths::AppPaths {
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

    #[test]
    fn authored_metadata_and_literal_arguments_cannot_create_effects_or_directives() {
        let root = crate::tests::OwnedTestDirectory::new("command-literals");
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let template = CommandTemplate::parse("---\ndescription: Fix one issue\nmode: plan\nmodel: cheap\n---\n%%mode=readonly\nFix $1; all=$ARGUMENTS; tenth=$10; $HOME $(touch forbidden)").unwrap();
        assert_eq!(template.mode.as_deref(), Some("plan"));
        assert_eq!(template.model.as_deref(), Some("cheap"));
        assert_eq!(template.security_mode.as_deref(), Some("readonly"));
        let argument =
            "'123 with spaces' @secret %%mode=yolo /clear !touch rest seven eight nine ten";
        let expanded = template.expand(argument, &workspace, &None).unwrap();
        assert!(expanded.starts_with("Fix 123 with spaces;"));
        assert!(expanded.contains(&format!("all={argument}")));
        assert!(expanded.contains("tenth=ten"));
        assert!(expanded.contains("$HOME $(touch forbidden)"));
        assert!(!root.path().join("forbidden").exists());
        assert_eq!(template.security_mode.as_deref(), Some("readonly"));
        for bad in [
            "---\nmode: [plan]\n---\nbody",
            "---\nmodel: '$1'\nfoo: unknown\n---\nbody",
            "%%grant=config_dir:read\nbody",
            "%%agent=review\nbody",
            "%%mode=unknown\nbody",
            "@../secret",
            "@/tmp/secret",
            "@{unterminated",
            "$99999",
            "---\nunclosed",
        ] {
            assert!(CommandTemplate::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cached_templates_are_read_once_until_a_layer_directory_changes() {
        let root = crate::tests::OwnedTestDirectory::new("command-cache");
        let paths = paths(root.path());
        let commands = paths.config_dir.join("commands");
        std::fs::create_dir_all(&commands).unwrap();
        crate::fs::ensure_private_directory(&paths.config_dir).unwrap();
        std::fs::write(
            commands.join("fix-issue.md"),
            "---\ndescription: One\n---\nFix",
        )
        .unwrap();
        let workspace_dir = root.path().join("workspace");
        std::fs::create_dir(&workspace_dir).unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(&workspace_dir).unwrap();
        let reads = || LAYER_READS.with(std::cell::Cell::take);
        reads();
        let mut cache = CommandCache::default();
        let first = load_cached_for_paths(&mut cache, &paths, &workspace).clone();
        assert_eq!(first, load_for_paths(&paths, &workspace));
        assert_eq!(first["fix-issue"].description, "One");
        reads();
        // A second `/` with unchanged directories reads no template.
        load_cached_for_paths(&mut cache, &paths, &workspace);
        assert_eq!(reads(), 0);
        // The `/help` listing derives from the same cache and re-reads nothing.
        let listed = described(load_cached_for_paths(&mut cache, &paths, &workspace));
        assert_eq!(listed, vec![("fix-issue".to_owned(), "One".to_owned())]);
        assert_eq!(reads(), 0);
        // Adding a template changes the layer and reloads it.
        std::fs::write(commands.join("ship-it.md"), "Ship").unwrap();
        let next = load_cached_for_paths(&mut cache, &paths, &workspace);
        assert!(next.contains_key("ship-it") && next.contains_key("fix-issue"));
        assert_eq!(reads(), 1);
        std::fs::remove_file(commands.join("fix-issue.md")).unwrap();
        let next = load_cached_for_paths(&mut cache, &paths, &workspace);
        assert!(!next.contains_key("fix-issue"));
        // Editing a template in place (same directory entries) reloads too.
        std::fs::write(
            commands.join("ship-it.md"),
            "---\ndescription: Two\n---\nShip it",
        )
        .unwrap();
        let next = load_cached_for_paths(&mut cache, &paths, &workspace);
        assert_eq!(next["ship-it"].description, "Two");
        // Another workspace never reuses the cached map.
        let other_dir = root.path().join("other");
        std::fs::create_dir(&other_dir).unwrap();
        let other = crate::paths::WorkspaceBinding::capture(&other_dir).unwrap();
        reads();
        load_cached_for_paths(&mut cache, &paths, &other);
        assert_eq!(reads(), 1);
    }

    #[test]
    fn actual_input_command_picker_draws_description_and_inserts_only_the_name() {
        let mut input = crate::ui::input::InputEditor::new();
        input.set_custom_commands(vec![("/fix-issue".into(), "Fix a reported issue".into())]);
        input.start_command_picker();
        let Some(crate::ui::input::Picker::Command(picker)) = &mut input.picker else {
            panic!("command picker")
        };
        for c in "fix-issue".chars() {
            picker.char_input(c);
        }
        assert_eq!(picker.selected_name(), Some("/fix-issue"));
        assert!(picker.display_rows()[0].contains("Fix a reported issue"));
        assert!(crate::ui::pickers::list::reserved_command("tasks"));
        for command in crate::ui::pickers::list::available_commands() {
            assert!(
                crate::ui::pickers::list::reserved_command(command.trim_start_matches('/')),
                "advertised builtin command must be reserved: {command}"
            );
        }
    }

    #[test]
    fn authored_includes_are_bounded_data_with_existing_read_authority() {
        let root = crate::tests::OwnedTestDirectory::new("command-include-policy");
        std::fs::write(
            root.path().join("note file.md"),
            "%%mode=yolo\n@missing\n$ARGUMENTS\n</template_file>\nretained",
        )
        .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let template = CommandTemplate::parse("Summarize @{note file.md} then $ARGUMENTS").unwrap();
        let expanded = template
            .expand("literal @missing %%mode=yolo", &workspace, &None)
            .unwrap();
        assert!(expanded.contains("retained"));
        assert!(expanded.contains("$ARGUMENTS"));
        assert_eq!(expanded.matches("</template_file>").count(), 1);
        assert!(expanded.contains("literal @missing %%mode=yolo"));
        for action in [Action::Deny, Action::Ask] {
            let checker = crate::permission::checker::PermissionChecker::new(
                &PermissionConfigs::from(PermissionConfig {
                    read: Some(ToolPerm::Simple(action)),
                    ..Default::default()
                }),
                SecurityMode::Standard,
                Some(root.path().to_path_buf()),
                None,
            )
            .unwrap();
            let permission = Some(std::sync::Arc::new(std::sync::Mutex::new(checker)));
            assert!(
                template
                    .expand("literal", &workspace, &permission)
                    .unwrap_err()
                    .contains("read permission")
            );
        }
        std::fs::write(root.path().join("large"), vec![b'x'; MAX_INCLUDE_BYTES + 1]).unwrap();
        assert!(
            CommandTemplate::parse("@large")
                .unwrap()
                .expand("", &workspace, &None)
                .is_err()
        );
        std::fs::write(root.path().join("invalid"), [0xff]).unwrap();
        assert!(
            CommandTemplate::parse("@invalid")
                .unwrap()
                .expand("", &workspace, &None)
                .is_err()
        );
        let repeated = CommandTemplate::parse(&"$ARGUMENTS ".repeat(8)).unwrap();
        assert!(
            repeated
                .expand(&"é".repeat(32_000), &workspace, &None)
                .unwrap_err()
                .contains("256 KiB")
        );
    }

    #[test]
    fn actual_project_trust_controls_layer_precedence_without_overriding_builtins() {
        let root = crate::tests::OwnedTestDirectory::new("command-layers");
        let paths = paths(root.path());
        crate::paths::prepare_storage_roots(&paths).unwrap();
        for directory in [
            paths.config_dir.join("commands"),
            root.path().join(".claude/commands"),
            root.path().join(".mini-agent/commands"),
            root.path().join(".zerostack"),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            paths.config_dir.join("commands/fix.md"),
            "global $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            root.path().join(".claude/commands/fix.md"),
            "claude $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            root.path().join(".mini-agent/commands/fix.md"),
            "mini $ARGUMENTS",
        )
        .unwrap();
        for name in [
            "help", "compact", "exit", "diff", "commit", "pr", "export", "goal", "tasks",
        ] {
            std::fs::write(
                paths.config_dir.join(format!("commands/{name}.md")),
                "must not override",
            )
            .unwrap();
        }
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let global = load_for_paths(&paths, &workspace);
        assert_eq!(global.len(), 1);
        assert_eq!(
            global["fix"].expand("123", &workspace, &None).unwrap(),
            "global 123"
        );
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = 'code'\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        assert!(crate::config::load::project_config_is_trusted_in_workspace(
            &paths, &workspace
        ));
        let trusted = load_for_paths(&paths, &workspace);
        assert_eq!(
            trusted["fix"].expand("123", &workspace, &None).unwrap(),
            "mini 123"
        );
        // Approval covers later command bytes, exactly like project personas;
        // changing the bound config invalidates that authority.
        std::fs::write(
            root.path().join(".mini-agent/commands/fix.md"),
            "updated $1",
        )
        .unwrap();
        assert_eq!(
            load_for_paths(&paths, &workspace)["fix"]
                .expand("123", &workspace, &None)
                .unwrap(),
            "updated 123"
        );
        std::fs::write(&config, "default_prompt = 'plan'\n").unwrap();
        assert_eq!(
            load_for_paths(&paths, &workspace)["fix"]
                .expand("123", &workspace, &None)
                .unwrap(),
            "global 123"
        );
    }

    #[cfg(unix)]
    #[test]
    fn public_or_hardlinked_trust_store_cannot_authorize_project_templates() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::tests::OwnedTestDirectory::new("command-trust-file-authority");
        let paths = paths(root.path());
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(root.path().join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = 'code'\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        assert!(crate::config::load::project_config_is_trusted_in_workspace(
            &paths, &workspace
        ));
        let store = paths.project_config_trust_file();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!crate::config::load::project_config_is_trusted_in_workspace(&paths, &workspace));
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&store, root.path().join("external-alias")).unwrap();
        assert!(!crate::config::load::project_config_is_trusted_in_workspace(&paths, &workspace));
        std::fs::remove_file(root.path().join("external-alias")).unwrap();
        assert!(crate::config::load::project_config_is_trusted_in_workspace(
            &paths, &workspace
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_and_replaced_workspace_are_rejected_by_actual_template_loader() {
        const CHILD: &str = "MINI_AGENT_TEST_COMMAND_FIFO";
        if std::env::var_os(CHILD).is_none() {
            let root = crate::tests::OwnedTestDirectory::new("command-fifo-child");
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "context::commands::tests::fifo_and_replaced_workspace_are_rejected_by_actual_template_loader", "--nocapture", "--test-threads=1"]).env(CHILD, root.path());
            let output = crate::sandbox::Sandbox::new(false, "none")
                .output_built_command_with_limits(
                    child,
                    crate::sandbox::CommandLimits {
                        timeout: std::time::Duration::from_secs(10),
                        stdout_bytes: 32 * 1024,
                        stderr_bytes: 32 * 1024,
                        combined_bytes: 64 * 1024,
                    },
                )
                .await
                .unwrap();
            assert_eq!(output.status, crate::sandbox::CommandStatus::Completed);
            assert!(
                output.exit_status.unwrap().success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("running 1 test") && stdout.contains("1 passed"),
                "{stdout}"
            );
            return;
        }
        let root = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        let workspace_dir = root.join("workspace");
        std::fs::create_dir(&workspace_dir).unwrap();
        let paths = paths(&workspace_dir);
        crate::paths::prepare_storage_roots(&paths).unwrap();
        std::fs::create_dir_all(paths.config_dir.join("commands")).unwrap();
        let workspace = crate::paths::WorkspaceBinding::capture(&workspace_dir).unwrap();
        let fifo = workspace_dir.join("fifo");
        std::fs::write(&fifo, "regular before swap").unwrap();
        std::fs::remove_file(&fifo).unwrap();
        drop(crate::tests::process_gate::ProcessGate::new(&fifo).unwrap());
        assert!(
            CommandTemplate::parse("@fifo")
                .unwrap()
                .expand("", &workspace, &None)
                .is_err()
        );
        std::fs::write(
            paths.config_dir.join("commands/fifo.md"),
            "regular template before swap",
        )
        .unwrap();
        std::fs::remove_file(paths.config_dir.join("commands/fifo.md")).unwrap();
        drop(
            crate::tests::process_gate::ProcessGate::new(
                &paths.config_dir.join("commands/fifo.md"),
            )
            .unwrap(),
        );
        assert!(load_for_paths(&paths, &workspace).is_empty());
        std::fs::create_dir_all(workspace_dir.join(".zerostack")).unwrap();
        let config = paths.project_config_file().unwrap();
        std::fs::write(&config, "default_prompt = 'code'\n").unwrap();
        crate::config::load::trust_project_config(&config, &paths.project_config_trust_file())
            .unwrap();
        assert!(crate::config::load::project_config_is_trusted_in_workspace(
            &paths, &workspace
        ));
        std::fs::remove_file(&config).unwrap();
        drop(
            crate::tests::process_gate::ProcessGate::new(&paths.project_config_file().unwrap())
                .unwrap(),
        );
        assert!(!crate::config::load::project_config_is_trusted_in_workspace(&paths, &workspace));
        crate::config::load::bounded_project_trust_tests::assert_fifo_readers_and_both_private_open_races(&workspace_dir, &paths, &workspace);
        let outside = root.join("outside");
        std::fs::write(&outside, "PRIVATE_OUTSIDE").unwrap();
        std::fs::write(workspace_dir.join("link"), "regular include before swap").unwrap();
        std::fs::remove_file(workspace_dir.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, workspace_dir.join("link")).unwrap();
        assert!(
            CommandTemplate::parse("@link")
                .unwrap()
                .expand("", &workspace, &None)
                .is_err()
        );
        std::fs::rename(&workspace_dir, root.join("retained-original")).unwrap();
        std::fs::create_dir(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("secret"), "REPLACEMENT").unwrap();
        assert!(
            CommandTemplate::parse("@secret")
                .unwrap()
                .expand("", &workspace, &None)
                .is_err()
        );
        assert!(!crate::config::load::project_config_is_trusted_in_workspace(&paths, &workspace));
    }
}
