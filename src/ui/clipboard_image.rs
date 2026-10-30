//! Operator-initiated clipboard images; fixed helpers, bounded bytes, no paths.
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::Duration;

use base64::Engine;

use crate::extras::multimodal::{ClipboardImageInfo, MediaAttachment};
use crate::sandbox::{CommandLimits, CommandStatus, Sandbox};
use crate::session::user::{MAX_USER_IMAGE_BYTES, MAX_USER_IMAGE_TOTAL, MAX_USER_IMAGES};

const ENCODED_LIMIT: usize = MAX_USER_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

pub(super) struct ClipboardImage {
    data: Vec<u8>,
    info: ClipboardImageInfo,
}

pub(super) trait ClipboardImageReader {
    fn read<'a>(
        &'a self,
        sandbox: &'a Sandbox,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<ClipboardImage>>> + Send + 'a>>;
}

pub(super) struct SystemClipboard;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "clipboard image is invalid or exceeds the image limits",
    )
}

pub(super) fn decode_png(data: Vec<u8>) -> io::Result<ClipboardImage> {
    if data.len() > MAX_USER_IMAGE_BYTES
        || data.len() < 33
        || !data.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR")
    {
        return Err(invalid());
    }
    let width = u32::from_be_bytes(data[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(data[20..24].try_into().unwrap());
    if width == 0
        || height == 0
        || width > 16_384
        || height > 16_384
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(invalid());
    }
    Ok(ClipboardImage {
        data,
        info: ClipboardImageInfo { width, height },
    })
}

fn decode_output(bytes: Vec<u8>, encoded: bool) -> io::Result<Option<ClipboardImage>> {
    if bytes.is_empty() || (encoded && bytes.iter().all(u8::is_ascii_whitespace)) {
        return Ok(None);
    }
    let data = if encoded {
        // Bound before allocating the decoded image. The fixed helpers emit a
        // single base64 value with at most the interpreter's trailing newline.
        if bytes.len() > ENCODED_LIMIT + 2 {
            return Err(invalid());
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid())?
            .trim_end_matches(['\r', '\n']);
        if text.len() > ENCODED_LIMIT {
            return Err(invalid());
        }
        let padding = text.bytes().rev().take_while(|byte| *byte == b'=').count();
        if text.len() % 4 != 0
            || padding > 2
            || (text.len() / 4 * 3).saturating_sub(padding) > MAX_USER_IMAGE_BYTES
        {
            return Err(invalid());
        }
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| invalid())?
    } else {
        bytes
    };
    decode_png(data).map(Some)
}

async fn read_helper(
    sandbox: &Sandbox,
    mut command: tokio::process::Command,
    encoded: bool,
) -> io::Result<Option<ClipboardImage>> {
    command.env_clear();
    // Desktop IPC needs these owner-supplied values. Provider credentials and
    // shell startup variables never enter the fixed clipboard interpreter.
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
        "USERPROFILE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let stdout_bytes = if encoded {
        ENCODED_LIMIT + 2
    } else {
        MAX_USER_IMAGE_BYTES
    };
    let result = sandbox
        .output_support_command(
            command,
            CommandLimits {
                timeout: Duration::from_secs(3),
                stdout_bytes,
                stderr_bytes: 4096,
                combined_bytes: stdout_bytes + 4096,
            },
        )
        .await?;
    if result.status != CommandStatus::Completed || result.descendants_escaped {
        return Err(io::Error::other(
            "clipboard image reader did not finish completely",
        ));
    }
    if !result.exit_status.is_some_and(|status| status.success()) {
        return Err(io::Error::other("clipboard image is unavailable"));
    }
    decode_output(result.stdout, encoded)
}

impl ClipboardImageReader for SystemClipboard {
    fn read<'a>(
        &'a self,
        sandbox: &'a Sandbox,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<ClipboardImage>>> + Send + 'a>> {
        crate::startup::heap_future(|| async move {
            #[cfg(target_os = "macos")]
            {
                let mut command = tokio::process::Command::new("/usr/bin/osascript");
                command.args(["-l", "JavaScript", "-e", MACOS_IMAGE_SCRIPT]);
                read_helper(sandbox, command, true).await
            }
            #[cfg(windows)]
            {
                let root = std::env::var_os("SystemRoot")
                    .ok_or_else(|| io::Error::other("Windows clipboard reader is unavailable"))?;
                let program = std::path::PathBuf::from(root)
                    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
                let mut command = tokio::process::Command::new(program);
                command.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-STA",
                    "-Command",
                    WINDOWS_IMAGE_SCRIPT,
                ]);
                read_helper(sandbox, command, true).await
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                let (program, args): (&str, &[&str]) =
                    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                        ("wl-paste", &["--no-newline", "--type", "image/png"])
                    } else {
                        (
                            "xclip",
                            &["-selection", "clipboard", "-out", "-target", "image/png"],
                        )
                    };
                let mut command = tokio::process::Command::new(program);
                command.args(args);
                read_helper(sandbox, command, false).await
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = sandbox;
                Err(io::Error::other(
                    "clipboard images are unavailable on this platform",
                ))
            }
        })
    }
}

#[cfg(target_os = "macos")]
const MACOS_IMAGE_SCRIPT: &str = r#"function run() {
ObjC.import('AppKit');
var p = $.NSPasteboard.generalPasteboard;
var d = p.dataForType($('public.png'));
if (d.isNil()) {
  var t = p.dataForType($('public.tiff'));
  if (t.isNil()) return '';
  if (Number(t.length) > 67108864) throw Error('image limit');
  var b = $.NSBitmapImageRep.imageRepWithData(t);
  if (b.isNil() || Number(b.pixelsWide) * Number(b.pixelsHigh) > 16777216) throw Error('image limit');
  d = b.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $.NSDictionary.dictionary);
}
if (d.isNil() || Number(d.length) > 2097152) throw Error('image limit');
return ObjC.unwrap(d.base64EncodedStringWithOptions(0));
}"#;

#[cfg(windows)]
const WINDOWS_IMAGE_SCRIPT: &str = r#"$ErrorActionPreference='Stop';
Add-Type -AssemblyName System.Windows.Forms,System.Drawing;
$image=[System.Windows.Forms.Clipboard]::GetImage();
if ($null -eq $image) { exit 0 };
$stream=[System.IO.MemoryStream]::new();
try {
 if ([long]$image.Width * [long]$image.Height -gt 16777216) { throw 'image limit' };
 $image.Save($stream,[System.Drawing.Imaging.ImageFormat]::Png);
 if ($stream.Length -gt 2097152) { throw 'image limit' };
 [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()));
} finally { $stream.Dispose(); $image.Dispose() }"#;

pub(super) fn stage_image(
    pending: &mut Vec<MediaAttachment>,
    image: ClipboardImage,
) -> io::Result<String> {
    let image_count = pending
        .iter()
        .filter(|item| matches!(item, MediaAttachment::Image { .. }))
        .count();
    let bytes = pending
        .iter()
        .filter_map(|item| match item {
            MediaAttachment::Image { data, .. } => Some(data.len()),
            _ => None,
        })
        .fold(0usize, usize::saturating_add);
    if image_count >= MAX_USER_IMAGES
        || bytes.saturating_add(image.data.len()) > MAX_USER_IMAGE_TOTAL
    {
        return Err(invalid());
    }
    let label = format!(
        "[image {}: {}x{} png]",
        pending.len() + 1,
        image.info.width,
        image.info.height
    );
    pending.push(MediaAttachment::Image {
        // Display identity only: clipboard bytes are never reopened by name.
        path: format!("clipboard-{}.png", uuid::Uuid::new_v4()).into(),
        data: image.data,
        mime: "image/png".into(),
        clipboard: Some(image.info),
    });
    Ok(label)
}

pub(super) fn labels(pending: &[MediaAttachment]) -> String {
    pending
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            MediaAttachment::Image {
                clipboard: Some(info),
                ..
            } => Some(format!(
                "[image {}: {}x{} png]",
                i + 1,
                info.width,
                info.height
            )),
            MediaAttachment::Image { mime, .. } => Some(format!(
                "[image {}: {}]",
                i + 1,
                crate::session::user::ImageMime::parse(mime)
                    .map_or("unsupported", |mime| mime.as_str())
            )),
            _ => None,
        })
        .take(MAX_USER_IMAGES)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn user_message(
    pending: &[MediaAttachment],
    text: &str,
) -> anyhow::Result<Option<crate::session::user::PersistedUserMessage>> {
    use crate::session::user::{ImageMime, PersistedUserMessage, UserBlock};
    if !pending
        .iter()
        .any(|item| matches!(item, MediaAttachment::Image { .. }))
    {
        return Ok(None);
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(UserBlock::Text {
            text: text.to_owned(),
        });
    }
    let mut count = 0usize;
    let mut bytes = 0usize;
    for item in pending {
        if let MediaAttachment::Image { data, mime, .. } = item {
            count += 1;
            bytes = bytes.saturating_add(data.len());
            anyhow::ensure!(
                count <= MAX_USER_IMAGES
                    && data.len() <= MAX_USER_IMAGE_BYTES
                    && bytes <= MAX_USER_IMAGE_TOTAL,
                "image attachment limits exceeded"
            );
            blocks.push(UserBlock::Image {
                data: base64::engine::general_purpose::STANDARD.encode(data),
                mime_type: ImageMime::parse(mime)
                    .ok_or_else(|| anyhow::anyhow!("unsupported image attachment MIME type"))?,
            });
        }
    }
    PersistedUserMessage::new(blocks)
        .map(Some)
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XklcAAAAASUVORK5CYII=").unwrap()
    }

    #[test]
    fn clipboard74_caps_precede_decode_and_header_dimensions_are_bounded() {
        let raw = png();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let image = decode_output(encoded.into_bytes(), true).unwrap().unwrap();
        assert_eq!((image.info.width, image.info.height), (1, 1));
        assert_eq!(image.data, raw);
        assert!(decode_output(vec![b'A'; ENCODED_LIMIT + 3], true).is_err());
        let one_over =
            base64::engine::general_purpose::STANDARD.encode(vec![0; MAX_USER_IMAGE_BYTES + 1]);
        assert_eq!(one_over.len(), ENCODED_LIMIT);
        assert!(decode_output(one_over.into_bytes(), true).is_err());
        assert!(decode_output(b"invalid base64".to_vec(), true).is_err());
        assert!(decode_output(b"\n".to_vec(), true).unwrap().is_none());
        for (width, height) in [(0, 1), (1, 0), (16_385, 1), (4097, 4096)] {
            let mut raw = png();
            raw[16..20].copy_from_slice(&u32::to_be_bytes(width));
            raw[20..24].copy_from_slice(&u32::to_be_bytes(height));
            assert!(decode_png(raw).is_err());
        }
        assert!(decode_png(vec![0; MAX_USER_IMAGE_BYTES + 1]).is_err());
        assert!(decode_png(b"file:///private/image.png".to_vec()).is_err());
    }

    #[test]
    fn clipboard74_staging_and_canonical_session_roundtrip_keep_exact_bytes() {
        let mut session = crate::session::Session::new("offline", "fixture", 32_000, "");
        assert!(
            user_message(&session.pending_media, "literal 雪")
                .unwrap()
                .is_none()
        );
        let first = png();
        assert_eq!(
            stage_image(
                &mut session.pending_media,
                decode_png(first.clone()).unwrap()
            )
            .unwrap(),
            "[image 1: 1x1 png]"
        );
        assert_eq!(labels(&session.pending_media), "[image 1: 1x1 png]");
        let user = user_message(&session.pending_media, "literal 雪")
            .unwrap()
            .unwrap();
        let image_only = user_message(&session.pending_media, "").unwrap().unwrap();
        assert_eq!(image_only.literal_text(), "");
        assert!(image_only.has_images());
        assert_eq!(user.literal_text(), "literal 雪");
        session.add_user_message(user.clone());
        let json = serde_json::to_string(&session).unwrap();
        let restored = crate::session::storage::decode_session_json(&json).unwrap();
        assert_eq!(restored.messages[0].user.as_ref(), Some(&user));
        assert!(
            !json.contains("clipboard-"),
            "virtual display identity is not persisted as authority"
        );
        session.pending_media.remove(0);
        assert!(labels(&session.pending_media).is_empty());
        assert!(
            user_message(&session.pending_media, "literal 雪")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn attachment_images_keep_file_mime_bytes_and_canonical_replay() {
        use crate::extras::multimodal::load_attachment;
        let root = crate::tests::OwnedTestDirectory::new("attached-image");
        let path = root.path().join("scan.jpg");
        let bytes = include_bytes!("../agent/tools/fixtures/read-image64/3x2.jpg");
        std::fs::write(&path, bytes).unwrap();
        let mut pending = vec![load_attachment(&path).unwrap()];
        stage_image(&mut pending, decode_png(png()).unwrap()).unwrap();
        assert!(labels(&pending).contains("[image 1: image/jpeg]"));
        let user = user_message(&pending, "read the scan").unwrap().unwrap();
        let image_only = user_message(&pending, "").unwrap().unwrap();
        assert!(image_only.has_images());
        assert!(image_only.literal_text().is_empty());
        let mut session = crate::session::Session::new("offline", "fixture", 32_000, "");
        session.add_user_message(user.clone());
        let encoded = serde_json::to_string(&session).unwrap();
        let restored = crate::session::storage::decode_session_json(&encoded).unwrap();
        assert_eq!(restored.messages[0].user.as_ref(), Some(&user));
        let expected = base64::engine::general_purpose::STANDARD.encode(bytes);
        let blocks = &user.blocks();
        assert!(
            matches!(&blocks[1], crate::session::user::UserBlock::Image { data, mime_type }
            if data == &expected && *mime_type == crate::session::user::ImageMime::Jpeg)
        );
        assert_eq!(
            crate::agent::runner::convert_history(&restored),
            vec![user.to_message()]
        );
        assert!(!encoded.contains(&path.to_string_lossy().to_string()));
        assert_eq!(pending.len(), 2, "construction never consumes the draft");
        if let MediaAttachment::Image { data, .. } = &mut pending[0] {
            data.resize(MAX_USER_IMAGE_BYTES + 1, 0);
        }
        assert!(user_message(&pending, "still retained").is_err());
        assert_eq!(pending.len(), 2);
    }

    #[tokio::test]
    async fn attachment_images_reach_actual_openai_requests_after_save_reload() {
        use crate::provider::{AnyModel, OpenAiModel};
        use rig::completion::{CompletionModel, CompletionRequest};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = crate::tests::OwnedTestDirectory::new("attached-image-wire");
        let path = root.path().join("scan.png");
        let bytes = png();
        std::fs::write(&path, &bytes).unwrap();
        let pending = vec![crate::extras::multimodal::load_attachment(&path).unwrap()];
        let user = user_message(&pending, "read this scan").unwrap().unwrap();
        let mut session = crate::session::Session::new("openai", "fixture", 32_000, "");
        session.add_user_message(user);
        let restored =
            crate::session::storage::decode_session_json(&serde_json::to_string(&session).unwrap())
                .unwrap();
        let expected = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        for style in ["completions", "responses"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let cfg: crate::config::Config = serde_json::from_value(serde_json::json!({
                "custom_providers": {"attachment-wire": {"provider_type":"openai", "base_url":format!("http://{address}"), "api_style":style}}
            })).unwrap();
            let client = crate::provider::create_client(
                "attachment-wire",
                Some("unused-fixture"),
                cfg.custom_providers_map(),
                None,
            )
            .unwrap();
            let model = client.completion_model("owned-vision-fixture");
            let request = CompletionRequest {
                model: None,
                preamble: None,
                chat_history: rig::OneOrMany::many(crate::agent::runner::convert_history(
                    &restored,
                ))
                .unwrap(),
                documents: Vec::new(),
                tools: Vec::new(),
                temperature: None,
                max_tokens: Some(64),
                tool_choice: None,
                additional_params: None,
                output_schema: None,
            };
            let send = async {
                match &model {
                    AnyModel::OpenAI(OpenAiModel::Completions(model)) => {
                        model.completion(request).await.map(|_| ())
                    }
                    AnyModel::OpenAI(OpenAiModel::Responses(model)) => {
                        model.completion(request).await.map(|_| ())
                    }
                    _ => panic!("expected OpenAI transport"),
                }
            };
            let receive = tokio::time::timeout(Duration::from_secs(5), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    assert!(bytes.len() < 64 * 1024);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let length: usize = std::str::from_utf8(&bytes[..end])
                    .unwrap()
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse().unwrap())
                    })
                    .unwrap();
                assert!(length < 64 * 1024);
                while bytes.len() < end + length {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[end..end + length]).unwrap();
                socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
                body
            });
            let (result, body) = tokio::join!(send, receive);
            assert!(result.is_err());
            let body = body.unwrap_or_else(|_| panic!("{style} refused before HTTP: {result:?}"));
            let rows = body
                .get("messages")
                .or_else(|| body.get("input"))
                .unwrap()
                .as_array()
                .unwrap();
            let parts: Vec<_> = rows
                .iter()
                .flat_map(|row| row["content"].as_array().into_iter().flatten())
                .collect();
            assert!(
                parts.iter().any(|part| {
                    (part["type"] == "image_url" && part["image_url"]["url"] == expected)
                        || (part["type"] == "input_image" && part["image_url"] == expected)
                }),
                "{style}: {body}"
            );
            assert!(
                parts.iter().any(|part| part["text"] == "read this scan"),
                "{style}: {body}"
            );
        }
    }

    #[test]
    fn clipboard74_aggregate_refusal_preserves_existing_pending_images() {
        let mut pending = Vec::new();
        let mut raw = png();
        raw.resize(MAX_USER_IMAGE_BYTES, 0);
        stage_image(&mut pending, decode_png(raw.clone()).unwrap()).unwrap();
        stage_image(&mut pending, decode_png(raw).unwrap()).unwrap();
        let before = labels(&pending);
        assert!(stage_image(&mut pending, decode_png(png()).unwrap()).is_err());
        assert_eq!(pending.len(), 2);
        assert_eq!(labels(&pending), before);
    }
}
