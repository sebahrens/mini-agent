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

/// Local time as RFC 3339 with a numeric offset, matching the previous
/// `to_rfc3339()` (fraction trimmed to 0, 3, 6 or 9 digits).
fn local_rfc3339(now: &jiff::ZonedDateTime) -> String {
    let nanos = now.subsec_nanosecond();
    let base = now.strftime("%Y-%m-%dT%H:%M:%S");
    let offset = now.strftime("%:z");
    if nanos == 0 {
        format!("{base}{offset}")
    } else if nanos % 1_000_000 == 0 {
        format!("{base}.{:03}{offset}", nanos / 1_000_000)
    } else if nanos % 1_000 == 0 {
        format!("{base}.{:06}{offset}", nanos / 1_000)
    } else {
        format!("{base}.{nanos:09}{offset}")
    }
}

pub fn resolve_crash_log_path() -> PathBuf {
    let dir = crash_log_dir();
    let ts = jiff::ZonedDateTime::now().strftime("%Y-%m-%d_%H-%M-%S");
    let pid = std::process::id();
    dir.join(format!("mini-agent-crash-{ts}_{pid}.log"))
}

pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use crate::ui::terminal::PanicScope;
        let scope = crate::ui::terminal::panic_scope();
        if scope != PanicScope::Background {
            crate::ui::terminal::restore_for_panic();
        }
        let report = write_crash_report(info);
        if scope == PanicScope::Background {
            // The TUI keeps running and owns the screen: writing to stderr
            // would corrupt it and tearing the terminal down would leave the
            // event loop painting on a cooked main screen. Tell the user in
            // the feed instead.
            let line = match &report {
                Some(path) => format!("internal task panicked; crash report at {}", path.display()),
                None => "internal task panicked (no crash report could be written)".to_string(),
            };
            if !crate::ui::post_notice(line) {
                tracing::error!("background panic while the TUI notice channel is closed");
            }
            return;
        }
        if let Some(path) = report {
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
    content.push_str(&format!(
        "time: {}\n",
        local_rfc3339(&jiff::ZonedDateTime::now())
    ));
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
        let ts = jiff::ZonedDateTime::now().strftime("%Y-%m-%d_%H-%M-%S");
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
        .with_ansi(stderr_ansi_enabled())
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
                if cli.log_file.is_some() {
                    write_run_marker(&file);
                } else {
                    prune_auto_logs_in_background();
                }
                registry.with(file_layer(file, file_filter)).init();
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

/// The log-file sink. It is never coloured: escape sequences break grep and
/// editors (mini-agent-nvnkx.155).
fn file_layer<S>(file: fs::File, filter: EnvFilter) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .with_filter(filter)
}

/// Colour on stderr only when it is a terminal and `NO_COLOR` is unset.
fn stderr_ansi_enabled() -> bool {
    use std::io::IsTerminal;
    io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// An explicit `--log-file` is appended to, so a later or concurrent run never
/// erases the log the user was about to inspect; each run starts with a marker.
fn write_run_marker(mut file: &fs::File) {
    let now = local_rfc3339(&jiff::ZonedDateTime::now());
    let _ = writeln!(
        file,
        "--- mini-agent run started {now} pid {} ---",
        std::process::id()
    );
}

/// Auto-named `-v` logs and crash reports kept besides the current one.
const KEEP_AUTO_FILES: usize = 20;
/// Auto-named files older than this are removed.
const KEEP_AUTO_DAYS: u64 = 14;

/// Remove stale files in `dir` named `prefix*suffix`: all but the newest `keep`
/// and any older than `max_age`. Only regular files are touched (never
/// symlinks, directories or other names), and `current` is never removed.
/// Returns how many were removed.
pub(crate) fn prune_stale_files(
    dir: &std::path::Path,
    matches: impl Fn(&str) -> bool,
    current: Option<&std::path::Path>,
    keep: usize,
    max_age: std::time::Duration,
) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !matches(name) {
                return None;
            }
            let path = entry.path();
            if current == Some(path.as_path()) {
                return None;
            }
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_file() {
                return None;
            }
            Some((metadata.modified().ok()?, path))
        })
        .collect();
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    let mut removed = 0;
    for (index, (modified, path)) in files.iter().enumerate() {
        let too_old = now.duration_since(*modified).is_ok_and(|age| age > max_age);
        if (index >= keep || too_old) && fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn is_auto_log_name(name: &str) -> bool {
    name.starts_with("mini-agent-")
        && !name.starts_with("mini-agent-crash-")
        && name.ends_with(".log")
}

fn is_crash_log_name(name: &str) -> bool {
    name.starts_with("mini-agent-crash-") && name.ends_with(".log")
}

/// Prune old auto-named logs and crash reports on a detached thread so startup
/// is not delayed.
fn prune_auto_logs_in_background() {
    let Ok(paths) = crate::paths::process_paths_ref() else {
        return;
    };
    let logs = paths.logs_dir();
    let crashes = paths.crash_logs_dir();
    let _ = std::thread::Builder::new()
        .name("log-retention".into())
        .spawn(move || {
            let max_age = std::time::Duration::from_secs(KEEP_AUTO_DAYS * 24 * 60 * 60);
            prune_stale_files(&logs, is_auto_log_name, None, KEEP_AUTO_FILES, max_age);
            prune_stale_files(&crashes, is_crash_log_name, None, KEEP_AUTO_FILES, max_age);
        });
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
        .create(true)
        .append(true)
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
        .create(true)
        .append(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_log(path: &std::path::Path, prepare_parent: bool) -> io::Result<fs::File> {
    if prepare_parent && let Some(parent) = path.parent() {
        ensure_private_log_directory(parent)?;
    }
    fs::OpenOptions::new().create(true).append(true).open(path)
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

#[cfg(test)]
mod rfc3339_tests {
    use super::local_rfc3339;

    #[test]
    fn local_rfc3339_matches_previous_digit_and_offset_rules() {
        let utc = jiff::tz::TimeZone::UTC;
        let at = |sec: i64, nanos: i32| {
            jiff::Timestamp::new(sec, nanos)
                .unwrap()
                .to_zoned_datetime(utc.clone())
        };
        assert_eq!(
            local_rfc3339(&at(1_797_000_000, 0)),
            "2026-12-11T14:40:00+00:00"
        );
        assert_eq!(
            local_rfc3339(&at(1_797_000_000, 120_000_000)),
            "2026-12-11T14:40:00.120+00:00"
        );
        assert_eq!(
            local_rfc3339(&at(1_797_000_000, 123_456_000)),
            "2026-12-11T14:40:00.123456+00:00"
        );
        assert_eq!(
            local_rfc3339(&at(1_797_000_000, 1)),
            "2026-12-11T14:40:00.000000001+00:00"
        );
        let plus_one = jiff::Timestamp::new(1_797_000_000, 0)
            .unwrap()
            .to_zoned_datetime(jiff::tz::TimeZone::fixed(jiff::tz::Offset::constant(1)));
        assert_eq!(local_rfc3339(&plus_one), "2026-12-11T15:40:00+01:00");
    }
}

#[cfg(all(test, unix))]
mod retention_tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ma-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_sink_never_contains_escape_bytes() {
        let dir = temp_dir("ansi");
        let path = dir.join("run.log");
        let file = open_private_log(&path, true).unwrap();
        let subscriber =
            tracing_subscriber::registry().with(file_layer(file, EnvFilter::new("debug")));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("coloured? no");
            tracing::debug!(key = "value", "second record");
        });
        let bytes = fs::read(&path).unwrap();
        assert!(!bytes.is_empty());
        assert!(
            !bytes.contains(&0x1b),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_explicit_log_file_is_appended_to_not_truncated() {
        let dir = temp_dir("append");
        let path = dir.join("run.log");
        for run in ["first", "second"] {
            let file = open_private_log(&path, false).unwrap();
            write_run_marker(&file);
            writeln!(&file, "{run} run line").unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("first run line") && text.contains("second run line"),
            "{text}"
        );
        assert_eq!(text.matches("run started").count(), 2, "{text}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_auto_logs_are_pruned_and_other_names_and_symlinks_survive() {
        let dir = temp_dir("prune");
        let current = dir.join("mini-agent-2099-01-01_00-00-00_1.log");
        fs::write(&current, "current").unwrap();
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for index in 0..30u64 {
            let path = dir.join(format!(
                "mini-agent-2000-01-01_00-00-{index:02}_{index}.log"
            ));
            fs::write(&path, "old").unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(base + std::time::Duration::from_secs(index))
                .unwrap();
        }
        fs::write(dir.join("notes.txt"), "keep").unwrap();
        fs::write(dir.join("mini-agent-crash-x.log"), "crash").unwrap();
        let target = dir.join("target.txt");
        fs::write(&target, "t").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("mini-agent-link.log")).unwrap();
        let removed = prune_stale_files(
            &dir,
            is_auto_log_name,
            Some(&current),
            20,
            std::time::Duration::from_secs(14 * 24 * 3600),
        );
        assert_eq!(removed, 10);
        let remaining = fs::read_dir(&dir).unwrap().count();
        // 20 stale + current + notes + crash + target + symlink.
        assert_eq!(remaining, 25);
        assert!(current.exists() && dir.join("notes.txt").exists());
        assert!(dir.join("mini-agent-link.log").exists());
        // The newest survive, the oldest go.
        assert!(dir.join("mini-agent-2000-01-01_00-00-29_29.log").exists());
        assert!(!dir.join("mini-agent-2000-01-01_00-00-00_0.log").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn files_older_than_the_age_bound_are_removed_even_within_the_count() {
        let dir = temp_dir("age");
        let path = dir.join("mini-agent-old.log");
        fs::write(&path, "old").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600),
        )
        .unwrap();
        let removed = prune_stale_files(
            &dir,
            is_auto_log_name,
            None,
            20,
            std::time::Duration::from_secs(14 * 24 * 3600),
        );
        assert_eq!(removed, 1);
        let _ = fs::remove_dir_all(dir);
    }
}
