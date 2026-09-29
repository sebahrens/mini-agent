//! Content binding for hook files that live inside the workspace.
//!
//! A hook's argv is fixed by configuration, but a workspace-resident
//! executable or script (`./guard.sh`, `sh hooks/guard.sh`, an `if`
//! condition naming `./check.sh`) is ordinary workspace content that the model
//! can rewrite with its own tools. Without a content binding, a rewritten
//! guard could approve its own tool calls or, under `trust: "trusted"`, run
//! outside the sandbox. [`HookContentPins`] records a SHA-256 digest of every
//! such file when hooks are loaded; every launch re-resolves the same operands
//! and denies the hook unless each workspace-resident file still has the
//! recorded content. Project hooks bind the digests into their confirmation
//! hash; global and managed hooks persist them per project in the trust store
//! (see `trust::pin_configured_hook_content`), so a rewrite in one session is
//! also caught at the next start.
//!
//! Operands are the resolved executable, each argument, and every
//! whitespace/shell-punctuation-separated token of the arguments and the `if`
//! condition, plus `$ZEROSTACK_PROJECT_DIR`-prefixed tokens expanded to the
//! workspace. Only operands that resolve (lexically or after following links)
//! into the workspace and name an existing regular file are bound. Other
//! variable expansion, files a script sources or reads on its own, and the
//! short window between verification and `exec` are outside this best-effort
//! binding (see "Workspace hook files are content-bound" in
//! `docs/agent/CONFIG.md`).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::settings::HookHandler;

/// Files larger than this are never pinned, so a launch that names one inside
/// the workspace is denied instead of hashing unbounded data per launch.
const MAX_PINNED_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Where a bound file lives. Workspace files are keyed relative to the root so
/// a worktree with identical content keeps working after execution rebinds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub(crate) enum PinnedLocation {
    Workspace(PathBuf),
    /// A workspace spelling whose link target leaves the workspace.
    External(PathBuf),
}

/// Content digests of the workspace-resident files one handler executes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HookContentPins {
    files: BTreeMap<PinnedLocation, String>,
    /// Set when the workspace files changed since a persisted approval and
    /// the change was not re-approved: every launch is denied (fail-closed).
    rejected: Option<String>,
}

/// The shell used for `if` conditions, as an absolute path on Unix so a
/// planted `sh` earlier on `PATH` can never be selected.
pub(crate) fn condition_shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("powershell", "-Command")
    } else {
        ("/bin/sh", "-c")
    }
}

impl HookContentPins {
    /// Pins every workspace-resident file `handler` would execute when run
    /// from `root`, including its `if` condition.
    pub(crate) fn capture(root: &Path, handler: &HookHandler) -> Self {
        let Ok(root) = std::fs::canonicalize(root) else {
            return Self::default();
        };
        let mut pins = Self::default();
        let search_path = handler_search_path(handler);
        if let Some(command) = handler.command.as_deref()
            && let Ok(program) = resolve_hook_program(command, search_path.as_deref(), &root)
        {
            pins.insert_operand(&root, program.as_os_str());
        }
        for token in operand_tokens(handler.args.iter().flatten().map(String::as_str), &root) {
            pins.insert_operand(&root, std::ffi::OsStr::new(&token));
        }
        if let Some(condition) = handler.condition.as_deref() {
            let (shell, _) = condition_shell();
            if let Ok(program) = resolve_hook_program(shell, search_path.as_deref(), &root) {
                pins.insert_operand(&root, program.as_os_str());
            }
            for token in operand_tokens(std::iter::once(condition), &root) {
                pins.insert_operand(&root, std::ffi::OsStr::new(&token));
            }
        }
        pins
    }

    /// A binding that denies every launch with `reason`, used when a global
    /// or managed hook's workspace files no longer match the persisted
    /// digests and the change was not re-approved.
    pub(crate) fn rejected(reason: impl Into<String>) -> Self {
        Self {
            files: BTreeMap::new(),
            rejected: Some(reason.into()),
        }
    }

    /// Stable SHA-256 over [`Self::entries`], persisted for global and
    /// managed hooks so a later session detects rewritten workspace files.
    pub(crate) fn content_digest(&self) -> String {
        let canonical = serde_json::to_vec(&self.entries())
            .expect("serializing hook content bindings cannot fail");
        crate::hex::encode_lower(Sha256::digest(canonical))
    }

    fn insert_operand(&mut self, root: &Path, operand: &std::ffi::OsStr) {
        if let Some((location, canonical)) = workspace_file(root, Path::new(operand))
            && let Ok(digest) = file_digest(&canonical)
        {
            self.files.insert(location, digest);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Stable `(location, sha256)` list for trust hashing and confirmation.
    pub(crate) fn entries(&self) -> Vec<(String, String)> {
        self.files
            .iter()
            .map(|(location, digest)| {
                let location = match location {
                    PinnedLocation::Workspace(path) => {
                        format!("workspace:{}", path.to_string_lossy())
                    }
                    PinnedLocation::External(path) => {
                        format!("link-target:{}", path.to_string_lossy())
                    }
                };
                (location, digest.clone())
            })
            .collect()
    }

    /// Workspace-relative paths of the bound files, for write protection.
    pub(crate) fn workspace_paths(&self) -> impl Iterator<Item = &Path> {
        self.files.keys().filter_map(|location| match location {
            PinnedLocation::Workspace(path) => Some(path.as_path()),
            PinnedLocation::External(_) => None,
        })
    }

    /// Verifies one launch from `root`: the resolved `program` and every
    /// argument operand that names an existing workspace-resident file must
    /// match a pin exactly.
    pub(crate) fn verify_launch(
        &self,
        root: &Path,
        program: &Path,
        args: &[String],
    ) -> Result<(), String> {
        if let Some(reason) = &self.rejected {
            return Err(reason.clone());
        }
        let operands = std::iter::once(program.as_os_str().to_os_string()).chain(
            operand_tokens(args.iter().map(String::as_str), root)
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        for operand in operands {
            let Some((location, canonical)) = workspace_file(root, Path::new(&operand)) else {
                continue;
            };
            let Some(expected) = self.files.get(&location) else {
                return Err(format!(
                    "workspace hook file {} was not present when hooks were loaded; restart to review it",
                    Path::new(&operand).display()
                ));
            };
            match file_digest(&canonical) {
                Ok(actual) if &actual == expected => {}
                Ok(_) => {
                    return Err(format!(
                        "workspace hook file {} changed after hooks were loaded; restart to review it",
                        Path::new(&operand).display()
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "workspace hook file {} cannot be verified: {error}",
                        Path::new(&operand).display()
                    ));
                }
            }
        }
        Ok(())
    }
}

fn handler_search_path(handler: &HookHandler) -> Option<std::ffi::OsString> {
    handler
        .env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
}

/// Keeps only absolute `PATH` entries. A relative or empty entry would
/// otherwise resolve a bare hook command against the workspace.
pub(crate) fn absolute_search_path(
    search_path: Option<&std::ffi::OsStr>,
) -> Option<std::ffi::OsString> {
    let search_path = search_path?;
    let absolute: Vec<PathBuf> = std::env::split_paths(search_path)
        .filter(|entry| entry.is_absolute())
        .collect();
    // An empty search string would itself mean "the current directory".
    if absolute.is_empty() {
        return None;
    }
    std::env::join_paths(absolute).ok()
}

/// Resolves a configured hook executable exactly as a launch does: relative
/// paths with a directory component from `project_dir` (never escaping it),
/// bare names through the absolute entries of `search_path`, absolute paths
/// unchanged.
pub(crate) fn resolve_hook_program(
    program: &str,
    search_path: Option<&std::ffi::OsStr>,
    project_dir: &Path,
) -> Result<PathBuf, String> {
    let program_path = Path::new(program);
    if program_path.is_relative() && program_path.components().count() > 1 {
        match project_dir.join(program_path).canonicalize() {
            Ok(path) if path.starts_with(project_dir) => Ok(path),
            Ok(_) => Err("relative hook executable escapes the project directory".to_string()),
            Err(error) => Err(format!(
                "failed to resolve relative hook executable: {error}"
            )),
        }
    } else if program_path.components().count() == 1 && program_path.is_relative() {
        let search_path = absolute_search_path(search_path);
        which::which_in(program, search_path.as_deref(), project_dir)
            .map_err(|error| format!("failed to resolve hook executable {program:?}: {error}"))
    } else {
        Ok(program_path.to_path_buf())
    }
}

/// Every argument plus its whitespace/shell-punctuation separated tokens, so
/// `sh -c "./check.sh --fast"` and `--config=hook.toml` bind their files.
/// A token that starts with `$ZEROSTACK_PROJECT_DIR` or
/// `${ZEROSTACK_PROJECT_DIR}` is also bound with that prefix replaced by
/// `root`, because the hook child receives exactly that value and a shell
/// condition or script argument expands it. Other variables are not expanded.
fn operand_tokens<'a>(args: impl Iterator<Item = &'a str>, root: &Path) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for arg in args {
        tokens.push(arg.to_string());
        tokens.extend(
            arg.split(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        ';' | '|' | '&' | '(' | ')' | '<' | '>' | '\'' | '"' | '`' | '='
                    )
            })
            .filter(|token| !token.is_empty() && *token != arg)
            .map(str::to_string),
        );
    }
    let expanded: Vec<String> = tokens
        .iter()
        .filter_map(|token| expand_project_dir(token, root))
        .collect();
    tokens.extend(expanded);
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// The hook environment variable naming the selected workspace.
const PROJECT_DIR_VARIABLE: &str = "ZEROSTACK_PROJECT_DIR";

fn expand_project_dir(token: &str, root: &Path) -> Option<String> {
    let rest = token.strip_prefix('$')?;
    let braced = format!("{{{PROJECT_DIR_VARIABLE}}}");
    let rest = match rest.strip_prefix(braced.as_str()) {
        Some(rest) => rest,
        // `$ZEROSTACK_PROJECT_DIRX` names a different variable.
        None => rest
            .strip_prefix(PROJECT_DIR_VARIABLE)
            .filter(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))?,
    };
    Some(format!("{}{rest}", root.to_string_lossy()))
}

/// Returns the pin key and canonical path when `operand` names an existing
/// regular file that is inside `root` either lexically or after resolution.
fn workspace_file(root: &Path, operand: &Path) -> Option<(PinnedLocation, PathBuf)> {
    if operand.as_os_str().is_empty() {
        return None;
    }
    let joined = if operand.is_absolute() {
        operand.to_path_buf()
    } else {
        root.join(operand)
    };
    let lexical = lexical_normalize(&joined);
    let canonical = std::fs::canonicalize(&joined).ok()?;
    let canonical_inside = canonical.starts_with(root);
    if !canonical_inside && !lexical.starts_with(root) {
        return None;
    }
    if !std::fs::metadata(&canonical).ok()?.is_file() {
        return None;
    }
    let location = if canonical_inside {
        PinnedLocation::Workspace(canonical.strip_prefix(root).ok()?.to_path_buf())
    } else {
        PinnedLocation::External(canonical.clone())
    };
    Some((location, canonical))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn file_digest(path: &Path) -> Result<String, String> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut limited = file.take(MAX_PINNED_FILE_BYTES + 1);
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = limited
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_PINNED_FILE_BYTES {
            return Err("file exceeds the hook content-binding size limit".to_string());
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::hex::encode_lower(hasher.finalize()))
}
