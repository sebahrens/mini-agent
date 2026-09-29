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
