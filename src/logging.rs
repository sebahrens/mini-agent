use std::backtrace::Backtrace;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::cli::Cli;

pub fn crash_log_dir() -> PathBuf {
    crate::paths::process_paths_ref()
        .expect("startup must initialize application paths")
        .crash_logs_dir()
}

pub fn resolve_crash_log_path() -> PathBuf {
    let dir = crash_log_dir();
    let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let pid = std::process::id();
    dir.join(format!("mini-agent-crash-{ts}_{pid}.log"))
}

pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::ui::terminal::restore_for_panic();
        if let Some(path) = write_crash_report(info) {
            eprintln!("crash report written to {}", path.display());
        }
        default_hook(info);
    }));
}

fn write_crash_report(info: &std::panic::PanicHookInfo) -> Option<PathBuf> {
    if crate::paths::artifact_disabled("logs") {
        return None;
    }
    let path = resolve_crash_log_path();
    if let Some(parent) = path.parent()
        && ensure_private_log_directory(parent).is_err()
    {
        return None;
    }

    let mut content = String::from("mini-agent crash report\n");
    content.push_str(&format!("time: {}\n", chrono::Local::now().to_rfc3339()));
    if let Some(version) = option_env!("CARGO_PKG_VERSION") {
        content.push_str(&format!("version: {version}\n"));
    }
    content.push('\n');

    if let Some(payload) = info.payload().downcast_ref::<&str>() {
        content.push_str(&format!("panic: {payload}\n"));
    } else if let Some(payload) = info.payload().downcast_ref::<String>() {
        content.push_str(&format!("panic: {payload}\n"));
    } else {
        content.push_str("panic: unknown\n");
    }

    if let Some(loc) = info.location() {
        content.push_str(&format!(
            "location: {}:{}:{}\n",
            loc.file(),
            loc.line(),
            loc.column()
        ));
    }

    content.push('\n');
    content.push_str(&format!("{:?}", Backtrace::capture()));

    crate::fs::atomic_create_sync(&path, content.as_bytes())
        .ok()
        .map(|_| path)
}

pub fn resolve_log_path(cli: &Cli) -> Option<PathBuf> {
    if let Some(ref path) = cli.log_file {
        return Some(path.clone());
    }
    if crate::paths::artifact_disabled("logs") {
        return None;
    }
    if cli.verbose {
        let logs_dir = crate::paths::process_paths_ref()
            .expect("startup must initialize application paths")
            .logs_dir();
        ensure_private_log_directory(&logs_dir).ok();
        let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let pid = std::process::id();
        return Some(logs_dir.join(format!("mini-agent-{ts}_{pid}.log")));
    }
    None
}

pub fn build_stderr_filter(cli: &Cli) -> EnvFilter {
    if let Some(ref lvl) = cli.log_level {
        if matches!(
            lvl.to_ascii_lowercase().as_str(),
            "off" | "error" | "warn" | "info" | "debug" | "trace"
        ) {
            return EnvFilter::new(format!("{lvl},rig=off"));
        }
        return EnvFilter::new("warn,rig=off");
    }
    if let Ok(f) = EnvFilter::try_from_default_env() {
        return f;
    }
    EnvFilter::new("warn,rig=off")
}

/// Directive for the `--verbose` / `--log-file` file layer.
///
/// The crate's own events carry the crate name as their tracing target
/// (`mini_agent`, derived from the `mini-agent` package name), so that target
/// is enabled at `trace` from `CARGO_CRATE_NAME` rather than hard-coded. The
/// explicit `mini_agent::audit::*` targets stay enabled and `rig` stays silent.
pub fn file_filter_directive() -> String {
    format!("{}=trace,rig=off", env!("CARGO_CRATE_NAME"))
}

pub fn build_file_filter() -> EnvFilter {
    EnvFilter::new(file_filter_directive())
}

/// Where the stderr layer's formatted records go.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplayTarget {
    Stderr,
    /// Discarded: the interactive TUI owns the terminal. Records still
    /// reach the `-v` / `--log-file` file layer, which is independent.
    Suppressed,
}

/// Interactive sessions never write tracing output to the terminal while the
/// TUI owns it (attached or suspended for an editor or pager): a log line
/// would be painted between rendered rows. Before the TUI attaches and after
/// it exits, and in every headless mode (`-p`, loop, ACP), stderr is used.
pub(crate) fn display_target(is_interactive: bool, tui_owns_terminal: bool) -> DisplayTarget {
    if is_interactive && tui_owns_terminal {
        DisplayTarget::Suppressed
    } else {
        DisplayTarget::Stderr
    }
}

const HEADLESS_LOG_CAPACITY: usize = 32;
const HEADLESS_LOG_MAX_BYTES: usize = 4096;

#[derive(Default)]
pub(crate) struct HeadlessDiagnostics(Mutex<HeadlessPending>);
#[derive(Default)]
struct HeadlessPending {
    messages: std::collections::VecDeque<String>,
    dropped: usize,
    draining: bool,
    closed: bool,
}
impl HeadlessDiagnostics {
    #[cfg(test)]
    pub(crate) fn admit_for_test(
        &self,
        message: String,
        cancellation: &crate::agent::runner::AgentWorkCancellation,
    ) -> bool {
        self.admit(message, cancellation)
    }

    fn admit(
        &self,
        message: String,
        cancellation: &crate::agent::runner::AgentWorkCancellation,
    ) -> bool {
        let mut pending = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.closed || cancellation.is_cancelled() {
            pending.closed = true;
            pending.messages.clear();
            return false;
        }
        if pending.messages.len() == HEADLESS_LOG_CAPACITY {
            pending.dropped = pending.dropped.saturating_add(1);
            return false;
        }
        pending.messages.push_back(message);
        if pending.draining {
            false
        } else {
            pending.draining = true;
            true
        }
    }
    fn next(&self, cancellation: &crate::agent::runner::AgentWorkCancellation) -> Option<String> {
        let mut pending = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cancellation.is_cancelled() {
            pending.closed = true;
            pending.messages.clear();
            pending.draining = false;
            return None;
        }
        if let Some(message) = pending.messages.pop_front() {
            return Some(message);
        }
        if pending.dropped != 0 {
            return Some(format!(
                "headless logging: {} diagnostics dropped\n",
                std::mem::take(&mut pending.dropped)
            ));
        }
        pending.draining = false;
        None
    }
    async fn drain(
        self: std::sync::Arc<Self>,
        cancellation: crate::agent::runner::AgentWorkCancellation,
    ) {
        while let Some(message) = self.next(&cancellation) {
            if crate::print::headless_stderr_async(message).await.is_err() {
                let mut pending = self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.closed = true;
                pending.draining = false;
                pending.messages.clear();
                break;
            }
        }
    }
}
struct HeadlessLogWriter {
    bytes: Vec<u8>,
    clipped: bool,
    pending: std::sync::Arc<HeadlessDiagnostics>,
    cancellation: crate::agent::runner::AgentWorkCancellation,
}
impl Write for HeadlessLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = HEADLESS_LOG_MAX_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.clipped |= bytes.len() > remaining;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for HeadlessLogWriter {
    fn drop(&mut self) {
        if self.bytes.is_empty() {
            return;
        }
        let raw = String::from_utf8_lossy(&self.bytes);
        let raw = if self.clipped {
            format!("{raw} [diagnostic truncated]\n")
        } else {
            raw.into_owned()
        };
        let (message, _) = crate::print::bounded_progress(&raw, HEADLESS_LOG_MAX_BYTES);
        if self.pending.admit(message, &self.cancellation) {
            let pending = self.pending.clone();
            let cancellation = self.cancellation.clone();
            // The current scope owns this worker and each blocking write/join.
            std::mem::drop(crate::agent::runner::spawn_async_scoped(
                pending.drain(cancellation),
            ));
        }
    }
}

pub fn init(cli: &Cli, is_interactive: bool) {
    let stderr_filter = build_stderr_filter(cli);
    let headless = cli.is_headless();
    let file_filter = build_file_filter();

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(!is_interactive)
        .with_writer(move || {
            match display_target(is_interactive, crate::ui::terminal::tui_owns_terminal()) {
                DisplayTarget::Suppressed => LogWriter::Sink,
                DisplayTarget::Stderr => {
                    if headless
                        && let Some((pending, cancellation)) =
                            crate::agent::runner::current_headless_diagnostics()
                    {
                        LogWriter::Headless(HeadlessLogWriter {
                            bytes: Vec::new(),
                            clipped: false,
                            pending,
                            cancellation,
                        })
                    } else {
                        LogWriter::Stderr(io::stderr())
                    }
                }
            }
        })
        .with_filter(stderr_filter);

    let registry = tracing_subscriber::registry().with(stderr_layer);

    let log_path = resolve_log_path(cli);
    if let Some(ref path) = log_path {
        match open_private_log(path, cli.log_file.is_none()) {
            Ok(file) => {
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_writer(Mutex::new(file))
                    .with_filter(file_filter);
                registry.with(file_layer).init();
                return;
            }
            Err(e) => {
                eprintln!(
                    "warning: could not create log file {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }

    registry.init();
}

enum LogWriter {
    Sink,
    Headless(HeadlessLogWriter),
    Stderr(io::Stderr),
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Sink => Ok(bytes.len()),
            Self::Headless(writer) => writer.write(bytes),
            Self::Stderr(writer) => writer.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Sink => Ok(()),
            Self::Headless(writer) => writer.flush(),
            Self::Stderr(writer) => writer.flush(),
        }
    }
}

fn ensure_private_log_directory(path: &std::path::Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_log(path: &std::path::Path, prepare_parent: bool) -> io::Result<fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    const OPEN_NOFOLLOW: std::os::raw::c_int = libc::O_NOFOLLOW;
    if prepare_parent && let Some(parent) = path.parent() {
        ensure_private_log_directory(parent)?;
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(OPEN_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(windows)]
fn open_private_log(path: &std::path::Path, prepare_parent: bool) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    if prepare_parent && let Some(parent) = path.parent() {
        ensure_private_log_directory(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_log(path: &std::path::Path, prepare_parent: bool) -> io::Result<fs::File> {
    if prepare_parent && let Some(parent) = path.parent() {
        ensure_private_log_directory(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod display_target_tests {
    use super::*;

    #[test]
    fn interactive_tui_suppresses_terminal_log_output() {
        assert_eq!(display_target(true, true), DisplayTarget::Suppressed);
    }

    #[test]
    fn startup_exit_and_headless_modes_keep_stderr() {
        // Interactive before the TUI attaches or after it exits.
        assert_eq!(display_target(true, false), DisplayTarget::Stderr);
        // Print (-p), loop and ACP modes are never interactive.
        assert_eq!(display_target(false, false), DisplayTarget::Stderr);
        assert_eq!(display_target(false, true), DisplayTarget::Stderr);
    }

    #[test]
    fn sink_writer_discards_bytes() {
        let mut writer = LogWriter::Sink;
        assert_eq!(writer.write(b"WARN leaked").unwrap(), 11);
        writer.flush().unwrap();
    }
}

#[cfg(test)]
mod headless_diagnostic_tests {
    use super::*;
    #[test]
    fn queue_admission_is_bounded_ordered_restartable_and_cancel_closes_it() {
        let scope = crate::agent::runner::AgentWorkScope::new();
        let cancellation = scope.cancellation_handle();
        let queue = HeadlessDiagnostics::default();
        for index in 0..HEADLESS_LOG_CAPACITY + 3 {
            assert_eq!(
                queue.admit(format!("record-{index}"), &cancellation),
                index == 0
            );
        }
        for index in 0..HEADLESS_LOG_CAPACITY {
            assert_eq!(
                queue.next(&cancellation).unwrap(),
                format!("record-{index}")
            );
        }
        assert_eq!(
            queue.next(&cancellation).unwrap(),
            "headless logging: 3 diagnostics dropped\n"
        );
        assert!(queue.next(&cancellation).is_none());
        assert!(queue.admit("restart".into(), &cancellation));
        cancellation.cancel();
        assert!(queue.next(&cancellation).is_none());
        assert!(!queue.admit("no fallback or new worker".into(), &cancellation));
        assert!(queue.0.lock().unwrap().closed);
        assert_eq!(scope.active_children(), 0);
    }
    #[test]
    fn formatted_writer_caps_bytes_and_retains_bounded_terminal_safe_diagnostic() {
        let scope = crate::agent::runner::AgentWorkScope::new();
        let queue = std::sync::Arc::new(HeadlessDiagnostics::default());
        // Keep pure fixture admission from launching a real transport worker.
        queue.0.lock().unwrap().draining = true;
        let mut writer = HeadlessLogWriter {
            bytes: Vec::new(),
            clipped: false,
            pending: queue.clone(),
            cancellation: scope.cancellation_handle(),
        };
        let bytes = format!("\x1b[31m{}", "é".repeat(HEADLESS_LOG_MAX_BYTES));
        assert_eq!(writer.write(bytes.as_bytes()).unwrap(), bytes.len());
        assert_eq!(writer.bytes.len(), HEADLESS_LOG_MAX_BYTES);
        assert!(writer.clipped);
        drop(writer);
        let record = queue.next(&scope.cancellation_handle()).unwrap();
        assert!(record.len() <= HEADLESS_LOG_MAX_BYTES);
        assert!(!record.contains('\x1b'));
        assert!(record.ends_with("[truncated]"));
    }
    #[tokio::test]
    async fn current_scope_owns_worker_to_exact_idle_and_canceled_writer_starts_no_job() {
        let scope = crate::agent::runner::AgentWorkScope::new();
        scope
            .run(async {
                let (queue, cancellation) =
                    crate::agent::runner::current_headless_diagnostics().unwrap();
                let mut writer = HeadlessLogWriter {
                    bytes: Vec::new(),
                    clipped: false,
                    pending: queue.clone(),
                    cancellation,
                };
                writer.write_all(b"owned diagnostic\n").unwrap();
                drop(writer);
                tokio::time::timeout(std::time::Duration::from_secs(2), scope.wait_idle())
                    .await
                    .unwrap();
                assert!(!queue.0.lock().unwrap().draining);
                scope.cancellation_handle().cancel();
                let (queue, cancellation) =
                    crate::agent::runner::current_headless_diagnostics().unwrap();
                let mut writer = HeadlessLogWriter {
                    bytes: Vec::new(),
                    clipped: false,
                    pending: queue.clone(),
                    cancellation,
                };
                writer.write_all(b"canceled diagnostic\n").unwrap();
                drop(writer);
                assert_eq!(scope.active_children(), 0);
                assert!(queue.0.lock().unwrap().messages.is_empty());
            })
            .await;
    }
}
