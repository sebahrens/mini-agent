use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compact_str::CompactString;
use rig::tool::Tool;
use tokio::io::AsyncWriteExt;

use crate::agent::tools::check_perm_canonical_path;
use crate::agent::tools::lsp::{LspArgs, LspTool};
use crate::config::types::{LspConfig, LspServerConfig};
use crate::extras::lsp::client::{DiagStore, store_diagnostics_for_test};
use crate::extras::lsp::registry::{resolve_servers, server_for_path};
use crate::extras::lsp::{LspManager, rpc};
use crate::permission::ask::{AskReceiver, UserDecision};
use crate::permission::checker::{PermCheck, PermissionChecker};
use crate::permission::{Action, PermissionConfig, PermissionConfigs, SecurityMode, ToolPerm};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mini-agent-lsp-permission-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn permission(root: &Path, config: PermissionConfig) -> PermCheck {
    Arc::new(Mutex::new(
        PermissionChecker::new(
            &PermissionConfigs::from(config),
            SecurityMode::Standard,
            Some(root.to_path_buf()),
            Some(vec!["standard".to_string()]),
        )
        .unwrap(),
    ))
}

fn read_permission(action: Action) -> PermissionConfig {
    PermissionConfig {
        read: Some(ToolPerm::Simple(action)),
        ..PermissionConfig::default()
    }
}

fn lsp_tool(root: &Path, config: PermissionConfig) -> (LspTool, Option<AskReceiver>, LspManager) {
    let manager = LspManager::new(&LspConfig::default(), root.to_path_buf());
    let permission = Some(permission(root, config));
    let (ask_tx, ask_rx) = tokio::sync::mpsc::channel(4);
    (
        LspTool::new(manager.clone(), permission, Some(ask_tx)),
        Some(ask_rx),
        manager,
    )
}

async fn answer_once(mut ask_rx: AskReceiver, expected_path: &Path) {
    let request = ask_rx.recv().await.expect("permission request");
    assert_eq!(request.tool.as_str(), "lsp_diagnostics");
    assert_eq!(Path::new(&request.input), expected_path);
    request.reply.send(UserDecision::AllowOnce).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn lsp_sync_rejects_file_swapped_to_external_symlink_after_resolution() {
    let temp = std::env::temp_dir().join(format!("mini-agent-lsp-swap-{}", uuid::Uuid::new_v4()));
    let workspace = temp.join("workspace");
    let external = temp.join("external");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&external).unwrap();
    let source = workspace.join("source.rs");
    let secret = external.join("secret.rs");
    std::fs::write(&source, "fn safe() {}").unwrap();
    std::fs::write(&secret, "compile_error!(\"LSP_SECRET\");").unwrap();

    let binding = std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(&workspace).unwrap());
    let manager = LspManager::new(&LspConfig::default(), binding);
    let approved = manager.resolve_path(Path::new("source.rs")).unwrap();
    std::fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink(&secret, &source).unwrap();

    assert!(
        crate::extras::lsp::client::read_stable_text(&approved)
            .await
            .is_err()
    );
    std::fs::remove_dir_all(temp).unwrap();
}

// ── rpc framing ─────────────────────────────────────────────────────────

#[tokio::test]
async fn frame_roundtrip() {
    let (mut a, mut b) = tokio::io::duplex(4096);
    rpc::write_frame(&mut a, br#"{"jsonrpc":"2.0","id":1}"#)
        .await
        .unwrap();
    rpc::write_frame(&mut a, br#"{"jsonrpc":"2.0","id":2}"#)
        .await
        .unwrap();
    assert_eq!(
        rpc::read_frame(&mut b).await.unwrap().as_deref(),
        Some(&br#"{"jsonrpc":"2.0","id":1}"#[..])
    );
    assert_eq!(
        rpc::read_frame(&mut b).await.unwrap().as_deref(),
        Some(&br#"{"jsonrpc":"2.0","id":2}"#[..])
    );
    drop(a);
    // Clean EOF before any header byte → None (server exited).
    assert_eq!(rpc::read_frame(&mut b).await.unwrap(), None);
}

#[tokio::test]
async fn frame_missing_content_length_errors() {
    let (mut a, mut b) = tokio::io::duplex(4096);
    a.write_all(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}")
        .await
        .unwrap();
    assert!(rpc::read_frame(&mut b).await.is_err());
}

#[tokio::test]
async fn frame_eof_mid_message_errors() {
    let (mut a, mut b) = tokio::io::duplex(4096);
    a.write_all(b"Content-Length: 100\r\n\r\n{}").await.unwrap();
    drop(a);
    assert!(rpc::read_frame(&mut b).await.is_err());
}

#[tokio::test]
async fn lsp_process_frames_reject_oversized_inbound_and_outbound_bodies() {
    let (mut a, mut b) = tokio::io::duplex(4096);
    a.write_all(format!("Content-Length: {}\r\n\r\n", rpc::MAX_BODY_BYTES + 1).as_bytes())
        .await
        .unwrap();
    assert_eq!(
        rpc::read_frame(&mut b).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );

    let (mut a, _b) = tokio::io::duplex(4096);
    let oversized = vec![b'x'; rpc::MAX_BODY_BYTES + 1];
    assert_eq!(
        rpc::write_frame(&mut a, &oversized)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn lsp_process_frames_reject_header_flood_and_invalid_utf8() {
    let (mut a, mut b) = tokio::io::duplex(rpc::MAX_HEADER_BYTES * 2);
    a.write_all(&vec![b'x'; rpc::MAX_HEADER_BYTES + 1])
        .await
        .unwrap();
    assert_eq!(
        rpc::read_frame(&mut b).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );

    let prefix = b"Content-Length: 2\r\nX-Pad: ";
    let suffix = b"\r\n\r\n";
    let padding = rpc::MAX_HEADER_BYTES - prefix.len() - suffix.len();
    let mut exact = Vec::with_capacity(rpc::MAX_HEADER_BYTES + 2);
    exact.extend_from_slice(prefix);
    exact.extend(std::iter::repeat_n(b'x', padding));
    exact.extend_from_slice(suffix);
    exact.extend_from_slice(b"{}");
    let (mut a, mut b) = tokio::io::duplex(exact.len());
    a.write_all(&exact).await.unwrap();
    assert_eq!(rpc::read_frame(&mut b).await.unwrap(), Some(b"{}".to_vec()));

    exact.insert(prefix.len(), b'x');
    let (mut a, mut b) = tokio::io::duplex(exact.len());
    a.write_all(&exact).await.unwrap();
    assert_eq!(
        rpc::read_frame(&mut b).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );

    let (mut a, mut b) = tokio::io::duplex(4096);
    a.write_all(b"Content-Length: 2\r\nX-Bad: \xff\r\n\r\n{}")
        .await
        .unwrap();
    assert_eq!(
        rpc::read_frame(&mut b).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn lsp_process_diagnostic_store_is_scoped_and_cumulatively_bounded() {
    let store = DiagStore::default();
    let workspace = "file:///workspace";
    let diagnostic = |uri: String, message: String| {
        serde_json::json!({
            "uri": uri,
            "diagnostics": [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}
                },
                "message": message,
                "source": "s".repeat(1024),
                "relatedInformation": [{
                    "location": {
                        "uri": "file:///outside",
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1}
                        }
                    },
                    "message": "related"
                }],
                "data": {"retained": "no"}
            }]
        })
    };

    assert_eq!(
        store_diagnostics_for_test(
            &store,
            "fixture",
            workspace,
            &diagnostic("file:///elsewhere/a.rs".to_string(), "ignored".to_string())
        ),
        Some(false)
    );
    for index in 0..128 {
        assert_eq!(
            store_diagnostics_for_test(
                &store,
                "fixture",
                workspace,
                &diagnostic(
                    format!("{workspace}/{index}.rs"),
                    if index == 0 {
                        "m".repeat(8 * 1024)
                    } else {
                        "bounded".to_string()
                    }
                )
            ),
            Some(true)
        );
    }
    let guard = store.lock().unwrap();
    assert_eq!(guard.len(), 128);
    let first = &guard["file:///workspace/0.rs"].diagnostics[0];
    assert!(first.message.len() <= 2 * 1024);
    assert!(first.source.is_none());
    assert!(first.related_information.is_none());
    assert!(first.data.is_none());
    drop(guard);
    assert_eq!(
        store_diagnostics_for_test(
            &store,
            "fixture",
            workspace,
            &diagnostic(
                "file:///workspace/overflow.rs".to_string(),
                "no".to_string()
            )
        ),
        None
    );
}

// ── registry ────────────────────────────────────────────────────────────

fn custom(command: &str, extensions: &[&str]) -> LspServerConfig {
    LspServerConfig {
        command: CompactString::from(command),
        extensions: extensions.iter().map(|s| CompactString::from(*s)).collect(),
        ..Default::default()
    }
}

#[test]
fn builtin_matches_rust_extension() {
    let servers = resolve_servers(&HashMap::new());
    let (name, _) = server_for_path(&servers, Path::new("src/main.rs")).unwrap();
    assert_eq!(name, "rust");
    assert!(server_for_path(&servers, Path::new("readme.txt")).is_none());
}

#[test]
fn user_override_replaces_builtin() {
    let mut user = HashMap::new();
    user.insert("rust".to_string(), custom("my-analyzer", &[".rs"]));
    let servers = resolve_servers(&user);
    let (name, cfg) = server_for_path(&servers, Path::new("main.rs")).unwrap();
    assert_eq!(name, "rust");
    assert_eq!(cfg.command.as_str(), "my-analyzer");
}

#[test]
fn disabled_removes_builtin() {
    let mut user = HashMap::new();
    user.insert(
        "rust".to_string(),
        LspServerConfig {
            disabled: true,
            ..Default::default()
        },
    );
    let servers = resolve_servers(&user);
    assert!(server_for_path(&servers, Path::new("main.rs")).is_none());
}

#[test]
fn custom_server_is_added() {
    let mut user = HashMap::new();
    user.insert("mine".to_string(), custom("my-ls", &[".my"]));
    let servers = resolve_servers(&user);
    let (name, _) = server_for_path(&servers, Path::new("x.my")).unwrap();
    assert_eq!(name, "mine");
}

#[test]
fn empty_command_is_dropped() {
    let mut user = HashMap::new();
    user.insert("bogus".to_string(), custom("", &[".bogus"]));
    let servers = resolve_servers(&user);
    assert!(server_for_path(&servers, Path::new("x.bogus")).is_none());
}

#[cfg(unix)]
#[test]
fn diagnostic_file_uri_round_trips_permission_paths() {
    let path = PathBuf::from("/workspace/space and-ü/%file.rs");
    let uri = crate::extras::lsp::client::file_uri(&path).unwrap();
    assert_eq!(uri, "file:///workspace/space%20and-%C3%BC/%25file.rs");
    assert_eq!(crate::extras::lsp::client::file_path(&uri), Some(path));
}

#[cfg(windows)]
#[test]
fn diagnostic_file_uri_uses_standard_windows_drive_and_unc_forms() {
    let drive = PathBuf::from(r"C:\workspace\space and-ü\file.rs");
    let drive_uri = crate::extras::lsp::client::file_uri(&drive).unwrap();
    assert_eq!(drive_uri, "file:///C:/workspace/space%20and-%C3%BC/file.rs");
    assert_eq!(
        crate::extras::lsp::client::file_path(&drive_uri),
        Some(drive)
    );

    let unc = PathBuf::from(r"\\server\share\space name\file.rs");
    let unc_uri = crate::extras::lsp::client::file_uri(&unc).unwrap();
    assert_eq!(unc_uri, "file://server/share/space%20name/file.rs");
    assert_eq!(crate::extras::lsp::client::file_path(&unc_uri), Some(unc));
}

#[test]
fn diagnostic_file_uri_rejects_non_file_and_malformed_inputs() {
    assert!(crate::extras::lsp::client::file_path("https://example.test/file.rs").is_none());
    assert!(crate::extras::lsp::client::file_path("file:///tmp/%GG.rs").is_none());
    assert!(crate::extras::lsp::client::file_path("file:///tmp/%FF.rs").is_none());
}

#[cfg(unix)]
#[test]
fn lsp_permission_path_rejects_lossy_non_utf8_collision() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let valid = PathBuf::from("sample-�.guarded");
    let invalid = PathBuf::from(OsString::from_vec(b"sample-\xff.guarded".to_vec()));
    assert_ne!(valid, invalid);
    assert_eq!(valid.to_str().unwrap(), invalid.to_string_lossy());
    assert!(crate::agent::tools::lsp::canonical_permission_path(&invalid).is_err());
}

// ── config ──────────────────────────────────────────────────────────────

#[test]
fn resolve_lsp_requires_enabled() {
    let mut cfg = crate::config::Config::default();
    assert!(cfg.resolve_lsp().is_none());
    cfg.lsp = Some(LspConfig::default());
    assert!(cfg.resolve_lsp().is_none());
    cfg.lsp = Some(LspConfig {
        enabled: true,
        ..Default::default()
    });
    assert!(cfg.resolve_lsp().is_some());
}

// ── manager formatting (no live server) ─────────────────────────────────

fn diag(
    severity: lsp_types::DiagnosticSeverity,
    line: u32,
    col: u32,
    msg: &str,
) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range {
            start: lsp_types::Position {
                line,
                character: col,
            },
            end: lsp_types::Position {
                line,
                character: col,
            },
        },
        severity: Some(severity),
        message: msg.to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn unhandled_extension_yields_nothing() {
    let manager = LspManager::new(
        &LspConfig::default(),
        std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(Path::new("/tmp")).unwrap()),
    );
    let path = Path::new("/tmp/x.unknownext");
    assert!(!manager.handles(path));
    assert!(
        manager
            .diagnostics_block(path, Duration::from_millis(10))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn injected_diagnostics_format_errors_first() {
    let manager = LspManager::new(
        &LspConfig::default(),
        std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(Path::new("/tmp")).unwrap()),
    );
    let path = Path::new("/tmp/x.rs");
    assert!(manager.handles(path));
    manager.inject_diagnostics(
        "file:///tmp/x.rs",
        "rust",
        vec![
            diag(
                lsp_types::DiagnosticSeverity::WARNING,
                4,
                2,
                "unused variable",
            ),
            diag(
                lsp_types::DiagnosticSeverity::ERROR,
                11,
                4,
                "mismatched types",
            ),
            diag(lsp_types::DiagnosticSeverity::HINT, 0, 0, "not shown"),
        ],
    );
    let block = manager
        .diagnostics_block(path, Duration::from_millis(10))
        .await
        .unwrap();
    let error_pos = block.find("12:5 error: mismatched types").unwrap();
    let warn_pos = block.find("5:3 warning: unused variable").unwrap();
    assert!(error_pos < warn_pos, "errors must sort first: {block}");
    assert!(
        !block.contains("not shown"),
        "hints are filtered out: {block}"
    );
}

#[tokio::test]
async fn first_empty_publish_completes_wait_without_deadline_delay() {
    let root = TempRoot::new("first-empty-publish");
    let file = root.path().join("clean.rs");
    std::fs::write(&file, "fn clean() {}").unwrap();
    let file = file.canonicalize().unwrap();
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    let uri = crate::extras::lsp::client::file_uri(&file).unwrap();

    let query_manager = manager.clone();
    let query_file = file.clone();
    let query = tokio::spawn(async move {
        query_manager
            .diagnostics_block(&query_file, Duration::from_secs(2))
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    manager.set_synced_document_for_test(&uri, 1, true);
    assert!(manager.publish_synced_diagnostics_for_test(&uri, "rust", None, Vec::new()));
    let result = tokio::time::timeout(Duration::from_millis(250), query)
        .await
        .expect("a first clean publish must advance the waiter before its deadline")
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn versionless_initial_publish_and_clear_are_accepted_within_unchanged_epoch() {
    let root = TempRoot::new("versionless-initial-clear");
    let file = root.path().join("main.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let file = file.canonicalize().unwrap();
    let uri = crate::extras::lsp::client::file_uri(&file).unwrap();
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    manager.set_synced_document_for_test(&uri, 1, true);

    assert!(manager.publish_null_version_diagnostics_for_test(
        &uri,
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "initial versionless diagnostic",
        )],
    ));
    assert!(manager.all_diagnostics_block().is_some());
    assert!(manager.publish_synced_diagnostics_for_test(&uri, "rust", None, Vec::new()));
    assert!(manager.all_diagnostics_block().is_none());
}

#[tokio::test]
async fn atomic_edit_waits_for_publish_bound_to_replacement_identity() {
    let root = TempRoot::new("atomic-edit-fresh-publish");
    let file = root.path().join("main.guarded");
    std::fs::write(&file, "old contents").unwrap();
    let file = file.canonicalize().unwrap();
    let mut cfg = LspConfig::default();
    cfg.servers.insert(
        "guarded".to_string(),
        custom("definitely-not-a-real-language-server", &[".guarded"]),
    );
    let manager = LspManager::new(&cfg, root.path().to_path_buf());
    let uri = crate::extras::lsp::client::file_uri(&file).unwrap();
    manager.inject_diagnostics(
        &uri,
        "guarded",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "stale diagnostic",
        )],
    );

    // This is the same checked atomic replacement primitive used by EditTool.
    let approved_parent = crate::fs::stable_path_metadata(root.path()).await.unwrap();
    crate::fs::atomic_write_resolved_checked(&file, "new contents", approved_parent)
        .await
        .unwrap();
    manager.notify_changed(&file).await;

    let query_manager = manager.clone();
    let query_file = file.clone();
    let query = tokio::spawn(async move {
        query_manager
            .diagnostics_block(&query_file, Duration::from_secs(2))
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    manager.set_synced_document_for_test(&uri, 2, false);
    assert!(
        !manager.publish_synced_diagnostics_for_test(&uri, "guarded", None, Vec::new()),
        "a versionless delayed clear cannot be attributed safely after an edit"
    );
    assert!(
        !manager.publish_synced_diagnostics_for_test(
            &uri,
            "guarded",
            None,
            vec![diag(
                lsp_types::DiagnosticSeverity::ERROR,
                1,
                2,
                "unanchored versionless diagnostic",
            )],
        ),
        "even a fresh-looking versionless publish cannot anchor a changed epoch"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !query.is_finished(),
        "the stale pre-edit publish must not satisfy the replacement waiter"
    );
    assert!(manager.publish_synced_diagnostics_for_test(
        &uri,
        "guarded",
        Some(2),
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            1,
            2,
            "fresh diagnostic",
        )],
    ));
    let block = tokio::time::timeout(Duration::from_millis(250), query)
        .await
        .expect("the fresh replacement publish must complete the edit waiter")
        .unwrap()
        .unwrap();
    assert!(block.contains("fresh diagnostic"), "{block}");
    assert!(!block.contains("stale diagnostic"), "{block}");
    assert!(
        manager.publish_synced_diagnostics_for_test(&uri, "guarded", None, Vec::new()),
        "an exact version anchor permits a later versionless clear in the unchanged epoch"
    );
    assert!(manager.all_diagnostics_block().is_none());
}

#[test]
fn clean_project_reports_nothing() {
    let manager = LspManager::new(
        &LspConfig::default(),
        std::sync::Arc::new(crate::paths::WorkspaceBinding::capture(Path::new("/tmp")).unwrap()),
    );
    assert!(manager.all_diagnostics_block().is_none());
    manager.inject_diagnostics(
        "file:///tmp/x.rs",
        "rust",
        vec![diag(lsp_types::DiagnosticSeverity::ERROR, 0, 0, "boom")],
    );
    let block = manager.all_diagnostics_block().unwrap();
    assert!(block.contains("x.rs:1:1 error: boom"), "{block}");
}

// ── tool permission boundary ────────────────────────────────────────────

#[tokio::test]
async fn explicit_in_root_path_obeys_read_allow_deny_and_ask() {
    let root = TempRoot::new("explicit-in-root");
    let file = root.path().join("sample.unknownext");
    std::fs::write(&file, "contents").unwrap();
    let canonical = file.canonicalize().unwrap();

    let (allow, _, _) = lsp_tool(root.path(), read_permission(Action::Allow));
    let output = allow
        .call(LspArgs {
            path: Some(file.display().to_string()),
        })
        .await
        .unwrap();
    assert!(output.starts_with("No diagnostics for "), "{output}");

    let (deny, _, _) = lsp_tool(root.path(), read_permission(Action::Deny));
    let error = deny
        .call(LspArgs {
            path: Some(file.display().to_string()),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Permission denied:"), "{error}");

    let (ask, ask_rx, _) = lsp_tool(root.path(), read_permission(Action::Ask));
    let (result, ()) = tokio::join!(
        ask.call(LspArgs {
            path: Some(file.display().to_string()),
        }),
        answer_once(ask_rx.unwrap(), &canonical),
    );
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn explicit_external_path_obeys_external_directory_allow_deny_and_ask() {
    let root = TempRoot::new("external-workspace");
    let external = TempRoot::new("external-file");
    let file = external.path().join("sample.unknownext");
    std::fs::write(&file, "contents").unwrap();
    let canonical = file.canonicalize().unwrap();

    for action in [Action::Allow, Action::Deny] {
        let config = PermissionConfig {
            read: Some(ToolPerm::Simple(Action::Allow)),
            external_directory: Some(
                [(canonical.display().to_string(), action)]
                    .into_iter()
                    .collect(),
            ),
            ..PermissionConfig::default()
        };
        let (tool, _, _) = lsp_tool(root.path(), config);
        let result = tool
            .call(LspArgs {
                path: Some(file.display().to_string()),
            })
            .await;
        match action {
            Action::Allow => assert!(result.is_ok(), "{result:?}"),
            Action::Deny => assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .starts_with("Permission denied:"),
                "external deny must fail closed"
            ),
            Action::Ask => unreachable!(),
        }
    }

    let config = PermissionConfig {
        external_directory: Some(
            [(canonical.display().to_string(), Action::Ask)]
                .into_iter()
                .collect(),
        ),
        ..PermissionConfig::default()
    };
    let (ask, ask_rx, _) = lsp_tool(root.path(), config);
    let (result, ()) = tokio::join!(
        ask.call(LspArgs {
            path: Some(file.display().to_string()),
        }),
        answer_once(ask_rx.unwrap(), &canonical),
    );
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn project_query_obeys_read_allow_deny_and_ask_before_cache_disclosure() {
    let root = TempRoot::new("project-scope");
    let canonical_root = root.path().canonicalize().unwrap();

    let (allow, _, _) = lsp_tool(root.path(), read_permission(Action::Allow));
    assert_eq!(
        allow.call(LspArgs { path: None }).await.unwrap(),
        "No diagnostics."
    );

    let (deny, _, manager) = lsp_tool(root.path(), read_permission(Action::Deny));
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&root.path().join("secret.rs")).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "cached secret diagnostic",
        )],
    );
    let error = deny
        .call(LspArgs { path: None })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Permission denied:"), "{error}");
    assert!(!error.contains("cached secret diagnostic"), "{error}");

    let (ask, ask_rx, _) = lsp_tool(root.path(), read_permission(Action::Ask));
    let (result, ()) = tokio::join!(
        ask.call(LspArgs { path: None }),
        answer_once(ask_rx.unwrap(), &canonical_root),
    );
    assert_eq!(result.unwrap(), "No diagnostics.");
}

#[tokio::test]
async fn project_query_filters_diagnostics_for_denied_files() {
    let root = TempRoot::new("aggregate-filter");
    let public = root.path().join("public.rs");
    let secret = root.path().join("secret.rs");
    std::fs::write(&public, "public").unwrap();
    std::fs::write(&secret, "secret").unwrap();
    let public = public.canonicalize().unwrap();
    let secret = secret.canonicalize().unwrap();

    let config = PermissionConfig {
        read: Some(ToolPerm::Granular(
            [(secret.display().to_string(), Action::Deny)]
                .into_iter()
                .collect(),
        )),
        ..PermissionConfig::default()
    };
    let (tool, _, manager) = lsp_tool(root.path(), config);
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&public).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::WARNING,
            1,
            2,
            "public warning",
        )],
    );
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&secret).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            3,
            4,
            "secret error",
        )],
    );

    let output = tool.call(LspArgs { path: None }).await.unwrap();
    assert!(
        output.contains("public.rs:2:3 warning: public warning"),
        "{output}"
    );
    assert!(!output.contains("secret.rs"), "{output}");
    assert!(!output.contains("secret error"), "{output}");
}

#[tokio::test]
async fn explicit_denial_prevents_sync_or_server_launch_and_cached_disclosure() {
    let root = TempRoot::new("deny-before-manager");
    let file = root.path().join("secret.guarded");
    std::fs::write(&file, "secret").unwrap();

    let mut cfg = LspConfig::default();
    cfg.servers.insert(
        "guarded".to_string(),
        custom("definitely-not-a-real-language-server", &[".guarded"]),
    );
    let manager = LspManager::new(&cfg, root.path().to_path_buf());
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&file).unwrap(),
        "guarded",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "cached secret diagnostic",
        )],
    );
    let tool = LspTool::new(
        manager.clone(),
        Some(permission(root.path(), read_permission(Action::Deny))),
        None,
    );

    let error = tool
        .call(LspArgs {
            path: Some(file.display().to_string()),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Permission denied:"), "{error}");
    assert!(!error.contains("cached secret diagnostic"), "{error}");
    assert_eq!(manager.cached_client_count().await, 0);
}

#[tokio::test]
async fn lsp_allow_always_uses_canonical_path_pattern_and_deny_still_wins() {
    let root = TempRoot::new("allow-always");
    let external = TempRoot::new("allow-always-external");
    let root_path = root.path().canonicalize().unwrap();
    let public = root.path().join("public.rs");
    let secret = root.path().join("secret.rs");
    let sibling = external.path().join("sibling.rs");
    std::fs::write(&public, "public").unwrap();
    std::fs::write(&secret, "secret").unwrap();
    std::fs::write(&sibling, "sibling").unwrap();
    let public = public.canonicalize().unwrap();
    let secret = secret.canonicalize().unwrap();
    let sibling = sibling.canonicalize().unwrap();
    let config = PermissionConfig {
        read: Some(ToolPerm::Granular(
            [
                (root_path.display().to_string(), Action::Ask),
                (secret.display().to_string(), Action::Deny),
            ]
            .into_iter()
            .collect(),
        )),
        ..PermissionConfig::default()
    };
    let (tool, mut ask_rx, manager) = lsp_tool(root.path(), config);
    let tool = Arc::new(tool);
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&public).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::WARNING,
            0,
            0,
            "public warning",
        )],
    );
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&secret).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "secret error",
        )],
    );
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&sibling).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "sibling error",
        )],
    );

    let first_tool = tool.clone();
    let task = tokio::spawn(async move { first_tool.call(LspArgs { path: None }).await });
    let request = ask_rx.as_mut().unwrap().recv().await.unwrap();
    assert_eq!(request.tool.as_str(), "lsp_diagnostics");
    assert_eq!(Path::new(&request.input), root_path);
    let pattern = request
        .suggested_pattern
        .clone()
        .expect("aggregate queries must supply their exact project-tree scope");
    assert_ne!(pattern, "*");
    assert_eq!(request.additional_allow_patterns.len(), 1);
    let exact_root_matcher = crate::permission::pattern::Pattern::new_generated_path_scope(
        &request.additional_allow_patterns[0],
    )
    .unwrap();
    assert!(exact_root_matcher.matches_path(root_path.to_str().unwrap()));
    assert!(!exact_root_matcher.matches_path(sibling.to_str().unwrap()));
    let matcher = crate::permission::pattern::Pattern::new_generated_path_scope(&pattern).unwrap();
    assert!(matcher.matches_path(public.to_str().unwrap()));
    assert!(!matcher.matches_path(sibling.to_str().unwrap()));
    request
        .reply
        .send(UserDecision::AllowAlways(pattern))
        .unwrap();
    let sibling_request =
        tokio::time::timeout(Duration::from_secs(1), ask_rx.as_mut().unwrap().recv())
            .await
            .expect("project AllowAlways must not authorize a sibling path")
            .unwrap();
    assert_eq!(Path::new(&sibling_request.input), sibling);
    sibling_request.reply.send(UserDecision::Deny).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("AllowAlways must cover project files after the sibling is denied")
        .unwrap()
        .unwrap();
    assert!(output.contains("public warning"), "{output}");
    assert!(!output.contains("secret error"), "{output}");
    assert!(!output.contains("sibling error"), "{output}");
    assert!(ask_rx.as_mut().unwrap().try_recv().is_err());

    let second_tool = tool.clone();
    let second = tokio::spawn(async move { second_tool.call(LspArgs { path: None }).await });
    let repeated_request =
        tokio::time::timeout(Duration::from_secs(1), ask_rx.as_mut().unwrap().recv())
            .await
            .expect("repeat aggregate must reach the still-unauthorized sibling")
            .unwrap();
    assert_eq!(
        Path::new(&repeated_request.input),
        sibling,
        "the exact project root grant must suppress a repeated root prompt"
    );
    repeated_request.reply.send(UserDecision::Deny).unwrap();
    second.await.unwrap().unwrap();
}

async fn assert_aggregate_literal_metachar_scope(project_name: &str, sibling_name: &str) {
    let parent = TempRoot::new("literal-metachar-scope");
    let project = parent.path().join(project_name);
    let sibling = parent.path().join(sibling_name);
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::create_dir_all(sibling.join("src")).unwrap();
    let project_file = project.join("src/main.rs");
    let sibling_file = sibling.join("src/secret.rs");
    std::fs::write(&project_file, "project").unwrap();
    std::fs::write(&sibling_file, "sibling").unwrap();
    let project = project.canonicalize().unwrap();
    let project_file = project_file.canonicalize().unwrap();
    let sibling_file = sibling_file.canonicalize().unwrap();

    let (tool, mut ask_rx, manager) = lsp_tool(&project, read_permission(Action::Ask));
    let tool = Arc::new(tool);
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&project_file).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "project diagnostic",
        )],
    );
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&sibling_file).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "sibling diagnostic",
        )],
    );

    let first_tool = tool.clone();
    let first = tokio::spawn(async move { first_tool.call(LspArgs { path: None }).await });
    let root_request = ask_rx.as_mut().unwrap().recv().await.unwrap();
    assert_eq!(Path::new(&root_request.input), project);
    let descendants = root_request.suggested_pattern.clone().unwrap();
    let exact = root_request.additional_allow_patterns.first().unwrap();
    let descendant_matcher =
        crate::permission::pattern::Pattern::new_generated_path_scope(&descendants).unwrap();
    let exact_matcher =
        crate::permission::pattern::Pattern::new_generated_path_scope(exact).unwrap();
    assert!(exact_matcher.matches_path(project.to_str().unwrap()));
    assert!(!exact_matcher.matches_path(sibling_file.to_str().unwrap()));
    assert!(descendant_matcher.matches_path(project_file.to_str().unwrap()));
    assert!(!descendant_matcher.matches_path(sibling_file.to_str().unwrap()));
    root_request
        .reply
        .send(UserDecision::AllowAlways(descendants))
        .unwrap();

    let sibling_request = ask_rx.as_mut().unwrap().recv().await.unwrap();
    assert_eq!(Path::new(&sibling_request.input), sibling_file);
    sibling_request.reply.send(UserDecision::Deny).unwrap();
    let output = first.await.unwrap().unwrap();
    assert!(output.contains("project diagnostic"), "{output}");
    assert!(!output.contains("sibling diagnostic"), "{output}");

    let second_tool = tool.clone();
    let second = tokio::spawn(async move { second_tool.call(LspArgs { path: None }).await });
    let repeated_request =
        tokio::time::timeout(Duration::from_secs(1), ask_rx.as_mut().unwrap().recv())
            .await
            .expect("the exact root scope must suppress a repeated root prompt")
            .unwrap();
    assert_eq!(Path::new(&repeated_request.input), sibling_file);
    repeated_request.reply.send(UserDecision::Deny).unwrap();
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn aggregate_allow_always_treats_star_in_project_root_literally() {
    assert_aggregate_literal_metachar_scope("project*literal", "project-other-literal").await;
}

#[tokio::test]
async fn aggregate_allow_always_treats_question_in_project_root_literally() {
    assert_aggregate_literal_metachar_scope("project?literal", "projectXliteral").await;
}

#[tokio::test]
async fn aggregate_allow_always_treats_bracket_in_project_root_literally() {
    assert_aggregate_literal_metachar_scope("project[literal", "project-other-literal").await;
}

#[cfg(unix)]
#[tokio::test]
async fn aggregate_binding_swap_restore_keeps_permission_and_result_on_cached_inode() {
    use std::os::unix::fs::symlink;

    let root = TempRoot::new("binding-swap-root");
    let external = TempRoot::new("binding-swap-external");
    let cached = root.path().join("cached.rs");
    let backup = root.path().join("cached-backup.rs");
    let allowed_target = external.path().join("allowed.rs");
    std::fs::write(&cached, "cached inode").unwrap();
    std::fs::write(&allowed_target, "different allowed inode").unwrap();
    let cached = cached.canonicalize().unwrap();
    let allowed_target = allowed_target.canonicalize().unwrap();

    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    manager.inject_diagnostics(
        &crate::extras::lsp::client::file_uri(&cached).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "cached inode diagnostic",
        )],
    );
    let uris = manager.diagnostic_candidate_uris();
    assert_eq!(uris.len(), 1);
    let binding = manager.bind_diagnostic_uri(&uris[0]).await.unwrap();
    assert_eq!(binding.path(), cached);

    std::fs::rename(&cached, &backup).unwrap();
    symlink(&allowed_target, &cached).unwrap();
    assert_eq!(cached.canonicalize().unwrap(), allowed_target);

    let config = PermissionConfig {
        read: Some(ToolPerm::Granular(
            [
                (cached.to_str().unwrap().to_string(), Action::Deny),
                (allowed_target.to_str().unwrap().to_string(), Action::Allow),
            ]
            .into_iter()
            .collect(),
        )),
        ..PermissionConfig::default()
    };
    let permission = Some(permission(root.path(), config));
    assert!(
        check_perm_canonical_path(
            &permission,
            &None,
            "lsp_diagnostics",
            cached.to_str().unwrap(),
            false,
        )
        .await
        .is_err(),
        "the allowed symlink target must not replace the bound cache path as the policy subject"
    );

    std::fs::remove_file(&cached).unwrap();
    std::fs::rename(&backup, &cached).unwrap();
    assert!(
        manager.all_diagnostics_block_for_snapshots(&[]).is_none(),
        "a denied binding must not disclose diagnostics after the original inode is restored"
    );
    let snapshot = manager
        .snapshot_bound_diagnostics(&binding, crate::extras::lsp::MAX_DIAG_LINES)
        .unwrap();
    drop(binding);
    assert!(
        manager
            .all_diagnostics_block_for_snapshots(&[snapshot])
            .unwrap()
            .contains("cached inode diagnostic"),
        "the held binding must remain tied to the exact cached inode"
    );
}

#[tokio::test]
async fn aggregate_authorization_bounds_open_handles_and_preserves_sorted_results() {
    let root = TempRoot::new("bounded-bindings");
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    for index in 0..64 {
        let path = root.path().join(format!("file-{index:02}.rs"));
        std::fs::write(&path, format!("file {index}")).unwrap();
        let path = path.canonicalize().unwrap();
        let severity = if index < 12 {
            lsp_types::DiagnosticSeverity::ERROR
        } else {
            lsp_types::DiagnosticSeverity::HINT
        };
        manager.inject_diagnostics(
            &crate::extras::lsp::client::file_uri(&path).unwrap(),
            "rust",
            vec![diag(severity, 0, 0, &format!("diagnostic-{index:02}"))],
        );
    }
    let tool = LspTool::new(
        manager.clone(),
        Some(permission(root.path(), PermissionConfig::default())),
        None,
    );
    let output = tool.call(LspArgs { path: None }).await.unwrap();
    let mut last = 0;
    for index in 0..12 {
        let needle = format!("file-{index:02}.rs:1:1 error: diagnostic-{index:02}");
        let position = output
            .find(&needle)
            .unwrap_or_else(|| panic!("missing {needle}: {output}"));
        assert!(position >= last, "aggregate output must remain URI-sorted");
        last = position;
    }
    assert!(!output.contains("diagnostic-12"));
    assert_eq!(
        manager.peak_bound_diagnostic_count(),
        1,
        "aggregate authorization must release each descriptor before opening the next"
    );
}

#[tokio::test]
async fn aggregate_snapshot_caps_high_cardinality_high_volume_diagnostics_before_cloning() {
    let root = TempRoot::new("bounded-snapshot-volume");
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    let mut accepted = 0;
    for file_index in 0..96 {
        let path = root.path().join(format!("volume-{file_index:03}.rs"));
        std::fs::write(&path, "contents").unwrap();
        let path = path.canonicalize().unwrap();
        let diagnostics = (0..128)
            .map(|diagnostic_index| {
                diag(
                    lsp_types::DiagnosticSeverity::ERROR,
                    diagnostic_index,
                    0,
                    &format!(
                        "volume-{file_index:03}-{diagnostic_index:03}-{}",
                        "x".repeat(2048)
                    ),
                )
            })
            .collect();
        accepted += usize::from(manager.try_inject_diagnostics(
            &crate::extras::lsp::client::file_uri(&path).unwrap(),
            "rust",
            diagnostics,
        ));
    }
    assert!(
        accepted > 0,
        "bounded cache must accept an initial diagnostic set"
    );
    assert!(
        accepted < 96,
        "bounded cache must reject an oversized aggregate"
    );

    let first_uri = manager.diagnostic_candidate_uris().remove(0);
    let binding = manager.bind_diagnostic_uri(&first_uri).await.unwrap();
    let snapshot = manager
        .snapshot_bound_diagnostics(&binding, crate::extras::lsp::MAX_DIAG_LINES)
        .unwrap();
    assert_eq!(snapshot.retained_line_count(), 20);
    assert!(snapshot.is_truncated());
    drop(binding);
    let bounded = manager
        .all_diagnostics_block_for_snapshots(&[snapshot])
        .unwrap();
    assert!(bounded.contains("… (truncated)"), "{bounded}");
    assert!(
        bounded.len() < 8_000,
        "snapshot must retain formatted, message-capped lines rather than full diagnostics"
    );

    let tool = LspTool::new(
        manager.clone(),
        Some(permission(root.path(), PermissionConfig::default())),
        None,
    );
    let output = tool.call(LspArgs { path: None }).await.unwrap();
    assert!(output.contains("… (truncated)"), "{output}");
    assert_eq!(manager.peak_bound_diagnostic_count(), 1);
}

#[tokio::test]
async fn diagnostic_cache_hard_cap_bounds_candidate_sort_cardinality() {
    let root = TempRoot::new("diagnostic-cache-cardinality-cap");
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    let cap = crate::extras::lsp::client::MAX_DIAGNOSTIC_FILES;
    for index in 0..(cap + 32) {
        let path = root.path().join(format!("candidate-{index:04}.rs"));
        std::fs::write(&path, "contents").unwrap();
        let uri = crate::extras::lsp::client::file_uri(&path.canonicalize().unwrap()).unwrap();
        let stored = manager.publish_diagnostics_for_test(
            &uri,
            "rust",
            vec![diag(
                lsp_types::DiagnosticSeverity::ERROR,
                0,
                0,
                "bounded candidate",
            )],
        );
        assert_eq!(stored, index < cap);
    }
    let candidates = manager.diagnostic_candidate_uris();
    assert_eq!(candidates.len(), cap);
    assert!(candidates.windows(2).all(|pair| pair[0] < pair[1]));
}

#[tokio::test]
async fn diagnostic_cache_sanitizes_entries_and_enforces_global_byte_budget_on_updates() {
    let root = TempRoot::new("diagnostic-cache-byte-budget");
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    let mut accepted_uris = Vec::new();
    let mut rejected_uri = None;

    // Sanitization bounds every file well below the global budget, so publish
    // enough files that the byte budget (not the file cap) is what rejects.
    for file_index in 0..crate::extras::lsp::client::MAX_DIAGNOSTIC_FILES {
        let path = root.path().join(format!("oversized-{file_index:03}.rs"));
        std::fs::write(&path, "contents").unwrap();
        let uri = crate::extras::lsp::client::file_uri(&path.canonicalize().unwrap()).unwrap();
        let diagnostics = (0..300)
            .map(|diagnostic_index| {
                let mut diagnostic = diag(
                    lsp_types::DiagnosticSeverity::ERROR,
                    diagnostic_index,
                    0,
                    &format!("{}-{diagnostic_index}", "m".repeat(4096)),
                );
                diagnostic.source = Some("untrusted-source".repeat(256));
                diagnostic.data = Some(serde_json::json!({ "blob": "d".repeat(4096) }));
                diagnostic
            })
            .collect();
        if manager.publish_diagnostics_for_test(&uri, "rust", diagnostics) {
            accepted_uris.push(uri);
        } else {
            rejected_uri = Some(uri);
            break;
        }
    }

    let (files, total_bytes, max_count, max_message, has_extension_payload) =
        manager.diagnostic_cache_metrics();
    assert_eq!(files, accepted_uris.len());
    assert!(files > 0);
    assert!(
        rejected_uri.is_some(),
        "global byte budget must reject growth"
    );
    assert!(
        accepted_uris.len() < crate::extras::lsp::client::MAX_DIAGNOSTIC_FILES,
        "rejection must come from the byte budget, not the file cap"
    );
    assert!(total_bytes <= crate::extras::lsp::client::MAX_DIAGNOSTIC_CACHE_BYTES);
    assert!(max_count <= crate::extras::lsp::client::MAX_DIAGNOSTICS_PER_FILE);
    assert!(max_message <= crate::extras::lsp::client::MAX_DIAGNOSTIC_MESSAGE_BYTES);
    assert!(!has_extension_payload);

    // Shrinking an existing entry must subtract its prior retained size so a
    // previously rejected new entry can consume the released budget.
    assert!(manager.publish_diagnostics_for_test(&accepted_uris[0], "rust", Vec::new()));
    let rejected_uri = rejected_uri.unwrap();
    let replacement = (0..300)
        .map(|index| {
            diag(
                lsp_types::DiagnosticSeverity::ERROR,
                index,
                0,
                &"r".repeat(4096),
            )
        })
        .collect();
    assert!(manager.publish_diagnostics_for_test(&rejected_uri, "rust", replacement));
    let (_, total_bytes, max_count, max_message, _) = manager.diagnostic_cache_metrics();
    assert!(total_bytes <= crate::extras::lsp::client::MAX_DIAGNOSTIC_CACHE_BYTES);
    assert!(max_count <= crate::extras::lsp::client::MAX_DIAGNOSTICS_PER_FILE);
    assert!(max_message <= crate::extras::lsp::client::MAX_DIAGNOSTIC_MESSAGE_BYTES);
}

#[tokio::test]
async fn oversized_existing_publish_invalidates_stale_diagnostics_and_advances_version() {
    let root = TempRoot::new("oversized-existing-publish");
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    let victim = root.path().join("victim.rs");
    std::fs::write(&victim, "contents").unwrap();
    let victim = victim.canonicalize().unwrap();
    let victim_uri = crate::extras::lsp::client::file_uri(&victim).unwrap();
    assert!(manager.publish_diagnostics_for_test(
        &victim_uri,
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "stale diagnostic",
        )],
    ));

    let large_publish = || {
        (0..crate::extras::lsp::client::MAX_DIAGNOSTICS_PER_FILE)
            .map(|index| {
                diag(
                    lsp_types::DiagnosticSeverity::ERROR,
                    index as u32,
                    0,
                    &"x".repeat(crate::extras::lsp::client::MAX_DIAGNOSTIC_MESSAGE_BYTES),
                )
            })
            .collect::<Vec<_>>()
    };

    // Fill with maximum retained entries, then consume the remaining slack
    // with smaller entries. The first rejected small entry proves that a
    // maximum-sized replacement of the tiny victim cannot fit globally.
    for index in 0..32 {
        let path = root.path().join(format!("large-{index:02}.rs"));
        std::fs::write(&path, "contents").unwrap();
        let uri = crate::extras::lsp::client::file_uri(&path.canonicalize().unwrap()).unwrap();
        if !manager.publish_diagnostics_for_test(&uri, "rust", large_publish()) {
            break;
        }
    }
    let mut rejected_small = false;
    for index in 0..crate::extras::lsp::client::MAX_DIAGNOSTIC_FILES {
        let path = root.path().join(format!("padding-{index:03}.rs"));
        std::fs::write(&path, "contents").unwrap();
        let uri = crate::extras::lsp::client::file_uri(&path.canonicalize().unwrap()).unwrap();
        let stored = manager.publish_diagnostics_for_test(
            &uri,
            "rust",
            vec![diag(
                lsp_types::DiagnosticSeverity::ERROR,
                0,
                0,
                &"p".repeat(crate::extras::lsp::client::MAX_DIAGNOSTIC_MESSAGE_BYTES),
            )],
        );
        if !stored {
            rejected_small = true;
            break;
        }
    }
    assert!(rejected_small, "test setup must exhaust the cache budget");

    let (old_version, old_count) = manager.diagnostic_cache_entry_metrics(&victim_uri).unwrap();
    assert_eq!(old_count, 1);
    assert!(
        manager
            .diagnostics_block(&victim, Duration::ZERO)
            .await
            .unwrap()
            .contains("stale diagnostic")
    );

    // The valid publish is applied as an empty tombstone when its payload
    // cannot be retained: callers are notified, the version advances, and the
    // superseded diagnostics can no longer be queried.
    assert!(manager.publish_diagnostics_for_test(&victim_uri, "rust", large_publish()));
    assert_eq!(
        manager.diagnostic_cache_entry_metrics(&victim_uri),
        Some((old_version + 1, 0))
    );
    assert!(
        manager
            .diagnostics_block(&victim, Duration::ZERO)
            .await
            .is_none()
    );
    let (_, total_bytes, _, _, _) = manager.diagnostic_cache_metrics();
    assert!(total_bytes <= crate::extras::lsp::client::MAX_DIAGNOSTIC_CACHE_BYTES);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_aggregate_binding_uses_manager_root_canonical_representation() {
    let root = TempRoot::new("windows-canonical-binding");
    let file = root.path().join("main.rs");
    std::fs::write(&file, "fn main() {}").unwrap();
    let canonical = file.canonicalize().unwrap();
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
    let uri = crate::extras::lsp::client::file_uri(&canonical).unwrap();
    assert!(manager.publish_diagnostics_for_test(
        &uri,
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "windows canonical diagnostic",
        )],
    ));
    let tool = LspTool::new(
        manager,
        Some(permission(root.path(), PermissionConfig::default())),
        None,
    );
    let output = tool.call(LspArgs { path: None }).await.unwrap();
    assert!(output.contains("main.rs:1:1 error: windows canonical diagnostic"));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_lsp_tool_verbatim_canonical_path_matches_ordinary_configured_deny() {
    let root = TempRoot::new("windows-verbatim-deny");
    let file = root.path().join("secret.rs");
    std::fs::write(&file, "secret").unwrap();
    let ordinary_policy_path = file.to_string_lossy().into_owned();
    let config = PermissionConfig {
        read: Some(ToolPerm::Granular(
            [(ordinary_policy_path.clone(), Action::Deny)].into(),
        )),
        ..PermissionConfig::default()
    };
    let (tool, _, manager) = lsp_tool(root.path(), config);

    let error = tool
        .call(LspArgs {
            path: Some(ordinary_policy_path),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Permission denied:"), "{error}");
    assert_eq!(manager.cached_client_count().await, 0);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_lsp_tool_verbatim_external_path_matches_ordinary_external_rule() {
    let root = TempRoot::new("windows-verbatim-external-root");
    let external = TempRoot::new("windows-verbatim-external-file");
    let file = external.path().join("secret.rs");
    std::fs::write(&file, "secret").unwrap();
    let ordinary_policy_path = file.to_string_lossy().into_owned();
    let config = PermissionConfig {
        read: Some(ToolPerm::Simple(Action::Allow)),
        external_directory: Some([(ordinary_policy_path.clone(), Action::Deny)].into()),
        ..PermissionConfig::default()
    };
    let (tool, _, manager) = lsp_tool(root.path(), config);

    let error = tool
        .call(LspArgs {
            path: Some(ordinary_policy_path),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Permission denied:"), "{error}");
    assert_eq!(manager.cached_client_count().await, 0);
}

#[cfg(windows)]
#[tokio::test]
async fn windows_lsp_tool_matches_ordinary_and_verbatim_raw_external_regex_denies() {
    let root = TempRoot::new("windows-verbatim-external-regex-root");
    let external = TempRoot::new("windows-verbatim-external-regex-file");
    let file = external.path().join("secret.rs");
    std::fs::write(&file, "secret").unwrap();
    let ordinary_policy_path = file.to_string_lossy().into_owned();
    let verbatim_policy_path = file.canonicalize().unwrap().to_string_lossy().into_owned();
    assert!(verbatim_policy_path.starts_with(r"\\?\"));
    assert_ne!(ordinary_policy_path, verbatim_policy_path);

    for denied_path in [&ordinary_policy_path, &verbatim_policy_path] {
        let mut external_rules =
            HashMap::from([(format!("^{}$", regex::escape(denied_path)), Action::Deny)]);
        if denied_path == &verbatim_policy_path {
            // A matching ordinary-form allow must not mask the deny found on
            // the canonical verbatim representation.
            external_rules.insert(
                format!("^{}$", regex::escape(&ordinary_policy_path)),
                Action::Allow,
            );
        }
        let configs = PermissionConfigs {
            glob: PermissionConfig::default(),
            regex: PermissionConfig {
                read: Some(ToolPerm::Simple(Action::Allow)),
                external_directory: Some(external_rules),
                ..PermissionConfig::default()
            },
        };
        let permission = Arc::new(Mutex::new(
            PermissionChecker::new(
                &configs,
                SecurityMode::Standard,
                Some(root.path().to_path_buf()),
                Some(vec!["standard".to_string()]),
            )
            .unwrap(),
        ));
        let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());
        let tool = LspTool::new(manager.clone(), Some(permission), None);

        let error = tool
            .call(LspArgs {
                path: Some(ordinary_policy_path.clone()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("Permission denied:"), "{error}");
        assert_eq!(manager.cached_client_count().await, 0);
    }
}

#[tokio::test]
async fn explicit_lsp_allow_always_suppresses_later_asks() {
    let root = TempRoot::new("explicit-allow-always");
    let file = root.path().join("sample.unknownext");
    std::fs::write(&file, "contents").unwrap();
    let canonical = file.canonicalize().unwrap();
    let (tool, mut ask_rx, _) = lsp_tool(root.path(), read_permission(Action::Ask));
    let tool = Arc::new(tool);

    let first_tool = tool.clone();
    let first_path = file.clone();
    let first = tokio::spawn(async move {
        first_tool
            .call(LspArgs {
                path: Some(first_path.display().to_string()),
            })
            .await
    });
    let request = ask_rx.as_mut().unwrap().recv().await.unwrap();
    assert_eq!(Path::new(&request.input), canonical);
    let pattern = crate::ui::utils::suggest_pattern(&request.tool, &request.input);
    assert_ne!(pattern, "*");
    request
        .reply
        .send(UserDecision::AllowAlways(pattern))
        .unwrap();
    first.await.unwrap().unwrap();

    let second = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(LspArgs {
            path: Some(file.display().to_string()),
        }),
    )
    .await
    .expect("stored path pattern must suppress a later Ask");
    assert!(second.is_ok(), "{second:?}");
    assert!(ask_rx.as_mut().unwrap().try_recv().is_err());
}

#[tokio::test]
async fn dropped_lsp_ask_reply_fails_closed_before_manager_access() {
    let root = TempRoot::new("dropped-ask");
    let file = root.path().join("sample.rs");
    std::fs::write(&file, "contents").unwrap();
    let (tool, mut ask_rx, manager) = lsp_tool(root.path(), read_permission(Action::Ask));

    let task = tokio::spawn(async move {
        tool.call(LspArgs {
            path: Some(file.display().to_string()),
        })
        .await
    });
    let request = ask_rx.as_mut().unwrap().recv().await.unwrap();
    drop(request);
    let error = task.await.unwrap().unwrap_err().to_string();
    assert_eq!(error, "Permission denied by user");
    assert_eq!(manager.cached_client_count().await, 0);
}

#[cfg(unix)]
#[tokio::test]
#[allow(unsafe_code)]
async fn explicit_fifo_is_rejected_promptly_before_lsp_client_access() {
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn mkfifo(path: *const c_char, mode: u32) -> i32;
    }

    let root = TempRoot::new("explicit-fifo");
    let fifo = root.path().join("blocked.guarded");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo_c` is a valid, NUL-terminated path for the duration of
    // the call. The return value is checked before the path is used.
    assert_eq!(unsafe { mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

    let mut cfg = LspConfig::default();
    cfg.servers.insert(
        "guarded".to_string(),
        custom("definitely-not-a-real-language-server", &[".guarded"]),
    );
    let manager = LspManager::new(&cfg, root.path().to_path_buf());
    let tool = LspTool::new(
        manager.clone(),
        Some(permission(root.path(), read_permission(Action::Allow))),
        None,
    );
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(LspArgs {
            path: Some(fifo.display().to_string()),
        }),
    )
    .await
    .expect("a FIFO query must never block while opening the node");
    let error = result.unwrap_err().to_string();
    assert!(error.contains("not a regular file"), "{error}");
    assert_eq!(manager.cached_client_count().await, 0);
}

// Darwin filesystems reject invalid-byte names at creation time. Linux keeps
// the end-to-end regression while the in-memory collision check above covers
// the UTF-8 gate on every Unix target.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_non_utf8_canonical_path_cannot_reuse_lossy_permission_key() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let root = TempRoot::new("explicit-non-utf8");
    let valid = root.path().join("sample-�.guarded");
    let invalid = root
        .path()
        .join(OsString::from_vec(b"sample-\xff.guarded".to_vec()));
    let requested = root.path().join("requested.guarded");
    std::fs::write(&valid, "valid UTF-8 path").unwrap();
    std::fs::write(&invalid, "invalid UTF-8 path").unwrap();
    symlink(&invalid, &requested).unwrap();

    let valid = valid.canonicalize().unwrap();
    let invalid = invalid.canonicalize().unwrap();
    assert_ne!(valid, invalid);
    assert_eq!(valid.to_str().unwrap(), invalid.to_string_lossy());

    // This exact allow is for the real U+FFFD filename. Lossy conversion of
    // the invalid-byte filename would collide with it and incorrectly grant
    // access under the previous implementation.
    let config = PermissionConfig {
        default: Some(Action::Deny),
        read: Some(ToolPerm::Granular(
            [(valid.to_str().unwrap().to_string(), Action::Allow)]
                .into_iter()
                .collect(),
        )),
        ..PermissionConfig::default()
    };
    let mut cfg = LspConfig::default();
    cfg.servers.insert(
        "guarded".to_string(),
        custom("definitely-not-a-real-language-server", &[".guarded"]),
    );
    let manager = LspManager::new(&cfg, root.path().to_path_buf());
    let tool = LspTool::new(manager.clone(), Some(permission(root.path(), config)), None);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        tool.call(LspArgs {
            path: Some(requested.display().to_string()),
        }),
    )
    .await
    .expect("a non-UTF-8 canonical path must fail before LSP access");
    let error = result.unwrap_err().to_string();
    assert!(error.contains("require a UTF-8 file path"), "{error}");
    assert_eq!(manager.cached_client_count().await, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_replacement_during_ask_never_reaches_lsp_client() {
    use std::os::unix::fs::symlink;

    let root = TempRoot::new("sync-swap-root");
    let external = TempRoot::new("sync-swap-external");
    let file = root.path().join("sample.guarded");
    let denied = external.path().join("denied.guarded");
    std::fs::write(&file, "allowed content").unwrap();
    std::fs::write(&denied, "DENIED CONTENT").unwrap();

    let mut cfg = LspConfig::default();
    cfg.servers.insert(
        "guarded".to_string(),
        custom("definitely-not-a-real-language-server", &[".guarded"]),
    );
    let manager = LspManager::new(&cfg, root.path().to_path_buf());
    let (ask_tx, mut ask_rx) = tokio::sync::mpsc::channel(1);
    let tool = LspTool::new(
        manager.clone(),
        Some(permission(root.path(), read_permission(Action::Ask))),
        Some(ask_tx),
    );
    let queried = file.clone();
    let task = tokio::spawn(async move {
        tool.call(LspArgs {
            path: Some(queried.display().to_string()),
        })
        .await
    });
    let request = ask_rx.recv().await.unwrap();
    std::fs::remove_file(&file).unwrap();
    symlink(&denied, &file).unwrap();
    request.reply.send(UserDecision::AllowOnce).unwrap();

    let output = task.await.unwrap().unwrap();
    assert!(output.starts_with("No diagnostics for "), "{output}");
    assert_eq!(
        manager.cached_client_count().await,
        0,
        "stable-file rejection must happen before client launch"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn aggregate_rejects_symlink_alias_uri_variants_and_stale_retargets() {
    use std::os::unix::fs::symlink;

    let root = TempRoot::new("stale-cache-root");
    let external = TempRoot::new("stale-cache-external");
    let file = root.path().join("space ü.rs");
    let alias = root.path().join("alias.rs");
    let denied = external.path().join("denied.rs");
    std::fs::write(&file, "allowed").unwrap();
    std::fs::write(&denied, "denied").unwrap();
    symlink(&denied, &alias).unwrap();
    let file = file.canonicalize().unwrap();
    let denied = denied.canonicalize().unwrap();
    let manager = LspManager::new(&LspConfig::default(), root.path().to_path_buf());

    assert!(!manager.publish_diagnostics_for_test(
        &crate::extras::lsp::client::file_uri(&alias).unwrap(),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "alias secret",
        )],
    ));
    let canonical_uri = crate::extras::lsp::client::file_uri(&file).unwrap();
    assert!(!manager.publish_diagnostics_for_test(
        &canonical_uri.replace("%C3%BC", "%c3%bc"),
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "variant secret",
        )],
    ));
    assert!(manager.publish_diagnostics_for_test(
        &canonical_uri,
        "rust",
        vec![diag(
            lsp_types::DiagnosticSeverity::ERROR,
            0,
            0,
            "stale diagnostic",
        )],
    ));

    std::fs::remove_file(&file).unwrap();
    std::fs::write(&file, "replacement").unwrap();
    let replacement_tool = LspTool::new(
        manager.clone(),
        Some(permission(root.path(), PermissionConfig::default())),
        None,
    );
    assert_eq!(
        replacement_tool.call(LspArgs { path: None }).await.unwrap(),
        "No diagnostics.",
        "a same-path inode replacement must invalidate cached diagnostics"
    );

    std::fs::remove_file(&file).unwrap();
    symlink(&denied, &file).unwrap();
    let config = PermissionConfig {
        external_directory: Some(
            [(denied.display().to_string(), Action::Deny)]
                .into_iter()
                .collect(),
        ),
        ..PermissionConfig::default()
    };
    let tool = LspTool::new(manager, Some(permission(root.path(), config)), None);
    let output = tool.call(LspArgs { path: None }).await.unwrap();
    assert_eq!(output, "No diagnostics.");
}
