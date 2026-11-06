//! Spooled head/tail capture for model-authored shell commands.
//!
//! Foreground `shell` calls and background jobs must not be killed merely
//! because they write a lot. This capture keeps an arrival-ordered head and a
//! rolling tail of tagged chunks in memory, counts what it drops in between,
//! and (when a private log path is configured) spools every byte to that file
//! so the model can recover the omitted middle with `read`/`grep`. Only an
//! optional hard ceiling stops the command, reported as
//! [`super::CommandStatus::OutputLimitExceeded`].

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Which pipe produced a chunk. `Merged` is a single pipe that carries both
/// stdout and stderr in exact write order (`2>&1`) or a pseudo-terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputChannel {
    Stdout,
    Stderr,
    Merged,
}

/// When the capture writes its full-output log file.
#[derive(Clone, Debug)]
pub(crate) enum LogPolicy {
    Disabled,
    /// Create the log the first time retention would drop bytes. Every byte
    /// seen so far is still retained at that point, so the log is complete.
    OnOverflow(PathBuf),
    /// Create the log immediately (background jobs) so it can be polled
    /// incrementally and outlives the in-memory record.
    Always(PathBuf),
}

#[derive(Clone, Debug)]
pub(crate) struct SpoolPolicy {
    pub head_bytes: usize,
    pub tail_bytes: usize,
    /// Total output after which the command is stopped. `None` never stops.
    pub hard_limit: Option<u64>,
    pub log: LogPolicy,
    /// Bytes after which the log file stops growing (the command continues).
    pub log_limit: u64,
}

/// Foreground shell retention: 768 KiB head plus 256 KiB tail in memory.
pub(crate) const SHELL_HEAD_BYTES: usize = 768 * 1024;
pub(crate) const SHELL_TAIL_BYTES: usize = 256 * 1024;
/// Pathological-volume ceiling for one foreground shell command.
pub(crate) const SHELL_HARD_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// Background jobs keep 48 KiB head plus 16 KiB tail in memory per job; the
/// complete stream lives in the job log up to this size.
pub(crate) const BACKGROUND_HEAD_BYTES: usize = 48 * 1024;
pub(crate) const BACKGROUND_TAIL_BYTES: usize = 16 * 1024;
pub(crate) const BACKGROUND_LOG_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// Retention for user-typed `!` commands, `verify_command`, goal checks and
/// loop validators: 500 KiB head plus 256 KiB tail per rendering. Two streams
/// of that size plus the omission marker still fit the 1.5 MiB combined cap
/// of the diagnostic renderers, so the tail is never clipped a second time.
pub(crate) const COMMAND_HEAD_BYTES: usize = 500 * 1024;
pub(crate) const COMMAND_TAIL_BYTES: usize = 256 * 1024;

impl SpoolPolicy {
    /// Spooled capture for commands the user or the configuration chose to
    /// run: never stopped for volume below the 64 MiB pathological ceiling,
    /// full output kept in `log` once retention overflows.
    pub(crate) fn captured_command(log: Option<PathBuf>) -> Self {
        Self {
            head_bytes: COMMAND_HEAD_BYTES,
            tail_bytes: COMMAND_TAIL_BYTES,
            hard_limit: Some(SHELL_HARD_OUTPUT_BYTES),
            log: log.map_or(LogPolicy::Disabled, LogPolicy::OnOverflow),
            log_limit: SHELL_HARD_OUTPUT_BYTES,
        }
    }
    pub(crate) fn foreground_shell(log: Option<PathBuf>) -> Self {
        Self {
            head_bytes: SHELL_HEAD_BYTES,
            tail_bytes: SHELL_TAIL_BYTES,
            hard_limit: Some(SHELL_HARD_OUTPUT_BYTES),
            log: log.map_or(LogPolicy::Disabled, LogPolicy::OnOverflow),
            log_limit: SHELL_HARD_OUTPUT_BYTES,
        }
    }
    pub(crate) fn background(log: Option<PathBuf>) -> Self {
        Self {
            head_bytes: BACKGROUND_HEAD_BYTES,
            tail_bytes: BACKGROUND_TAIL_BYTES,
            hard_limit: None,
            log: log.map_or(LogPolicy::Disabled, LogPolicy::Always),
            log_limit: BACKGROUND_LOG_LIMIT_BYTES,
        }
    }
}

/// One retained piece of the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptPart {
    Output(OutputChannel, Vec<u8>),
    /// Bytes dropped between the retained head and tail.
    Omitted(u64),
}

/// A read of the complete stream; see [`SpooledCapture::read_window`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputWindow {
    pub start: u64,
    pub head: Vec<u8>,
    /// Bytes between `head` and `tail` that this read does not return.
    pub skipped: u64,
    pub tail: Vec<u8>,
    /// Offset to continue from.
    pub next: u64,
}

/// The full-output log of a capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutputLog {
    pub path: PathBuf,
    pub bytes: u64,
    /// `false` when the log hit its size limit and stopped growing.
    pub complete: bool,
}

/// Model-facing view of a capture: arrival-ordered retained parts plus
/// accounting for what was dropped and where the full output lives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputTranscript {
    pub parts: Vec<TranscriptPart>,
    pub total_bytes: u64,
    pub omitted_bytes: u64,
    pub log: Option<OutputLog>,
    pub log_error: Option<String>,
}

impl OutputTranscript {
    /// Concatenated retained bytes of one channel (`Merged` also counts as
    /// stdout), with an omission marker where the middle was dropped.
    pub(crate) fn channel_bytes(&self, channel: OutputChannel) -> Vec<u8> {
        let mut bytes = Vec::new();
        for part in &self.parts {
            match part {
                TranscriptPart::Output(source, chunk)
                    if *source == channel
                        || (channel == OutputChannel::Stdout
                            && *source == OutputChannel::Merged) =>
                {
                    bytes.extend_from_slice(chunk)
                }
                TranscriptPart::Output(..) => {}
                TranscriptPart::Omitted(omitted) => bytes
                    .extend_from_slice(format!("\n[... {omitted} bytes omitted ...]\n").as_bytes()),
            }
        }
        bytes
    }
}

#[derive(Debug)]
enum LogState {
    NotStarted,
    Open(std::fs::File),
    Full,
    Failed(String),
}

#[derive(Debug)]
pub(crate) struct SpooledCapture {
    policy: SpoolPolicy,
    head: Vec<(OutputChannel, Vec<u8>)>,
    head_bytes: usize,
    tail: VecDeque<(OutputChannel, Vec<u8>)>,
    tail_bytes: usize,
    omitted: u64,
    total: u64,
    log: LogState,
    log_bytes: u64,
}

/// The hard ceiling was reached; the excess was not retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HardLimitReached;

impl SpooledCapture {
    pub(crate) fn new(policy: SpoolPolicy) -> Self {
        let mut capture = Self {
            policy,
            head: Vec::new(),
            head_bytes: 0,
            tail: VecDeque::new(),
            tail_bytes: 0,
            omitted: 0,
            total: 0,
            log: LogState::NotStarted,
            log_bytes: 0,
        };
        if let LogPolicy::Always(path) = &capture.policy.log {
            let path = path.clone();
            capture.open_log(&path);
        }
        capture
    }

    fn log_path(&self) -> Option<&Path> {
        match &self.policy.log {
            LogPolicy::Disabled => None,
            LogPolicy::OnOverflow(path) | LogPolicy::Always(path) => Some(path),
        }
    }

    fn open_log(&mut self, path: &Path) {
        self.log = match create_private_log(path) {
            Ok(file) => LogState::Open(file),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to create command output log");
                LogState::Failed(error.to_string())
            }
        };
    }

    fn write_log(&mut self, bytes: &[u8]) {
        let LogState::Open(file) = &mut self.log else {
            return;
        };
        let room = self.policy.log_limit.saturating_sub(self.log_bytes);
        let accepted = usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len());
        if let Err(error) = file.write_all(&bytes[..accepted]) {
            self.log = LogState::Failed(error.to_string());
            return;
        }
        self.log_bytes += accepted as u64;
        if accepted < bytes.len() {
            self.log = LogState::Full;
        }
    }

    fn retained_bytes(&self) -> usize {
        self.head_bytes + self.tail_bytes
    }

    /// Append one chunk in arrival order. Returns [`HardLimitReached`] after
    /// accepting the bytes that still fit when the ceiling is crossed.
    pub(crate) fn push(
        &mut self,
        channel: OutputChannel,
        bytes: &[u8],
    ) -> Result<(), HardLimitReached> {
        let (bytes, reached) = match self.policy.hard_limit {
            Some(limit) => {
                let room = limit.saturating_sub(self.total);
                let accepted = usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len());
                (&bytes[..accepted], accepted < bytes.len())
            }
            None => (bytes, false),
        };
        if !bytes.is_empty() {
            self.total += bytes.len() as u64;
            if matches!(self.log, LogState::NotStarted)
                && let LogPolicy::OnOverflow(path) = &self.policy.log
                && self.retained_bytes() + bytes.len()
                    > self.policy.head_bytes + self.policy.tail_bytes
            {
                let path = path.clone();
                self.open_log(&path);
                let retained: Vec<u8> = self
                    .head
                    .iter()
                    .chain(self.tail.iter())
                    .flat_map(|(_, chunk)| chunk.iter().copied())
                    .collect();
                self.write_log(&retained);
            }
            self.write_log(bytes);
            self.retain(channel, bytes);
        }
        if reached {
            Err(HardLimitReached)
        } else {
            Ok(())
        }
    }

    fn retain(&mut self, channel: OutputChannel, bytes: &[u8]) {
        let head_room = self.policy.head_bytes.saturating_sub(self.head_bytes);
        let to_head = bytes.len().min(head_room);
        if to_head > 0 {
            append_chunk(&mut self.head, channel, &bytes[..to_head]);
            self.head_bytes += to_head;
        }
        let rest = &bytes[to_head..];
        if rest.is_empty() {
            return;
        }
        match self.tail.back_mut() {
            Some((last, chunk)) if *last == channel => chunk.extend_from_slice(rest),
            _ => self.tail.push_back((channel, rest.to_vec())),
        }
        self.tail_bytes += rest.len();
        while self.tail_bytes > self.policy.tail_bytes {
            let excess = self.tail_bytes - self.policy.tail_bytes;
            let Some((_, front)) = self.tail.front_mut() else {
                break;
            };
            if front.len() <= excess {
                let dropped = front.len();
                self.tail.pop_front();
                self.tail_bytes -= dropped;
                self.omitted += dropped as u64;
            } else {
                front.drain(..excess);
                self.tail_bytes -= excess;
                self.omitted += excess as u64;
            }
        }
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.total
    }

    pub(crate) fn transcript(&self) -> OutputTranscript {
        let mut parts: Vec<TranscriptPart> = self
            .head
            .iter()
            .map(|(channel, chunk)| TranscriptPart::Output(*channel, chunk.clone()))
            .collect();
        if self.omitted > 0 {
            parts.push(TranscriptPart::Omitted(self.omitted));
        }
        parts.extend(
            self.tail
                .iter()
                .map(|(channel, chunk)| TranscriptPart::Output(*channel, chunk.clone())),
        );
        let (log, log_error) = match (&self.log, self.log_path()) {
            (LogState::Open(_), Some(path)) => (
                Some(OutputLog {
                    path: path.to_path_buf(),
                    bytes: self.log_bytes,
                    complete: true,
                }),
                None,
            ),
            (LogState::Full, Some(path)) => (
                Some(OutputLog {
                    path: path.to_path_buf(),
                    bytes: self.log_bytes,
                    complete: false,
                }),
                None,
            ),
            (LogState::Failed(error), _) => (None, Some(error.clone())),
            _ => (None, None),
        };
        OutputTranscript {
            parts,
            total_bytes: self.total,
            omitted_bytes: self.omitted,
            log,
            log_error,
        }
    }

    /// Offset where the in-memory tail starts; everything before it and
    /// after the head was dropped (unless nothing was dropped at all).
    fn retained_tail_start(&self) -> u64 {
        if self.omitted == 0 {
            self.head_bytes as u64
        } else {
            self.total - self.tail_bytes as u64
        }
    }

    fn log_readable_up_to(&self) -> u64 {
        match self.log {
            LogState::Open(_) | LogState::Full => self.log_bytes,
            _ => 0,
        }
    }

    /// The exact bytes `[from, to)` of the complete stream, from memory or
    /// the log, or `None` when any of them is no longer available.
    fn exact(&self, from: u64, to: u64) -> Option<Vec<u8>> {
        if from >= to {
            return Some(Vec::new());
        }
        let flatten = |chunks: &mut dyn Iterator<Item = &(OutputChannel, Vec<u8>)>| -> Vec<u8> {
            chunks
                .flat_map(|(_, chunk)| chunk.iter().copied())
                .collect()
        };
        if to <= self.head_bytes as u64 {
            let head = flatten(&mut self.head.iter());
            return Some(head[from as usize..to as usize].to_vec());
        }
        let tail_start = self.retained_tail_start();
        if self.omitted == 0 && from < tail_start {
            let all = flatten(&mut self.head.iter().chain(self.tail.iter()));
            return Some(all[from as usize..to as usize].to_vec());
        }
        if from >= tail_start {
            let tail = flatten(&mut self.tail.iter());
            return Some(tail[(from - tail_start) as usize..(to - tail_start) as usize].to_vec());
        }
        if to <= self.log_readable_up_to() {
            let path = self.log_path()?;
            let bytes = read_log_range(path, from, (to - from) as usize).ok()?;
            return (bytes.len() as u64 == to - from).then_some(bytes);
        }
        None
    }

    /// A window of the complete stream starting at `start`: the whole rest
    /// when it fits in `max` bytes, otherwise its first quarter and last
    /// three quarters. Bytes that are neither retained nor logged, or that
    /// did not fit, are counted in `skipped` between `head` and `tail`.
    pub(crate) fn read_window(&self, start: u64, max: usize) -> OutputWindow {
        let total = self.total;
        let start = start.min(total);
        let available = total - start;
        let max = max as u64;
        if available <= max
            && let Some(all) = self.exact(start, total)
        {
            return OutputWindow {
                start,
                head: all,
                skipped: 0,
                tail: Vec::new(),
                next: total,
            };
        }
        let (head_len, tail_len) = if available <= max {
            (available, 0)
        } else {
            (max / 4, max - max / 4)
        };
        let head_end = start + head_len;
        let head = self
            .exact(start, head_end)
            .or_else(|| {
                let prefix_end = head_end.min(self.head_bytes as u64);
                (start < prefix_end)
                    .then(|| self.exact(start, prefix_end))
                    .flatten()
            })
            .unwrap_or_default();
        let after_head = start + head.len() as u64;
        let wanted_from = if tail_len == 0 {
            after_head
        } else {
            (total - tail_len).max(after_head)
        };
        let tail = self
            .exact(wanted_from, total)
            .or_else(|| self.exact(self.retained_tail_start().max(wanted_from), total))
            .unwrap_or_default();
        OutputWindow {
            start,
            skipped: total - start - head.len() as u64 - tail.len() as u64,
            head,
            tail,
            next: total,
        }
    }
}

fn append_chunk(chunks: &mut Vec<(OutputChannel, Vec<u8>)>, channel: OutputChannel, bytes: &[u8]) {
    match chunks.last_mut() {
        Some((last, chunk)) if *last == channel => chunk.extend_from_slice(bytes),
        _ => chunks.push((channel, bytes.to_vec())),
    }
}

/// Create an empty owner-only log file and open it for appending.
fn create_private_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        crate::fs::ensure_private_directory(parent)?;
    }
    crate::fs::private_atomic_create_sync(path, b"")?;
    crate::fs::open_private_file_checked(path, crate::fs::PrivateFileAccess::Append)
}

/// Write `bytes` as a new private log file at `path`.
pub(crate) fn write_private_log(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = create_private_log(path)?;
    file.write_all(bytes)
}

/// Read up to `max` bytes of a private log starting at `offset`.
pub(crate) fn read_log_range(path: &Path, offset: u64, max: usize) -> std::io::Result<Vec<u8>> {
    let mut file = crate::fs::open_private_file_checked(path, crate::fs::PrivateFileAccess::Read)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(max.min(1024 * 1024));
    file.take(max as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Opaque, unique log file name for one command.
pub(crate) fn new_log_path(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!("{prefix}-{}.log", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(head: usize, tail: usize, hard: Option<u64>, log: LogPolicy) -> SpoolPolicy {
        SpoolPolicy {
            head_bytes: head,
            tail_bytes: tail,
            hard_limit: hard,
            log,
            log_limit: u64::MAX,
        }
    }

    #[test]
    fn spooled_capture_keeps_arrival_order_head_tail_and_counts_the_gap() {
        let mut capture = SpooledCapture::new(policy(4, 4, None, LogPolicy::Disabled));
        capture.push(OutputChannel::Stdout, b"ab").unwrap();
        capture.push(OutputChannel::Stderr, b"cd").unwrap();
        capture.push(OutputChannel::Stdout, b"efghij").unwrap();
        capture.push(OutputChannel::Stderr, b"kl").unwrap();
        let transcript = capture.transcript();
        assert_eq!(transcript.total_bytes, 12);
        assert_eq!(transcript.omitted_bytes, 4);
        assert_eq!(
            transcript.parts,
            vec![
                TranscriptPart::Output(OutputChannel::Stdout, b"ab".to_vec()),
                TranscriptPart::Output(OutputChannel::Stderr, b"cd".to_vec()),
                TranscriptPart::Omitted(4),
                TranscriptPart::Output(OutputChannel::Stdout, b"ij".to_vec()),
                TranscriptPart::Output(OutputChannel::Stderr, b"kl".to_vec()),
            ]
        );
        assert_eq!(
            transcript.channel_bytes(OutputChannel::Stderr),
            b"cd\n[... 4 bytes omitted ...]\nkl".to_vec()
        );
    }

    #[test]
    fn spooled_capture_stops_only_at_the_hard_ceiling() {
        let mut capture = SpooledCapture::new(policy(2, 2, Some(10), LogPolicy::Disabled));
        assert!(capture.push(OutputChannel::Stdout, b"12345678").is_ok());
        assert_eq!(
            capture.push(OutputChannel::Stdout, b"9abc"),
            Err(HardLimitReached)
        );
        assert_eq!(capture.transcript().total_bytes, 10);
    }

    #[test]
    fn spooled_capture_overflow_log_contains_every_byte_and_reads_by_offset() {
        let dir = crate::tests::support::temp::TempDir::new("spool-log");
        let path = dir.path().join("logs").join("out.log");
        let mut capture =
            SpooledCapture::new(policy(3, 3, None, LogPolicy::OnOverflow(path.clone())));
        capture.push(OutputChannel::Stdout, b"abc").unwrap();
        assert!(!path.exists(), "no log before anything is dropped");
        capture.push(OutputChannel::Stderr, b"def").unwrap();
        assert!(!path.exists());
        capture.push(OutputChannel::Stdout, b"ghijkl").unwrap();
        let transcript = capture.transcript();
        let log = transcript.log.expect("overflow creates the log");
        assert_eq!(log.bytes, 12);
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdefghijkl");
        // Middle bytes come from the log, tail bytes from memory.
        assert_eq!(capture.exact(4, 8), Some(b"efgh".to_vec()));
        assert_eq!(capture.read_window(9, 100).head, b"jkl".to_vec());
        let all = capture.read_window(0, 100);
        assert_eq!((all.head, all.skipped), (b"abcdefghijkl".to_vec(), 0));
        let window = capture.read_window(0, 8);
        assert_eq!(
            (window.head, window.skipped, window.tail, window.next),
            (b"ab".to_vec(), 4, b"ghijkl".to_vec(), 12)
        );
        assert_eq!(
            capture.read_window(12, 100),
            OutputWindow {
                start: 12,
                next: 12,
                ..OutputWindow::default()
            }
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "log must be owner-only");
        }
    }

    #[test]
    fn spooled_capture_without_log_cannot_serve_dropped_ranges() {
        let mut capture = SpooledCapture::new(policy(2, 2, None, LogPolicy::Disabled));
        capture.push(OutputChannel::Stdout, b"abcdefgh").unwrap();
        // The head prefix and the tail stay readable; the middle is skipped.
        let window = capture.read_window(0, 100);
        assert_eq!(
            (window.head, window.skipped, window.tail),
            (b"ab".to_vec(), 4, b"gh".to_vec())
        );
        assert_eq!(capture.exact(3, 5), None);
        assert_eq!(capture.read_window(6, 100).head, b"gh".to_vec());
    }

    #[test]
    fn spooled_capture_log_limit_marks_the_log_incomplete() {
        let dir = crate::tests::support::temp::TempDir::new("spool-limit");
        let path = dir.path().join("job.log");
        let mut capture = SpooledCapture::new(SpoolPolicy {
            log_limit: 5,
            ..policy(2, 2, None, LogPolicy::Always(path.clone()))
        });
        assert!(path.exists(), "Always creates the log up front");
        capture.push(OutputChannel::Merged, b"abcdefgh").unwrap();
        let log = capture.transcript().log.unwrap();
        assert!(!log.complete);
        assert_eq!(std::fs::read(&path).unwrap(), b"abcde");
    }
}
