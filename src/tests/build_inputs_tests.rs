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
        "data/prompts/default.md",
        "docs/agent/CONFIG.md",
        "docs/vscode-acp-setup.md",
        "assets/learned-skills/json-parse.json",
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

#[test]
fn build_inputs_cover_embedded_guides_and_skill_packages_without_maintainer_docs() {
    let root = fixture();
    let mut previous = fingerprint(root.path());
    for relative in [
        "docs/agent/CONFIG.md",
        "docs/vscode-acp-setup.md",
        "assets/learned-skills/json-parse.json",
        "data/prompts/default.md",
    ] {
        std::fs::write(root.path().join(relative), b"changed embedded content\n").unwrap();
        let current = fingerprint(root.path());
        assert_ne!(previous, current, "embedded input omitted: {relative}");
        previous = current;
    }
    let uncompiled = root.path().join("docs/reviews/private-review.md");
    std::fs::create_dir_all(uncompiled.parent().unwrap()).unwrap();
    std::fs::write(uncompiled, b"uncompiled maintainer notes\n").unwrap();
    assert_eq!(previous, fingerprint(root.path()));
    let embedded_cache = root.path().join("docs/agent/__pycache__/embedded.pyc");
    std::fs::create_dir_all(embedded_cache.parent().unwrap()).unwrap();
    std::fs::write(embedded_cache, b"also embedded by include_dir!\n").unwrap();
    assert_ne!(previous, fingerprint(root.path()));
    let inputs = build_inputs::collect(root.path());
    assert!(inputs.windows(2).all(|pair| pair[0] < pair[1]));
}

#[cfg(unix)]
#[test]
fn build_inputs_refuse_linked_source_and_embedded_files_or_directories() {
    for (relative, directory) in [
        ("src/linked.rs", false),
        ("docs/agent/linked.md", false),
        ("docs/agent/linked-directory", true),
        ("docs/agent", true),
    ] {
        let root = fixture();
        let target = root.path().join("uncompiled-link-target");
        if directory {
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("content.md"), b"linked input\n").unwrap();
        } else {
            std::fs::write(&target, b"linked input\n").unwrap();
        }
        let link = root.path().join(relative);
        if link.exists() {
            std::fs::remove_dir_all(&link).unwrap();
        }
        std::os::unix::fs::symlink(target, link).unwrap();
        let error = std::panic::catch_unwind(|| build_inputs::collect(root.path()))
            .expect_err("compile input links must fail instead of silently losing their bytes");
        let message = error
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| error.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(message.contains("must not be a symbolic link"), "{message}");
    }
}
