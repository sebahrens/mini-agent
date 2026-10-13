#[path = "../../build_inputs.rs"]
mod build_inputs;

use sha2::{Digest, Sha256};

fn fixture() -> crate::tests::OwnedTestDirectory {
    let directory = crate::tests::OwnedTestDirectory::new("build-inputs");
    for path in [
        "Cargo.lock",
        "Cargo.toml",
        "build.rs",
        "build_inputs.rs",
        "src/main.rs",
        "scripts/helper.py",
        "data/prompt.md",
        "patched-deps/rig-core/Cargo.toml",
        "patched-deps/rig-core/src/lib.rs",
    ] {
        let path = directory.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"original input\n").unwrap();
    }
    directory
}

fn fingerprint(root: &std::path::Path) -> String {
    let mut digest = Sha256::new();
    build_inputs::hash_field(&mut digest, b"mini-agent/exact-build-v1");
    build_inputs::hash(&mut digest, root, &build_inputs::collect(root));
    crate::hex::encode_lower(digest.finalize())
}

#[test]
fn build_inputs_include_path_dependency_and_detect_metadata_preserving_changes() {
    let root = fixture();
    let path = root.path().join("patched-deps/rig-core/src/lib.rs");
    let before = std::fs::metadata(&path).unwrap();
    let original = fingerprint(root.path());
    std::fs::write(&path, b"modified input\n").unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    assert_eq!(before.len(), file.metadata().unwrap().len());
    assert_eq!(
        before.modified().unwrap(),
        file.metadata().unwrap().modified().unwrap()
    );
    assert_ne!(
        original,
        fingerprint(root.path()),
        "SDK content must change identity even with identical metadata"
    );
    let changed = fingerprint(root.path());
    std::fs::write(
        root.path().join("patched-deps/rig-core/Cargo.toml"),
        b"new manifest\n",
    )
    .unwrap();
    assert_ne!(
        changed,
        fingerprint(root.path()),
        "the path dependency manifest is a compile input too"
    );
}

#[test]
fn build_input_identity_is_location_independent_and_ignores_uncompiled_caches() {
    let first = fixture();
    let second = fixture();
    assert_eq!(fingerprint(first.path()), fingerprint(second.path()));
    for path in [
        "src/__pycache__/ignored.pyc",
        "scripts/ignored.pyc",
        "scripts/ignored.pyo",
        "patched-deps/rig-core/target/debug/cache",
    ] {
        let path = first.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"ignored cache data").unwrap();
    }
    assert_eq!(fingerprint(first.path()), fingerprint(second.path()));
    let inputs = build_inputs::collect(first.path());
    assert!(inputs.windows(2).all(|pair| pair[0] < pair[1]));
    std::fs::write(
        first.path().join("patched-deps/rig-core/build.rs"),
        b"fn main() {}\n",
    )
    .unwrap();
    assert_ne!(
        fingerprint(first.path()),
        fingerprint(second.path()),
        "a future dependency build script must join the identity"
    );
}
