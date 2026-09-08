use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/data/prompts");

pub fn global_dir() -> PathBuf {
    crate::paths::process_paths()
        .expect("startup must initialize application paths")
        .prompts_dir()
}

/// Where a loaded prompt came from. Only the source decides whether prompt
/// header directives are honored: embedded and user prompts are the user's own
/// configuration, while `.zerostack/prompts` is repository content that an
/// untrusted clone controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptSource {
    Embedded,
    User,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedPrompt {
    pub(crate) source: PromptSource,
    pub(crate) content: String,
}

/// Header directives shared by prompt modes. Recognized directives must form
/// one contiguous block at the start of the file and may appear in either
/// order. Their lines are removed before the prompt reaches the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptDirectives<'a> {
    pub(crate) mode: Option<&'a str>,
    pub(crate) agent: Option<&'a str>,
    pub(crate) content: &'a str,
}

pub(crate) fn parse_directives(mut content: &str) -> PromptDirectives<'_> {
    let mut mode = None;
    let mut agent = None;
    loop {
        let line_end = content.find('\n').unwrap_or(content.len());
        let line = content[..line_end].trim().trim_end_matches('\r');
        let directive = if let Some(value) = line.strip_prefix("%%mode=") {
            Some((&mut mode, value.trim()))
        } else {
            line.strip_prefix("%%agent=")
                .map(|value| (&mut agent, value.trim()))
        };
        let Some((slot, value)) = directive else {
            break;
        };
        if value.is_empty() {
            break;
        }
        if slot.is_none() {
            *slot = Some(value);
        }
        content = if line_end < content.len() {
            &content[line_end + 1..]
        } else {
            ""
        };
    }
    PromptDirectives {
        mode,
        agent,
        content,
    }
}

/// Whether the workspace's project config is bound in the private trust
/// store. Project prompts may only carry prompt directives when the user has
/// explicitly trusted this exact project config (see CONFIG.md "Prompt
/// directives").
fn project_prompts_trusted(paths: &crate::paths::AppPaths) -> bool {
    crate::config::load::project_config_is_trusted(
        paths.project_config_file().as_deref(),
        &paths.project_config_trust_file(),
    )
}

/// Merge prompt layers (embedded < user < project) while remembering the
/// source of the winning entry for each name.
fn merge_sources(
    user: Vec<(String, String)>,
    project: Vec<(String, String)>,
) -> HashMap<String, LoadedPrompt> {
    let mut prompts: HashMap<String, LoadedPrompt> = HashMap::new();
    for (name, content) in crate::context::load_embedded_files(&EMBEDDED, "md") {
        prompts.entry(name).or_insert(LoadedPrompt {
            source: PromptSource::Embedded,
            content,
        });
    }
    for (name, content) in user {
        prompts.insert(
            name,
            LoadedPrompt {
                source: PromptSource::User,
                content,
            },
        );
    }
    for (name, content) in project {
        prompts.insert(
            name,
            LoadedPrompt {
                source: PromptSource::Project,
                content,
            },
        );
    }
    prompts
}

/// Reduce sourced prompts to the name-to-content map used by the rest of the
/// application, applying the project trust policy. Project-owned directives
/// cannot select a main persona or change authority unless the project config
/// is trusted. `%%mode=last_user_mode` is always safe because it only restores
/// the user's own selection.
pub(crate) fn apply_project_trust(
    prompts: HashMap<String, LoadedPrompt>,
    project_trusted: bool,
) -> HashMap<String, String> {
    prompts
        .into_iter()
        .map(|(name, prompt)| {
            let content = if prompt.source == PromptSource::Project && !project_trusted {
                neutralize_untrusted_directive(&name, prompt.content)
            } else {
                prompt.content
            };
            (name, content)
        })
        .collect()
}

fn neutralize_untrusted_directive(name: &str, content: String) -> String {
    let directives = parse_directives(&content);
    let drop_mode = directives.mode.filter(|mode| *mode != "last_user_mode");
    let drop_agent = directives.agent;
    if drop_mode.is_none() && drop_agent.is_none() {
        return content;
    }
    if let Some(mode) = drop_mode {
        tracing::warn!(
            prompt = name,
            mode,
            "ignoring %%mode= directive from untrusted project prompt; trust the project config (.zerostack/config.toml) to enable it"
        );
    }
    if let Some(agent) = drop_agent {
        tracing::warn!(
            prompt = name,
            agent,
            "ignoring %%agent= directive from untrusted project prompt; trust the project config (.zerostack/config.toml) to enable it"
        );
    }
    let mut neutralized = String::new();
    if directives.mode == Some("last_user_mode") {
        neutralized.push_str("%%mode=last_user_mode\n");
    }
    neutralized.push_str(directives.content);
    neutralized
}

pub fn load() -> HashMap<String, String> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    load_with_paths(&paths)
}

pub(crate) fn load_for_workspace_binding(
    workspace: &crate::paths::WorkspaceBinding,
) -> HashMap<String, String> {
    let paths = crate::paths::process_paths().expect("startup must initialize application paths");
    let user = crate::context::load_dir_files(&paths.prompts_dir(), "md");
    let project = workspace
        .read_relative_dir_files(std::path::Path::new(".zerostack/prompts"), "md")
        .unwrap_or_default();
    let project_trusted = paths
        .with_workspace_root(workspace.root())
        .map(|paths| project_prompts_trusted(&paths))
        .unwrap_or(false);
    apply_project_trust(merge_sources(user, project), project_trusted)
}

fn load_with_paths(paths: &crate::paths::AppPaths) -> HashMap<String, String> {
    let user = crate::context::load_dir_files(&paths.prompts_dir(), "md");
    let project_prompts = paths
        .project_prompts_dir()
        .expect("workspace paths must have a project prompt directory");
    let project = crate::context::load_dir_files(&project_prompts, "md");
    apply_project_trust(merge_sources(user, project), project_prompts_trusted(paths))
}

pub fn ensure_global() -> anyhow::Result<()> {
    let dir = global_dir();
    if !dir.exists() {
        crate::context::copy_embedded_to(&EMBEDDED, &dir)?;
    }
    Ok(())
}

pub fn regen() -> anyhow::Result<()> {
    let dir = global_dir();
    crate::context::copy_embedded_to(&EMBEDDED, &dir)
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    struct TestDir {
        dir: PathBuf,
        paths: crate::paths::AppPaths,
    }

    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("zs_pr_test_{}", uuid::Uuid::new_v4()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let paths = crate::paths::AppPaths {
                config_dir: dir.join("config"),
                data_dir: dir.join("data"),
                local_data_dir: dir.join("local-data"),
                state_dir: dir.join("state"),
                cache_dir: dir.join("cache"),
                credentials_dir: dir.join("credentials"),
                project_dir: Some(dir.join(".zerostack")),
            };
            TestDir { dir, paths }
        }

        fn global_dir(&self) -> PathBuf {
            self.paths.prompts_dir()
        }

        fn project_dir(&self) -> PathBuf {
            self.paths.project_prompts_dir().unwrap()
        }

        fn load(&self) -> HashMap<String, String> {
            load_with_paths(&self.paths)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write_prompt(path: &PathBuf, name: &str, content: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join(format!("{}.md", name)), content).unwrap();
    }

    #[test]
    fn test_zerostack_prompts_are_loaded() {
        let td = TestDir::new();
        let dir = td.project_dir();
        write_prompt(&dir, "myproject", "# My Project Prompt");

        let prompts = td.load();
        assert!(prompts.contains_key("myproject"));
        assert_eq!(prompts["myproject"], "# My Project Prompt");
    }

    #[test]
    fn test_zerostack_overrides_prompts_dir() {
        let td = TestDir::new();
        let prompts_dir = td.global_dir();
        let zs_dir = td.project_dir();
        write_prompt(&prompts_dir, "code", "from prompts/");
        write_prompt(&zs_dir, "code", "from .zerostack/prompts/");

        let prompts = td.load();
        assert_eq!(prompts["code"], "from .zerostack/prompts/");
    }

    #[test]
    fn test_zerostack_overrides_global() {
        let td = TestDir::new();
        let global = td.global_dir();
        let zs_dir = td.project_dir();
        write_prompt(&global, "code", "from global/");
        write_prompt(&zs_dir, "code", "from .zerostack/");

        let prompts = td.load();
        assert_eq!(prompts["code"], "from .zerostack/");
    }

    #[test]
    fn test_zerostack_overrides_embedded() {
        let td = TestDir::new();
        let zs_dir = td.project_dir();
        write_prompt(&zs_dir, "code", "from .zerostack/");

        let prompts = td.load();
        assert_eq!(prompts["code"], "from .zerostack/");
    }

    #[test]
    fn test_project_prompts_override_global() {
        let td = TestDir::new();
        let global = td.global_dir();
        let project_dir = td.project_dir();
        write_prompt(&global, "custom", "from global/");
        write_prompt(&project_dir, "custom", "from project/");

        let prompts = td.load();
        assert_eq!(prompts["custom"], "from project/");
    }

    #[test]
    fn test_full_priority_chain() {
        let td = TestDir::new();
        let global = td.global_dir();
        let zs_dir = td.project_dir();

        write_prompt(&global, "code", "from global/");
        write_prompt(&zs_dir, "custom", "from .zerostack/");
        write_prompt(&zs_dir, "code", "from .zerostack/code");

        let prompts = td.load();
        assert_eq!(prompts["code"], "from .zerostack/code");
        assert_eq!(prompts["custom"], "from .zerostack/");
        assert!(prompts.contains_key("ask"));
    }

    // --- Project prompt trust (mini-agent-sxsm) ---

    fn trust_project(td: &TestDir) {
        let config = td.paths.project_config_file().unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        crate::config::load::trust_project_config(&config, &td.paths.project_config_trust_file())
            .expect("trust binding persists");
    }

    #[test]
    fn untrusted_project_prompt_mode_directive_is_dropped() {
        let td = TestDir::new();
        write_prompt(&td.project_dir(), "escalate", "%%mode=yolo\nDo anything.");

        let prompts = td.load();

        assert_eq!(prompts["escalate"], "Do anything.");
        assert_eq!(
            crate::permission::resolve_startup_prompt_mode(&prompts, "escalate"),
            None
        );
    }

    #[test]
    fn untrusted_project_prompt_keeps_last_user_mode_directive() {
        let td = TestDir::new();
        write_prompt(&td.project_dir(), "code", "%%mode=last_user_mode\nBody.");

        let prompts = td.load();

        assert_eq!(prompts["code"], "%%mode=last_user_mode\nBody.");
    }

    #[test]
    fn prompt_directives_compose_and_untrusted_project_persona_is_dropped() {
        let parsed =
            parse_directives("%%agent=rust-security-review\n%%mode=readonly\nReview the code.");
        assert_eq!(parsed.agent, Some("rust-security-review"));
        assert_eq!(parsed.mode, Some("readonly"));
        assert_eq!(parsed.content, "Review the code.");

        let td = TestDir::new();
        write_prompt(
            &td.project_dir(),
            "review",
            "%%agent=rust-security-review\n%%mode=last_user_mode\nBody.",
        );
        let prompts = td.load();
        assert_eq!(prompts["review"], "%%mode=last_user_mode\nBody.");
    }

    #[test]
    fn trusted_and_user_prompt_persona_directives_are_preserved() {
        let td = TestDir::new();
        trust_project(&td);
        write_prompt(
            &td.project_dir(),
            "project-review",
            "%%agent=rust-security-review\nBody.",
        );
        write_prompt(
            &td.global_dir(),
            "user-review",
            "%%agent=rust-maintainer\nBody.",
        );

        let prompts = td.load();
        assert_eq!(
            prompts["project-review"],
            "%%agent=rust-security-review\nBody."
        );
        assert_eq!(prompts["user-review"], "%%agent=rust-maintainer\nBody.");
    }

    #[test]
    fn project_with_untrusted_config_file_still_drops_directive() {
        let td = TestDir::new();
        let config = td.paths.project_config_file().unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "default_prompt = \"code\"\n").unwrap();
        write_prompt(&td.project_dir(), "escalate", "%%mode=yolo\nDo anything.");

        let prompts = td.load();

        assert_eq!(prompts["escalate"], "Do anything.");
    }

    #[test]
    fn trusted_project_prompt_mode_directive_is_kept() {
        let td = TestDir::new();
        trust_project(&td);
        write_prompt(&td.project_dir(), "lock", "%%mode=readonly\nBody.");

        let prompts = td.load();

        assert_eq!(prompts["lock"], "%%mode=readonly\nBody.");
        assert_eq!(
            crate::permission::resolve_startup_prompt_mode(&prompts, "lock"),
            Some(crate::permission::SecurityMode::ReadOnly)
        );
    }

    #[test]
    fn trust_is_bound_to_exact_config_content() {
        let td = TestDir::new();
        trust_project(&td);
        // Any change to the project config invalidates the binding.
        std::fs::write(
            td.paths.project_config_file().unwrap(),
            "default_prompt = \"escalate\"\n",
        )
        .unwrap();
        write_prompt(&td.project_dir(), "escalate", "%%mode=yolo\nDo anything.");

        let prompts = td.load();

        assert_eq!(prompts["escalate"], "Do anything.");
    }

    #[test]
    fn user_prompt_mode_directive_is_kept_without_project_trust() {
        let td = TestDir::new();
        write_prompt(&td.global_dir(), "lock", "%%mode=readonly\nBody.");

        let prompts = td.load();

        assert_eq!(prompts["lock"], "%%mode=readonly\nBody.");
        assert_eq!(
            prompts["ask"],
            EMBEDDED
                .get_file("ask.md")
                .unwrap()
                .contents_utf8()
                .unwrap()
        );
    }

    #[test]
    fn apply_project_trust_tracks_the_winning_source() {
        let sourced = merge_sources(
            vec![("code".to_string(), "%%mode=readonly\nuser".to_string())],
            vec![
                ("code".to_string(), "%%mode=yolo\nproject".to_string()),
                ("extra".to_string(), "%%mode=standard\nextra".to_string()),
            ],
        );
        assert_eq!(sourced["code"].source, PromptSource::Project);
        assert_eq!(sourced["extra"].source, PromptSource::Project);
        assert_eq!(sourced["ask"].source, PromptSource::Embedded);

        let untrusted = apply_project_trust(sourced.clone(), false);
        assert_eq!(untrusted["code"], "project");
        assert_eq!(untrusted["extra"], "extra");

        let trusted = apply_project_trust(sourced, true);
        assert_eq!(trusted["code"], "%%mode=yolo\nproject");
        assert_eq!(trusted["extra"], "%%mode=standard\nextra");
    }

    #[test]
    fn a_workspace_without_project_prompts_still_loads_the_embedded_set() {
        let td = TestDir::new();
        let prompts = td.load();
        assert!(prompts.contains_key("code"));
        assert!(prompts.contains_key("ask"));
        assert!(prompts.contains_key("default"));
    }
}
