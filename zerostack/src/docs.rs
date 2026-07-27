use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/docs");

pub fn global_docs_dir() -> PathBuf {
    crate::session::storage::data_dir().join("docs")
}

pub fn show_get_started() -> anyhow::Result<()> {
    ensure_global()?;
    let doc_path = global_docs_dir().join("GET_STARTED.md");
    if !doc_path.exists() {
        anyhow::bail!(
            "GET_STARTED.md not found at {}. Try reinstalling zerostack.",
            doc_path.display()
        );
    }
    let status = std::process::Command::new("less").arg(&doc_path).status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
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
