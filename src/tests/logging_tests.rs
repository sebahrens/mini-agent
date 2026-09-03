use std::path::PathBuf;

use crate::cli::Cli;
use crate::logging;

use clap::Parser;

fn parse_cli(args: &[&str]) -> Cli {
    let mut full = vec!["zerostack"];
    full.extend(args);
    Cli::parse_from(full)
}

#[test]
fn test_resolve_log_path_default_no_verbose() {
    let cli = parse_cli(&[]);
    let log_path = logging::resolve_log_path(&cli);
    assert!(log_path.is_none());
}

#[test]
fn test_resolve_log_path_verbose() {
    let cli = parse_cli(&["-v"]);
    let log_path = logging::resolve_log_path(&cli);
    assert!(log_path.is_some());
    let path = log_path.unwrap();
    assert!(path.to_string_lossy().contains("zerostack-"));
    assert!(path.to_string_lossy().ends_with(".log"));
    assert!(
        path.to_string_lossy()
            .contains(&std::process::id().to_string())
    );
}

#[test]
fn test_resolve_log_path_cli_override() {
    let cli = parse_cli(&["--log-file", "/tmp/test-zerostack.log"]);
    let log_path = logging::resolve_log_path(&cli);
    assert_eq!(log_path, Some(PathBuf::from("/tmp/test-zerostack.log")));
}

#[test]
fn test_resolve_log_path_cli_overrides_verbose() {
    let cli = parse_cli(&["-v", "--log-file", "/tmp/override.log"]);
    let log_path = logging::resolve_log_path(&cli);
    assert_eq!(log_path, Some(PathBuf::from("/tmp/override.log")));
}

#[test]
fn test_build_stderr_filter_default() {
    let cli = parse_cli(&[]);
    let filter = logging::build_stderr_filter(&cli);
    let s = format!("{}", filter);
    assert!(s.contains("warn"));
}

#[test]
fn test_build_stderr_filter_log_level() {
    let cli = parse_cli(&["--log-level", "info"]);
    let filter = logging::build_stderr_filter(&cli);
    let s = format!("{}", filter);
    assert!(s.contains("info"));
}

#[test]
fn test_build_stderr_filter_invalid_log_level_does_not_panic() {
    let cli = parse_cli(&["--log-level", "invalid"]);
    let _filter = logging::build_stderr_filter(&cli);
}

#[test]
fn test_verbose_flag_is_false_by_default() {
    let cli = parse_cli(&[]);
    assert!(!cli.verbose);
}

#[test]
fn test_verbose_flag_set() {
    let cli = parse_cli(&["-v"]);
    assert!(cli.verbose);
}

#[test]
fn test_verbose_flag_long_form() {
    let cli = parse_cli(&["--verbose"]);
    assert!(cli.verbose);
}

#[test]
fn test_crash_log_path_format() {
    let path = logging::resolve_crash_log_path();
    let s = path.to_string_lossy();
    assert!(s.contains("crashes"));
    assert!(s.contains("zerostack-crash-"));
    assert!(s.ends_with(".log"));
    assert!(s.contains(&std::process::id().to_string()));
}

#[test]
fn test_crash_log_dir_is_under_data_logs() {
    let dir = logging::crash_log_dir();
    let s = dir.to_string_lossy();
    assert!(s.contains("logs"));
    assert!(s.ends_with("crashes"));
}

#[test]
fn test_file_filter_directive_targets_this_crate_and_audit_targets() {
    let directive = logging::file_filter_directive();
    assert!(
        directive.contains(&format!("{}=trace", env!("CARGO_CRATE_NAME"))),
        "file filter must enable this crate's own tracing target: {directive}"
    );
    assert!(directive.contains("zerostack=trace"), "{directive}");
    assert!(directive.contains("rig=off"), "{directive}");
}

#[test]
fn test_file_filter_passes_debug_events_from_this_crate() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let buffer = Shared(Arc::new(Mutex::new(Vec::new())));
    let writer = buffer.clone();
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .with_filter(logging::build_file_filter());
    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::with_default(subscriber, || {
        tracing::debug!("file-filter-probe-crate-debug");
        tracing::event!(
            target: "zerostack::audit::explicit_shell",
            tracing::Level::TRACE,
            "file-filter-probe-audit-trace"
        );
        tracing::event!(
            target: "rig::probe",
            tracing::Level::ERROR,
            "file-filter-probe-rig-error"
        );
    });

    let captured = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    assert!(
        captured.contains("file-filter-probe-crate-debug"),
        "a tracing::debug! from this crate must reach the log file: {captured:?}"
    );
    assert!(
        captured.contains("file-filter-probe-audit-trace"),
        "explicit zerostack audit targets must still be captured: {captured:?}"
    );
    assert!(
        !captured.contains("file-filter-probe-rig-error"),
        "rig must stay silenced: {captured:?}"
    );
}
