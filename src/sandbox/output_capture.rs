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
use std::io::Write;
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
    #[cfg_attr(not(test), allow(dead_code))]
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
impl SpoolPolicy {
    pub(crate) fn foreground_shell(log: Option<PathBuf>) -> Self {
        Self {
            head_bytes: SHELL_HEAD_BYTES,
            tail_bytes: SHELL_TAIL_BYTES,
            hard_limit: Some(SHELL_HARD_OUTPUT_BYTES),
            log: log.map_or(LogPolicy::Disabled, LogPolicy::OnOverflow),
            log_limit: SHELL_HARD_OUTPUT_BYTES,
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "log must be owner-only");
        }
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
