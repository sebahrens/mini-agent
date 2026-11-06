//! Opt-in persisted "always allow in this project" approvals.
//!
//! Enabled only by `persist_project_approvals = true`. Rules live in a private
//! per-user state file keyed by the canonical workspace root
//! (`<state_dir>/project-approvals/<sha256>.json`), never inside the
//! repository: a checked-out project cannot ship pre-approved rules, and
//! writing one never changes the trusted project config bytes. Loaded rules
//! are ordinary session allow entries, so every `deny` rule still wins.
//! Headless runs never load them.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const MAX_RULES: usize = 256;
const MAX_FIELD_BYTES: usize = 4 * 1024;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectApproval {
    pub(crate) tool: String,
    pub(crate) pattern: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Store {
    /// Display only; the file name binds the workspace.
    workspace: String,
    rules: Vec<ProjectApproval>,
}

fn store_path(state_dir: &Path, workspace: &Path) -> PathBuf {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    state_dir
        .join("project-approvals")
        .join(format!("{}.json", crate::hex::encode_lower(digest)))
}

fn valid(rule: &ProjectApproval) -> bool {
    !rule.tool.is_empty()
        && rule.tool.len() <= 128
        && !rule.pattern.is_empty()
        && rule.pattern.len() <= MAX_FIELD_BYTES
        && !rule.tool.chars().any(char::is_control)
        && !rule.pattern.contains(['\0', '\n', '\r'])
}

fn read(state_dir: &Path, workspace: &Path) -> anyhow::Result<Store> {
    let path = store_path(state_dir, workspace);
    let file = match crate::fs::open_private_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(error.into()),
    };
    let text = match crate::paths::read_utf8_bounded_status(file, MAX_FILE_BYTES)? {
        crate::paths::BoundedUtf8Read::Content(text) => text,
        _ => anyhow::bail!("project approval store is oversized or not UTF-8"),
    };
    let mut store: Store = serde_json::from_str(&text)?;
    store.rules.retain(valid);
    store.rules.truncate(MAX_RULES);
    Ok(store)
}

fn write(state_dir: &Path, workspace: &Path, store: &Store) -> anyhow::Result<()> {
    let path = store_path(state_dir, workspace);
    crate::session::storage::atomic_write(&path, &serde_json::to_string_pretty(store)?)
}

/// The persisted rules for `workspace` (canonical root).
pub(crate) fn load(state_dir: &Path, workspace: &Path) -> anyhow::Result<Vec<ProjectApproval>> {
    Ok(read(state_dir, workspace)?.rules)
}

/// Persist `rule` for `workspace`; an identical rule is not duplicated.
pub(crate) fn add(state_dir: &Path, workspace: &Path, rule: ProjectApproval) -> anyhow::Result<()> {
    anyhow::ensure!(valid(&rule), "approval rule is empty or oversized");
    let mut store = read(state_dir, workspace)?;
    if store.rules.contains(&rule) {
        return Ok(());
    }
    anyhow::ensure!(
        store.rules.len() < MAX_RULES,
        "at most {MAX_RULES} project approvals; remove some with /permissions forget"
    );
    store.workspace = workspace.to_string_lossy().into_owned();
    store.rules.push(rule);
    write(state_dir, workspace, &store)
}

/// Remove the rule at 1-based `index`, or every rule when `None`. Returns the
/// removed rules.
pub(crate) fn remove(
    state_dir: &Path,
    workspace: &Path,
    index: Option<usize>,
) -> anyhow::Result<Vec<ProjectApproval>> {
    let mut store = read(state_dir, workspace)?;
    let removed = match index {
        None => std::mem::take(&mut store.rules),
        Some(index) => {
            anyhow::ensure!(
                (1..=store.rules.len()).contains(&index),
                "no project approval #{index}"
            );
            vec![store.rules.remove(index - 1)]
        }
    };
    store.workspace = workspace.to_string_lossy().into_owned();
    write(state_dir, workspace, &store)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(tool: &str, pattern: &str) -> ProjectApproval {
        ProjectApproval {
            tool: tool.into(),
            pattern: pattern.into(),
        }
    }

    #[test]
    fn approvals_round_trip_per_workspace_dedupe_and_remove() {
        let root = crate::tests::OwnedTestDirectory::new("project-approvals");
        let state = root.path().join("state");
        let (one, two) = (root.path().join("one"), root.path().join("two"));
        assert!(load(&state, &one).unwrap().is_empty());
        add(&state, &one, rule("shell", "cargo test")).unwrap();
        add(&state, &one, rule("shell", "cargo test")).unwrap();
        add(&state, &one, rule("read", "src/**")).unwrap();
        assert_eq!(load(&state, &one).unwrap().len(), 2);
        assert!(
            load(&state, &two).unwrap().is_empty(),
            "scoped to its workspace"
        );
        assert!(add(&state, &one, rule("shell", "a\nb")).is_err());
        assert_eq!(
            remove(&state, &one, Some(1)).unwrap(),
            vec![rule("shell", "cargo test")]
        );
        assert!(remove(&state, &one, Some(5)).is_err());
        assert_eq!(remove(&state, &one, None).unwrap().len(), 1);
        assert!(load(&state, &one).unwrap().is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path(&state, &one))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "private store");
        }
    }
}
