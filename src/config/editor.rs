//! Explicit operator edits of one known setting. Effective CLI/layer values
//! never become the document being persisted, and saved policy stays inactive
//! in the current session except for explicitly supported presentation updates
//! and finite subagent sizing applied to the next delegation.
use super::{Config, ConfigLayer, keys, layers, load};
use crate::paths::{AppPaths, WorkspaceBinding};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Global,
    Project,
    Local,
}
impl Target {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Local => "project-local",
        }
    }
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Global => "/config ",
            Self::Project => "/config --project ",
            Self::Local => "/config --local ",
        }
    }
}
pub(crate) const ERROR: &str =
    "Settings could not be safely read, validated or saved (values omitted)";

pub(crate) struct Edit {
    pub(crate) path: PathBuf,
    pub(crate) confirmation: Option<String>,
    actual_workspace: Arc<WorkspaceBinding>,
    work: Work,
}
enum Work {
    Global {
        before: Box<Config>,
        after: Box<Config>,
    },
    Project {
        root: Arc<WorkspaceBinding>,
        relative: PathBuf,
        identity: Box<Option<crate::fs::CheckedMetadata>>,
        digest: Option<crate::fs::ContentDigest>,
        content: String,
        sha: String,
        trust: PathBuf,
    },
}

fn read_global(path: &Path) -> Result<Config, &'static str> {
    let content = match load::read_private_content_mode(path, 1024 * 1024, false, false) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(_) => return Err(ERROR),
    };
    load::parse_config_content(path, &content).map_err(|_| ERROR)
}
fn overlay(base: &Config, value: &toml::Value) -> Result<Config, &'static str> {
    let mut serializable = base.clone();
    load::strip_injected_mcp_defaults(&mut serializable);
    let mut json = layers::normalized_json(&serializable)?;
    layers::deep_merge_json(&mut json, layers::normalized_json(value)?);
    let mut cfg: Config =
        serde_json::from_value(layers::normalized_json(&json)?).map_err(|_| ERROR)?;
    load::validate_extra_body(&mut cfg).map_err(|_| ERROR)?;
    Ok(cfg)
}

/// Conservative row metadata; value-sensitive tables still undergo the exact
/// loader split when preparing a candidate.
pub(crate) fn may_need_project_approval(name: &str) -> bool {
    !load::BENIGN_PROJECT_CONFIG_KEYS.contains(&name) || matches!(name, "goal" | "statusline")
}

/// All filesystem work is called by the UI's existing owned blocking scope.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    paths: &AppPaths,
    global_path: &Path,
    workspace: Arc<WorkspaceBinding>,
    home: Option<&Path>,
    target: Target,
    key: &str,
    literal: &str,
) -> Result<Edit, &'static str> {
    let assignment = format!("{key}={literal}");
    let mut parsed = layers::parse_overrides(&[assignment])?;
    let value = parsed.pop().ok_or(ERROR)?;
    let canonical = value
        .as_table()
        .and_then(|t| t.keys().next())
        .and_then(|name| keys::lookup(name))
        .ok_or(ERROR)?
        .name;
    workspace.validate().map_err(|_| ERROR)?;
    let base = read_global(global_path)?;
    if target == Target::Global {
        let after = overlay(&base, &value)?;
        return Ok(Edit {
            path: global_path.into(),
            confirmation: None,
            actual_workspace: workspace,
            work: Work::Global {
                before: Box::new(base),
                after: Box::new(after),
            },
        });
    }
    if matches!(
        canonical,
        "max_budget_usd"
            | "max_session_input_tokens"
            | "max_session_total_tokens"
            | "cache_ttl"
            | "permission_mode_cycle"
    ) {
        return Err("This setting can only be changed in global configuration");
    }
    let found = layers::discover(&workspace, home)?;
    let root = found
        .as_ref()
        .map(|layers| Arc::clone(&layers.source))
        .unwrap_or_else(|| Arc::clone(&workspace));
    let relative = PathBuf::from(if target == Target::Project {
        ".zerostack/config.toml"
    } else {
        ".zerostack/config.local.toml"
    });
    let (content, identity, digest) = match root.open_regular_relative(&relative) {
        Ok(file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if file.metadata().map_err(|_| ERROR)?.nlink() != 1 {
                    return Err(ERROR);
                }
            }
            #[cfg(windows)]
            if crate::fs::windows_file_link_count(&file).map_err(|_| ERROR)? != 1 {
                return Err(ERROR);
            }
            let identity = crate::fs::checked_file_metadata(&file).map_err(|_| ERROR)?;
            let crate::paths::BoundedUtf8Read::Content(text) =
                crate::paths::read_utf8_bounded_status(file, 256 * 1024).map_err(|_| ERROR)?
            else {
                return Err(ERROR);
            };
            let digest = crate::fs::ContentDigest::of(text.as_bytes());
            (text, Some(identity), Some(digest))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), None, None),
        Err(_) => return Err(ERROR),
    };
    let mut document = load::parse_project_toml(&content).map_err(|_| ERROR)?;
    deep_merge_toml(&mut document, value);
    let changed = toml::to_string(&document).map_err(|_| ERROR)?;
    layers::validate_toml_bounds(&changed)?;
    let mut composed = base;
    for candidate in [
        Path::new(".zerostack/config.toml"),
        Path::new(".zerostack/config.local.toml"),
    ] {
        let text = if candidate == relative {
            changed.clone()
        } else {
            match load::read_project_config_text_in_workspace(&root, candidate) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(ERROR),
            }
        };
        let (benign, sensitive, _) = load::split_project_override(&text).map_err(|_| ERROR)?;
        composed = load::merge_config_value_layer(&composed, benign, ConfigLayer::Project)
            .map_err(|_| ERROR)?;
        if candidate == relative
            || load::project_config_relative_is_trusted_at_readonly(
                &paths.project_config_trust_file(),
                &root,
                candidate,
            )
        {
            composed = load::merge_config_value_layer(&composed, sensitive, ConfigLayer::Project)
                .map_err(|_| ERROR)?;
        }
    }
    load::validate_extra_body(&mut composed).map_err(|_| ERROR)?;
    let (_, sensitive, sensitive_keys) =
        load::split_project_override(&changed).map_err(|_| ERROR)?;
    let sha = crate::hex::encode_lower(Sha256::digest(changed.as_bytes()));
    let path = root.root().join(&relative);
    let confirmation=(!sensitive_keys.is_empty()).then(||format!(
        "Save and trust this exact project configuration?\nProject: {}\nConfig: {}\nSHA-256: {}\nSensitive settings (secret values redacted):\n{}\nThe current session's permissions and provider remain unchanged.",
        crate::ui::events::sanitize_output(&root.root().display().to_string()),crate::ui::events::sanitize_output(&path.display().to_string()),sha,load::sensitive_settings_summary(&sensitive)));
    root.validate().map_err(|_| ERROR)?;
    workspace.validate().map_err(|_| ERROR)?;
    Ok(Edit {
        path,
        confirmation,
        actual_workspace: workspace,
        work: Work::Project {
            root,
            relative,
            identity: Box::new(identity),
            digest,
            content: changed,
            sha,
            trust: paths.project_config_trust_file(),
        },
    })
}
fn deep_merge_toml(base: &mut toml::Value, value: toml::Value) {
    match (base, value) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (key, value) in over {
                if let Some(existing) = base.get_mut(&key) {
                    deep_merge_toml(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (slot, value) => *slot = value,
    }
}
impl Edit {
    pub(crate) fn publish(self, approved: bool) -> Result<(), &'static str> {
        if self.confirmation.is_some() && !approved {
            return Err("Project settings were not changed; approval was declined");
        }
        self.actual_workspace.validate().map_err(|_| ERROR)?;
        match self.work {
            Work::Global { before, after } => {
                load::save_config_changes_at(&self.path, &before, &after).map_err(|_| ERROR)?
            }
            Work::Project {
                root,
                relative,
                identity,
                digest,
                content,
                sha,
                trust,
            } => {
                root.validate().map_err(|_| ERROR)?;
                if let (Some(identity), Some(digest)) = (*identity, digest) {
                    root.replace_relative_private_atomic_expecting(
                        &relative,
                        content.as_bytes(),
                        &identity,
                        &digest,
                    )
                    .map_err(|error| {
                        if error.kind() == std::io::ErrorKind::Unsupported {
                            "Project settings cannot be atomically updated on this platform"
                        } else {
                            ERROR
                        }
                    })?;
                } else {
                    root.create_relative_private_atomic(&relative, content.as_bytes())
                        .map_err(|_| ERROR)?;
                }
                if self.confirmation.is_some() {
                    load::approve_editor_project_config(&root,&relative,&sha,&trust).map_err(|_|"Settings saved, but exact project trust could not be recorded; restart will require approval")?;
                }
            }
        }
        self.actual_workspace.validate().map_err(|_| ERROR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (
        crate::tests::OwnedTestDirectory,
        AppPaths,
        Arc<WorkspaceBinding>,
        PathBuf,
    ) {
        let owner = crate::tests::OwnedTestDirectory::new("settings68");
        let root = owner.path();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        let paths = AppPaths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            local_data_dir: root.join("local"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            credentials_dir: root.join("credentials"),
            project_dir: Some(workspace.join(".zerostack")),
        };
        let path = paths.config_dir.join("config.toml");
        let binding = Arc::new(WorkspaceBinding::capture(&workspace).unwrap());
        (owner, paths, binding, path)
    }
    fn prepare_at(
        paths: &AppPaths,
        path: &Path,
        root: &Arc<WorkspaceBinding>,
        target: Target,
        key: &str,
        value: &str,
    ) -> Edit {
        prepare(paths, path, root.clone(), None, target, key, value).unwrap()
    }
    #[test]
    fn config68_global_structural_delta_preserves_unknown_and_unedited_layer_values() {
        let (_owner, paths, root, path) = fixture();
        crate::fs::private_atomic_write_sync(
            &path,
            b"model = 'disk-model'\nmax_tokens = 51\n[future_settings]\nkeep = 'opaque'\n",
        )
        .unwrap();
        // The editor intentionally receives no effective/CLI Config to persist.
        prepare_at(&paths, &path, &root, Target::Global, "temperature", "0.3")
            .publish(true)
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg = read_global(&path).unwrap();
        assert_eq!(cfg.model.as_deref(), Some("disk-model"));
        assert_eq!(cfg.max_tokens, Some(51));
        assert_eq!(cfg.temperature, Some(0.3));
        assert!(text.contains("opaque"));
        prepare_at(&paths, &path, &root, Target::Global, "max_tokens", "99")
            .publish(true)
            .unwrap();
        let cfg = read_global(&path).unwrap();
        assert_eq!(cfg.temperature, Some(0.3));
        assert_eq!(cfg.max_tokens, Some(99));
    }
    #[test]
    fn config68_sensitive_local_decline_and_exact_independent_trust() {
        let (_owner, paths, root, path) = fixture();
        let relative = Path::new(".zerostack/config.toml");
        let edit = prepare_at(
            &paths,
            &path,
            &root,
            Target::Project,
            "provider",
            "\"openrouter\"",
        );
        assert!(edit.confirmation.is_some());
        assert!(edit.publish(false).is_err());
        assert!(!root.root().join(relative).exists());
        assert!(!paths.project_config_trust_file().exists());
        prepare_at(
            &paths,
            &path,
            &root,
            Target::Project,
            "provider",
            "\"openrouter\"",
        )
        .publish(true)
        .unwrap();
        assert!(load::project_config_relative_is_trusted_at_readonly(
            &paths.project_config_trust_file(),
            &root,
            relative
        ));
        let local = Path::new(".zerostack/config.local.toml");
        let edit = prepare_at(&paths, &path, &root, Target::Local, "sandbox", "false");
        assert!(edit.confirmation.is_some());
        assert!(!load::project_config_relative_is_trusted_at_readonly(
            &paths.project_config_trust_file(),
            &root,
            local
        ));
        edit.publish(true).unwrap();
        assert!(load::project_config_relative_is_trusted_at_readonly(
            &paths.project_config_trust_file(),
            &root,
            local
        ));
        assert!(load::project_config_relative_is_trusted_at_readonly(
            &paths.project_config_trust_file(),
            &root,
            relative
        ));
    }
    #[cfg(unix)]
    #[test]
    fn config68_project_cas_rejects_changed_candidate_and_replaced_workspace() {
        let (_owner, paths, root, path) = fixture();
        prepare_at(&paths, &path, &root, Target::Project, "model", "\"old\"")
            .publish(true)
            .unwrap();
        let edit = prepare_at(
            &paths,
            &path,
            &root,
            Target::Project,
            "provider",
            "\"anthropic\"",
        );
        let target = root.root().join(".zerostack/config.toml");
        std::fs::write(&target, "model='editor-change'\n").unwrap();
        assert!(edit.publish(true).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "model='editor-change'\n"
        );
        assert!(!paths.project_config_trust_file().exists());
        let edit = prepare_at(&paths, &path, &root, Target::Local, "model", "\"new\"");
        let moved = root.root().with_extension("moved");
        std::fs::rename(root.root(), &moved).unwrap();
        std::fs::create_dir(root.root()).unwrap();
        assert!(edit.publish(true).is_err());
        assert!(!root.root().join(".zerostack").exists());
        assert!(!moved.join(".zerostack/config.local.toml").exists());
    }
    #[test]
    fn config68_validation_rejects_secret_errors_and_project_global_ceiling_without_writes() {
        let (_owner, paths, root, path) = fixture();
        for (target, key, value) in [
            (Target::Global, "model", "'unterminated-secret"),
            (Target::Global, "max_budget_usd", "nan"),
            (Target::Project, "max_budget_usd", "2"),
            (Target::Global, "unknown-secret", "1"),
        ] {
            let error = prepare(&paths, &path, root.clone(), None, target, key, value)
                .err()
                .unwrap();
            assert!(!error.contains("secret"));
            assert!(!path.exists());
            assert!(!root.root().join(".zerostack").exists());
        }
    }
}
