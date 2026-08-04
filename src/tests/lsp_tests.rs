use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use compact_str::CompactString;
use tokio::io::AsyncWriteExt;

use crate::config::types::{LspConfig, LspServerConfig};
use crate::extras::lsp::client::{DiagStore, store_diagnostics_for_test};
use crate::extras::lsp::registry::{resolve_servers, server_for_path};
use crate::extras::lsp::{LspManager, rpc};

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
    assert!(
        first
            .source
            .as_ref()
            .is_some_and(|source| source.len() <= 256)
    );
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
    let manager = LspManager::new(&LspConfig::default(), PathBuf::from("/tmp"));
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
    let manager = LspManager::new(&LspConfig::default(), PathBuf::from("/tmp"));
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

#[test]
fn clean_project_reports_nothing() {
    let manager = LspManager::new(&LspConfig::default(), PathBuf::from("/tmp"));
    assert!(manager.all_diagnostics_block().is_none());
    manager.inject_diagnostics(
        "file:///tmp/x.rs",
        "rust",
        vec![diag(lsp_types::DiagnosticSeverity::ERROR, 0, 0, "boom")],
    );
    let block = manager.all_diagnostics_block().unwrap();
    assert!(block.contains("x.rs:1:1 error: boom"), "{block}");
}
