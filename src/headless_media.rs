//! Image attachments for headless `-p` runs: `-i/--image FILE` and `image`
//! content blocks on `--input-format stream-json`.
//!
//! Limits and downscaling are the TUI's (`MAX_USER_IMAGES`, per-image and total
//! bytes); the persisted record is the same typed user message, and a model the
//! catalog lists as text-only refuses the run before any request is made.

use std::path::{Path, PathBuf};

/// Where an image block of a stream-json user message gets its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageSource {
    Base64 {
        media_type: Option<String>,
        data: String,
    },
    Path(PathBuf),
}

/// One parsed `--input-format stream-json` user line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamUserInput {
    pub text: String,
    pub images: Vec<ImageSource>,
}

/// `{"type":"user","message":{"content": "..." | [blocks]}}` or the flat
/// `{"type":"user","content":...}`. Blocks are `{"type":"text","text"}` and
/// `{"type":"image","source":{"type":"base64","media_type","data"}}` (Claude
/// Code shape) or `{"type":"image","source":{"type":"path","path"}}`.
pub(crate) fn parse_stream_user_input(line: &str) -> Result<StreamUserInput, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| format!("invalid JSON: {error}"))?;
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("user") => {}
        Some(other) => return Err(format!("unsupported input type {other:?} (only \"user\")")),
        None => return Err("input line has no \"type\"".into()),
    }
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))
        .or_else(|| value.get("content"))
        .ok_or("user message has no content")?;
    let mut input = StreamUserInput::default();
    match content {
        serde_json::Value::String(text) => input.text.clone_from(text),
        serde_json::Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => parts.push(
                        block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .ok_or("text block has no \"text\"")?,
                    ),
                    Some("image") => input.images.push(parse_image_source(block)?),
                    _ => return Err("only text and image content blocks are supported".into()),
                }
            }
            input.text = parts.join("\n");
        }
        _ => return Err("content must be a string or an array of content blocks".into()),
    }
    if input.text.trim().is_empty() {
        return Err(if input.images.is_empty() {
            "empty user message".into()
        } else {
            "an image message needs accompanying text".into()
        });
    }
    Ok(input)
}

fn parse_image_source(block: &serde_json::Value) -> Result<ImageSource, String> {
    let source = block.get("source").ok_or("image block has no \"source\"")?;
    let field = |name: &str| source.get(name).and_then(serde_json::Value::as_str);
    match field("type") {
        Some("base64") => Ok(ImageSource::Base64 {
            media_type: field("media_type").map(str::to_owned),
            data: field("data")
                .filter(|data| !data.is_empty())
                .ok_or("base64 image source has no \"data\"")?
                .to_owned(),
        }),
        Some("path") => Ok(ImageSource::Path(PathBuf::from(
            field("path")
                .filter(|path| !path.is_empty())
                .ok_or("path image source has no \"path\"")?,
        ))),
        _ => Err("image source type must be \"base64\" or \"path\"".into()),
    }
}

#[cfg(feature = "multimodal")]
pub(crate) type Attachment = crate::extras::multimodal::MediaAttachment;
/// Without the `multimodal` feature no attachment can exist.
#[cfg(not(feature = "multimodal"))]
#[derive(Debug)]
pub(crate) enum Attachment {}

#[cfg(not(feature = "multimodal"))]
pub(crate) fn resolve_images(
    _workspace: &Path,
    sources: Vec<ImageSource>,
) -> Result<Vec<Attachment>, String> {
    if sources.is_empty() {
        Ok(Vec::new())
    } else {
        Err("this build has no multimodal support, so images cannot be attached".into())
    }
}

/// Load, validate and downscale the images. Counts and sizes are checked
/// before decoding or reading anything large; failures name the image.
#[cfg(feature = "multimodal")]
pub(crate) fn resolve_images(
    workspace: &Path,
    sources: Vec<ImageSource>,
) -> Result<Vec<Attachment>, String> {
    use crate::session::user::{MAX_USER_IMAGE_BYTES, MAX_USER_IMAGE_TOTAL, MAX_USER_IMAGES};
    use base64::Engine as _;

    if sources.len() > MAX_USER_IMAGES {
        return Err(format!(
            "at most {MAX_USER_IMAGES} images can be attached to one message"
        ));
    }
    let mut out = Vec::with_capacity(sources.len());
    let mut total = 0usize;
    for (index, source) in sources.into_iter().enumerate() {
        let name = format!("image {}", index + 1);
        let mut attachment = match source {
            ImageSource::Path(path) => {
                let path = if path.is_absolute() {
                    path
                } else {
                    workspace.join(path)
                };
                let attachment = crate::extras::multimodal::load_attachment(&path)
                    .map_err(|error| format!("{name} ({}): {error}", path.display()))?;
                if !matches!(attachment, Attachment::Image { .. }) {
                    return Err(format!(
                        "{name} ({}): only images (png, jpeg, gif, webp) can be attached",
                        path.display()
                    ));
                }
                attachment
            }
            ImageSource::Base64 { media_type, data } => {
                // Bound the encoded size before allocating the decoded bytes.
                if data.len() > crate::extras::multimodal::MAX_MEDIA_BYTES as usize / 3 * 4 + 8 {
                    return Err(format!("{name}: base64 data is too large"));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data.as_bytes())
                    .map_err(|_| format!("{name}: invalid base64 data"))?;
                crate::extras::multimodal::image_from_bytes(
                    Path::new("stdin-image"),
                    bytes,
                    media_type.as_deref(),
                )
                .map_err(|error| format!("{name}: {error}"))?
            }
        };
        crate::extras::multimodal::fit_image_attachment(&mut attachment)
            .map_err(|error| format!("{name}: {error}"))?;
        if let Attachment::Image { data, .. } = &attachment {
            total = total.saturating_add(data.len());
            if data.len() > MAX_USER_IMAGE_BYTES || total > MAX_USER_IMAGE_TOTAL {
                return Err(format!(
                    "{name}: attached images exceed the {} MiB total limit",
                    MAX_USER_IMAGE_TOTAL / (1024 * 1024)
                ));
            }
        }
        out.push(attachment);
    }
    Ok(out)
}

/// Refuse images for a model the catalog lists as text-only. An unknown model
/// is allowed: the provider then reports its own error.
#[cfg(feature = "multimodal")]
pub(crate) fn ensure_vision(provider: &str, model: &str) -> Result<(), String> {
    vision_verdict(
        crate::models_catalog::model_image_input(provider, model),
        provider,
        model,
    )
}

#[cfg(feature = "multimodal")]
fn vision_verdict(known: Option<bool>, provider: &str, model: &str) -> Result<(), String> {
    match known {
        Some(false) => Err(format!(
            "model {model} ({provider}) does not accept image input; choose a vision model or drop the images"
        )),
        _ => Ok(()),
    }
}

/// The typed user message that persists and replays the images.
#[cfg(feature = "multimodal")]
pub(crate) fn images_user_message(
    media: &[Attachment],
) -> anyhow::Result<crate::session::user::PersistedUserMessage> {
    use crate::session::user::{ImageMime, PersistedUserMessage, UserBlock};
    use base64::Engine as _;

    let mut blocks = Vec::new();
    for item in media {
        if let Attachment::Image { data, mime, .. } = item {
            blocks.push(UserBlock::Image {
                data: base64::engine::general_purpose::STANDARD.encode(data),
                mime_type: ImageMime::parse(mime)
                    .ok_or_else(|| anyhow::anyhow!("unsupported image MIME type"))?,
            });
        }
    }
    PersistedUserMessage::new(blocks).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_shapes_still_parse_and_non_text_non_image_blocks_still_fail() {
        let parsed = parse_stream_user_input(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#,
        )
        .unwrap();
        assert_eq!(parsed.text, "a\nb");
        assert!(parsed.images.is_empty());
        for bad in [
            "not json",
            r#"{"type":"control"}"#,
            r#"{"type":"user","content":"  "}"#,
            r#"{"type":"user","content":[{"type":"audio","source":{}}]}"#,
            r#"{"type":"user","content":[{"type":"text"}]}"#,
            r#"{"type":"user","content":[{"type":"image","source":{}},{"type":"text","text":"x"}]}"#,
            r#"{"type":"user","content":[{"type":"image","source":{"type":"url","url":"https://x/y.png"}},{"type":"text","text":"x"}]}"#,
            r#"{"type":"user","content":[{"type":"image","source":{"type":"base64","data":""}},{"type":"text","text":"x"}]}"#,
            r#"{"type":"user","content":[{"type":"image","source":{"type":"path","path":""}},{"type":"text","text":"x"}]}"#,
        ] {
            assert!(parse_stream_user_input(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn image_blocks_parse_in_both_source_shapes_but_need_text() {
        let parsed = parse_stream_user_input(
            r#"{"type":"user","message":{"content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},
                {"type":"image","source":{"type":"path","path":"shot.png"}},
                {"type":"text","text":"what is this"}]}}"#,
        )
        .unwrap();
        assert_eq!(parsed.text, "what is this");
        assert_eq!(
            parsed.images,
            vec![
                ImageSource::Base64 {
                    media_type: Some("image/png".into()),
                    data: "AAAA".into()
                },
                ImageSource::Path("shot.png".into())
            ]
        );
        let image_only = r#"{"type":"user","content":[{"type":"image","source":{"type":"path","path":"a.png"}}]}"#;
        assert!(
            parse_stream_user_input(image_only)
                .unwrap_err()
                .contains("needs accompanying text")
        );
    }

    #[cfg(not(feature = "multimodal"))]
    #[test]
    fn images_are_refused_without_the_multimodal_feature() {
        assert!(
            resolve_images(Path::new("."), Vec::new())
                .unwrap()
                .is_empty()
        );
        assert!(
            resolve_images(Path::new("."), vec![ImageSource::Path("a.png".into())])
                .unwrap_err()
                .contains("no multimodal support")
        );
    }

    #[cfg(feature = "multimodal")]
    mod multimodal {
        use super::super::*;
        use base64::Engine as _;

        fn png() -> Vec<u8> {
            // The signature is all `image_from_bytes` and `load_attachment` inspect.
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.extend_from_slice(&[0; 32]);
            bytes
        }

        fn b64(bytes: &[u8]) -> String {
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }

        #[test]
        fn base64_and_path_images_load_and_persist_as_one_typed_message() {
            let dir = std::env::temp_dir().join(format!("headless-media-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("shot.png"), png()).unwrap();
            let media = resolve_images(
                &dir,
                vec![
                    ImageSource::Base64 {
                        media_type: Some("image/png".into()),
                        data: b64(&png()),
                    },
                    ImageSource::Path("shot.png".into()),
                ],
            )
            .unwrap();
            assert_eq!(media.len(), 2);
            let message = images_user_message(&media).unwrap();
            assert_eq!(
                message.to_message(),
                images_user_message(&media).unwrap().to_message()
            );
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn hostile_images_are_rejected_before_use() {
            let work = Path::new(".");
            let base64 = |media_type: Option<&str>, bytes: &[u8]| ImageSource::Base64 {
                media_type: media_type.map(str::to_owned),
                data: b64(bytes),
            };
            // Not an image, wrong declared type, empty, invalid base64.
            assert!(resolve_images(work, vec![base64(None, b"plain text")]).is_err());
            assert!(
                resolve_images(work, vec![base64(Some("image/jpeg"), &png())])
                    .unwrap_err()
                    .contains("does not match")
            );
            assert!(resolve_images(work, vec![base64(None, b"")]).is_err());
            assert!(
                resolve_images(
                    work,
                    vec![ImageSource::Base64 {
                        media_type: None,
                        data: "!!!not base64".into()
                    }]
                )
                .unwrap_err()
                .contains("invalid base64")
            );
            // Oversized encoded payloads are refused before decoding.
            let huge = "A".repeat(crate::extras::multimodal::MAX_MEDIA_BYTES as usize / 3 * 4 + 64);
            assert!(
                resolve_images(
                    work,
                    vec![ImageSource::Base64 {
                        media_type: None,
                        data: huge
                    }]
                )
                .unwrap_err()
                .contains("too large")
            );
            // Count bound, missing and non-image paths.
            let many = (0..=crate::session::user::MAX_USER_IMAGES)
                .map(|_| base64(None, &png()))
                .collect();
            assert!(resolve_images(work, many).unwrap_err().contains("at most"));
            assert!(
                resolve_images(work, vec![ImageSource::Path("/nonexistent/x.png".into())]).is_err()
            );
            assert!(resolve_images(work, vec![ImageSource::Path("Cargo.toml".into())]).is_err());
        }

        #[test]
        fn text_only_catalog_models_refuse_images_and_unknown_models_pass() {
            assert!(ensure_vision("no-such-provider", "no-such-model").is_ok());
            assert!(vision_verdict(None, "p", "m").is_ok());
            assert!(vision_verdict(Some(true), "p", "m").is_ok());
            let refusal = vision_verdict(Some(false), "openai", "text-only").unwrap_err();
            assert!(
                refusal.contains("text-only") && refusal.contains("does not accept image input")
            );
        }
    }
}
