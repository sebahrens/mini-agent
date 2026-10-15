//! Startup-frozen global Git sources and native conditional include selection.
//!
//! Capture precedes hooks, MCP and model work. Later operations parse a private
//! mirror of these bytes, preserving Git's own includeIf evaluation while never
//! reopening an operator source. Operator changes require a process restart.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use super::runner::GitRunner;
use super::runtime::{read_bounded, require_single_link};

pub(crate) const ENVIRONMENT: &str = "MINI_AGENT_GIT_GLOBAL_SNAPSHOT";
const MAX_SOURCES: usize = 128;
const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_GRAPH_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024;
static EFFECTS_STARTED: AtomicBool = AtomicBool::new(false);
static SOURCES: OnceLock<Result<Arc<GlobalSources>, String>> = OnceLock::new();

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    key: Vec<u8>,
    value: Option<Vec<u8>>,
}
struct Source {
    path: PathBuf,
    condition_parent: PathBuf,
    entries: Vec<Entry>,
    includes: HashMap<usize, PathBuf>,
}
struct Helper {
    path: PathBuf,
    target: PathBuf,
    authority: Arc<super::runtime::TrustedProgram>,
    prefix: Vec<u8>,
}
pub(super) struct GlobalSources {
    sources: Vec<Source>,
    roots: Vec<PathBuf>,
    helpers: HashMap<Vec<u8>, Result<Helper, String>>,
    #[cfg(windows)]
    shell: Result<Arc<super::runtime::TrustedShell>, String>,
}

pub(super) struct SelectedGlobals {
    pub(super) environment: OsString,
    #[cfg(windows)]
    pub(super) shell: Option<Result<Arc<super::runtime::TrustedShell>, String>>,
}

pub(crate) fn mark_effects_started() {
    EFFECTS_STARTED.store(true, Ordering::SeqCst);
}

/// Called once immediately after AppPaths installation, before project effects.
/// Failure is retained for Git capabilities; unrelated execution modes may run.
pub(crate) async fn initialize(workspace: &Path) {
    if SOURCES.get().is_some() {
        return;
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        if EFFECTS_STARTED.load(Ordering::SeqCst) {
            return Err("guarded Git globals were not captured before project effects".into());
        }
        let runner = GitRunner::discover_guarded()?;
        let result = GlobalSources::capture(&runner, workspace).await?;
        if EFFECTS_STARTED.load(Ordering::SeqCst) {
            return Err("guarded Git global capture overlapped project effects".into());
        }
        Ok(Arc::new(result))
    })
    .await
    .unwrap_or_else(|_| Err("guarded Git global capture exceeded its deadline".into()));
    let _ = SOURCES.set(result);
}

pub(super) async fn capture(
    runner: &GitRunner,
    repository: &Path,
    roots: &[PathBuf],
) -> Result<SelectedGlobals, String> {
    let sources = SOURCES
        .get()
        .ok_or("guarded Git globals were not captured at startup")?
        .as_ref()
        .map_err(Clone::clone)?;
    sources.select(runner, repository, roots).await
}

impl GlobalSources {
    async fn capture(runner: &GitRunner, workspace: &Path) -> Result<Self, String> {
        let mut roots = Vec::new();
        if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
            roots.push(PathBuf::from(global));
        } else {
            if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
                roots.push(PathBuf::from(xdg).join("git/config"));
            } else if let Some(home) = crate::paths::process_home_dir() {
                roots.push(home.join(".config/git/config"));
            }
            if let Some(home) = crate::paths::process_home_dir() {
                roots.push(home.join(".gitconfig"));
            }
        }
        Self::capture_roots(runner, workspace, roots, None).await
    }

    pub(super) async fn capture_roots(
        runner: &GitRunner,
        workspace: &Path,
        roots: Vec<PathBuf>,
        helper_search: Option<&Path>,
    ) -> Result<Self, String> {
        let roots = roots
            .iter()
            .map(|path| absolute_source(path, workspace))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pending = roots.clone();
        let mut sources = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut total = 0usize;
        let mut writable = super::runtime::model_write_roots()?;
        writable.push(
            workspace
                .canonicalize()
                .map_err(|_| "guarded Git startup workspace is unavailable")?,
        );
        writable.extend(runner.startup_untrusted_roots(workspace).await?);
        runner.guarded_runtime()?.validate(&writable)?;
        while let Some(path) = pending.pop() {
            let path = absolute_source(&path, workspace)?;
            if !visited.insert(path.clone()) {
                continue;
            }
            if visited.len() > MAX_SOURCES {
                return Err("guarded Git global source count exceeds its bound".into());
            }
            let Some(bytes) = read_source(&path, &writable)? else {
                continue;
            };
            total = total
                .checked_add(bytes.len())
                .ok_or("guarded Git global source graph is too large")?;
            if total > MAX_GRAPH_BYTES {
                return Err("guarded Git global source graph exceeds its size bound".into());
            }
            let parsed = runner
                .snapshot_probe(
                    workspace,
                    &[
                        OsString::from("config"),
                        "--null".into(),
                        "--no-includes".into(),
                        "--file".into(),
                        "-".into(),
                        "--list".into(),
                    ],
                    Some(bytes.clone()),
                    None,
                )
                .await?;
            let entries = parse_entries(&parsed)?;
            // Git itself performs ~user / %(prefix) path interpolation. Relative
            // results retain their original source directory, never the mirror.
            let paths = runner
                .snapshot_probe(
                    workspace,
                    &[
                        OsString::from("config"),
                        "--null".into(),
                        "--no-includes".into(),
                        "--file".into(),
                        "-".into(),
                        "--type=path".into(),
                        "--get-regexp".into(),
                        r"^include(if\..*)?\.path$".into(),
                    ],
                    Some(bytes),
                    None,
                )
                .await?;
            let mut expanded = parse_entries(&paths)?.into_iter();
            let mut includes = HashMap::new();
            for (index, entry) in entries.iter().enumerate() {
                if !include_key(&entry.key) {
                    continue;
                }
                let target = expanded
                    .next()
                    .ok_or("guarded Git include path capture is malformed")?;
                if target.key != entry.key {
                    return Err("guarded Git include path capture is malformed".into());
                }
                let value = target
                    .value
                    .ok_or("guarded Git include path is not a value")?;
                let target = absolute_source(
                    &PathBuf::from(os_string(value)?),
                    path.parent()
                        .ok_or("guarded Git global source has no parent")?,
                )?;
                includes.insert(index, target.clone());
                pending.push(target);
            }
            if expanded.next().is_some() {
                return Err("guarded Git include path capture is malformed".into());
            }
            let condition_parent = path
                .canonicalize()
                .map_err(|_| "guarded Git global source identity changed")?
                .parent()
                .ok_or("guarded Git global source has no parent")?
                .to_path_buf();
            sources.push(Source {
                path,
                condition_parent,
                entries,
                includes,
            });
        }
        let needs_external = sources
            .iter()
            .flat_map(|source| &source.entries)
            .any(|entry| {
                credential_helper(&entry.key)
                    && entry.value.as_ref().is_some_and(|value| {
                        !value.is_empty()
                            && value[0] != b'!'
                            && !value.starts_with(b"/")
                            && !matches!(
                                value.split(|byte| byte.is_ascii_whitespace()).next(),
                                Some(b"store" | b"cache")
                            )
                    })
            });
        let execpath = if let Some(path) = helper_search {
            path.to_path_buf()
        } else if needs_external {
            runner.original_exec_path(workspace).await?
        } else {
            runner.guarded_runtime()?.directory().to_path_buf()
        };
        let mut helpers = HashMap::new();
        for entry in sources.iter().flat_map(|source| &source.entries) {
            if credential_helper(&entry.key)
                && let Some(value) = &entry.value
                && !value.is_empty()
                && value[0] != b'!'
                && !Path::new(&os_string(value.clone())?).is_absolute()
            {
                helpers.entry(value.clone()).or_insert_with(|| {
                    resolve_helper(
                        value,
                        &execpath,
                        runner.guarded_runtime().map(|runtime| runtime.program()),
                        &writable,
                    )
                });
            }
        }
        Ok(Self {
            sources,
            roots,
            helpers,
            #[cfg(windows)]
            shell: runner.capture_original_shell(&writable),
        })
    }

    pub(super) async fn select(
        &self,
        runner: &GitRunner,
        repository: &Path,
        roots: &[PathBuf],
    ) -> Result<SelectedGlobals, String> {
        let runtime = runner.guarded_runtime()?;
        runtime.validate(roots)?;
        // This directory is outside every retained model write grant. Source
        // deletion on cancelled parsing can only fail the read-only query.
        let mirror = SnapshotDirectory::new(runtime.directory())?;
        let mut names = HashMap::new();
        for (index, source) in self.sources.iter().enumerate() {
            names.insert(
                source.path.clone(),
                mirror.path().join(format!("source-{index}")),
            );
        }
        for source in &self.sources {
            let mut entries = source.entries.clone();
            for (index, target) in &source.includes {
                let destination = names
                    .get(target)
                    .cloned()
                    .unwrap_or_else(|| mirror.path().join("absent-source"));
                entries[*index].value = Some(path_bytes(&destination)?);
                rewrite_relative_condition(&mut entries[*index].key, &source.condition_parent)?;
            }
            let text = serialize(&entries)?;
            std::fs::write(&names[&source.path], text)
                .map_err(|_| "guarded Git captured global mirror could not be written")?;
        }
        let root = mirror.path().join("root");
        let mut entries = Vec::new();
        for source in &self.roots {
            let source = absolute_source(source, repository)?;
            if let Some(path) = names.get(&source) {
                entries.push(Entry {
                    key: b"include.path".to_vec(),
                    value: Some(path_bytes(path)?),
                });
            }
        }
        std::fs::write(&root, serialize(&entries)?)
            .map_err(|_| "guarded Git captured global root could not be written")?;
        let selected = runner
            .snapshot_probe(
                repository,
                &[
                    OsString::from("config"),
                    "--null".into(),
                    "--includes".into(),
                    "--show-scope".into(),
                    "--show-origin".into(),
                    "--list".into(),
                ],
                None,
                Some(&root),
            )
            .await?;
        let fields = if selected.is_empty() {
            Vec::new()
        } else {
            selected
                .strip_suffix(b"\0")
                .ok_or("guarded Git global selection is malformed")?
                .split(|b| *b == 0)
                .collect::<Vec<_>>()
        };
        if fields.len() % 3 != 0 {
            return Err("guarded Git global selection is malformed".into());
        }
        let mut entries = Vec::new();
        for record in fields.chunks_exact(3) {
            if record[0] != b"global" {
                continue;
            }
            let mut entry = parse_entry(record[2])?;
            if include_key(&entry.key) {
                continue;
            }
            // GLOBAL may only originate in the private mirror graph.
            let origin = record[1]
                .strip_prefix(b"file:")
                .ok_or("guarded Git global selection has no file origin")?;
            let origin = PathBuf::from(os_string(origin.to_vec())?);
            if origin != root && !names.values().any(|name| *name == origin) {
                return Err("guarded Git global selection escaped its captured sources".into());
            }
            if credential_helper(&entry.key)
                && let Some(value) = &entry.value
                && let Some(helper) = self.helpers.get(value)
            {
                let helper = helper.as_ref().map_err(Clone::clone)?;
                super::runtime::require_disjoint(&helper.path, roots)?;
                super::runtime::require_disjoint(&helper.target, roots)?;
                helper.authority.validate()?;
                let token = value
                    .iter()
                    .position(|byte| byte.is_ascii_whitespace())
                    .unwrap_or(value.len());
                let mut rewritten = helper.prefix.clone();
                rewritten.extend_from_slice(&value[token..]);
                entry.value = Some(rewritten);
            }
            entries.push(entry);
        }
        #[cfg(windows)]
        let shell = if entries.iter().any(shell_dependent_service) {
            Some(self.shell.clone())
        } else {
            None
        };
        Ok(SelectedGlobals {
            environment: os_string(serialize(&entries)?)?,
            #[cfg(windows)]
            shell,
        })
    }
}

/// Exclusive, private scratch for read-only native config evaluation. It never
/// grants source authority and has no process/task ownership responsibilities.
pub(super) struct SnapshotDirectory(PathBuf);
impl SnapshotDirectory {
    pub(super) fn new(parent: &Path) -> Result<Self, String> {
        let parent = parent
            .canonicalize()
            .map_err(|_| "guarded Git private scratch parent is unavailable")?;
        let path = parent.join(format!("git-snapshot-{}", uuid::Uuid::new_v4()));
        #[cfg(not(windows))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        #[cfg(not(windows))]
        builder
            .create(&path)
            .map_err(|_| "guarded Git private scratch creation failed")?;
        // Windows std directory creation can inherit an Administrators owner
        // under an elevated token. Existing-directory validation must refuse
        // that owner, so install our explicit private descriptor at creation.
        #[cfg(windows)]
        crate::fs::create_private_directory_exclusive(&path)
            .map_err(|_| "guarded Git private scratch creation failed")?;
        let owned = Self(path);
        crate::paths::ensure_private_directory(owned.path())
            .map_err(|_| "guarded Git private scratch authority is unavailable")?;
        Ok(owned)
    }
    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for SnapshotDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn include_key(key: &[u8]) -> bool {
    key == b"include.path" || (key.starts_with(b"includeif.") && key.ends_with(b".path"))
}
fn credential_helper(key: &[u8]) -> bool {
    key.starts_with(b"credential.") && key.ends_with(b".helper") || key == b"credential.helper"
}
#[cfg(windows)]
fn shell_dependent_service(entry: &Entry) -> bool {
    let Some(value) = entry.value.as_ref().filter(|value| !value.is_empty()) else {
        return false;
    };
    // Credential helpers append the operation to one shell command. Other
    // services follow pinned Git prepare_shell_cmd's argv[0] decision exactly.
    credential_helper(&entry.key)
        || (matches!(entry.key.as_slice(), b"core.sshcommand" | b"core.askpass")
            && value
                .iter()
                .any(|byte| b"|&;<>()$`\\\"' \t\n*?[#~=%".contains(byte)))
}
fn null_source(path: &Path) -> bool {
    #[cfg(windows)]
    {
        path.as_os_str()
            .to_str()
            .is_some_and(|path| path.eq_ignore_ascii_case("NUL"))
    }
    #[cfg(not(windows))]
    {
        path == Path::new("/dev/null")
    }
}

fn absolute_source(path: &Path, base: &Path) -> Result<PathBuf, String> {
    // NUL is a Windows device spelling independent of CWD. Retain only
    // this exact documented null spelling; ordinary devices remain rejected.
    if null_source(path) {
        return Ok(path.to_path_buf());
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(path)
}
fn rewrite_relative_condition(key: &mut Vec<u8>, original_parent: &Path) -> Result<(), String> {
    for prefix in [
        b"includeif.gitdir:./".as_slice(),
        b"includeif.gitdir/i:./".as_slice(),
    ] {
        if key.starts_with(prefix) {
            let mut rewritten = prefix[..prefix.len() - 2].to_vec();
            rewritten.extend_from_slice(&path_bytes(original_parent)?);
            rewritten.push(b'/');
            rewritten.extend_from_slice(&key[prefix.len()..]);
            *key = rewritten;
            break;
        }
    }
    Ok(())
}
fn path_bytes(path: &Path) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        path.to_str()
            .map(|s| s.as_bytes().to_vec())
            .ok_or("guarded Git path is not representable".into())
    }
}
fn resolve_helper(
    value: &[u8],
    execpath: &Path,
    bundled: Result<&Path, String>,
    roots: &[PathBuf],
) -> Result<Helper, String> {
    let token = value
        .split(|byte| byte.is_ascii_whitespace())
        .next()
        .unwrap_or_default();
    if token.is_empty()
        || !token
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_'))
    {
        return Err("guarded Git cannot resolve this approved credential-helper shorthand; configure an absolute path or explicit ! command".into());
    }
    let (path, builtin) = if matches!(token, b"store" | b"cache") {
        (bundled?.to_path_buf(), true)
    } else {
        let mut name = b"git-credential-".to_vec();
        name.extend_from_slice(token);
        if cfg!(windows) {
            name.extend_from_slice(b".exe");
        }
        let name = os_string(name)?;
        let mut dirs = vec![execpath.to_path_buf()];
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path).filter(|path| path.is_absolute()));
        }
        (
            dirs.into_iter()
                .map(|directory| directory.join(&name))
                .find(|path| path.is_file())
                .ok_or("guarded Git approved credential-helper shorthand is unavailable")?,
            false,
        )
    };
    let target = path
        .canonicalize()
        .map_err(|_| "guarded Git approved credential-helper authority is unavailable")?;
    super::runtime::require_disjoint(&path, roots)?;
    super::runtime::require_disjoint(&target, roots)?;
    let authority = super::runtime::TrustedProgram::capture(&path, roots)?;
    let mut prefix = b"!".to_vec();
    shell_quote(&path_bytes(&path)?, &mut prefix);
    if builtin {
        prefix.extend_from_slice(b" credential-");
        prefix.extend_from_slice(token);
    }
    Ok(Helper {
        path,
        target,
        authority,
        prefix,
    })
}

fn shell_quote(value: &[u8], text: &mut Vec<u8>) {
    text.push(b'\'');
    for byte in value {
        if *byte == b'\'' {
            text.extend_from_slice(b"'\\''");
        } else {
            text.push(*byte);
        }
    }
    text.push(b'\'');
}
fn read_source(path: &Path, roots: &[PathBuf]) -> Result<Option<Vec<u8>>, String> {
    if null_source(path) {
        return Ok(None);
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("guarded Git global source authority is unavailable".into()),
        _ => {}
    }
    for ancestor in path.ancestors().filter(|path| path.exists()) {
        let resolved = ancestor
            .canonicalize()
            .map_err(|_| "guarded Git global source authority is unavailable")?;
        if roots
            .iter()
            .any(|root| ancestor.starts_with(root) || resolved.starts_with(root))
        {
            return Err("guarded Git global source overlaps model-writable storage".into());
        }
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| "guarded Git global source authority is unavailable")?;
    super::runtime::require_disjoint(&canonical, roots)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&canonical)
        .map_err(|_| "guarded Git global source cannot be opened")?;
    let before = crate::fs::checked_file_metadata(&file)
        .map_err(|_| "guarded Git global source identity is unavailable")?;
    require_single_link(&before)?;
    let bytes = read_bounded(&file, MAX_SOURCE_BYTES)?;
    let after = crate::fs::checked_file_metadata(&file)
        .map_err(|_| "guarded Git global source identity is unavailable")?;
    let current_path = path
        .canonicalize()
        .map_err(|_| "guarded Git global source identity changed")?;
    let current = crate::fs::checked_path_metadata(&current_path)
        .map_err(|_| "guarded Git global source identity changed")?;
    if current_path != canonical
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("guarded Git global source changed during capture".into());
    }
    crate::fs::ensure_same_file(&canonical, &before, &after)
        .and_then(|()| crate::fs::ensure_same_file(&canonical, &after, &current))
        .map_err(|_| "guarded Git global source identity changed".to_string())?;
    Ok(Some(bytes))
}

fn parse_entries(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    bytes
        .strip_suffix(b"\0")
        .ok_or("guarded Git captured syntax is malformed")?
        .split(|byte| *byte == 0)
        .map(parse_entry)
        .collect()
}

fn parse_entry(pair: &[u8]) -> Result<Entry, String> {
    let (key, value) = match pair.iter().position(|byte| *byte == b'\n') {
        Some(index) => (&pair[..index], Some(pair[index + 1..].to_vec())),
        None => (pair, None),
    };
    if key.is_empty() || !key.contains(&b'.') || key.contains(&0) {
        return Err("guarded Git captured syntax is malformed".into());
    }
    Ok(Entry {
        key: key.to_vec(),
        value,
    })
}

fn quote(value: &[u8], output: &mut Vec<u8>) {
    output.push(b'"');
    for byte in value {
        match byte {
            b'"' | b'\\' => {
                output.push(b'\\');
                output.push(*byte);
            }
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\t' => output.extend_from_slice(b"\\t"),
            8 => output.extend_from_slice(b"\\b"),
            byte => output.push(*byte),
        }
    }
    output.push(b'"');
}

fn serialize(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let mut text = Vec::new();
    for entry in entries {
        let first = entry
            .key
            .iter()
            .position(|byte| *byte == b'.')
            .ok_or("guarded Git captured key is malformed")?;
        let last = entry
            .key
            .iter()
            .rposition(|byte| *byte == b'.')
            .ok_or("guarded Git captured key is malformed")?;
        text.push(b'[');
        text.extend_from_slice(&entry.key[..first]);
        if last > first {
            text.push(b' ');
            text.push(b'"');
            for byte in &entry.key[first + 1..last] {
                if matches!(*byte, b'"' | b'\\') {
                    text.push(b'\\');
                }
                text.push(*byte);
            }
            text.push(b'"');
        }
        text.extend_from_slice(b"]\n\t");
        text.extend_from_slice(&entry.key[last + 1..]);
        if let Some(value) = &entry.value {
            text.extend_from_slice(b" = ");
            quote(value, &mut text);
        }
        text.push(b'\n');
        if text.len() > MAX_SNAPSHOT_BYTES {
            return Err("guarded Git global snapshot exceeds its environment size bound".into());
        }
    }
    Ok(text)
}

fn os_string(bytes: Vec<u8>) -> Result<OsString, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(OsString::from)
            .map_err(|_| "guarded Git captured data is not representable on this platform".into())
    }
}

pub(super) fn validate_environment(command: &tokio::process::Command) -> Result<(), String> {
    let mut units = 1usize;
    for (key, value) in command.as_std().get_envs() {
        let Some(value) = value else { continue };
        #[cfg(windows)]
        let count = {
            use std::os::windows::ffi::OsStrExt;
            key.encode_wide().count() + value.encode_wide().count() + 2
        };
        #[cfg(not(windows))]
        let count = key.len() + value.len() + 2;
        units = units
            .checked_add(count)
            .ok_or("guarded Git environment exceeds its size bound")?;
    }
    let limit = if cfg!(windows) { 30_000 } else { 96 * 1024 };
    if units > limit {
        return Err("guarded Git environment exceeds its platform size bound; global configuration was not truncated".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(storage: &Path) -> GitRunner {
        let runtime = super::super::runtime::GuardedRuntime::materialize(
            super::super::runtime::bundled_approval().expect("test requires approved bundle"),
            storage,
            &[],
        )
        .unwrap();
        GitRunner::from_runtime(Arc::new(runtime)).unwrap()
    }

    #[tokio::test]
    async fn canonical_snapshot_roundtrips_using_actual_git_parser() {
        let fixture = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let runner = runner(&fixture.path().join("runtime"));
        let entries = vec![
            Entry {
                key: b"filter.a.b\\c\"d.clean".to_vec(),
                value: Some(b"!value\n\t\r\x08\\\"x".to_vec()),
            },
            Entry {
                key: b"credential.helper".to_vec(),
                value: Some(Vec::new()),
            },
            Entry {
                key: b"credential.helper".to_vec(),
                value: Some(b"!approved duplicate".to_vec()),
            },
            Entry {
                key: b"core.bare".to_vec(),
                value: None,
            },
        ];
        let args =
            ["config", "--null", "--no-includes", "--file", "-", "--list"].map(OsString::from);
        let bytes = runner
            .snapshot_probe(
                fixture.path(),
                &args,
                Some(serialize(&entries).unwrap()),
                None,
            )
            .await
            .unwrap();
        assert!(parse_entries(&bytes).unwrap() == entries);
        let mut command = tokio::process::Command::new(runner.guarded_runtime().unwrap().program());
        command.env_clear().env(ENVIRONMENT, "x".repeat(100 * 1024));
        assert!(
            validate_environment(&command)
                .unwrap_err()
                .contains("not truncated")
        );
    }

    #[tokio::test]
    async fn startup_graph_freezes_unselected_and_missing_sources() {
        let fixture = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let project = fixture.path().join("project");
        let authority = fixture.path().join("operator");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&authority).unwrap();
        let runner = runner(&fixture.path().join("runtime"));
        let empty = GlobalSources {
            sources: Vec::new(),
            roots: Vec::new(),
            helpers: HashMap::new(),
            #[cfg(windows)]
            shell: Err("fixture has no selected shell service".into()),
        };
        let runner = runner.with_test_globals(empty);
        runner
            .run(
                &project,
                "init",
                ["init", "--quiet"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        let root = authority.join("global");
        let later = authority.join("later");
        let missing = authority.join("missing");
        std::fs::write(&root, "[user]\nname = approved\n[includeIf \"onbranch:later\"]\npath = later\n[include]\npath = missing\n").unwrap();
        std::fs::write(&later, "[user]\nemail = retained@example.invalid\n").unwrap();
        let captured = GlobalSources::capture_roots(&runner, &project, vec![root.clone()], None)
            .await
            .unwrap();
        assert_eq!(
            captured.sources.len(),
            2,
            "unselected conditional source is frozen"
        );
        // A writable descriptor survives unlinking its workspace hardlink.
        // Future launch authority must use captured bytes, never nlink/path
        // readback of the now singly-linked operator file.
        let alias = project.join("held-global-alias");
        std::fs::hard_link(&root, &alias).unwrap();
        let mut held = std::fs::OpenOptions::new()
            .write(true)
            .open(&alias)
            .unwrap();
        std::fs::remove_file(&alias).unwrap();
        held.set_len(0).unwrap();
        std::io::Write::write_all(&mut held, b"[user]\nname = changed-through-held-fd\n").unwrap();
        std::fs::write(&later, "[user]\nemail = changed@example.invalid\n").unwrap();
        std::fs::write(&missing, "[user]\nname = newly-created\n").unwrap();
        let runner = runner.with_test_globals(captured);
        runner
            .run(
                &project,
                "branch",
                ["symbolic-ref", "HEAD", "refs/heads/later"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        let output = runner
            .run(
                &project,
                "config",
                ["config", "--get", "user.email"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, b"retained@example.invalid\n");
        let output = runner
            .run(
                &project,
                "config",
                ["config", "--get", "user.name"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, b"approved\n");
        std::fs::write(project.join(".git/config"), "[user]\nname = local preference\n[remote \"origin\"]\nurl = https://example.invalid/repo\n").unwrap();
        let output = runner
            .run(
                &project,
                "config",
                ["config", "--get", "user.name"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(
            output.stdout, b"local preference\n",
            "local data retains precedence"
        );
        let output = runner
            .run(
                &project,
                "config",
                ["config", "--get", "remote.origin.url"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, b"https://example.invalid/repo\n");
    }
    #[tokio::test]
    async fn linked_admin_sources_do_not_become_trusted_global_input() {
        let fixture = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let root = fixture.path();
        let main = root.join("main");
        let linked = root.join("linked");
        std::fs::create_dir(&main).unwrap();
        let runner = runner(&root.join("runtime"));
        let empty = GlobalSources::capture_roots(&runner, &main, Vec::new(), None)
            .await
            .unwrap();
        let runner = runner.with_test_globals(empty);
        runner
            .run(
                &main,
                "init",
                ["init", "--quiet"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        runner
            .run(
                &main,
                "commit",
                [
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "initial",
                ],
                super::super::runner::LOCAL_MUTATION_LIMITS,
            )
            .await
            .unwrap();
        runner
            .run(
                &main,
                "worktree",
                [
                    std::ffi::OsStr::new("worktree"),
                    "add".as_ref(),
                    "--detach".as_ref(),
                    linked.as_os_str(),
                ],
                super::super::runner::LOCAL_MUTATION_LIMITS,
            )
            .await
            .unwrap();
        let source = main.join(".git/operator-looking-global");
        std::fs::write(
            &source,
            "[credential]\nhelper = !untrusted-global-looking-command\n",
        )
        .unwrap();
        let result = GlobalSources::capture_roots(&runner, &linked, vec![source], None).await;
        assert!(result.err().unwrap().contains("model-writable"));
    }
    #[tokio::test]
    async fn explicit_null_global_source_keeps_guarded_git_functional() {
        let fixture = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let project = fixture.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let runner = runner(&fixture.path().join("runtime"));
        let null = PathBuf::from(if cfg!(windows) { "NUL" } else { "/dev/null" });
        let sources = GlobalSources::capture_roots(&runner, &project, vec![null], None)
            .await
            .unwrap();
        assert!(sources.sources.is_empty());
        let runner = runner.with_test_globals(sources);
        runner
            .run(
                &project,
                "init",
                ["init", "--quiet"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        assert!(
            runner
                .run(
                    &project,
                    "config",
                    ["config", "--get", "user.name"],
                    super::super::runner::QUERY_LIMITS
                )
                .await
                .is_err()
        );
        std::fs::write(project.join("payload"), "ordinary\n").unwrap();
        runner
            .run(
                &project,
                "add",
                ["add", "--", "payload"],
                super::super::runner::LOCAL_MUTATION_LIMITS,
            )
            .await
            .unwrap();
        runner
            .run(
                &project,
                "commit",
                [
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "null globals",
                ],
                super::super::runner::LOCAL_MUTATION_LIMITS,
            )
            .await
            .unwrap();
        let output = runner
            .run(
                &project,
                "show",
                ["show", "HEAD:payload"],
                super::super::runner::QUERY_LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, b"ordinary\n");
        #[cfg(unix)]
        assert!(
            read_source(Path::new("/dev/zero"), &[]).is_err(),
            "null exception must not bless arbitrary devices"
        );
    }
}
