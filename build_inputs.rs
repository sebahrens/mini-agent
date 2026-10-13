//! Deterministic source inputs shared by the build script and its native tests.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub(crate) const SOURCE_DIRECTORIES: &[&str] = &["data", "scripts", "src"];
pub(crate) const PATCHED_RIG: &str = "patched-deps/rig-core";

pub(crate) fn collect(root: &Path) -> Vec<PathBuf> {
    let mut files = ["Cargo.lock", "Cargo.toml", "build.rs", "build_inputs.rs"]
        .into_iter()
        .map(|path| root.join(path))
        .collect::<Vec<_>>();
    for directory in SOURCE_DIRECTORIES {
        collect_directory(&root.join(directory), &mut files);
    }
    // Cargo.lock has no content checksum for this path dependency. Its
    // manifest and Rust sources are part of the production binary's identity.
    let rig = root.join(PATCHED_RIG);
    files.push(rig.join("Cargo.toml"));
    let build = rig.join("build.rs");
    if build.exists() {
        files.push(build);
    }
    collect_directory(&rig.join("src"), &mut files);
    files.sort();
    files
}

fn collect_directory(directory: &Path, files: &mut Vec<PathBuf>) {
    if !directory.exists() {
        return;
    }
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", directory.display()));
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == "__pycache__")
            || path
                .extension()
                .is_some_and(|extension| extension == "pyc" || extension == "pyo")
        {
            continue;
        }
        let kind = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
        if kind.is_dir() {
            collect_directory(&path, files);
        } else if kind.is_file() {
            files.push(path);
        }
    }
}

pub(crate) fn hash(digest: &mut Sha256, root: &Path, inputs: &[PathBuf]) {
    for path in inputs {
        let relative = path
            .strip_prefix(root)
            .expect("build input must stay inside manifest root")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        hash_field(digest, relative.as_bytes());
        hash_field(
            digest,
            &fs::read(path).unwrap_or_else(|error| {
                panic!(
                    "failed to read exact-build input {}: {error}",
                    path.display()
                )
            }),
        );
    }
}

pub(crate) fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}
