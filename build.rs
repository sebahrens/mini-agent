use std::env;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};
mod build_inputs;
use build_inputs::hash_field;

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
    let inputs = build_inputs::collect(&root);
    for directory in build_inputs::SOURCE_DIRECTORIES
        .iter()
        .copied()
        .chain(std::iter::once(build_inputs::PATCHED_RIG))
    {
        println!("cargo:rerun-if-changed={}", root.join(directory).display());
    }

    let mut digest = Sha256::new();
    hash_field(&mut digest, b"mini-agent/exact-build-v1");
    for input in &inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    build_inputs::hash(&mut digest, &root, &inputs);

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
    println!(
        "cargo:rustc-env=MINI_AGENT_BUILD_TARGET={}",
        env::var("TARGET").expect("Cargo must provide TARGET")
    );
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
