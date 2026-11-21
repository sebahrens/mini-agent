//! Command-backed status-bar segments (`item = "command"`).
//!
//! A segment names a program (`command`, `args`; run directly, never through a
//! shell) that is re-run every `interval_secs` off the UI task. It gets a small
//! JSON document on stdin (model, cwd, session id, context percentage, cost),
//! and the first line of its stdout, stripped of terminal control sequences,
//! is cached and shown. A command that fails, prints nothing or outlives its
//! timeout shows nothing. Only the global config can define such a segment: a
//! project layer's command segments are dropped when it loads.
//!
//! Launching reuses the `api_key_commands` launcher (`key_command::run_with`):
//! direct argv, own process group, timeout, 16 KiB output cap, stderr
//! discarded, user environment, outside the model sandbox.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::types::StatusLineSegment;

const DEFAULT_INTERVAL_SECS: u64 = 10;
const MAX_INTERVAL_SECS: u64 = 3600;
const DEFAULT_TIMEOUT_SECS: u64 = 2;
const MAX_TIMEOUT_SECS: u64 = 10;
const MAX_ARGS: usize = 32;
/// Longest text one segment may show, in characters.
const MAX_TEXT_CHARS: usize = 120;

/// A validated command segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub(crate) argv: Vec<String>,
    pub(crate) interval: Duration,
    pub(crate) timeout: Duration,
}

impl CommandSpec {
    /// `None` when the segment is not a usable command segment.
    pub(crate) fn of(segment: &StatusLineSegment) -> Option<Self> {
        if segment.item != "command" {
            return None;
        }
        let program = segment.command.as_deref()?.trim();
        let args = segment.args.clone().unwrap_or_default();
        if program.is_empty()
            || program.contains('\0')
            || args.len() > MAX_ARGS
            || args.iter().any(|arg| arg.contains('\0'))
        {
            return None;
        }
        let mut argv = vec![program.to_owned()];
        argv.extend(args);
        Some(Self {
            argv,
            interval: Duration::from_secs(
                segment
                    .interval_secs
                    .unwrap_or(DEFAULT_INTERVAL_SECS)
                    .clamp(1, MAX_INTERVAL_SECS),
            ),
            timeout: Duration::from_secs(
                segment
                    .timeout_secs
                    .unwrap_or(DEFAULT_TIMEOUT_SECS)
                    .clamp(1, MAX_TIMEOUT_SECS),
            ),
        })
    }

    fn key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut state = std::collections::hash_map::DefaultHasher::new();
        self.argv.hash(&mut state);
        state.finish()
    }
}

#[derive(Default)]
struct Entry {
    text: Option<String>,
    started: Option<Instant>,
    in_flight: bool,
}

static STORE: Mutex<Option<HashMap<u64, Entry>>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn with_store<T>(f: impl FnOnce(&mut HashMap<u64, Entry>) -> T) -> T {
    let mut guard = STORE.lock().unwrap_or_else(|error| error.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Bumps whenever any cached text changes; part of the statusline cache key.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// The cached text of a command segment, if any.
pub(crate) fn cached_text(segment: &StatusLineSegment) -> Option<String> {
    let spec = CommandSpec::of(segment)?;
    with_store(|store| store.get(&spec.key()).and_then(|entry| entry.text.clone()))
}

/// Specs whose interval elapsed and that are not already running; they are
/// marked in flight, so the caller must run each one and `finish` it.
pub(crate) fn due(specs: &[CommandSpec], now: Instant) -> Vec<CommandSpec> {
    with_store(|store| {
        let mut due = Vec::new();
        for spec in specs {
            let entry = store.entry(spec.key()).or_default();
            if entry.in_flight {
                continue;
            }
            if entry
                .started
                .is_none_or(|at| now.saturating_duration_since(at) >= spec.interval)
            {
                entry.in_flight = true;
                entry.started = Some(now);
                if !due.contains(spec) {
                    due.push(spec.clone());
                }
            }
        }
        due
    })
}

/// First stdout line as display text: terminal control sequences removed,
/// whitespace trimmed, bounded. `Err` (nothing shown) for empty output.
pub(crate) fn display_line(output: &[u8]) -> Result<String, &'static str> {
    let text = String::from_utf8_lossy(output);
    let first = text.lines().next().unwrap_or("");
    let clean = crate::ui::events::sanitize_output(first);
    let clean = clean.replace('\n', " ");
    let clean = clean.trim();
    if clean.is_empty() {
        return Err("produced no output");
    }
    Ok(clean.chars().take(MAX_TEXT_CHARS).collect())
}

/// Record a run's outcome and mark the entry idle. Returns whether the shown
/// text changed.
pub(crate) fn finish(spec: &CommandSpec, result: Result<String, String>) -> bool {
    let changed = with_store(|store| {
        let entry = store.entry(spec.key()).or_default();
        entry.in_flight = false;
        let text = result.ok();
        let changed = entry.text != text;
        entry.text = text;
        changed
    });
    if changed {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    changed
}

/// Run one due command with `input` on stdin and record the result.
pub(crate) async fn run(spec: CommandSpec, input: Vec<u8>) -> bool {
    let result = crate::key_command::run_with(
        "statusline",
        spec.argv.clone(),
        spec.timeout.as_secs(),
        Some(input),
        display_line,
    )
    .await;
    finish(&spec, result)
}

/// The JSON document a segment's command reads on stdin.
pub(crate) fn input_json(
    session: &crate::session::Session,
    workspace: &std::path::Path,
) -> Vec<u8> {
    let percent = session
        .effective_context_tokens()
        .saturating_mul(100)
        .checked_div(session.context_window);
    serde_json::to_vec(&serde_json::json!({
        "model": session.model.as_str(),
        "provider": session.provider.as_str(),
        "cwd": workspace.to_string_lossy(),
        "session_id": session.id.as_str(),
        "context_percentage": percent,
        "cost": session.recorded_total_cost(),
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(program: &str, args: &[&str]) -> StatusLineSegment {
        StatusLineSegment {
            item: "command".into(),
            command: Some(program.to_owned()),
            args: Some(args.iter().map(|a| (*a).to_owned()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn specs_validate_and_clamp() {
        assert!(CommandSpec::of(&StatusLineSegment { item: "command".into(), ..Default::default() }).is_none());
        assert!(CommandSpec::of(&segment("", &[])).is_none());
        assert!(CommandSpec::of(&segment("a\0b", &[])).is_none());
        let mut seg = segment("kubectl", &["config", "current-context"]);
        seg.interval_secs = Some(0);
        seg.timeout_secs = Some(999);
        let spec = CommandSpec::of(&seg).unwrap();
        assert_eq!(spec.argv, ["kubectl", "config", "current-context"]);
        assert_eq!(spec.interval, Duration::from_secs(1));
        assert_eq!(spec.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
        let mut other = segment("kubectl", &[]);
        other.item = "model".into();
        assert!(CommandSpec::of(&other).is_none());
    }

    #[test]
    fn output_is_one_sanitised_bounded_line() {
        assert_eq!(display_line(b"\x1b[31mprod\x1b[0m\nsecond"), Ok("prod".into()));
        assert_eq!(
            display_line(b"a\x1b]0;evil title\x07b\n"),
            Ok("ab".into())
        );
        assert!(display_line(b"\n\n").is_err());
        assert!(display_line(b"   \n").is_err());
        assert_eq!(display_line("x".repeat(500).as_bytes()).unwrap().chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn due_respects_interval_and_in_flight_and_caches_results() {
        let seg = segment("unique-statusline-test-a", &[]);
        let spec = CommandSpec::of(&seg).unwrap();
        let now = Instant::now();
        assert_eq!(due(std::slice::from_ref(&spec), now), vec![spec.clone()]);
        // Running: not due again even after the interval.
        assert!(due(std::slice::from_ref(&spec), now + Duration::from_secs(60)).is_empty());
        let before = generation();
        assert!(finish(&spec, Ok("prod".into())));
        assert!(generation() > before);
        assert_eq!(cached_text(&seg), Some("prod".into()));
        // Idle but inside the interval: not due; after it: due.
        assert!(due(std::slice::from_ref(&spec), now + Duration::from_secs(1)).is_empty());
        assert_eq!(due(std::slice::from_ref(&spec), now + Duration::from_secs(11)).len(), 1);
        // A failure clears the shown text.
        assert!(finish(&spec, Err("timed out".into())));
        assert_eq!(cached_text(&seg), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_command_is_killed_at_the_timeout_and_shows_nothing() {
        let mut seg = segment("sh", &["-c", "sleep 30"]);
        seg.timeout_secs = Some(1);
        let spec = CommandSpec::of(&seg).unwrap();
        due(std::slice::from_ref(&spec), Instant::now());
        let started = Instant::now();
        run(spec, b"{}".to_vec()).await;
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_eq!(cached_text(&seg), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_gets_the_json_on_stdin_and_its_first_line_is_cached() {
        let seg = segment("sh", &["-c", "read line; printf '\\033[1mctx:%s\\033[0m\\nignored' \"$line\""]);
        let spec = CommandSpec::of(&seg).unwrap();
        due(std::slice::from_ref(&spec), Instant::now());
        run(spec, b"{\"k\":1}\n".to_vec()).await;
        assert_eq!(cached_text(&seg), Some("ctx:{\"k\":1}".into()));
    }

    #[test]
    fn project_layers_lose_command_segments() {
        let mut value: toml::Value = toml::from_str(
            "[[lines]]\n[[lines.segments]]\nitem = \"model\"\n[[lines.segments]]\nitem = \"command\"\ncommand = \"/tmp/x\"\n",
        )
        .unwrap();
        assert_eq!(crate::config::load::strip_statusline_commands(&mut value), 1);
        let left = value["lines"][0]["segments"].as_array().unwrap();
        assert_eq!(left.len(), 1);
    }
}
