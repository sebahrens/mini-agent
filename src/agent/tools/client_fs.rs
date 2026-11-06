//! Seam that lets the read, edit and write tools use an ACP client's own
//! `fs/read_text_file` and `fs/write_text_file` methods.
//!
//! The seam is closed by default: a [`ReadTracker`](super::ReadTracker) carries
//! no handle unless the ACP server was both opted in (`acp_client_file_access`)
//! and the connected client advertised the capability. A handle only changes
//! where the bytes of an already-authorised, workspace-bound file come from and
//! go to. Every workspace, symlink and permission check still runs first on the
//! on-disk path, so a client can never widen what a tool may touch; it can only
//! supply (and receive) the text of files the tool was already allowed to open.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// Largest text one client transfer may carry, in either direction. Matches
/// the editor extension's own cap so both ends agree on what is refused.
pub(crate) const MAX_CLIENT_TRANSFER_BYTES: usize = 2 * 1024 * 1024;

pub(crate) type ClientFsFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// The client's file methods, keyed by absolute path.
pub(crate) trait ClientFileAccess: Send + Sync + std::fmt::Debug {
    /// Whether the client advertised `fs.readTextFile`.
    fn can_read(&self) -> bool;
    /// Whether the client advertised `fs.writeTextFile`.
    fn can_write(&self) -> bool;
    /// The whole current text of `path`, unsaved editor changes included.
    fn read_text<'a>(&'a self, path: &'a Path) -> ClientFsFuture<'a, String>;
    /// Replace the whole text of `path` through the editor.
    fn write_text<'a>(&'a self, path: &'a Path, content: &'a str) -> ClientFsFuture<'a, ()>;
}

pub(crate) type ClientFs = Arc<dyn ClientFileAccess>;

/// Absolute path the client is asked about for a workspace-bound target.
pub(crate) fn client_path(workspace: &crate::paths::WorkspaceBinding, relative: &Path) -> PathBuf {
    workspace.root().join(relative)
}

/// The client's text for `path`, or `None` when the client refuses, fails, or
/// returns more than [`MAX_CLIENT_TRANSFER_BYTES`]. `None` always means "use
/// the on-disk file", so a failing client degrades to the ordinary behaviour.
pub(crate) async fn read_client_text(client: &ClientFs, path: &Path) -> Option<String> {
    if !client.can_read() {
        return None;
    }
    match client.read_text(path).await {
        Ok(text) if text.len() <= MAX_CLIENT_TRANSFER_BYTES => Some(text),
        Ok(text) => {
            tracing::debug!(
                bytes = text.len(),
                "client fs read exceeded the transfer cap; using the on-disk file"
            );
            None
        }
        Err(error) => {
            tracing::debug!(%error, "client fs read failed; using the on-disk file");
            None
        }
    }
}

/// Publish `content` through the client after confirming the client still
/// holds exactly `expected` (the text the change was computed from). A buffer
/// edited in the meantime is never overwritten with a stale computation.
pub(crate) async fn write_client_text(
    client: &ClientFs,
    path: &Path,
    expected: &str,
    content: &str,
) -> Result<(), String> {
    if !client.can_write() {
        return Err("the client does not support fs/write_text_file".to_string());
    }
    if content.len() > MAX_CLIENT_TRANSFER_BYTES {
        return Err(format!(
            "the result is {} bytes, above the {MAX_CLIENT_TRANSFER_BYTES}-byte client transfer limit",
            content.len()
        ));
    }
    let current = client
        .read_text(path)
        .await
        .map_err(|error| format!("could not re-read the editor text before writing: {error}"))?;
    if current != expected {
        return Err(
            "the file changed in the editor after it was read; no changes were applied. Re-read it and retry"
                .to_string(),
        );
    }
    client
        .write_text(path, content)
        .await
        .map_err(|error| format!("the editor refused the write: {error}"))
}

/// A usable handle for a tool that both reads and rewrites a file.
pub(crate) fn read_write_client(client: Option<&ClientFs>) -> Option<&ClientFs> {
    client.filter(|client| client.can_read() && client.can_write())
}

#[cfg(test)]
pub(crate) mod fake {
    //! In-memory client used by the tool tests.
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    pub(crate) struct FakeClient {
        pub(crate) files: Mutex<HashMap<PathBuf, String>>,
        pub(crate) reads: Mutex<Vec<PathBuf>>,
        pub(crate) writes: Mutex<Vec<PathBuf>>,
        pub(crate) fail_writes: std::sync::atomic::AtomicBool,
        pub(crate) fail_reads: std::sync::atomic::AtomicBool,
        pub(crate) read_only: bool,
        /// When set, every read after the first returns this text, as if the
        /// user kept typing between the agent's read and its write.
        pub(crate) typed_meanwhile: Mutex<Option<String>>,
    }

    impl FakeClient {
        pub(crate) fn with(path: PathBuf, text: &str) -> Arc<Self> {
            let client = Self::default();
            client.files.lock().unwrap().insert(path, text.to_string());
            Arc::new(client)
        }
        pub(crate) fn calls(&self) -> usize {
            self.reads.lock().unwrap().len() + self.writes.lock().unwrap().len()
        }
    }

    impl ClientFileAccess for FakeClient {
        fn can_read(&self) -> bool {
            true
        }
        fn can_write(&self) -> bool {
            !self.read_only
        }
        fn read_text<'a>(&'a self, path: &'a Path) -> ClientFsFuture<'a, String> {
            Box::pin(async move {
                let seen = {
                    let mut reads = self.reads.lock().unwrap();
                    reads.push(path.to_path_buf());
                    reads.len()
                };
                if seen > 1
                    && let Some(text) = self.typed_meanwhile.lock().unwrap().clone()
                {
                    return Ok(text);
                }
                if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("read refused".to_string());
                }
                self.files
                    .lock()
                    .unwrap()
                    .get(path)
                    .cloned()
                    .ok_or_else(|| "not open".to_string())
            })
        }
        fn write_text<'a>(&'a self, path: &'a Path, content: &'a str) -> ClientFsFuture<'a, ()> {
            Box::pin(async move {
                self.writes.lock().unwrap().push(path.to_path_buf());
                if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("write refused".to_string());
                }
                self.files
                    .lock()
                    .unwrap()
                    .insert(path.to_path_buf(), content.to_string());
                Ok(())
            })
        }
    }
}
