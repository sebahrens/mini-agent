//! Native general-bwrap controls; explicitly run on a suitable Linux runner.
//! No successful skip can stand in for containment evidence.

use super::*;
use std::os::unix::fs::symlink;

struct NativeDirectory(PathBuf);

impl NativeDirectory {
    fn new(parent: &Path, label: &str) -> Self {
        let path = parent.join(format!("mini-agent-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("native fixture parent must permit owned directories");
        Self(path)
    }
}

impl Drop for NativeDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn required_backend() {
    assert!(
        bwrap_exists(),
        "native control requires a working trusted bubblewrap backend"
    );
    assert!(
        bwrap_supports_bind_fd(bwrap_path().unwrap()),
        "both descriptor-bind options are required"
    );
}

pub(crate) fn bound(root: &Path, cache: &Path) -> Sandbox {
    let sandbox = Sandbox::new(true, "bwrap").with_workspace_binding(Arc::new(
        crate::paths::WorkspaceBinding::capture(root).unwrap(),
    ));
    *sandbox.cached_cache_dir.lock().unwrap() = Some(Arc::new(cache.to_path_buf()));
    sandbox
}

pub(crate) fn plain_repository(parent: &Path, name: &str) -> PathBuf {
    git_metadata::fixtures::plain_repository(parent, name)
}

pub(crate) fn protected_script(real: &Path, spelled: &Path) -> String {
    format!(
        r#"test "$(pwd -P)" = '{real}' || exit 21
test "$(cat '{real}/README.md')" = original || exit 22
test "$(cat '{spelled}/README.md')" = original || exit 23
printf shared > '{spelled}/created.txt' || exit 24
test "$(cat '{real}/created.txt')" = shared || exit 25
test ! -e /proc/self/fd/197 || exit 26
test "$(readlink /proc/self/fd/0)" = /dev/null || exit 27
if printf changed > '{spelled}/.git/config' 2>/dev/null; then exit 28; fi
if touch '{real}/.git/hooks/new-hook' 2>/dev/null; then exit 29; fi
if touch '{real}/.git/info/new-info' 2>/dev/null; then exit 30; fi
printf '%s\n' '/workspace is literal output, not a replacement token'
printf NATIVE_REAL_PATH_PASS
"#,
        real = real.display(),
        spelled = spelled.display(),
    )
}

async fn completed(sandbox: &Sandbox, script: &str) -> CommandOutput {
    let output = sandbox
        .output_command_with_limits(script, DEFAULT_COMMAND_LIMITS)
        .await
        .unwrap();
    assert_eq!(
        output.status,
        CommandStatus::Completed,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.exit_status.is_some_and(|status| status.success()),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn real_and_spelled(parent: &Path) {
    required_backend();
    let owned = NativeDirectory::new(parent, "real-path-native");
    let cache = owned.0.join("runtime-cache");
    crate::fs::ensure_private_directory(&cache).unwrap();
    let real_parent = owned.0.join("real");
    std::fs::create_dir(&real_parent).unwrap();
    let real = plain_repository(&real_parent, "repo");
    std::fs::write(real.join("README.md"), "original").unwrap();
    let spelling = owned.0.join("spelled");
    symlink("real", &spelling).unwrap();
    let spelled = spelling.join("repo");
    let sandbox = bound(&spelled, &cache);
    assert_eq!(sandbox.shell_cwd_alias(), None);
    let output = completed(&sandbox, &protected_script(&real, &spelled)).await;
    assert_eq!(
        output.stdout,
        b"/workspace is literal output, not a replacement token\nNATIVE_REAL_PATH_PASS"
    );
    assert_eq!(std::fs::read(real.join("created.txt")).unwrap(), b"shared");
    assert!(!real.join(".git/hooks/new-hook").exists());
    assert!(!real.join(".git/info/new-info").exists());
}

#[tokio::test]
#[ignore = "requires native Linux namespaces and descriptor-capable bubblewrap"]
async fn linux_native_workspace_under_tmp_keeps_real_and_spelled_paths_and_git_masks() {
    real_and_spelled(Path::new("/tmp")).await;
}

#[tokio::test]
#[ignore = "requires native Linux and an owned /usr/mini-agent-native-tests fixture parent"]
async fn linux_native_workspace_under_readonly_runtime_keeps_one_writable_view() {
    // The runner prepares this empty parent for its own UID. No system file
    // is changed: each test owns only its uniquely named child directory.
    real_and_spelled(Path::new("/usr/mini-agent-native-tests")).await;
}

#[tokio::test]
#[ignore = "requires native Linux namespaces and descriptor-capable bubblewrap"]
async fn linux_native_workspace_control_destination_uses_visible_nonroot_fallback() {
    required_backend();
    let owned = NativeDirectory::new(Path::new("/dev/shm"), "fallback-native");
    let cache = owned.0.join("runtime-cache");
    crate::fs::ensure_private_directory(&cache).unwrap();
    let root = git_metadata::fixtures::plain_repository(&owned.0, "repo");
    std::fs::write(root.join("README.md"), "original").unwrap();
    let sandbox = bound(&root, &cache);
    assert_eq!(sandbox.shell_cwd_alias(), Some(PathBuf::from("/workspace")));
    let output = completed(
        &sandbox,
        &protected_script(Path::new("/workspace"), Path::new("/workspace")),
    )
    .await;
    assert_eq!(
        output.stdout,
        b"/workspace is literal output, not a replacement token\nNATIVE_REAL_PATH_PASS"
    );
    assert_eq!(std::fs::read(root.join("created.txt")).unwrap(), b"shared");
    let root_binding = bound(Path::new("/"), &cache);
    let refused = root_binding
        .wrap_command("printf must-not-run")
        .unwrap_err();
    assert!(refused.contains("refusing to expose filesystem root"));
}

#[tokio::test]
#[ignore = "requires native Linux namespaces and descriptor-capable bubblewrap"]
async fn linux_native_workspace_replacement_after_construction_retains_original_tree_and_masks() {
    required_backend();
    let owned = NativeDirectory::new(Path::new("/tmp"), "replacement-native");
    let cache = owned.0.join("runtime-cache");
    crate::fs::ensure_private_directory(&cache).unwrap();
    let first = git_metadata::fixtures::plain_repository(&owned.0, "first");
    let replacement = git_metadata::fixtures::plain_repository(&owned.0, "replacement");
    std::fs::write(first.join("README.md"), "original").unwrap();
    std::fs::write(replacement.join("README.md"), "replacement-secret").unwrap();
    let spelled = owned.0.join("spelled");
    symlink("first", &spelled).unwrap();
    let sandbox = bound(&spelled, &cache);
    let script = protected_script(&first, &spelled);
    let command = sandbox.wrap_command(&script).unwrap();
    // This is the exact check→spawn interval. The constructed command already
    // owns directory/Git descriptors and fixed alias bytes.
    std::fs::rename(&first, owned.0.join("original-held")).unwrap();
    std::fs::rename(&replacement, &first).unwrap();
    std::fs::remove_file(&spelled).unwrap();
    symlink("first", &spelled).unwrap();
    let output = sandbox
        .output_built_command_with_limits(command, DEFAULT_COMMAND_LIMITS)
        .await
        .unwrap();
    assert_eq!(output.status, CommandStatus::Completed);
    assert!(
        output.exit_status.is_some_and(|status| status.success()),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"/workspace is literal output, not a replacement token\nNATIVE_REAL_PATH_PASS"
    );
    assert!(!first.join("created.txt").exists());
    assert_eq!(
        std::fs::read(owned.0.join("original-held/created.txt")).unwrap(),
        b"shared"
    );
    assert_eq!(
        std::fs::read(first.join("README.md")).unwrap(),
        b"replacement-secret"
    );
}

#[tokio::test]
#[ignore = "requires native Linux namespaces and descriptor-capable bubblewrap"]
async fn linux_native_workspace_cancellation_reaps_owned_real_path_children() {
    required_backend();
    let owned = NativeDirectory::new(Path::new("/tmp"), "cancel-native");
    let root = owned.0.join("repo");
    let cache = owned.0.join("runtime-cache");
    std::fs::create_dir(&root).unwrap();
    crate::fs::ensure_private_directory(&cache).unwrap();
    let sandbox = bound(&root, &cache);
    let marker = root.join("started");
    let script = format!("printf started > '{}'; sleep 300 & wait", marker.display());
    let cancellation = CommandCancellation::new();
    let task_sandbox = sandbox.clone();
    let task_cancel = cancellation.clone();
    let scope = crate::agent::runner::AgentWorkScope::new();
    let task_scope = scope.clone();
    let (lifecycle_tx, _lifecycle_rx) = tokio::sync::mpsc::channel(1);
    let cleanup = crate::agent::runner::AgentRunCleanupGuard::new(scope.clone(), lifecycle_tx);
    let mut task = tokio::spawn(async move {
        task_scope
            .run(task_sandbox.output_command_with_limits_cancelled(
                &script,
                DEFAULT_COMMAND_LIMITS,
                &task_cancel,
            ))
            .await
    });
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    // Always cancel/join even when setup failed to reach its marker.
    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await;
    let joined = match result {
        Ok(result) => result,
        Err(_) => {
            task.abort();
            sandbox.kill_active();
            let _ = task.await;
            tokio::time::timeout(std::time::Duration::from_secs(5), cleanup.settle())
                .await
                .unwrap();
            panic!("native cancellation did not settle within its deadline");
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), cleanup.settle())
        .await
        .unwrap();
    let output = joined.unwrap().unwrap();
    assert!(
        started.is_ok(),
        "native child never reached the real workspace"
    );
    assert_eq!(output.status, CommandStatus::Cancelled);
    assert_eq!(sandbox.active_group_count(), 0);
}

#[tokio::test]
#[ignore = "requires native Linux and the same Rust toolchain staged under /usr/local/lib/mini-agent-native-rust"]
async fn linux_native_workspace_cargo_artifacts_are_reused_at_host_real_paths() {
    required_backend();
    let compiler = Path::new("/usr/local/lib/mini-agent-native-rust/bin/rustc");
    let cargo = compiler.with_file_name("cargo");
    assert!(
        compiler.is_file() && cargo.is_file(),
        "native runner must stage its existing Rust toolchain inside an already permitted readonly runtime root"
    );
    let owned = NativeDirectory::new(Path::new("/tmp"), "cargo-native");
    let root = owned.0.join("repo");
    let cache = owned.0.join("runtime-cache");
    std::fs::create_dir_all(root.join("src")).unwrap();
    crate::fs::ensure_private_directory(&cache).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='native-workspace-cache'\nversion='0.0.0'\nedition='2024'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "#[test] fn fixture_control() { assert_eq!(2 + 2, 4); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("build.rs"),
        r#"fn main() {
        let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let counter = root.join("build-count");
        let n: u32 = std::fs::read_to_string(&counter).unwrap_or_default().parse().unwrap_or(0);
        std::fs::write(counter, (n + 1).to_string()).unwrap();
        println!("cargo:rerun-if-changed=build.rs");
    }"#,
    )
    .unwrap();
    let script = format!(
        "PATH='/usr/local/lib/mini-agent-native-rust/bin:/usr/bin:/bin' RUSTC='{}' CARGO_HOME='{}/cargo-home' CARGO_TARGET_DIR='{}/target' '{}' test --offline",
        compiler.display(),
        root.display(),
        root.display(),
        cargo.display(),
    );
    completed(&bound(&root, &cache), &script).await;
    assert_eq!(std::fs::read(root.join("build-count")).unwrap(), b"1");
    let artifacts: Vec<_> = std::fs::read_dir(root.join("target/debug/deps"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "d"))
        .collect();
    assert!(!artifacts.is_empty());
    let before: Vec<_> = artifacts
        .iter()
        .map(|path| {
            (
                path.clone(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect();
    assert!(artifacts.iter().any(|path| {
        std::fs::read_to_string(path)
            .unwrap()
            .contains(&root.display().to_string())
    }));
    assert!(artifacts.iter().all(|path| {
        !std::fs::read_to_string(path)
            .unwrap()
            .contains("/workspace/")
    }));
    let host = Sandbox::new(false, "bwrap").with_workspace_binding(Arc::new(
        crate::paths::WorkspaceBinding::capture(&root).unwrap(),
    ));
    completed(&host, &script).await;
    assert_eq!(
        std::fs::read(root.join("build-count")).unwrap(),
        b"1",
        "host test must reuse the sandbox build-script artifact"
    );
    for (path, modified) in before {
        assert_eq!(
            std::fs::metadata(path).unwrap().modified().unwrap(),
            modified
        );
    }
}
