//! Plugin manifest, component discovery, content hashing and bounded copying.
//!
//! The layout is Claude Code's: `.claude-plugin/plugin.json` plus `commands/`,
//! `agents/`, `skills/`, `hooks/hooks.json` and `.mcp.json` (the manifest may
//! also name them). Everything is read from a directory that is already a
//! private copy; symbolic links are refused wherever a file is read or copied.
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const MAX_FILES: usize = 512;
pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 6;

/// A plugin name: lowercase letters, digits and single hyphens, 1-32 chars.
pub(crate) fn valid_plugin_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) description: Option<String>,
}

/// A data-only component: its local name (`sub:name` for nested commands) and
/// its path relative to the plugin root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) name: String,
    pub(crate) rel: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HookGroup {
    pub(crate) event: String,
    pub(crate) group: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Components {
    pub(crate) commands: Vec<Part>,
    pub(crate) agents: Vec<Part>,
    pub(crate) skills: Vec<Part>,
    pub(crate) hooks: Vec<HookGroup>,
    /// `(server name, definition)` with the definition as authored.
    pub(crate) mcp: Vec<(String, Value)>,
}

fn is_plain(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

fn read_json(path: &Path) -> Result<Value, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is not a regular file under 1 MiB",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn read_manifest(root: &Path) -> Result<(Manifest, Value), String> {
    let json = read_json(&root.join(".claude-plugin/plugin.json"))
        .map_err(|e| format!("not a plugin ({e})"))?;
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .ok_or("plugin.json needs a name")?;
    if !valid_plugin_name(name) {
        return Err(format!(
            "plugin name '{name}' must be lowercase letters, digits and single hyphens (at most 32)"
        ));
    }
    let text = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .map(|s| s.chars().take(200).collect())
    };
    Ok((
        Manifest {
            name: name.to_owned(),
            version: text("version"),
            description: text("description"),
        },
        json,
    ))
}

/// Directories named by a manifest key (a string or an array), always inside
/// the plugin, defaulting to `default`.
fn dirs_for(json: &Value, key: &str, default: &str) -> Result<Vec<PathBuf>, String> {
    let mut dirs = vec![PathBuf::from(default)];
    let extra: Vec<&str> = match json.get(key) {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for raw in extra {
        let path = PathBuf::from(raw.trim_start_matches("./"));
        if !is_plain(&path) {
            return Err(format!("{key} path '{raw}' must stay inside the plugin"));
        }
        if !dirs.contains(&path) {
            dirs.push(path);
        }
    }
    Ok(dirs)
}

fn md_files(root: &Path, dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() && depth < 3 {
                walk(base, &path, out, depth + 1);
            } else if kind.is_file()
                && path.extension().is_some_and(|e| e == "md")
                && let Ok(rel) = path.strip_prefix(base)
            {
                out.push((rel.to_path_buf(), path.clone()));
            }
        }
    }
    let mut out = Vec::new();
    let start = root.join(dir);
    walk(&start, &start, &mut out, 0);
    out.into_iter()
        .map(|(rel_to_dir, _)| (rel_to_dir.clone(), dir.join(rel_to_dir)))
        .collect()
}

fn hook_groups(value: &Value) -> Vec<HookGroup> {
    let events = value.get("hooks").unwrap_or(value);
    let Some(events) = events.as_object() else {
        return Vec::new();
    };
    events
        .iter()
        .flat_map(|(event, groups)| {
            groups
                .as_array()
                .into_iter()
                .flatten()
                .filter(|group| group.is_object())
                .map(|group| HookGroup {
                    event: event.clone(),
                    group: group.clone(),
                })
        })
        .collect()
}

/// A manifest value that is either inline JSON or a path to a JSON file.
fn inline_or_file(
    root: &Path,
    json: &Value,
    key: &str,
    default_file: &str,
) -> Result<Option<Value>, String> {
    match json.get(key) {
        Some(Value::Object(_)) => Ok(json.get(key).cloned()),
        Some(Value::String(path)) => {
            let path = PathBuf::from(path.trim_start_matches("./"));
            if !is_plain(&path) {
                return Err(format!("{key} path must stay inside the plugin"));
            }
            read_json(&root.join(path)).map(Some)
        }
        _ => {
            let default = root.join(default_file);
            if std::fs::symlink_metadata(&default).is_ok() {
                read_json(&default).map(Some)
            } else {
                Ok(None)
            }
        }
    }
}

pub(crate) fn discover(root: &Path, json: &Value) -> Result<Components, String> {
    let mut found = Components::default();
    for dir in dirs_for(json, "commands", "commands")? {
        for (rel_to_dir, rel) in md_files(root, &dir) {
            let stem = rel_to_dir.with_extension("");
            let parts: Vec<String> = stem
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
                continue;
            }
            let name = parts.join(":");
            if !found.commands.iter().any(|c| c.name == name) {
                found.commands.push(Part { name, rel });
            }
        }
    }
    for dir in dirs_for(json, "agents", "agents")? {
        for (rel_to_dir, rel) in md_files(root, &dir) {
            if rel_to_dir.components().count() != 1 {
                continue;
            }
            let name = rel_to_dir.with_extension("").to_string_lossy().into_owned();
            if crate::context::agents::valid_agent_name(&name)
                && !found.agents.iter().any(|a| a.name == name)
            {
                found.agents.push(Part { name, rel });
            }
        }
    }
    for dir in dirs_for(json, "skills", "skills")? {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| e.path().join("SKILL.md").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if !found.skills.iter().any(|s| s.name == name) {
                found.skills.push(Part {
                    rel: dir.join(&name),
                    name,
                });
            }
        }
    }
    if let Some(hooks) = inline_or_file(root, json, "hooks", "hooks/hooks.json")? {
        found.hooks = hook_groups(&hooks);
    }
    if let Some(mcp) = inline_or_file(root, json, "mcpServers", ".mcp.json")? {
        let servers = mcp.get("mcpServers").unwrap_or(&mcp);
        if let Some(servers) = servers.as_object() {
            for (name, def) in servers {
                if def.is_object() {
                    found.mcp.push((name.clone(), def.clone()));
                }
            }
        }
    }
    Ok(found)
}

/// SHA-256 over every file's relative path and bytes in sorted order.
pub(crate) fn content_hash(root: &Path) -> Result<String, String> {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err(format!("{} is a symbolic link", entry.path().display()));
            }
            if kind.is_dir() {
                collect(&entry.path(), out)?;
            } else {
                out.push(entry.path());
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    collect(root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        let rel = file.strip_prefix(root).map_err(|e| e.to_string())?;
        let bytes = std::fs::read(&file).map_err(|e| e.to_string())?;
        hasher.update((rel.to_string_lossy().len() as u64).to_le_bytes());
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Copy a plugin tree into `to`, skipping `.git`, refusing symbolic links and
/// enforcing file-count and size bounds.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    struct Budget {
        files: usize,
        bytes: u64,
    }
    fn walk(from: &Path, to: &Path, budget: &mut Budget, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("plugin directory nesting is too deep".into());
        }
        std::fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
        for entry in std::fs::read_dir(from)
            .map_err(|e| format!("{}: {e}", from.display()))?
            .flatten()
        {
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            let source = entry.path();
            if kind.is_symlink() {
                return Err(format!(
                    "{} is a symbolic link; plugins may not contain links",
                    source.display()
                ));
            }
            if kind.is_dir() {
                walk(&source, &to.join(&name), budget, depth + 1)?;
            } else if kind.is_file() {
                let len = entry.metadata().map_err(|e| e.to_string())?.len();
                budget.files += 1;
                budget.bytes += len;
                if budget.files > MAX_FILES
                    || len > MAX_FILE_BYTES
                    || budget.bytes > MAX_TOTAL_BYTES
                {
                    return Err(
                        "plugin is too large (limits: 512 files, 1 MiB per file, 8 MiB total)"
                            .into(),
                    );
                }
                std::fs::copy(&source, to.join(&name))
                    .map_err(|e| format!("cannot copy {}: {e}", source.display()))?;
            }
        }
        Ok(())
    }
    walk(from, to, &mut Budget { files: 0, bytes: 0 }, 0)
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;

    /// A plugin with one command, agent, skill, hook and MCP server.
    pub(crate) fn write_plugin(root: &Path, name: &str) {
        let w = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        w(
            ".claude-plugin/plugin.json",
            &format!("{{\"name\":\"{name}\",\"version\":\"1.0.0\",\"description\":\"fixture\"}}"),
        );
        w(
            "commands/ship.md",
            "---\ndescription: ship it\n---\nShip $ARGUMENTS",
        );
        w(
            "agents/reviewer.md",
            "---\nname: reviewer\ndescription: reviews\n---\nYou review.",
        );
        w(
            "skills/lint/SKILL.md",
            "---\nname: lint\ndescription: lints\n---\nLint.",
        );
        w(
            "hooks/hooks.json",
            "{\"hooks\":{\"PreToolUse\":[{\"matcher\":\"Bash\",\"hooks\":[{\"type\":\"command\",\"command\":\"${CLAUDE_PLUGIN_ROOT}/guard.sh\",\"args\":[]}]}]}}",
        );
        w(
            ".mcp.json",
            "{\"mcpServers\":{\"tools\":{\"command\":\"${CLAUDE_PLUGIN_ROOT}/serve\",\"args\":[\"--stdio\"],\"env\":{\"API_TOKEN\":\"sk-secret-value\"}}}}",
        );
        w("guard.sh", "#!/bin/sh\nexit 0\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_manifest_and_all_five_components_are_discovered() {
        let root =
            std::env::temp_dir().join(format!("mini-agent-plugin-man-{}", uuid::Uuid::new_v4()));
        fixture::write_plugin(&root, "acme");
        let (manifest, json) = read_manifest(&root).unwrap();
        assert_eq!(
            (manifest.name.as_str(), manifest.version.as_deref()),
            ("acme", Some("1.0.0"))
        );
        let found = discover(&root, &json).unwrap();
        assert_eq!(
            found.commands,
            vec![Part {
                name: "ship".into(),
                rel: "commands/ship.md".into()
            }]
        );
        assert_eq!(found.agents[0].name, "reviewer");
        assert_eq!(found.skills[0].name, "lint");
        assert_eq!(found.hooks.len(), 1);
        assert_eq!(found.hooks[0].event, "PreToolUse");
        assert_eq!(found.mcp[0].0, "tools");
        let hash = content_hash(&root).unwrap();
        assert_eq!(hash.len(), 64);
        std::fs::write(root.join("guard.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        assert_ne!(content_hash(&root).unwrap(), hash);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_names_and_escaping_paths_are_refused() {
        for bad in ["Acme", "a--b", "-a", "a_b", ""] {
            assert!(!valid_plugin_name(bad), "{bad}");
        }
        let json: Value = serde_json::from_str("{\"commands\":\"../outside\"}").unwrap();
        assert!(discover(Path::new("/nonexistent"), &json).is_err());
        let json: Value = serde_json::from_str("{\"hooks\":\"/etc/passwd\"}").unwrap();
        assert!(discover(Path::new("/nonexistent"), &json).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn copying_refuses_links_and_skips_git() {
        let root =
            std::env::temp_dir().join(format!("mini-agent-plugin-copy-{}", uuid::Uuid::new_v4()));
        fixture::write_plugin(&root.join("src"), "acme");
        std::fs::create_dir_all(root.join("src/.git")).unwrap();
        std::fs::write(root.join("src/.git/config"), "x").unwrap();
        copy_tree(&root.join("src"), &root.join("dst")).unwrap();
        assert!(root.join("dst/commands/ship.md").is_file());
        assert!(!root.join("dst/.git").exists());
        std::os::unix::fs::symlink("/etc/hosts", root.join("src/link")).unwrap();
        assert!(
            copy_tree(&root.join("src"), &root.join("dst2"))
                .unwrap_err()
                .contains("symbolic link")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
