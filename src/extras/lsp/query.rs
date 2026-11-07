//! Explicit-path `lsp_diagnostics` queries that tell a clean file apart from
//! every non-answer (mini-agent-lk8yo): no server, document not synchronized,
//! server cooling down or failed, and a wait that elapsed without a publish.

use std::path::Path;
use std::time::Duration;

use super::{DiagPage, LspManager, client, registry};

/// Wait for an explicit query when neither the tool call nor the server's
/// `wait_ms` says otherwise. Longer than the post-edit wait.
pub(crate) const DEFAULT_QUERY_WAIT: Duration = Duration::from_secs(3);
/// Most a caller may ask a query to wait.
pub(crate) const MAX_QUERY_WAIT: Duration =
    Duration::from_millis(crate::config::types::LspServerConfig::MAX_WAIT_MS);

/// Where the queried file is, as the tool resolved it.
pub(crate) enum DiagTarget<'a> {
    /// Workspace-relative path opened through the captured workspace.
    Relative(&'a Path),
    /// Canonical absolute path; outside the workspace it has no server.
    Absolute(&'a Path),
}

/// What an explicit per-file diagnostics query learned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileDiagStatus {
    /// The server published and the file has nothing to report.
    Clean { server: String },
    /// Formatted diagnostics block.
    Diagnostics(String),
    /// No configured server handles the file.
    NoServer {
        extension: Option<String>,
        /// Why a matching built-in is withheld, when that is the cause.
        withheld: Option<String>,
        outside_workspace: bool,
    },
    /// The document could not be sent to the server, so nothing was checked.
    NotSynced { server: String, reason: String },
    /// The server could not be started or is cooling down after a failure.
    Unavailable {
        server: String,
        retry_in_ms: Option<u64>,
    },
    /// The server did not publish within the wait: the status is unknown.
    TimedOut {
        server: String,
        wait_ms: u128,
        /// Diagnostics still stored for the current file contents, if any.
        partial: Option<String>,
    },
}

fn not_synced_reason(error: &std::io::Error) -> String {
    let text = error.to_string();
    if text.contains("byte limit") {
        format!(
            "the file is larger than the {} MiB synchronization limit",
            client::MAX_DOCUMENT_BYTES / (1024 * 1024)
        )
    } else if text.contains("UTF-8") {
        "the file is not valid UTF-8 text".to_string()
    } else {
        format!("the file could not be read ({:?})", error.kind())
    }
}

impl LspManager {
    /// Syncs the file and waits for the server's publish, distinguishing the
    /// outcomes of [`FileDiagStatus`]. `wait` of `None` uses the server's
    /// `wait_ms` when configured, else [`DEFAULT_QUERY_WAIT`]; any value is
    /// capped at [`MAX_QUERY_WAIT`].
    pub(crate) async fn file_diagnostics_status(
        &self,
        target: DiagTarget<'_>,
        wait: Option<Duration>,
        page: DiagPage,
    ) -> FileDiagStatus {
        let root = self.inner.workspace.root();
        let relative = match &target {
            DiagTarget::Relative(relative) => relative.to_path_buf(),
            DiagTarget::Absolute(path) => match path.strip_prefix(root) {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => {
                    return FileDiagStatus::NoServer {
                        extension: extension_of(path),
                        withheld: None,
                        outside_workspace: true,
                    };
                }
            },
        };
        let lookup = root.join(&relative);
        let Some((server, cfg)) = registry::server_for_path(&self.inner.servers, &lookup) else {
            return FileDiagStatus::NoServer {
                extension: extension_of(&lookup),
                withheld: self.withheld_explanation(&lookup),
                outside_workspace: false,
            };
        };
        let server = server.clone();
        let wait = wait
            .or_else(|| cfg.wait_ms.map(Duration::from_millis))
            .unwrap_or(DEFAULT_QUERY_WAIT)
            .min(MAX_QUERY_WAIT);
        let not_synced = |reason: String| FileDiagStatus::NotSynced {
            server: server.clone(),
            reason,
        };
        if self.inner.workspace.validate().is_err() {
            return not_synced("the workspace directory changed".to_string());
        }
        let file = match self.inner.workspace.open_relative(&relative) {
            Ok(file) => file,
            Err(error) => {
                return not_synced(format!("the file could not be opened ({:?})", error.kind()));
            }
        };
        match file.metadata() {
            Ok(metadata) if metadata.is_file() => {}
            _ => return not_synced("it is not a regular file".to_string()),
        }
        let document = match client::read_document(tokio::fs::File::from_std(file)).await {
            Ok(document) => document,
            Err(error) => return not_synced(not_synced_reason(&error)),
        };
        let Some(client) = self.client_for(&lookup).await else {
            let retry_in_ms = self
                .inner
                .failures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(server.as_str())
                .map(|failure| {
                    failure
                        .retry_after
                        .saturating_duration_since(std::time::Instant::now())
                        .as_millis() as u64
                })
                .filter(|ms| *ms > 0);
            return FileDiagStatus::Unavailable {
                server,
                retry_in_ms,
            };
        };
        let Some(baseline) = client.sync_document(&lookup, document).await else {
            return not_synced("the server refused or lost the document".to_string());
        };
        let query_path = match &target {
            DiagTarget::Relative(relative) => self
                .resolve_relative(relative)
                .unwrap_or_else(|| lookup.clone()),
            DiagTarget::Absolute(path) => path.to_path_buf(),
        };
        let Some(uri) = client::file_uri(&query_path) else {
            return not_synced("the path has no file URI".to_string());
        };
        let (output, published) = self
            .diagnostics_block_wait(uri.clone(), wait, Some(baseline), page)
            .await;
        match (output, published) {
            (Some(block), true) => FileDiagStatus::Diagnostics(block),
            (None, true) => FileDiagStatus::Clean { server },
            (partial, false) => {
                // The server did not republish (for example unchanged content),
                // but an entry that still matches the file is an answer.
                let stored = self.stored_diagnostics_are_current(&uri).await;
                match (partial, stored) {
                    (Some(block), true) => FileDiagStatus::Diagnostics(block),
                    (None, true) => FileDiagStatus::Clean { server },
                    (partial, false) => FileDiagStatus::TimedOut {
                        server,
                        wait_ms: wait.as_millis(),
                        partial,
                    },
                }
            }
        }
    }

    /// Whether a stored publish exists for `uri` and still matches the file.
    async fn stored_diagnostics_are_current(&self, uri: &str) -> bool {
        let Some(entry) = self.inner.diags.lock().unwrap().get(uri).cloned() else {
            return false;
        };
        let uri = uri.to_string();
        crate::agent::runner::spawn_blocking_scoped(move || {
            super::diagnostic_identity_is_current(&uri, &entry)
        })
        .await
        .unwrap_or(false)
    }

    /// One-line summary appended to project-wide listings: what the cache
    /// covers and which servers are running.
    pub(crate) fn project_listing_scope_note(&self) -> String {
        let snapshot = self.diagnostic_snapshot();
        let running = match snapshot.live_names {
            Some(names) if !names.is_empty() => names.join(", "),
            Some(_) => "none".to_string(),
            None => "unknown (busy)".to_string(),
        };
        format!(
            "[Covers only files a language server has published diagnostics for (servers publish for documents they have open, so untouched files are absent; query a file with `path` to check it). Running servers: {running}. Configured: {}.]",
            if snapshot.configured_names.is_empty() {
                "none".to_string()
            } else {
                snapshot.configured_names.join(", ")
            }
        )
    }
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
}
