//! Executable resolution for trusted launchers (hooks, language servers, and
//! stdio MCP servers).
//!
//! A `PATH` entry that is empty or relative names a directory relative to the
//! launcher's current directory, which is normally the selected workspace. A
//! repository could therefore plant `rust-analyzer` (or any configured bare
//! command) at a matching relative location and have it run with the
//! launcher's authority. Every resolver here consults only absolute `PATH`
//! entries and never treats the process cwd as a search root.

use std::ffi::{OsStr, OsString};
#[cfg(any(feature = "lsp", feature = "mcp"))]
use std::path::Path;
use std::path::PathBuf;

/// Keeps only absolute `PATH` entries. A relative or empty entry would
/// otherwise resolve a bare command against the current (workspace)
/// directory. Returns `None` when no absolute entry remains, because an empty
/// search string would itself mean "the current directory".
pub(crate) fn absolute_search_path(search_path: Option<&OsStr>) -> Option<OsString> {
    let search_path = search_path?;
    let absolute: Vec<PathBuf> = std::env::split_paths(search_path)
        .filter(|entry| entry.is_absolute())
        .collect();
    if absolute.is_empty() {
        return None;
    }
    std::env::join_paths(absolute).ok()
}

/// Resolves the configured executable of a trusted workspace service (LSP
/// server or stdio MCP server) to the identity passed to `exec`.
///
/// * An absolute path is used as written (it must name an executable file).
/// * A bare name (one path component) is searched only through the absolute
///   entries of `search_path`; relative and empty entries are ignored.
/// * A relative path with a directory component (`./server`, `bin/server`)
///   is accepted only when the configuration supplies an explicit `anchor`
///   directory; it then resolves from that directory and must stay inside
///   it. Without an anchor it is rejected rather than resolved against the
///   process cwd.
#[cfg(any(feature = "lsp", feature = "mcp"))]
pub(crate) fn resolve_service_executable(
    command: &str,
    search_path: Option<&OsStr>,
    anchor: Option<&Path>,
) -> Result<PathBuf, String> {
    if command.is_empty() {
        return Err("executable is empty".to_string());
    }
    let program = Path::new(command);
    if program.is_absolute() {
        return which::which_in(program, None::<&OsStr>, program)
            .map_err(|error| format!("executable '{command}' was not found: {error}"));
    }
    if program.components().count() > 1 {
        let Some(anchor) = anchor else {
            return Err(format!(
                "relative executable path '{command}' has a directory component and no \
                 configured working directory anchors it; use an absolute path or a bare \
                 name resolved through absolute PATH entries"
            ));
        };
        let anchor = anchor.canonicalize().map_err(|error| {
            format!(
                "anchor directory '{}' is unavailable: {error}",
                anchor.display()
            )
        })?;
        let resolved = which::which_in(program, None::<&OsStr>, &anchor)
            .map_err(|error| format!("executable '{command}' was not found: {error}"))?;
        let resolved = resolved
            .canonicalize()
            .map_err(|error| format!("executable '{command}' cannot be resolved: {error}"))?;
        if !resolved.starts_with(&anchor) {
            return Err(format!(
                "relative executable path '{command}' escapes its configured working directory"
            ));
        }
        return Ok(resolved);
    }
    let search_path = absolute_search_path(search_path).ok_or_else(|| {
        format!("executable '{command}' was not found: PATH has no absolute entries")
    })?;
    // Every remaining entry is absolute and the name has no separator, so the
    // cwd argument is never consulted; pass the first search root rather than
    // the (workspace) process cwd.
    let neutral_cwd = std::env::split_paths(&search_path)
        .next()
        .unwrap_or_default();
    which::which_in(program, Some(&search_path), neutral_cwd)
        .map_err(|error| format!("executable '{command}' was not found: {error}"))
}

/// Test fixture: a temporary `bin` directory outside a temporary workspace,
/// with helpers to plant fake executables in either location.
#[cfg(all(test, unix, any(feature = "lsp", feature = "mcp")))]
pub(crate) struct PlantedExecutables {
    pub(crate) root: PathBuf,
    pub(crate) bin: PathBuf,
    pub(crate) workspace: PathBuf,
}

#[cfg(all(test, unix, any(feature = "lsp", feature = "mcp")))]
impl PlantedExecutables {
    pub(crate) fn new(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("exec-search-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let bin = root.join("bin");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        Self {
            root,
            bin,
            workspace,
        }
    }

    pub(crate) fn plant(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `PATH` with empty and relative (`.`) entries around the absolute bin,
    /// in both orders, so a cwd-relative resolver would find the workspace
    /// copy first.
    pub(crate) fn hostile_search_paths(&self) -> Vec<OsString> {
        let bin = self.bin.display();
        vec![
            OsString::from(format!("{bin}::")),
            OsString::from(format!("::.:{bin}")),
        ]
    }
}

#[cfg(all(test, unix, any(feature = "lsp", feature = "mcp")))]
impl Drop for PlantedExecutables {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[cfg(all(unix, any(feature = "lsp", feature = "mcp")))]
    #[test]
    fn bare_names_resolve_only_through_absolute_entries() {
        let fixture = PlantedExecutables::new("bare");
        let planted = PlantedExecutables::plant(&fixture.workspace, "fake-ls");
        for search in fixture.hostile_search_paths() {
            // Control: an unfiltered cwd-relative lookup from the workspace
            // does find the planted copy, so the fixture is attack-capable.
            if search.to_string_lossy().starts_with(':') {
                let found = which::which_in("fake-ls", Some(&search), &fixture.workspace).unwrap();
                assert_eq!(found.canonicalize().unwrap(), planted);
            }
            let error = resolve_service_executable("fake-ls", Some(&search), None).unwrap_err();
            assert!(error.contains("not found"), "{error}");
        }
        let trusted = PlantedExecutables::plant(&fixture.bin, "fake-ls");
        for search in fixture.hostile_search_paths() {
            assert_eq!(
                resolve_service_executable("fake-ls", Some(&search), None).unwrap(),
                trusted
            );
        }
    }

    #[cfg(all(unix, any(feature = "lsp", feature = "mcp")))]
    #[test]
    fn anchored_relative_paths_stay_inside_their_anchor() {
        let fixture = PlantedExecutables::new("anchored");
        let planted = PlantedExecutables::plant(&fixture.workspace, "tools/server");
        assert_eq!(
            resolve_service_executable("./tools/server", None, Some(&fixture.workspace)).unwrap(),
            planted
        );
        PlantedExecutables::plant(&fixture.bin, "server");
        let error = resolve_service_executable("../bin/server", None, Some(&fixture.workspace))
            .unwrap_err();
        assert!(error.contains("escapes"), "{error}");
    }

    #[test]
    fn relative_and_empty_entries_are_dropped() {
        let bin = std::env::temp_dir();
        let joined =
            std::env::join_paths([bin.as_path(), Path::new(""), Path::new("rel")]).unwrap();
        let filtered = absolute_search_path(Some(&joined)).unwrap();
        assert_eq!(
            std::env::split_paths(&filtered).collect::<Vec<_>>(),
            vec![bin]
        );
        assert!(absolute_search_path(Some(OsStr::new(""))).is_none());
        assert!(absolute_search_path(None).is_none());
    }

    #[cfg(any(feature = "lsp", feature = "mcp"))]
    #[test]
    fn unanchored_relative_paths_with_a_directory_are_rejected() {
        let error = resolve_service_executable("./server", Some(OsStr::new("")), None).unwrap_err();
        assert!(error.contains("directory component"), "{error}");
        let error = resolve_service_executable("bin/server", None, None).unwrap_err();
        assert!(error.contains("directory component"), "{error}");
    }

    #[cfg(any(feature = "lsp", feature = "mcp"))]
    #[test]
    fn empty_commands_are_rejected() {
        assert!(resolve_service_executable("", None, None).is_err());
    }
}
