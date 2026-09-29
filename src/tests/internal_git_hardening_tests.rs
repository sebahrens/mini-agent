//! End-to-end checks that host-side Git never runs what the (sandboxed) model
//! can author inside the repository: `.git/config`, `.git/hooks`,
//! `.git/info/attributes`, and the process environment it would inherit
//! (mini-agent-93gx4).

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use crate::git::runner::{
    GitRunner, LOCAL_MUTATION_LIMITS, QUERY_LIMITS, allow_repository_execution_for_test,
};

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .status()
        .expect("run fixture git");
    assert!(status.success(), "fixture git {args:?} failed");
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mini-agent-93gx4-{label}-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repository");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        git(
            &repo,
            &["config", "user.email", "mini-agent@example.invalid"],
        );
        git(&repo, &["config", "user.name", "Mini Agent Test"]);
        std::fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        Self { root, repo }
    }

    /// A script that records that it ran (and its environment) in
    /// `<root>/<name>.marker`, then behaves like `cat` so a filter that does
    /// run still produces valid output.
    fn marker_script(&self, name: &str) -> (PathBuf, PathBuf) {
        let marker = self.root.join(format!("{name}.marker"));
        let script = self.root.join(format!("{name}.sh"));
        std::fs::write(
            &script,
            format!("#!/bin/sh\nenv > '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, marker)
    }

    fn hook(&self, name: &str) -> PathBuf {
        let (script, marker) = self.marker_script(name);
        let hook = self.repo.join(".git/hooks").join(name);
        std::fs::copy(&script, &hook).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        marker
    }

    /// Configure the model-writable attacks every hardened command must
    /// ignore: fsmonitor and a required clean/smudge filter on every file,
    /// then dirty the tracked file.
    fn arm_fsmonitor_and_filters(&self) -> Vec<PathBuf> {
        let (fsmonitor, fsmonitor_marker) = self.marker_script("fsmonitor");
        let (clean, clean_marker) = self.marker_script("clean-filter");
        let (smudge, smudge_marker) = self.marker_script("smudge-filter");
        let repo = &self.repo;
        git(
            repo,
            &["config", "core.fsmonitor", fsmonitor.to_str().unwrap()],
        );
        git(
            repo,
            &["config", "filter.probe.clean", clean.to_str().unwrap()],
        );
        git(
            repo,
            &["config", "filter.probe.smudge", smudge.to_str().unwrap()],
        );
        git(repo, &["config", "filter.probe.required", "true"]);
        std::fs::write(repo.join(".git/info/attributes"), "* filter=probe\n").unwrap();
        std::fs::write(repo.join("tracked.txt"), "changed by the model\n").unwrap();
        vec![fsmonitor_marker, clean_marker, smudge_marker]
    }
}

fn assert_not_run(markers: &[PathBuf]) {
    for marker in markers {
        assert!(
            !marker.exists(),
            "repository-configured command ran on the host: {}",
            marker.display()
        );
    }
}

#[tokio::test]
async fn internal_git_status_ignores_workspace_fsmonitor() {
    let fixture = Fixture::new("fsmonitor");
    let (fsmonitor, marker) = fixture.marker_script("fsmonitor");
    git(
        &fixture.repo,
        &["config", "core.fsmonitor", fsmonitor.to_str().unwrap()],
    );
    std::fs::write(fixture.repo.join("tracked.txt"), "dirty\n").unwrap();

    let status = crate::session::Session::detect_git_status(&fixture.repo)
        .await
        .expect("hardened status still reports");
    assert_eq!(status.modified, 1, "{status:?}");
    assert_not_run(std::slice::from_ref(&marker));

    // The fixture is live: an unhardened status runs the script.
    let _ = StdCommand::new("git")
        .arg("-C")
        .arg(&fixture.repo)
        .args(["status", "--porcelain=v2"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(marker.exists(), "fixture fsmonitor was not configured");
}

#[tokio::test]
async fn internal_git_status_does_not_run_clean_filters() {
    let fixture = Fixture::new("clean-filter");
    let markers = fixture.arm_fsmonitor_and_filters();

    let status = crate::session::Session::detect_git_status(&fixture.repo)
        .await
        .expect("hardened status still reports");
    assert_eq!(status.modified, 1, "{status:?}");
    let baseline = crate::print::capture_workspace_change_baseline(&fixture.repo).await;
    assert!(baseline.is_some(), "headless change baseline still works");
    assert_not_run(&markers);

    // The fixture is live: an unhardened add runs the clean filter.
    git(&fixture.repo, &["add", "tracked.txt"]);
    assert!(
        markers[1].exists(),
        "fixture clean filter was not configured"
    );
}

#[tokio::test]
async fn undo_stash_uses_hardened_runner() {
    let fixture = Fixture::new("undo-stash");
    let markers = fixture.arm_fsmonitor_and_filters();

    crate::ui::git_stash_in_workspace(&fixture.repo)
        .await
        .expect("hardened stash succeeds");
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("tracked.txt")).unwrap(),
        "initial\n",
        "the change was stashed"
    );
    assert_not_run(&markers);
}

/// Plant a model-written repository at `<outer>/<relative>` with a committed
/// file and a clean filter (configured in the nested repository's own
/// `.git/config`) selected for every file, and record it in the outer index
/// as a gitlink. Returns the filter marker.
fn plant_nested_repository(fixture: &Fixture, outer: &Path, relative: &str, name: &str) -> PathBuf {
    let nested = outer.join(relative);
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet", "-b", "main"]);
    git(
        &nested,
        &["config", "user.email", "mini-agent@example.invalid"],
    );
    git(&nested, &["config", "user.name", "Mini Agent Test"]);
    std::fs::write(nested.join("payload.txt"), "nested\n").unwrap();
    std::fs::write(nested.join(".gitattributes"), "* filter=nested\n").unwrap();
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "--quiet", "-m", "nested"]);
    let (clean, marker) = fixture.marker_script(name);
    git(
        &nested,
        &["config", "filter.nested.clean", clean.to_str().unwrap()],
    );
    git(&nested, &["config", "filter.nested.required", "true"]);
    // Adding an untracked nested repository records a gitlink without
    // descending into it.
    git(outer, &["add", relative]);
    git(outer, &["commit", "--quiet", "-m", "embed"]);
    marker
}

/// Give the nested file a new mtime with unchanged content, so any Git that
/// descends into the nested repository must re-hash it through the filter.
fn touch_nested(fixture: &Fixture, relative: &str) {
    let file = std::fs::File::options()
        .write(true)
        .open(fixture.repo.join(relative).join("payload.txt"))
        .unwrap();
    // A distinct, past timestamp per call (2020-01-01 plus a minute each).
    static TOUCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let minutes = TOUCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800 + minutes * 60);
    file.set_modified(at).unwrap();
}

/// A nested repository the model created inside the workspace cannot supply
/// executable config that Git consults when a worktree mutation descends
/// into it (mini-agent-jj4qw).
#[cfg(feature = "git-worktree")]
#[tokio::test]
async fn worktree_auto_commit_neutralises_nested_repository_filters() {
    let fixture = Fixture::new("nested-auto-commit");
    let marker = plant_nested_repository(&fixture, &fixture.repo, "vendor/inner", "nested-clean");
    // A second level: the nested repository embeds its own repository.
    let inner = fixture.repo.join("vendor/inner");
    let deep_marker = plant_nested_repository(&fixture, &inner, "deeper", "deep-clean");
    std::fs::write(fixture.repo.join("tracked.txt"), "changed by the model\n").unwrap();
    touch_nested(&fixture, "vendor/inner");
    touch_nested(&fixture, "vendor/inner/deeper");
    // The unhardened fixture commits may have run the filters already.
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&deep_marker);

    crate::extras::git_worktree::worktree_auto_commit_all(&fixture.repo)
        .await
        .expect("auto-commit succeeds with the nested repository");
    assert_not_run(&[marker.clone(), deep_marker.clone()]);

    // The status query every merge flow starts with descends as well.
    touch_nested(&fixture, "vendor/inner");
    touch_nested(&fixture, "vendor/inner/deeper");
    let dirty = crate::extras::git_worktree::worktree_has_uncommitted(&fixture.repo)
        .await
        .expect("status with the nested repository");
    assert!(!dirty, "only timestamps changed");
    assert_not_run(&[marker.clone(), deep_marker.clone()]);

    // The fixture is live: an unhardened status descends and runs both.
    touch_nested(&fixture, "vendor/inner");
    touch_nested(&fixture, "vendor/inner/deeper");
    let _ = StdCommand::new("git")
        .arg("-C")
        .arg(&fixture.repo)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(marker.exists(), "fixture nested filter was not configured");
    assert!(
        deep_marker.exists(),
        "fixture deep filter was not configured"
    );
}

/// The undo stash is a workspace mutation too and probes nested repositories
/// like the worktree flows; the stash itself must still work with one.
#[tokio::test]
async fn undo_stash_neutralises_nested_repository_filters() {
    let fixture = Fixture::new("nested-undo-stash");
    let marker = plant_nested_repository(&fixture, &fixture.repo, "inner", "nested-clean");
    std::fs::write(fixture.repo.join("tracked.txt"), "changed by the model\n").unwrap();
    touch_nested(&fixture, "inner");
    let _ = std::fs::remove_file(&marker);

    crate::ui::git_stash_in_workspace(&fixture.repo)
        .await
        .expect("hardened stash succeeds");
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("tracked.txt")).unwrap(),
        "initial\n",
        "the change was stashed"
    );
    assert_not_run(std::slice::from_ref(&marker));
}

#[tokio::test]
async fn internal_git_env_excludes_provider_credentials() {
    let fixture = Fixture::new("environment");
    let marker = fixture.hook("pre-commit");
    // Hooks are neutralised in production, so let this one fixture run its
    // hook to observe exactly what Git would hand a child process.
    let _allowed = allow_repository_execution_for_test(&fixture.repo);
    // (Git redirection variables such as GIT_CONFIG_PARAMETERS are covered
    // by the allow-list unit test; setting them process-wide here would
    // disturb concurrently running fixture Git commands.)
    let _environment = crate::tests::ScopedProcessEnv::set(&[(
        "ANTHROPIC_API_KEY",
        Some(OsString::from("sk-ant-mini-agent-93gx4-sentinel")),
    )]);
    GitRunner::discover()
        .unwrap()
        .run(
            &fixture.repo,
            "commit",
            ["commit", "--quiet", "--allow-empty", "-m", "probe"],
            LOCAL_MUTATION_LIMITS,
        )
        .await
        .expect("commit with the probe hook");
    let seen = std::fs::read_to_string(&marker).expect("probe hook ran");
    assert!(seen.contains("PATH="), "{seen}");
    assert!(!seen.contains("ANTHROPIC_API_KEY"), "{seen}");
    assert!(!seen.contains("sk-ant-mini-agent-93gx4-sentinel"), "{seen}");
}

#[tokio::test]
async fn network_fetch_refuses_repository_upload_pack() {
    let fixture = Fixture::new("fetch");
    let remote = fixture.root.join("remote.git");
    git(
        &fixture.repo,
        &["clone", "--quiet", "--bare", ".", remote.to_str().unwrap()],
    );
    git(
        &fixture.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let (upload_pack, marker) = fixture.marker_script("upload-pack");
    git(
        &fixture.repo,
        &[
            "config",
            "remote.origin.uploadpack",
            upload_pack.to_str().unwrap(),
        ],
    );

    let runner = GitRunner::discover().unwrap();
    // Git keeps the first upload-pack it reads, so the repository's value
    // cannot be overridden: the fetch is refused instead.
    let error = runner
        .run_network(&fixture.repo, "fetch", ["fetch", "origin"], QUERY_LIMITS)
        .await
        .err()
        .expect("repository upload-pack refuses the fetch");
    assert!(error.contains("remote.origin.uploadpack"), "{error}");
    assert_not_run(std::slice::from_ref(&marker));

    // A legitimate local-path remote still fetches under the network profile.
    git(
        &fixture.repo,
        &["config", "--unset", "remote.origin.uploadpack"],
    );
    runner
        .run_network(&fixture.repo, "fetch", ["fetch", "origin"], QUERY_LIMITS)
        .await
        .expect("fetch from the real upload-pack");
    assert_not_run(std::slice::from_ref(&marker));
}
