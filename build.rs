use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const BUILD_ENVIRONMENT: &[&str] = &[
    "CARGO_CFG_TARGET_ABI",
    "CARGO_CFG_TARGET_ARCH",
    "CARGO_CFG_TARGET_ENV",
    "CARGO_CFG_TARGET_FEATURE",
    "CARGO_CFG_TARGET_OS",
    "CARGO_ENCODED_RUSTFLAGS",
    "DEBUG",
    "HOST",
    "OPT_LEVEL",
    "PROFILE",
    "TARGET",
];

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let mut inputs = vec![
        root.join("Cargo.lock"),
        root.join("Cargo.toml"),
        root.join("build.rs"),
    ];
    for directory in ["data", "scripts", "src"] {
        collect_files(&root.join(directory), &mut inputs);
    }
    inputs.sort();

    let mut digest = Sha256::new();
    hash_field(&mut digest, b"mini-agent/exact-build-v1");
    for input in inputs {
        println!("cargo:rerun-if-changed={}", input.display());
        let relative = input
            .strip_prefix(&root)
            .expect("build input must be inside the manifest directory")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        hash_field(&mut digest, relative.as_bytes());
        hash_field(
            &mut digest,
            &fs::read(&input).unwrap_or_else(|error| {
                panic!(
                    "failed to read exact-build input {}: {error}",
                    input.display()
                )
            }),
        );
    }

    for key in BUILD_ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={key}");
        hash_environment(&mut digest, key);
    }
    let mut features = env::vars()
        .filter(|(key, _)| key.starts_with("CARGO_FEATURE_"))
        .collect::<Vec<_>>();
    features.sort();
    for (key, value) in features {
        hash_field(&mut digest, key.as_bytes());
        hash_field(&mut digest, value.as_bytes());
    }

    println!("cargo:rerun-if-env-changed=RUSTC");
    let rustc = env::var_os("RUSTC").expect("Cargo must provide RUSTC");
    let rustc_version = Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("query rustc version for exact-build identity");
    if !rustc_version.status.success() {
        panic!("rustc -vV failed while computing exact-build identity");
    }
    hash_field(&mut digest, &rustc_version.stdout);

    let fingerprint = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("cargo:rustc-env=MINI_AGENT_BUILD_FINGERPRINT={fingerprint}");
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
    if !directory.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", directory.display()));
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
        if file_type.is_dir() {
            collect_files(&path, files);
        } else if file_type.is_file() {
            files.push(path);
        }
    }
}

fn hash_environment(digest: &mut Sha256, key: &str) {
    hash_field(digest, key.as_bytes());
    hash_field(
        digest,
        env::var_os(key)
            .unwrap_or_default()
            .to_string_lossy()
            .as_bytes(),
    );
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}
