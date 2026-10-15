//! Native Windows service compatibility through the publisher-approved bundle.
//!
//! The loopback challenge requires the real bundled HTTP helper to consume
//! startup-captured global credentials. The caller exposes Git/cmd, not the
//! original installation's usr/bin, matching an ordinary Git-for-Windows PATH.

use super::super::global_snapshot::{GlobalSources, SnapshotDirectory};
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn shell_path(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_str()
            .unwrap()
            .replace('\\', "/")
            .replace('\'', "'\\''")
    )
}

fn config_value(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

/// Do not inherit MSYS2's development PATH: it would conceal a missing shell
/// selection by the private Git runtime. Preserve the real HOME unchanged.
fn cmd_only_environment(runner: &mut GitRunner) {
    let original = resolve_git_executable(std::env::var_os("PATH").as_deref())
        .expect("Windows runner must have an original Git installation");
    let parent = original.parent().unwrap();
    let cmd = if parent.file_name().is_some_and(|name| name == "cmd") {
        parent.to_path_buf()
    } else {
        // Hosted GitHub Windows runners install Git into <root>/cmd. The
        // installation is a test prerequisite, never a fallback Git runtime.
        parent
            .ancestors()
            .map(|ancestor| ancestor.join("cmd"))
            .find(|candidate| candidate.join("git.exe").is_file())
            .expect("original Git/cmd directory is required")
    };
    assert!(!cmd.join("sh.exe").exists());
    let windows = PathBuf::from(std::env::var_os("SYSTEMROOT").expect("SYSTEMROOT"));
    let path = std::env::join_paths([cmd, windows.join("System32"), windows]).unwrap();
    let mut environment = runner.captured_environment.as_ref().clone();
    environment.retain(|(name, _)| {
        name != "PATH"
            && !name
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("proxy")
    });
    environment.push(("PATH".into(), path));
    environment.push(("NO_PROXY".into(), "127.0.0.1,localhost".into()));
    runner.captured_environment = Arc::new(environment);
}

#[tokio::test]
async fn windows_embedded_http_global_helpers_with_cmd_only_path_and_spaces() {
    for mode in ["shell", "store-shorthand"] {
        let owned = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
        let root = owned.path();
        let project = root.join("repository with spaces");
        let operator = root.join("operator with spaces");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&operator).unwrap();
        let approval = super::super::runtime::bundled_approval()
            .expect("Windows acceptance requires the publisher-approved embedded bundle");
        assert_eq!(approval.target, "x86_64-pc-windows-msvc");
        let runtime = super::super::runtime::GuardedRuntime::materialize(
            approval,
            &root.join("private runtime with spaces"),
            &[],
        )
        .unwrap();
        assert!(runtime.directory().join("git-remote-http.exe").is_file());
        let mut runner = GitRunner::from_runtime(Arc::new(runtime)).unwrap();
        // Narrow only this runner's captured environment before startup service
        // capture; no process environment or account is changed.
        cmd_only_environment(&mut runner);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let marker = operator.join("helper invocations");
        let credentials = operator.join("inert credentials");
        let helper = if mode == "shell" {
            format!(
                "!f() {{ printf '%s\\n' \"$1\" >> {}; if test \"$1\" = get; then printf 'username=probe\\npassword=fixture-only\\n'; fi; }}; f",
                shell_path(&marker)
            )
        } else {
            std::fs::write(
                &credentials,
                format!("http://probe:fixture-only@127.0.0.1:{port}\n"),
            )
            .unwrap();
            format!("store --file={}", shell_path(&credentials))
        };
        let global = operator.join("approved global config");
        std::fs::write(
            &global,
            format!("[credential]\nhelper = {}\n", config_value(&helper)),
        )
        .unwrap();
        let globals = GlobalSources::capture_roots(&runner, &project, vec![global], None)
            .await
            .unwrap();
        let runner = runner.with_test_globals(globals);
        runner
            .run(&project, "init", ["init", "--quiet"], QUERY_LIMITS)
            .await
            .unwrap();
        let url = format!("http://127.0.0.1:{port}/fixture.git");
        runner
            .run(
                &project,
                "config",
                ["config", "remote.origin.url", &url],
                QUERY_LIMITS,
            )
            .await
            .unwrap();
        let (done, mut retired) = tokio::sync::oneshot::channel::<()>();
        let server = async {
            let mut challenges = 0;
            let mut authorized = 0;
            loop {
                let accepted = tokio::select! {
                    _ = &mut retired => break,
                    accepted = listener.accept() => accepted,
                };
                let (mut stream, _) = accepted.unwrap();
                let mut request = Vec::new();
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !request.ends_with(b"\r\n\r\n") {
                        assert!(request.len() < 8192, "bounded fixture request headers");
                        let mut byte = [0];
                        if stream.read(&mut byte).await.unwrap() == 0 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                })
                .await
                .unwrap();
                let request = String::from_utf8(request).unwrap();
                let auth = request
                    .to_ascii_lowercase()
                    .contains("authorization: basic chjvymu6zml4dhvyzs1vbmx5");
                let response = if !auth {
                    challenges += 1;
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"inert-fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    authorized += 1;
                    let body = if request.lines().next().unwrap().contains("/HEAD ") {
                        "ref: refs/heads/main\n"
                    } else {
                        ""
                    };
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            (challenges, authorized)
        };
        let client = async {
            let result = runner
                .run_network(&project, "ls-remote", ["ls-remote", "origin"], QUERY_LIMITS)
                .await;
            let _ = done.send(());
            result
        };
        // Both futures are owned here. Deadline/error drops the listener and
        // sockets; no detached HTTP task or helper process survives the test.
        let (result, (challenges, authorized)) = tokio::join!(client, server);
        result.unwrap_or_else(|error| panic!("{mode} actual embedded HTTP/helper: {error}"));
        assert!(
            challenges > 0 && authorized > 0,
            "{mode} must answer a real HTTP credential challenge"
        );
        if mode == "shell" {
            assert!(
                std::fs::read_to_string(&marker)
                    .unwrap()
                    .lines()
                    .any(|line| line == "get")
            );
        }
    }
}

#[test]
fn windows_shell_service_rejects_overlap_later_grants_and_replacement() {
    use super::super::runtime::{TrustedShell, check_model_write, record_model_root};
    let owned = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
    let directory = owned.path().join("original usr bin");
    std::fs::create_dir(&directory).unwrap();
    let program = directory.join("sh.exe");
    std::fs::write(&program, b"inert shell identity fixture").unwrap();
    assert!(TrustedShell::capture(&directory, std::slice::from_ref(&directory)).is_err());
    let shell = TrustedShell::capture(&directory, &[]).unwrap();
    assert!(check_model_write(&program).is_err());
    assert!(check_model_write(&directory.join("adjacent-runtime.dll")).is_err());
    let binding = Arc::new(crate::paths::WorkspaceBinding::capture(&directory).unwrap());
    assert!(
        record_model_root(&binding).is_err(),
        "later sandbox grant must refuse the entire service directory"
    );
    drop(binding);
    shell.validate(&[]).unwrap();
    // Hold the old file at another name so replacement cannot reuse identity.
    std::fs::rename(&program, directory.join("original-shell-retired")).unwrap();
    std::fs::write(&program, b"replacement is never launched").unwrap();
    assert!(shell.validate(&[]).is_err());
    drop(shell);
}

#[tokio::test]
async fn windows_missing_shell_refuses_only_selected_shell_services() {
    let owned = SnapshotDirectory::new(&std::env::temp_dir()).unwrap();
    let root = owned.path();
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    let runtime = super::super::runtime::GuardedRuntime::materialize(
        super::super::runtime::bundled_approval().expect("test requires approved bundle"),
        &root.join("runtime"),
        &[],
    )
    .unwrap();
    let mut runner = GitRunner::from_runtime(Arc::new(runtime)).unwrap();
    let windows = PathBuf::from(std::env::var_os("SYSTEMROOT").unwrap());
    let path = std::env::join_paths([windows.join("System32"), windows]).unwrap();
    let mut environment = runner.captured_environment.as_ref().clone();
    environment.retain(|(name, _)| name != "PATH");
    environment.push(("PATH".into(), path));
    runner.captured_environment = Arc::new(environment);
    let global = root.join("global");
    std::fs::write(&global, "[credential]\nhelper = !printf should-not-run\n").unwrap();
    let globals = GlobalSources::capture_roots(&runner, &project, vec![global.clone()], None)
        .await
        .unwrap();
    let runner = runner.with_test_globals(globals);
    runner
        .run(&project, "init", ["init", "--quiet"], QUERY_LIMITS)
        .await
        .unwrap();
    let result = runner
        .run_network(
            &project,
            "config",
            ["config", "--get", "credential.helper"],
            QUERY_LIMITS,
        )
        .await;
    let error = match result {
        Ok(_) => panic!("selected shell helper must refuse missing shell authority"),
        Err(error) => error,
    };
    assert!(
        error.contains("require an original Git-for-Windows shell"),
        "{error}"
    );
    // Native argv[0] without Git's shell metacharacters remains usable even
    // when no original shell exists. Spaces/quotes require the shell instead.
    for (value, needs_shell) in [
        ("C:/native-service.exe", false),
        ("C:/native service.exe", true),
    ] {
        std::fs::write(
            &global,
            format!("[core]\nsshCommand = {}\n", config_value(value)),
        )
        .unwrap();
        let globals = GlobalSources::capture_roots(&runner, &project, vec![global.clone()], None)
            .await
            .unwrap();
        let runner = runner.clone().with_test_globals(globals);
        let result = runner
            .run_network(
                &project,
                "config",
                ["config", "--get", "core.sshCommand"],
                QUERY_LIMITS,
            )
            .await;
        assert_eq!(result.is_err(), needs_shell, "{value}");
        if let Ok(output) = result {
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), value);
        }
    }
}
