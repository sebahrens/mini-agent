//! Deterministic source inputs shared by the build script and its native tests.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub(crate) const SOURCE_DIRECTORIES: &[&str] = &["data", "scripts", "src"];
pub(crate) const EMBEDDED_DIRECTORIES: &[&str] = &[
    "docs/agent",
    "assets/learned-skills",
    "data/agents",
    "data/prompts",
    "data/themes",
];
pub(crate) const PATH_DEPENDENCIES: &[&str] = &[
    "patched-deps/rig-core",
    "patched-deps/google-cloud-auth",
    "patched-deps/crossterm",
];

pub(crate) fn collect(root: &Path) -> Vec<PathBuf> {
    let mut files = [
        "Cargo.lock",
        "Cargo.toml",
        "build.rs",
        "build_inputs.rs",
        "docs/vscode-acp-setup.md",
    ]
    .into_iter()
    .map(|path| root.join(path))
    .collect::<Vec<_>>();
    for directory in SOURCE_DIRECTORIES {
        collect_directory(&root.join(directory), &mut files, true);
    }
    // include_dir! embeds every regular file, including files whose names
    // would otherwise identify an uncompiled cache in source directories.
    for directory in EMBEDDED_DIRECTORIES {
        collect_directory(&root.join(directory), &mut files, false);
    }
    // Cargo.lock has no content checksums for local path dependencies. Their
    // manifests, Rust sources and optional build scripts bind exact identity.
    for dependency in PATH_DEPENDENCIES {
        let dependency = root.join(dependency);
        files.push(dependency.join("Cargo.toml"));
        let build = dependency.join("build.rs");
        if build.exists() {
            files.push(build);
        }
        collect_directory(&dependency.join("src"), &mut files, true);
    }
    files.sort();
    files.dedup();
    files
}

fn collect_directory(directory: &Path, files: &mut Vec<PathBuf>, ignore_caches: bool) {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => panic!("failed to inspect {}: {error}", directory.display()),
    };
    assert!(
        !metadata.file_type().is_symlink(),
        "build input directory must not be a symbolic link: {}",
        directory.display()
    );
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", directory.display()));
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if ignore_caches
            && (path.file_name().is_some_and(|name| name == "__pycache__")
                || path
                    .extension()
                    .is_some_and(|extension| extension == "pyc" || extension == "pyo"))
        {
            continue;
        }
        let kind = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
        assert!(
            !kind.is_symlink(),
            "build input must not be a symbolic link: {}",
            path.display()
        );
        if kind.is_dir() {
            collect_directory(&path, files, ignore_caches);
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
