//! Build-time embedding of an explicitly supplied, publisher-built Git bundle.
//! The manifest is inventory, not runtime authority: its captured bytes and
//! digests become constants in the trusted application build.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const POLICY: &str = "mini-agent-executable-scope-v3-env";
const SOURCE: &str = "26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3";
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MAX_TOTAL: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct PreparedRuntime {
    pub(crate) identity: String,
    pub(crate) inputs: Vec<PathBuf>,
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let before = fs::symlink_metadata(path).map_err(|_| "Git bundle input is missing")?;
    if !before.is_file() || before.len() > limit {
        return Err("Git bundle input must be a bounded regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.nlink() != 1 {
            return Err("Git bundle input must have one link".into());
        }
    }
    let file = fs::File::open(path).map_err(|_| "Cannot open Git bundle input")?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect Git bundle input")?;
    if !opened.is_file() || opened.len() != before.len() {
        return Err("Git bundle input changed while opening".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1 || opened.dev() != before.dev() || opened.ino() != before.ino() {
            return Err("Git bundle input changed while opening".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read Git bundle input")?;
    if bytes.len() as u64 != before.len() || bytes.len() as u64 > limit {
        return Err("Git bundle input changed while reading".into());
    }
    Ok(bytes)
}

fn safe_leaf(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 128
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        || name.contains("..")
        || name.ends_with('.')
    {
        return false;
    }
    let stem = name.split('.').next().unwrap().to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

fn digest_text(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn prepare_default(
    root: &Path,
    out: &Path,
    target: &str,
    python: &std::ffi::OsStr,
) -> Result<PreparedRuntime, String> {
    fs::create_dir_all(out).map_err(|_| "Cannot create Git build output")?;
    // Never reuse output built from a different bootstrap or source policy.
    // Each checkout's Python bootstrap separately locks its own target cache.
    let mut key = Sha256::new();
    key.update(target.as_bytes());
    for relative in [
        "scripts/build-git-runtime.py",
        "scripts/prepare-git-runtime.py",
        "scripts/git-runtime/policy.patch",
        "scripts/git-runtime/git-2.56.0.tar.xz",
    ] {
        let bytes = read_regular(&root.join(relative), 32 * 1024 * 1024)?;
        key.update((bytes.len() as u64).to_le_bytes());
        key.update(bytes);
    }
    let command = || {
        let mut command = Command::new(python);
        command
            .arg(root.join("scripts/build-git-runtime.py"))
            .arg("--target")
            .arg(target)
            .current_dir(root)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit());
        command
    };
    // The outer cache must select the current toolchain even when its old
    // bundle is valid. Python's nonbuilding preflight also supplies the files
    // whose changes must rerun this Cargo build script.
    let preflight = command()
        .arg("--print-cache-identity")
        .output()
        .map_err(|error| format!("Cannot start pinned Git bootstrap: {error}"))?;
    if !preflight.status.success() || preflight.stdout.len() > 65536 {
        return Err("Pinned Git toolchain identity query failed; provision the documented native toolchain (Debian/Ubuntu: apt install build-essential libcurl4-openssl-dev zlib1g-dev python3; Fedora: dnf install gcc make libcurl-devel zlib-devel python3; macOS: xcode-select --install) or supply MINI_AGENT_GIT_RUNTIME_BUNDLE".into());
    }
    let text = std::str::from_utf8(&preflight.stdout)
        .map_err(|_| "Git toolchain identity is not UTF-8")?;
    let mut lines = text.lines();
    let identity = lines.next().ok_or("Git toolchain identity is absent")?;
    if !digest_text(identity) {
        return Err("Git toolchain identity is malformed".into());
    }
    key.update(identity.as_bytes());
    let toolchain_inputs = lines.map(PathBuf::from).collect::<Vec<_>>();
    if toolchain_inputs.len() > 128 || toolchain_inputs.iter().any(|path| !path.is_absolute()) {
        return Err("Git toolchain input inventory is malformed".into());
    }
    let key = key
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let bundle = out.join(format!("git-runtime-bootstrap-{key}"));
    if !bundle
        .try_exists()
        .map_err(|_| "Cannot inspect Git bootstrap output")?
    {
        let status = command()
            .arg("--output")
            .arg(&bundle)
            .stdout(Stdio::null())
            .status()
            .map_err(|error| format!("Cannot start pinned Git bootstrap: {error}. Install Python 3 or set MINI_AGENT_GIT_PYTHON; cross/Windows builds may supply MINI_AGENT_GIT_RUNTIME_BUNDLE."))?;
        if !status.success() {
            return Err("Pinned Git bootstrap failed; provision its documented native dependencies (Debian/Ubuntu: apt install build-essential libcurl4-openssl-dev zlib1g-dev python3; Fedora: dnf install gcc make libcurl-devel zlib-devel python3; macOS: xcode-select --install) or supply MINI_AGENT_GIT_RUNTIME_BUNDLE for the exact target".into());
        }
    }
    // An existing output is still checked in full. Do not repair or replace it.
    let mut runtime = prepare(root, out, target, Some(&bundle))?;
    runtime.inputs.extend(toolchain_inputs);
    Ok(runtime)
}

pub(crate) fn prepare(
    root: &Path,
    out: &Path,
    target: &str,
    bundle: Option<&Path>,
) -> Result<PreparedRuntime, String> {
    fs::create_dir_all(out).map_err(|_| "Cannot create Git build output")?;
    let generated = out.join("git_runtime_approval.rs");
    let Some(bundle) = bundle else {
        fs::write(
            generated,
            "pub(crate) fn bundled_approval() -> Option<&'static RuntimeApproval> { None }\n",
        )
        .map_err(|_| "Cannot write absent Git approval")?;
        return Ok(PreparedRuntime {
            identity: "no-bundled-git-runtime".into(),
            inputs: Vec::new(),
        });
    };
    let kind = fs::symlink_metadata(bundle).map_err(|_| "Git bundle directory is missing")?;
    if !kind.is_dir() {
        return Err("Git bundle must be a directory, not a symbolic link".into());
    }
    let bundle = bundle
        .canonicalize()
        .map_err(|_| "Cannot resolve Git bundle")?;
    let manifest_path = bundle.join("bundle.manifest");
    let manifest = read_regular(&manifest_path, 64 * 1024)?;
    let text = std::str::from_utf8(&manifest).map_err(|_| "Git bundle manifest is not UTF-8")?;
    let mut lines = text.lines();
    let policy_path = root.join("scripts/git-runtime/policy.patch");
    let policy_hash = sha(&read_regular(&policy_path, 1024 * 1024)?);
    for expected in [
        "mini-agent-git-runtime-v1".to_owned(),
        format!("target={target}"),
        format!("source-sha256={SOURCE}"),
        format!("policy-sha256={policy_hash}"),
        format!("policy={POLICY}"),
    ] {
        if lines.next() != Some(expected.as_str()) {
            return Err("Git bundle target, source or policy does not match this build".into());
        }
    }
    let mut entries = Vec::new();
    let mut names = BTreeSet::new();
    let mut total = 0_u64;
    let mut inputs = vec![manifest_path, policy_path];
    for line in lines {
        if entries.len() == 128 {
            return Err("Git bundle has too many files".into());
        }
        let fields: Vec<_> = line
            .strip_prefix("file=")
            .unwrap_or("")
            .split('\t')
            .collect();
        if fields.len() != 3
            || !safe_leaf(fields[0])
            || !digest_text(fields[1])
            || !matches!(fields[2], "x" | "r")
            || !names.insert(fields[0].to_ascii_lowercase())
        {
            return Err("Git bundle has an invalid or duplicate file entry".into());
        }
        let input = bundle.join(fields[0]);
        let bytes = read_regular(&input, MAX_FILE)?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL || sha(&bytes) != fields[1] {
            return Err("Git bundle payload size or digest does not match".into());
        }
        // Embed captured, validated bytes, never a later read of the input path.
        let captured = out.join(format!("git-payload-{}.bin", fields[1]));
        fs::write(&captured, bytes).map_err(|_| "Cannot capture Git bundle payload")?;
        entries.push((
            fields[0].to_owned(),
            fields[1].to_owned(),
            fields[2] == "x",
            captured,
        ));
        inputs.push(input);
    }
    let suffix = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    for required in ["git", "git-remote-http", "git-remote-https"] {
        let name = format!("{required}{suffix}");
        if !entries.iter().any(|entry| entry.0 == name && entry.2) {
            return Err("Git bundle is missing a required executable".into());
        }
    }
    let identity = sha(&manifest);
    let mut source = String::from("static BUNDLED_GIT_FILES: &[RuntimeFile] = &[\n");
    for (name, digest, executable, captured) in entries {
        source.push_str(&format!(
            "RuntimeFile {{ relative_path: {name:?}, sha256: {digest:?}, contents: include_bytes!({:?}), executable: {executable} }},\n",
            captured.to_str().ok_or("Git build output path must be UTF-8")?
        ));
    }
    source.push_str(&format!(
        "];\nstatic BUNDLED_GIT_APPROVAL: RuntimeApproval = RuntimeApproval {{ id: {identity:?}, target: {target:?}, policy: {POLICY:?}, files: BUNDLED_GIT_FILES }};\npub(crate) fn bundled_approval() -> Option<&'static RuntimeApproval> {{ Some(&BUNDLED_GIT_APPROVAL) }}\n"
    ));
    fs::write(generated, source).map_err(|_| "Cannot write Git runtime approval")?;
    Ok(PreparedRuntime { identity, inputs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const TARGET: &str = "aarch64-unknown-linux-gnu";
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mini-agent-git-build-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let owner = Self(root);
            fs::create_dir_all(owner.0.join("scripts/git-runtime")).unwrap();
            fs::create_dir(owner.bundle()).unwrap();
            fs::write(
                owner.0.join("scripts/git-runtime/policy.patch"),
                b"owned policy bytes",
            )
            .unwrap();
            for file in ["git", "git-remote-http", "git-remote-https"] {
                fs::write(owner.bundle().join(file), file.as_bytes()).unwrap();
            }
            owner.write_manifest();
            owner
        }
        fn bundle(&self) -> PathBuf {
            self.0.join("bundle")
        }
        fn out(&self) -> PathBuf {
            self.0.join("out")
        }
        fn manifest(&self) -> PathBuf {
            self.bundle().join("bundle.manifest")
        }
        fn write_manifest(&self) {
            let policy = sha(b"owned policy bytes");
            let mut text = format!(
                "mini-agent-git-runtime-v1\ntarget={TARGET}\nsource-sha256={SOURCE}\npolicy-sha256={policy}\npolicy={POLICY}\n"
            );
            for file in ["git", "git-remote-http", "git-remote-https"] {
                text.push_str(&format!("file={file}\t{}\tx\n", sha(file.as_bytes())));
            }
            fs::write(self.manifest(), text).unwrap();
        }
        fn prepare(&self) -> Result<PreparedRuntime, String> {
            prepare(&self.0, &self.out(), TARGET, Some(&self.bundle()))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn captured_bundle_bytes_and_identity_survive_later_input_changes() {
        let fixture = Fixture::new();
        let built = fixture.prepare().unwrap();
        assert_eq!(built.identity, sha(&fs::read(fixture.manifest()).unwrap()));
        let captured = fixture
            .out()
            .join(format!("git-payload-{}.bin", sha(b"git")));
        fs::write(
            fixture.bundle().join("git"),
            b"substituted input after capture",
        )
        .unwrap();
        assert_eq!(fs::read(captured).unwrap(), b"git");
        let generated = fs::read_to_string(fixture.out().join("git_runtime_approval.rs")).unwrap();
        assert!(generated.contains(&built.identity));
        assert!(generated.contains("contents: include_bytes!"));
        assert!(!generated.contains(fixture.bundle().to_str().unwrap()));
        assert!(fixture.prepare().unwrap_err().contains("digest"));
    }

    #[test]
    fn mismatched_target_source_and_policy_never_produce_approval() {
        for replacement in [
            (
                format!("target={TARGET}"),
                "target=x86_64-pc-windows-msvc".into(),
            ),
            (
                format!("source-sha256={SOURCE}"),
                format!("source-sha256={}", "0".repeat(64)),
            ),
            (format!("policy={POLICY}"), "policy=unreviewed".into()),
            (
                format!("policy-sha256={}", sha(b"owned policy bytes")),
                format!("policy-sha256={}", "0".repeat(64)),
            ),
        ] {
            let fixture = Fixture::new();
            let text = fs::read_to_string(fixture.manifest())
                .unwrap()
                .replace(&replacement.0, &replacement.1);
            fs::write(fixture.manifest(), text).unwrap();
            assert!(fixture.prepare().is_err());
            assert!(!fixture.out().join("git_runtime_approval.rs").exists());
        }
    }

    #[test]
    fn unsafe_or_case_colliding_paths_are_rejected() {
        for name in [
            "../escape",
            "sub/git",
            "C:git",
            "NUL.exe",
            "COM1.dll",
            "git.",
            "git\"injection",
            "GIT",
        ] {
            let fixture = Fixture::new();
            let mut text = fs::read_to_string(fixture.manifest()).unwrap();
            text.push_str(&format!("file={name}\t{}\tx\n", sha(b"git")));
            fs::write(fixture.manifest(), text).unwrap();
            assert!(fixture.prepare().is_err(), "accepted {name}");
            assert!(!fixture.out().join("git_runtime_approval.rs").exists());
        }
    }

    #[test]
    fn missing_helpers_and_oversized_payloads_fail_before_approval() {
        let fixture = Fixture::new();
        let text = fs::read_to_string(fixture.manifest()).unwrap();
        fs::write(
            fixture.manifest(),
            text.lines()
                .filter(|line| !line.starts_with("file=git-remote-https\t"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        assert!(fixture.prepare().unwrap_err().contains("missing"));
        fixture.write_manifest();
        fs::File::options()
            .write(true)
            .open(fixture.bundle().join("git"))
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        assert!(fixture.prepare().unwrap_err().contains("bounded"));
        assert!(!fixture.out().join("git_runtime_approval.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_and_multiple_link_payloads_are_not_embedded() {
        use std::os::unix::fs::symlink;
        for symbolic in [false, true] {
            let fixture = Fixture::new();
            let file = fixture.bundle().join("git");
            if symbolic {
                fs::remove_file(&file).unwrap();
                symlink("git-remote-http", &file).unwrap();
            } else {
                fs::hard_link(&file, fixture.bundle().join("alias")).unwrap();
            }
            assert!(fixture.prepare().is_err());
            assert!(!fixture.out().join("git_runtime_approval.rs").exists());
        }
    }

    #[test]
    fn absent_bundle_is_explicitly_unavailable() {
        let fixture = Fixture::new();
        let built = prepare(&fixture.0, &fixture.out(), TARGET, None).unwrap();
        assert_eq!(built.identity, "no-bundled-git-runtime");
        assert!(built.inputs.is_empty());
        assert!(
            fs::read_to_string(fixture.out().join("git_runtime_approval.rs"))
                .unwrap()
                .contains("{ None }")
        );
    }

    #[test]
    fn default_bootstrap_reuses_exact_inputs_and_refuses_changed_output() {
        let fixture = Fixture::new();
        let script = fixture.0.join("scripts/build-git-runtime.py");
        fs::write(
            &script,
            r#"import hashlib, pathlib, shutil, sys
root = pathlib.Path(__file__).resolve().parents[1]
if '--print-cache-identity' in sys.argv:
    state = root / 'selected-toolchain'
    print(hashlib.sha256(state.read_bytes()).hexdigest())
    print(state)
    raise SystemExit(0)
output = pathlib.Path(sys.argv[sys.argv.index('--output') + 1])
shutil.copytree(root / 'bundle', output)
with (root / 'invocations').open('a') as log:
    log.write('built\n')
"#,
        )
        .unwrap();
        fs::write(
            fixture.0.join("scripts/prepare-git-runtime.py"),
            b"fixture producer",
        )
        .unwrap();
        fs::write(
            fixture.0.join("scripts/git-runtime/git-2.56.0.tar.xz"),
            b"fixture source",
        )
        .unwrap();
        fs::write(
            fixture.0.join("selected-toolchain"),
            b"compiler and backend A",
        )
        .unwrap();
        let python = std::ffi::OsStr::new("python3");
        let first = prepare_default(&fixture.0, &fixture.out(), TARGET, python).unwrap();
        let second = prepare_default(&fixture.0, &fixture.out(), TARGET, python).unwrap();
        assert_eq!(first.identity, second.identity);
        assert_eq!(
            fs::read_to_string(fixture.0.join("invocations")).unwrap(),
            "built\n"
        );
        // Change only the selected compiler/backend; unchanged outer inputs
        // must reach Python preflight and choose a new approved bundle.
        fs::write(
            fixture.0.join("selected-toolchain"),
            b"compiler and backend B",
        )
        .unwrap();
        let changed = prepare_default(&fixture.0, &fixture.out(), TARGET, python).unwrap();
        assert_ne!(first.inputs[0], changed.inputs[0]);
        let selected = fixture.0.join("selected-toolchain").canonicalize().unwrap();
        assert!(changed.inputs.contains(&selected));
        assert_eq!(
            fs::read_to_string(fixture.0.join("invocations")).unwrap(),
            "built\nbuilt\n"
        );
        let mut source = fs::read(&script).unwrap();
        source.extend_from_slice(b"\n# changed bootstrap input\n");
        fs::write(&script, source).unwrap();
        let third = prepare_default(&fixture.0, &fixture.out(), TARGET, python).unwrap();
        assert_ne!(first.inputs[0], third.inputs[0]);
        assert_eq!(
            fs::read_to_string(fixture.0.join("invocations")).unwrap(),
            "built\nbuilt\nbuilt\n"
        );
        fs::write(
            third.inputs[0].parent().unwrap().join("git"),
            b"changed after bootstrap",
        )
        .unwrap();
        assert!(
            prepare_default(&fixture.0, &fixture.out(), TARGET, python)
                .unwrap_err()
                .contains("digest")
        );
        assert_eq!(
            fs::read_to_string(fixture.0.join("invocations")).unwrap(),
            "built\nbuilt\nbuilt\n"
        );
    }

    #[test]
    fn missing_bootstrap_interpreter_never_produces_approval() {
        let fixture = Fixture::new();
        for path in [
            "scripts/build-git-runtime.py",
            "scripts/prepare-git-runtime.py",
            "scripts/git-runtime/git-2.56.0.tar.xz",
        ] {
            fs::write(fixture.0.join(path), b"fixture").unwrap();
        }
        let result = prepare_default(
            &fixture.0,
            &fixture.out(),
            TARGET,
            std::ffi::OsStr::new("nonexistent-mini-agent-bootstrap-interpreter"),
        );
        assert!(
            result
                .unwrap_err()
                .contains("Cannot start pinned Git bootstrap")
        );
        assert!(!fixture.out().join("git_runtime_approval.rs").exists());
    }
}
