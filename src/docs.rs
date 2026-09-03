use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

use crate::process_creation::StdCommandCreationExt;

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/docs");

pub fn global_docs_dir() -> PathBuf {
    crate::paths::process_paths()
        .expect("startup must initialize application paths")
        .docs_dir()
}

pub fn show_get_started() -> anyhow::Result<()> {
    ensure_global()?;
    let doc_path = global_docs_dir().join("GET_STARTED.md");
    if !doc_path.exists() {
        anyhow::bail!(
            "GET_STARTED.md not found at {}. Try reinstalling {}.",
            doc_path.display(),
            crate::product::PUBLIC_NAME
        );
    }
    // Never `process::exit` from here: the TUI's `/tutor` calls this with the
    // terminal suspended and must get control back to restore it.
    match std::process::Command::new("less")
        .arg(&doc_path)
        .status_guarded()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("less exited with {}", status),
        // No pager on this system (typical on Windows): print the document.
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
    let dir = global_docs_dir();
    let version_file = dir.join("current_version");
    let current_version = env!("CARGO_PKG_VERSION");

    let should_copy = match std::fs::read_to_string(&version_file) {
        Ok(stored) => stored.trim() != current_version,
        Err(_) => true,
    };

    if should_copy {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        copy_embedded(&dir)?;
        std::fs::write(&version_file, current_version)?;
        return Ok(true);
    }

    Ok(false)
}

fn copy_embedded(dest: &Path) -> anyhow::Result<()> {
    for file in EMBEDDED.files() {
        if let Some(name) = file.path().file_name().and_then(|s| s.to_str()) {
            let dest_path = dest.join(name);
            if let Some(content) = file.contents_utf8() {
                std::fs::write(&dest_path, content)?;
            }
        }
    }
    Ok(())
}
