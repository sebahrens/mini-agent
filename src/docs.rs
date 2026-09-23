use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

#[cfg(not(windows))]
use crate::process_creation::StdCommandCreationExt;

/// User-facing documentation shipped with the binary. Only `docs/agent`
/// (installed as `<docs_dir>/agent/...`) and the top-level user guides below
/// are embedded; maintainer material under `docs/` (plans, reviews, specs,
/// benchmarks, decisions) stays in the repository.
static EMBEDDED_AGENT: Dir = include_dir!("$CARGO_MANIFEST_DIR/docs/agent");

/// Directory under the installed docs root that mirrors `docs/agent`.
pub const AGENT_DOCS_SUBDIR: &str = "agent";

/// A nested file whose presence proves the install used the recursive layout.
const INSTALL_SENTINEL: &str = "agent/CONFIG.md";

/// Top-level user guides installed at the docs root, keyed by file name.
const EMBEDDED_TOP_LEVEL: &[(&str, &str)] = &[(
    "vscode-acp-setup.md",
    include_str!("../docs/vscode-acp-setup.md"),
)];

pub fn global_docs_dir() -> PathBuf {
    crate::paths::process_paths()
        .expect("startup must initialize application paths")
        .docs_dir()
}

pub fn show_get_started() -> anyhow::Result<()> {
    ensure_global()?;
    let doc_path = global_docs_dir()
        .join(AGENT_DOCS_SUBDIR)
        .join("GET_STARTED.md");
    if !doc_path.exists() {
        anyhow::bail!(
            "GET_STARTED.md not found at {}. Try reinstalling {}.",
            doc_path.display(),
            crate::product::PUBLIC_NAME
        );
    }
    #[cfg(windows)]
    return print_document(&doc_path);

    // Never `process::exit` from here: the TUI's `/tutor` calls this with the
    // terminal suspended and must get control back to restore it.
    #[cfg(not(windows))]
    match std::process::Command::new("less")
        .arg(&doc_path)
        .status_guarded()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("less exited with {}", status),
        // No pager on this system: print the document.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => print_document(&doc_path),
        Err(error) => Err(anyhow::Error::new(error).context("failed to launch less")),
    }
}

fn print_document(path: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    let text = std::fs::read_to_string(path)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

pub fn ensure_global() -> anyhow::Result<bool> {
    ensure_installed(&global_docs_dir())
}

/// Install the embedded docs into `dir` when the stored version differs or
/// the install predates the nested layout. Returns whether it copied.
fn ensure_installed(dir: &Path) -> anyhow::Result<bool> {
    let version_file = dir.join("current_version");
    let current_version = env!("CARGO_PKG_VERSION");

    // Installs written before nested docs were copied carry the current
    // version but no `agent/` tree; the sentinel check repairs them.
    let should_copy = match std::fs::read_to_string(&version_file) {
        Ok(stored) => stored.trim() != current_version || !dir.join(INSTALL_SENTINEL).is_file(),
        Err(_) => true,
    };

    if should_copy {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        copy_embedded(dir)?;
        std::fs::write(&version_file, current_version)?;
        return Ok(true);
    }

    Ok(false)
}

/// Install every embedded document under `dest`, preserving each file's
/// path relative to its embedded root (e.g. `agent/providers/Minimax.md`).
fn copy_embedded(dest: &Path) -> anyhow::Result<()> {
    // `Dir::extract` recurses and joins each entry's root-relative path, so
    // nested files such as `providers/Minimax.md` keep their directories.
    let agent_dest = dest.join(AGENT_DOCS_SUBDIR);
    std::fs::create_dir_all(&agent_dest)?;
    EMBEDDED_AGENT.extract(&agent_dest)?;
    for (name, content) in EMBEDDED_TOP_LEVEL {
        std::fs::write(dest.join(name), content)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed_tree() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("zs_docs_test_{}", uuid::Uuid::new_v4()));
        let dest = root.join("docs");
        std::fs::create_dir_all(&dest).unwrap();
        copy_embedded(&dest).unwrap();
        (root, dest)
    }

    #[test]
    fn installed_docs_preserve_nested_relative_paths() {
        let (root, dest) = installed_tree();
        for relative in [
            "agent/CONFIG.md",
            "agent/GET_STARTED.md",
            "agent/COMMANDS.md",
            "agent/providers/Minimax.md",
            "vscode-acp-setup.md",
        ] {
            assert!(dest.join(relative).is_file(), "missing {relative}");
        }
        let installed = std::fs::read_to_string(dest.join("agent/CONFIG.md")).unwrap();
        assert_eq!(
            installed,
            EMBEDDED_AGENT
                .get_file("CONFIG.md")
                .unwrap()
                .contents_utf8()
                .unwrap()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_flat_install_with_the_current_version_is_repaired() {
        let root = std::env::temp_dir().join(format!("zs_docs_test_{}", uuid::Uuid::new_v4()));
        let dest = root.join("docs");
        std::fs::create_dir_all(&dest).unwrap();
        // The pre-fix layout: flattened top-level files and a current version.
        std::fs::write(dest.join("vscode-acp-setup.md"), "old").unwrap();
        std::fs::write(dest.join("current_version"), env!("CARGO_PKG_VERSION")).unwrap();

        assert!(ensure_installed(&dest).unwrap());
        assert!(dest.join("agent/CONFIG.md").is_file());
        assert!(!ensure_installed(&dest).unwrap(), "second call is a no-op");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn installed_docs_exclude_maintainer_material() {
        let (root, dest) = installed_tree();
        for excluded in [
            "plans",
            "reviews",
            "specs",
            "benchmarks",
            "decisions",
            "superpowers",
        ] {
            assert!(!dest.join(excluded).exists(), "{excluded} was installed");
            assert!(!dest.join(AGENT_DOCS_SUBDIR).join(excluded).exists());
        }
        // No flattened copies of nested files at the root.
        assert!(!dest.join("CONFIG.md").exists());
        assert!(!dest.join("Minimax.md").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
