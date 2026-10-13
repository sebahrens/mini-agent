//! Actual process isolation for the optional release-check cache lease.

const CHILD: &str = "MINI_AGENT_TEST_UPDATE_ADMISSION";
const NOW: &str = "MINI_AGENT_TEST_UPDATE_NOW";
const TEST: &str =
    "tests::update_tests::update_admission_is_exclusive_across_processes_and_after_cancellation";

pub(crate) async fn run_admission_child(state: &std::path::Path, now: u64) {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env(CHILD, state)
        .env(NOW, now.to_string());
    let output = crate::sandbox::Sandbox::new(false, "none")
        .output_built_command_with_limits(
            command,
            crate::sandbox::CommandLimits {
                timeout: std::time::Duration::from_secs(15),
                stdout_bytes: 32 * 1024,
                stderr_bytes: 32 * 1024,
                combined_bytes: 64 * 1024,
            },
        )
        .await
        .unwrap();
    assert_eq!(output.status, crate::sandbox::CommandStatus::Completed);
    assert!(
        output.exit_status.unwrap().success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("running 1 test"), "{stdout}");
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed"),
        "{stdout}"
    );
}

#[tokio::test]
async fn update_admission_is_exclusive_across_processes_and_after_cancellation() {
    if let Some(root) = std::env::var_os(CHILD) {
        let now = std::env::var(NOW).unwrap().parse().unwrap();
        crate::update::verify_cross_process_admission(std::path::Path::new(&root), now, false)
            .await
            .unwrap();
        return;
    }
    let root = crate::tests::OwnedTestDirectory::new("update-process-admission");
    let now = 1_000_000;
    crate::update::verify_cross_process_admission(root.path(), now, true)
        .await
        .unwrap();
    // The first fetch future and its lease have now been dropped. A new
    // process must still find the durably reserved attempt and do no network.
    run_admission_child(root.path(), now + 1).await;
}
