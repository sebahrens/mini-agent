//! Bounded discovery metadata for workspace-scoped interactive session selection.
//! Stored workspace text labels history; it is never opened or resolved.

use std::io::{BufReader, Read};
use std::path::{Component, Path};

use compact_str::CompactString;
use serde::{Deserialize, Deserializer};

use super::{SESSION_TITLE_CHARS, Session};

pub(crate) const MAX_ENTRIES: usize = 2_048;
pub(crate) const MAX_ROWS: usize = 100;
pub(crate) const MAX_FILE_BYTES: u64 = super::storage::MAX_SESSION_BYTES as u64;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ScanLimits {
    file_bytes: u64,
    total_bytes: u64,
}

const SCAN_LIMITS: ScanLimits = ScanLimits {
    file_bytes: MAX_FILE_BYTES,
    total_bytes: MAX_TOTAL_BYTES,
};
const MAX_MESSAGES: usize = 100_000;

#[derive(Clone, Debug)]
pub(crate) struct SessionRow {
    pub id: CompactString,
    pub title: CompactString,
    pub updated_at: CompactString,
    pub working_dir: CompactString,
    pub message_count: usize,
    pub provider: CompactString,
    pub model: CompactString,
    pub current_workspace: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ResumeSelection {
    pub row: SessionRow,
    pub all: bool,
    pub workspace: std::sync::Arc<crate::paths::WorkspaceBinding>,
}

#[derive(Default)]
pub(crate) struct Catalogue {
    pub rows: Vec<SessionRow>,
    pub truncated: bool,
    pub skipped: usize,
}

#[derive(Default, Deserialize)]
struct MessagePeek {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: String,
}

#[derive(Default)]
struct MessageSummary {
    count: usize,
    first_user: String,
}

fn messages<'de, D: Deserializer<'de>>(deserializer: D) -> Result<MessageSummary, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = MessageSummary;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a bounded session message array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut summary = MessageSummary::default();
            while let Some(message) = sequence.next_element::<MessagePeek>()? {
                summary.count += 1;
                if summary.count > MAX_MESSAGES {
                    return Err(serde::de::Error::custom(
                        "session message count exceeds catalogue budget",
                    ));
                }
                if summary.first_user.is_empty()
                    && message.role == "user"
                    && !message.content.trim().is_empty()
                {
                    summary.first_user = short_text(&message.content);
                }
            }
            Ok(summary)
        }
    }
    deserializer.deserialize_seq(Visitor)
}

#[derive(Default, Deserialize)]
struct Header {
    id: CompactString,
    #[serde(default)]
    name: String,
    #[serde(default)]
    generated_title: Option<String>,
    #[serde(default)]
    updated_at: CompactString,
    #[serde(default)]
    working_dir: CompactString,
    #[serde(default)]
    provider: CompactString,
    #[serde(default)]
    model: CompactString,
    #[serde(default, deserialize_with = "messages")]
    messages: MessageSummary,
}

// serde_json's reader deserializer buffers a complete string before calling a
// visitor. Project strings first so a multi-megabyte message cannot become its
// scratch buffer. The projection validates the *entire* original token, even
// its discarded suffix; serde then validates the complete JSON grammar. Exact
// metadata is never truncated into a different identity/workspace label.
const MAX_PREVIEW_CHARS: usize = 4_096;
const MAX_METADATA_BYTES: usize = 64 * 1024;

struct PreviewReader<R> {
    source: BufReader<R>,
    pending: Vec<u8>,
    position: usize,
    previous: Option<u8>,
    exact_key: bool,
    messages_key: bool,
    depth: usize,
    messages_depth: Option<usize>,
}

impl<R: Read> PreviewReader<R> {
    fn new(source: R) -> Self {
        Self {
            source: BufReader::new(source),
            pending: Vec::new(),
            position: 0,
            previous: None,
            exact_key: false,
            messages_key: false,
            depth: 0,
            messages_depth: None,
        }
    }

    fn byte(&mut self) -> std::io::Result<Option<u8>> {
        let mut byte = [0];
        Ok((self.source.read(&mut byte)? != 0).then_some(byte[0]))
    }

    fn required(&mut self) -> std::io::Result<u8> {
        self.byte()?
            .ok_or_else(|| Self::invalid("unterminated JSON string"))
    }

    fn invalid(message: &'static str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, message)
    }

    fn hex_quad(&mut self) -> std::io::Result<u16> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = (self.required()? as char)
                .to_digit(16)
                .ok_or_else(|| Self::invalid("invalid JSON Unicode escape"))?;
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }

    fn escaped(&mut self) -> std::io::Result<char> {
        Ok(match self.required()? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let first = self.hex_quad()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if self.required()? != b'\\' || self.required()? != b'u' {
                        return Err(Self::invalid("invalid JSON surrogate pair"));
                    }
                    let second = self.hex_quad()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(Self::invalid("invalid JSON surrogate pair"));
                    }
                    0x10000 + ((first as u32 - 0xd800) << 10) + second as u32 - 0xdc00
                } else {
                    first as u32
                };
                char::from_u32(scalar)
                    .ok_or_else(|| Self::invalid("invalid JSON Unicode scalar"))?
            }
            _ => return Err(Self::invalid("invalid JSON escape")),
        })
    }

    fn string(&mut self) -> std::io::Result<String> {
        let exact = self.previous == Some(b':') && self.exact_key;
        let trim_start = self.previous == Some(b':') && !exact;
        let mut text = String::new();
        let mut chars = 0;
        loop {
            let byte = self.required()?;
            let ch = match byte {
                b'"' => break,
                b'\\' => self.escaped()?,
                0..=0x1f => return Err(Self::invalid("control byte in JSON string")),
                0x20..=0x7f => byte as char,
                _ => {
                    let width = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(Self::invalid("invalid JSON UTF-8")),
                    };
                    let mut bytes = [byte, 0, 0, 0];
                    for byte in &mut bytes[1..width] {
                        *byte = self.required()?;
                    }
                    std::str::from_utf8(&bytes[..width])
                        .map_err(|_| Self::invalid("invalid JSON UTF-8"))?
                        .chars()
                        .next()
                        .expect("a validated scalar")
                }
            };
            if trim_start && chars == 0 && ch.is_whitespace() {
                continue;
            }
            if exact {
                if text.len() + ch.len_utf8() > MAX_METADATA_BYTES {
                    return Err(Self::invalid("session catalogue metadata exceeds budget"));
                }
                text.push(ch);
            } else if chars < MAX_PREVIEW_CHARS {
                text.push(ch);
                chars += 1;
            }
        }
        Ok(text)
    }
}

impl<R: Read> Read for PreviewReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let mut written = 0;
        while written < output.len() {
            if self.position < self.pending.len() {
                let count = (output.len() - written).min(self.pending.len() - self.position);
                output[written..written + count]
                    .copy_from_slice(&self.pending[self.position..self.position + count]);
                self.position += count;
                written += count;
                continue;
            }
            self.pending.clear();
            self.position = 0;
            let Some(byte) = self.byte()? else { break };
            if byte == b'"' {
                let text = self.string()?;
                self.exact_key = if self.depth == 1 {
                    matches!(
                        text.as_str(),
                        "id" | "updated_at" | "working_dir" | "provider" | "model"
                    )
                } else {
                    self.messages_depth
                        .is_some_and(|depth| self.depth == depth + 1)
                        && text == "role"
                };
                self.messages_key = self.depth == 1 && text == "messages";
                serde_json::to_writer(&mut self.pending, &text).map_err(std::io::Error::other)?;
                self.previous = Some(b'"');
            } else {
                output[written] = byte;
                written += 1;
                match byte {
                    b'[' | b'{' => {
                        self.depth = self.depth.saturating_add(1);
                        if byte == b'[' && self.previous == Some(b':') && self.messages_key {
                            self.messages_depth = Some(self.depth);
                        }
                    }
                    b']' | b'}' => {
                        if byte == b']' && self.messages_depth == Some(self.depth) {
                            self.messages_depth = None;
                        }
                        self.depth = self.depth.saturating_sub(1);
                    }
                    _ => {}
                }
                if !byte.is_ascii_whitespace() {
                    self.previous = Some(byte);
                }
            }
        }
        Ok(written)
    }
}

fn read_header(reader: impl Read) -> serde_json::Result<Header> {
    serde_json::from_reader(BufReader::new(PreviewReader::new(reader)))
}

fn short_text(value: &str) -> String {
    let sanitized = crate::ui::events::sanitize_output(value);
    let text = sanitized
        .split_whitespace()
        .take(SESSION_TITLE_CHARS)
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = text.chars();
    let result: String = chars.by_ref().take(SESSION_TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

pub(crate) fn same_workspace(saved: &str, workspace: &crate::paths::WorkspaceBinding) -> bool {
    let saved = Path::new(saved);
    saved.is_absolute()
        && !saved
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        && saved == workspace.root()
}

fn open_catalogue_file(path: &Path) -> anyhow::Result<std::fs::File> {
    let file = crate::fs::open_private_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            file.metadata()?.nlink() == 1,
            "session catalogue rejects hard links"
        );
    }
    #[cfg(windows)]
    anyhow::ensure!(
        crate::fs::windows_file_link_count(&file)? == 1,
        "session catalogue rejects hard links"
    );
    Ok(file)
}

pub(crate) fn list(
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
) -> anyhow::Result<Catalogue> {
    list_mode(workspace, all, false)
}

/// Explicit metadata view with no creation or mode/ACL repair.
pub(crate) fn list_readonly(
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
) -> anyhow::Result<Catalogue> {
    list_mode(workspace, all, true)
}

fn list_mode(
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
    readonly: bool,
) -> anyhow::Result<Catalogue> {
    list_with_limits(workspace, all, readonly, SCAN_LIMITS)
}

fn list_with_limits(
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
    readonly: bool,
    limits: ScanLimits,
) -> anyhow::Result<Catalogue> {
    workspace.validate().map_err(anyhow::Error::msg)?;
    let mut catalogue = Catalogue::default();
    if crate::paths::artifact_disabled("sessions") {
        return Ok(catalogue);
    }
    let directory = if readonly {
        let directory = crate::paths::process_paths_ref()
            .expect("startup captures paths")
            .sessions_dir();
        match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "unsafe session catalogue directory"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(catalogue),
            Err(error) => return Err(error.into()),
        }
        directory
    } else {
        let Some(directory) = super::storage::existing_session_dir()? else {
            return Ok(catalogue);
        };
        directory
    };
    let retained = crate::paths::WorkspaceBinding::capture(&directory)?;
    if readonly {
        retained.validate_private_root()?;
    }
    let mut entries = Vec::new();
    for (index, entry) in std::fs::read_dir(&directory)?.enumerate() {
        if index >= MAX_ENTRIES {
            catalogue.truncated = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                catalogue.skipped += 1;
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let modified = std::fs::symlink_metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        entries.push((modified, path));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut total = 0_u64;
    for (_, path) in entries {
        retained.validate().map_err(anyhow::Error::msg)?;
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            catalogue.skipped += 1;
            continue;
        };
        if uuid::Uuid::try_parse(stem).is_err() {
            catalogue.skipped += 1;
            continue;
        }
        let candidate = (|| -> anyhow::Result<Header> {
            let relative = std::path::PathBuf::from(format!("{stem}.json"));
            let mut file = if readonly {
                let file = retained.open_regular_relative(&relative)?;
                crate::fs::validate_private_file(&file)?;
                file
            } else {
                open_catalogue_file(&path)?
            };
            let identity = readonly
                .then(|| crate::fs::checked_file_metadata(&file))
                .transpose()?;
            retained.validate().map_err(anyhow::Error::msg)?;
            let length = file.metadata()?.len();
            anyhow::ensure!(
                length <= limits.file_bytes,
                "oversized session catalogue entry"
            );
            if total.saturating_add(length) > limits.total_bytes {
                catalogue.truncated = true;
                anyhow::bail!("catalogue byte budget exhausted");
            }
            let remaining = limits.total_bytes - total;
            let cap = remaining.min(limits.file_bytes) + 1;
            let mut input = file.by_ref().take(cap);
            let header = read_header(&mut input);
            // Charge malformed records too, and detect growth with one byte
            // beyond the remaining budget. Neither path retains file bytes.
            std::io::copy(&mut input, &mut std::io::sink())?;
            let consumed = cap - input.limit();
            total += consumed;
            if consumed > remaining {
                catalogue.truncated = true;
                anyhow::bail!("catalogue byte budget exhausted");
            }
            if let Some(identity) = identity {
                crate::fs::validate_private_file(&file)?;
                let current = retained.open_regular_relative(&relative)?;
                crate::fs::validate_private_file(&current)?;
                crate::fs::ensure_same_file(
                    &path,
                    &identity,
                    &crate::fs::checked_file_metadata(&current)?,
                )?;
                retained.validate_private_root()?;
            }
            anyhow::ensure!(
                consumed <= limits.file_bytes,
                "session grew beyond catalogue budget"
            );
            let header = header?;
            anyhow::ensure!(header.id == stem, "session filename/id mismatch");
            Ok(header)
        })();
        let header = match candidate {
            Ok(header) => header,
            Err(_) => {
                catalogue.skipped += 1;
                if catalogue.truncated {
                    break;
                }
                continue;
            }
        };
        let current = same_workspace(&header.working_dir, workspace);
        if !all && !current {
            continue;
        }
        let title = if !header.name.trim().is_empty() {
            format!("[{}]", short_text(&header.name))
        } else {
            header
                .generated_title
                .as_deref()
                .map(short_text)
                .filter(|title| !title.is_empty())
                .unwrap_or(header.messages.first_user)
        };
        catalogue.rows.push(SessionRow {
            id: header.id,
            title: title.into(),
            updated_at: header.updated_at,
            working_dir: header.working_dir,
            message_count: header.messages.count,
            provider: header.provider,
            model: header.model,
            current_workspace: current,
        });
        if catalogue.rows.len() == MAX_ROWS {
            catalogue.truncated = true;
            break;
        }
    }
    retained.validate().map_err(anyhow::Error::msg)?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    Ok(catalogue)
}

/// An explicitly selected private session can transfer visible history across
/// workspaces, but its old approvals and concealed redo never follow it.
pub(crate) fn for_workspace(
    session: Session,
    workspace: &crate::paths::WorkspaceBinding,
) -> anyhow::Result<Session> {
    workspace.validate().map_err(anyhow::Error::msg)?;
    let mut session = if same_workspace(&session.working_dir, workspace) {
        session
    } else {
        let mut fork = super::lock::fork_session(&session)?;
        fork.permission_allowlist.clear();
        fork.rewind_undo = None;
        fork.retain_visible_replay_metadata();
        fork
    };
    session.working_dir = workspace.root().to_string_lossy().into();
    Ok(session)
}

/// Reload the exact private record, then decide scope again. A changed row
/// cannot silently turn a workspace-filtered selection into a foreign resume.
pub(crate) fn selected(
    row: &SessionRow,
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
) -> anyhow::Result<Session> {
    selected_with_limit(row, workspace, all, MAX_FILE_BYTES)
}

fn selected_with_limit(
    row: &SessionRow,
    workspace: &crate::paths::WorkspaceBinding,
    all: bool,
    max_file_bytes: u64,
) -> anyhow::Result<Session> {
    workspace.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        uuid::Uuid::try_parse(&row.id).is_ok(),
        "invalid selected session identity"
    );
    let directory = super::storage::existing_session_dir()?
        .ok_or_else(|| anyhow::anyhow!("selected session disappeared"))?;
    let retained = crate::paths::WorkspaceBinding::capture(&directory)?;
    let mut file = open_catalogue_file(&directory.join(format!("{}.json", row.id)))?;
    retained.validate().map_err(anyhow::Error::msg)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_file_bytes + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= max_file_bytes,
        "selected session exceeds reload budget"
    );
    let session: Session = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        session.messages.len() <= MAX_MESSAGES,
        "selected session message count exceeds reload budget"
    );
    retained.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(session.id == row.id, "selected session identity changed");
    anyhow::ensure!(
        session.working_dir == row.working_dir,
        "selected session workspace changed; refresh the picker"
    );
    let current = same_workspace(&session.working_dir, workspace);
    anyhow::ensure!(
        all || current,
        "selected session belongs to another workspace"
    );
    let session = for_workspace(session, workspace)?;
    workspace.validate().map_err(anyhow::Error::msg)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::MessageRole;
    fn environment(root: &crate::tests::OwnedTestDirectory) -> crate::tests::ScopedProcessEnv {
        crate::tests::ScopedProcessEnv::set(&[
            (
                "MINI_AGENT_STATE_DIR",
                Some(root.path().join("state").into_os_string()),
            ),
            (
                "MINI_AGENT_CONFIG_DIR",
                Some(root.path().join("config").into_os_string()),
            ),
            (
                "MINI_AGENT_DATA_DIR",
                Some(root.path().join("data").into_os_string()),
            ),
            (
                "MINI_AGENT_CACHE_DIR",
                Some(root.path().join("cache").into_os_string()),
            ),
        ])
    }
    fn stored(workspace: &Path, title: &str) -> Session {
        let mut session = Session::new("anthropic", "test", 128_000, "");
        session.working_dir = workspace.to_string_lossy().into();
        session.add_message(MessageRole::User, title);
        session.add_message(MessageRole::Assistant, "first answer");
        let file = super::super::storage::session_path(&session.id).unwrap();
        super::super::storage::atomic_write(&file, &serde_json::to_string(&session).unwrap())
            .unwrap();
        session
    }

    #[test]
    fn catalogue_and_storage_share_fixed_session_byte_limit() {
        assert_eq!(
            MAX_FILE_BYTES,
            super::super::storage::MAX_SESSION_BYTES as u64
        );
        assert_eq!(MAX_FILE_BYTES, 32 * 1024 * 1024);
        assert_eq!(MAX_TOTAL_BYTES, 256 * 1024 * 1024);
    }

    #[test]
    fn catalogue_streams_large_messages_and_unknown_fields_with_bounded_allocations() {
        struct CountReader<R> {
            source: R,
            bytes: usize,
            max_request: usize,
        }
        impl<R: Read> Read for CountReader<R> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.max_request = self.max_request.max(buffer.len());
                let read = self.source.read(buffer)?;
                self.bytes += read;
                Ok(read)
            }
        }
        // Independent virtual documents exercise repeated discovery without
        // allocating or writing ninety MiB of fixture payloads to disk.
        for index in 0..3 {
            let id = format!("fixture-{index}");
            let prefix =
                format!(r#"{{"id":"{id}","messages":[{{"content":"Preview \u96ea \ud83d\ude00 "#);
            let middle =
                br#"","role":"user"},{"content":"answer","role":"assistant"}],"padding":{"model":""#;
            let suffix = br#""},"working_dir":"/late/workspace","model":"late-model","provider":"late-provider"}"#;
            let size = 15 * 1024 * 1024;
            let source = prefix
                .as_bytes()
                .chain(std::io::repeat(b'x').take(size))
                .chain(middle.as_slice())
                .chain(std::io::repeat(b'y').take(size))
                .chain(suffix.as_slice());
            let mut source = CountReader {
                source,
                bytes: 0,
                max_request: 0,
            };
            let (header, _, allocated) =
                crate::tests::alloc_counter::measure(|| read_header(&mut source).unwrap());
            assert_eq!(header.id, id);
            assert_eq!(header.working_dir, "/late/workspace");
            assert_eq!(header.model, "late-model");
            assert_eq!(header.provider, "late-provider");
            assert_eq!(header.messages.count, 2);
            assert!(header.messages.first_user.starts_with("Preview 雪 😀 "));
            assert_eq!(
                source.bytes,
                prefix.len() + middle.len() + suffix.len() + 2 * size as usize
            );
            assert!(source.bytes > 30 * 1024 * 1024);
            assert!((source.bytes as u64) < MAX_FILE_BYTES);
            assert!(
                source.max_request <= 8 * 1024,
                "document {index} read request: {}",
                source.max_request
            );
            assert!(
                allocated < 512 * 1024,
                "document {index} allocated {allocated} bytes for streamed metadata"
            );
        }
    }

    #[test]
    fn catalogue_validates_discarded_string_suffixes_and_exact_metadata() {
        let mut valid = br#"{"id":"fixture","messages":[{"content":""#.to_vec();
        valid.extend(std::iter::repeat_n(b'x', MAX_PREVIEW_CHARS + 10));
        valid.extend_from_slice(br#"\u96ea\ud83d\ude00","role":"user"}],"working_dir":"/late"}"#);
        let header = read_header(valid.as_slice()).unwrap();
        assert_eq!(header.messages.count, 1);
        assert_eq!(header.working_dir, "/late");
        for tail in [br#"\q"#.as_slice(), br#"\ud800"#, &[0xff], b"\n"] {
            let mut invalid = br#"{"id":"fixture","messages":[{"content":""#.to_vec();
            invalid.extend(std::iter::repeat_n(b'x', MAX_PREVIEW_CHARS + 10));
            invalid.extend_from_slice(tail);
            invalid.extend_from_slice(br#"","role":"user"}]}"#);
            assert!(read_header(invalid.as_slice()).is_err());
        }
        assert!(read_header(br#"{"id":"fixture","messages":[]}"#.as_slice()).is_ok());
        assert!(read_header(br#"{"id":"fixture","messages":[]} trailing"#.as_slice()).is_err());
        let oversized_metadata = format!(
            r#"{{"id":"fixture","working_dir":"{}"}}"#,
            "x".repeat(MAX_METADATA_BYTES + 1)
        );
        assert!(read_header(oversized_metadata.as_bytes()).is_err());
        // Truncation must never turn an unknown key into a recognized field,
        // nor trim leading spaces from exact metadata or key names.
        let header = read_header(
            br#"{"id":"fixture"," working_dir":"ignored","working_dir":" /exact "}"#.as_slice(),
        )
        .unwrap();
        assert_eq!(header.working_dir, " /exact ");
    }

    #[test]
    fn catalogue_roundtrips_exact_saved_session_above_previous_eight_mib_limit() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-large-roundtrip");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let mut session = Session::new("provider", "model", 32_000, "Large roundtrip");
        session.working_dir = root.path().to_string_lossy().into();
        let content = format!("Large retained message 雪 {}", "x".repeat(9 * 1024 * 1024));
        session.add_message(MessageRole::User, &content);
        super::super::storage::save_session(&session).unwrap();
        let path = super::super::storage::session_path(&session.id).unwrap();
        let length = std::fs::metadata(&path).unwrap().len();
        assert!(length > 8 * 1024 * 1024 && length < MAX_FILE_BYTES);
        let catalogue = list_readonly(&binding, false).unwrap();
        assert_eq!(catalogue.rows.len(), 1);
        assert_eq!(catalogue.skipped, 0);
        assert!(!catalogue.truncated);
        let row = &catalogue.rows[0];
        assert_eq!(row.id, session.id);
        assert_eq!(row.title, "[Large roundtrip]");
        assert_eq!(row.message_count, 1);
        let loaded = selected(row, &binding, false).unwrap();
        assert_eq!(loaded.messages[0].content, content);
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        let loaded = super::super::storage::load_session_exact(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        let sessions = crate::paths::WorkspaceBinding::capture(path.parent().unwrap()).unwrap();
        let imported =
            super::super::storage::load_session_exact_read_only(&sessions, &session.id, &binding)
                .unwrap()
                .unwrap();
        assert_eq!(imported.messages[0].content, content);
    }

    #[cfg(unix)]
    #[test]
    fn busy67_readonly_catalogue_never_creates_or_repairs_private_storage() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::tests::OwnedTestDirectory::new("readonly-catalogue67");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let directory = crate::paths::process_paths_ref().unwrap().sessions_dir();
        assert!(list_readonly(&binding, false).unwrap().rows.is_empty());
        assert!(!directory.exists());
        let session = stored(root.path(), "Saved bounded title");
        assert_eq!(
            list_readonly(&binding, false).unwrap().rows[0].id,
            session.id
        );
        let path = super::super::storage::session_path(&session.id).unwrap();
        let contents = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let rejected = list_readonly(&binding, false).unwrap();
        assert!(rejected.rows.is_empty() && rejected.skipped == 1);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(list_readonly(&binding, false).is_err());
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    #[test]
    fn catalogue_filters_workspace_and_all_selection_forks_foreign_trust() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-scope");
        let _env = environment(&root);
        let here = root.path().join("here");
        let there = root.path().join("there");
        std::fs::create_dir(&here).unwrap();
        std::fs::create_dir(&there).unwrap();
        let binding = crate::paths::WorkspaceBinding::capture(&here).unwrap();
        let current = stored(&here, "Current 雪");
        let mut foreign = stored(&there, "Foreign 雪");
        foreign.permission_allowlist = vec![crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "*".into(),
        }];
        foreign.add_message(MessageRole::User, "concealed");
        foreign.rewind_to(2);
        super::super::storage::atomic_write(
            &super::super::storage::session_path(&foreign.id).unwrap(),
            &serde_json::to_string(&foreign).unwrap(),
        )
        .unwrap();
        let scoped = list(&binding, false).unwrap();
        assert_eq!(scoped.rows.len(), 1);
        assert_eq!(scoped.rows[0].id, current.id);
        assert_eq!(scoped.rows[0].title, "Current 雪");
        assert_eq!(scoped.rows[0].message_count, 2);
        let all = list(&binding, true).unwrap();
        assert_eq!(all.rows.len(), 2);
        let row = all.rows.iter().find(|row| row.id == foreign.id).unwrap();
        assert!(!row.current_workspace);
        assert!(selected(row, &binding, false).is_err());
        let fork = selected(row, &binding, true).unwrap();
        assert_ne!(fork.id, foreign.id);
        assert_eq!(fork.messages.len(), 2);
        assert!(fork.permission_allowlist.is_empty());
        assert!(fork.rewind_undo.is_none());
        assert_eq!(fork.working_dir, current.working_dir);
        let loaded = selected(&scoped.rows[0], &binding, false).unwrap();
        assert_eq!(loaded.id, current.id);
    }
    #[test]
    fn catalogue_reloads_private_record_and_rejects_workspace_changes_and_large_files() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-reload");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let mut session = stored(root.path(), "fresh");
        let row = list(&binding, false).unwrap().rows.remove(0);
        session.working_dir = "/other/workspace".into();
        let path = super::super::storage::session_path(&session.id).unwrap();
        super::super::storage::atomic_write(&path, &serde_json::to_string(&session).unwrap())
            .unwrap();
        assert!(
            selected(&row, &binding, true)
                .unwrap_err()
                .to_string()
                .contains("workspace changed")
        );
        super::super::storage::atomic_write(&path, &"x".repeat(1_025)).unwrap();
        assert!(
            selected_with_limit(&row, &binding, true, 1_024)
                .unwrap_err()
                .to_string()
                .contains("reload budget")
        );
        let catalogue = list_with_limits(
            &binding,
            true,
            false,
            ScanLimits {
                file_bytes: 1_024,
                total_bytes: 8_192,
            },
        )
        .unwrap();
        assert!(catalogue.rows.is_empty());
        assert_eq!(catalogue.skipped, 1);
    }
    #[test]
    fn catalogue_limits_rows_sanitizes_titles_and_checks_filename_identity() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-budget");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        for _ in 0..MAX_ROWS + 1 {
            stored(root.path(), "label\u{1b}[31m雪\nsecond");
        }
        let catalogue = list(&binding, false).unwrap();
        assert_eq!(catalogue.rows.len(), MAX_ROWS);
        assert!(catalogue.truncated);
        assert!(
            catalogue
                .rows
                .iter()
                .all(|row| !row.title.contains('\u{1b}') && !row.title.contains('\n'))
        );
        let session = stored(root.path(), "id mismatch");
        let file = super::super::storage::session_path(&session.id).unwrap();
        let mut value = serde_json::to_value(&session).unwrap();
        value["id"] = uuid::Uuid::new_v4().to_string().into();
        super::super::storage::atomic_write(&file, &value.to_string()).unwrap();
        let mut row = SessionRow {
            id: session.id,
            title: "".into(),
            updated_at: "".into(),
            working_dir: session.working_dir,
            message_count: 2,
            provider: "".into(),
            model: "".into(),
            current_workspace: true,
        };
        assert!(
            selected(&row, &binding, false)
                .unwrap_err()
                .to_string()
                .contains("identity changed")
        );
        row.id = "../escape".into();
        assert!(selected(&row, &binding, true).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn catalogue_never_follows_symlink_or_hardlink_session_files() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-nofollow");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let session = stored(root.path(), "private target");
        let path = super::super::storage::session_path(&session.id).unwrap();
        let outside = root.path().join("outside.json");
        std::fs::rename(&path, &outside).unwrap();
        symlink(&outside, &path).unwrap();
        assert!(list(&binding, true).unwrap().rows.is_empty());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert!(list(&binding, true).unwrap().rows.is_empty());
    }
    #[test]
    fn catalogue_total_bytes_and_directory_entry_limits_stop_discovery() {
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-total-budget");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        let directory = super::super::storage::session_path(&uuid::Uuid::new_v4().to_string())
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        crate::paths::ensure_private_directory(&directory).unwrap();
        for _ in 0..9 {
            let id = uuid::Uuid::new_v4().to_string();
            let header=serde_json::json!({"id":id,"working_dir":root.path().to_string_lossy(),"messages":[]}).to_string();
            let mut payload = header.trim_end_matches('}').to_owned();
            payload.push_str(",\"padding\":\"");
            let room = 1_024 - payload.len() - 2;
            payload.extend(std::iter::repeat_n('x', room));
            payload.push_str("\"}");
            assert_eq!(payload.len(), 1_024);
            super::super::storage::atomic_write(&directory.join(format!("{id}.json")), &payload)
                .unwrap();
        }
        let result = list_with_limits(
            &binding,
            true,
            false,
            ScanLimits {
                file_bytes: 1_024,
                total_bytes: 8_192,
            },
        )
        .unwrap();
        assert_eq!(result.rows.len(), 8);
        assert!(result.truncated);
        for entry in std::fs::read_dir(&directory).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for index in 0..MAX_ENTRIES + 1 {
            std::fs::write(directory.join(format!("ignored-{index}.not-json")), []).unwrap();
        }
        let result = list(&binding, true).unwrap();
        assert!(result.rows.is_empty());
        assert!(result.truncated);
    }
    #[cfg(unix)]
    #[test]
    fn catalogue_selected_rejects_replaced_captured_workspace_before_private_reload() {
        use std::os::unix::fs::symlink;
        let root = crate::tests::OwnedTestDirectory::new("session-catalogue-replaced-root");
        let parked = crate::tests::OwnedTestDirectory::new("session-catalogue-parked-root");
        let _env = environment(&root);
        let binding = crate::paths::WorkspaceBinding::capture(root.path()).unwrap();
        stored(root.path(), "old root");
        let row = list(&binding, false).unwrap().rows.remove(0);
        std::fs::rename(root.path(), parked.path().join("original")).unwrap();
        symlink(parked.path(), root.path()).unwrap();
        assert!(selected(&row, &binding, true).is_err());
        assert!(list(&binding, true).is_err());
        std::fs::remove_file(root.path()).unwrap();
    }
}
