use std::process::Command;

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-headless-json-process-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).expect("create private app root");
        let path = std::fs::canonicalize(path).expect("canonicalize private app root");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("make app root private");
        }
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn print_json_reserves_stdout_for_one_machine_readable_value() {
    let root = TempRoot::new();
    let output = Command::new(env!("CARGO_BIN_EXE_mini-agent"))
        .env("ZS_CONFIG_DIR", &root.0)
        .env("ZS_DATA_DIR", &root.0)
        .env("ZS_LOCAL_DATA_DIR", &root.0)
        .env("ZS_STATE_DIR", &root.0)
        .env("ZS_CACHE_DIR", &root.0)
        .args([
            "--no-sandbox",
            "--no-session",
            "--no-tools",
            "--no-context-files",
            "-p",
            "--output",
            "json",
            "!echo headless-json-smoke",
        ])
        .output()
        .expect("run installed test binary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8 JSON");
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should contain only one JSON value");
    assert!(value["result"].is_string());
    assert_eq!(value["files_changed"], serde_json::json!([]));
    assert_eq!(
        value["tool_calls"],
        serde_json::json!({"total": 0, "by_name": {}})
    );
    assert_eq!(value["usage"]["total_tokens"], 0);
    assert_eq!(value["cost"], 0.0);
    assert_eq!(value["stop_reason"], "completed");
}
