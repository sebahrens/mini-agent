#![cfg(feature = "multimodal")]

use crate::extras::multimodal::{MediaAttachment, detect_media, load_attachment};
use std::path::Path;

// --- detect_media tests ---

#[test]
fn detect_media_image_extensions() {
    assert_eq!(detect_media(Path::new("photo.png")), Some("image/png"));
    assert_eq!(detect_media(Path::new("photo.jpg")), Some("image/jpeg"));
    assert_eq!(detect_media(Path::new("photo.jpeg")), Some("image/jpeg"));
    assert_eq!(detect_media(Path::new("photo.GIF")), Some("image/gif"));
    assert_eq!(detect_media(Path::new("photo.webp")), Some("image/webp"));
}

#[test]
fn detect_media_audio_extensions() {
    assert_eq!(detect_media(Path::new("song.mp3")), Some("audio/mpeg"));
    assert_eq!(detect_media(Path::new("song.wav")), Some("audio/wav"));
    assert_eq!(detect_media(Path::new("song.ogg")), Some("audio/ogg"));
    assert_eq!(detect_media(Path::new("song.flac")), Some("audio/flac"));
    assert_eq!(detect_media(Path::new("song.m4a")), Some("audio/mp4"));
    assert_eq!(detect_media(Path::new("song.aac")), Some("audio/aac"));
}

#[test]
fn detect_media_document_extension() {
    assert_eq!(detect_media(Path::new("doc.pdf")), Some("application/pdf"));
}

#[test]
fn detect_media_unknown_returns_none() {
    assert_eq!(detect_media(Path::new("code.rs")), None);
    assert_eq!(detect_media(Path::new("README.md")), None);
    assert_eq!(detect_media(Path::new("script.sh")), None);
    assert_eq!(detect_media(Path::new("Dockerfile")), None);
    assert_eq!(detect_media(Path::new("data.txt")), None);
}

#[test]
fn detect_media_no_extension_returns_none() {
    assert_eq!(detect_media(Path::new("Makefile")), None);
    assert_eq!(detect_media(Path::new("/usr/bin/binary")), None);
}

// --- load_attachment tests ---

#[test]
fn load_attachment_file_not_found() {
    let err = load_attachment(Path::new("/nonexistent/file.png")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn load_attachment_unknown_media_type() {
    let err = load_attachment(Path::new("Cargo.toml")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn load_attachment_rejects_extension_signature_mismatch() {
    let path = std::env::temp_dir().join(format!(
        "mini-agent-media-mismatch-{}.png",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, b"%PDF-1.7\n").unwrap();
    let error = load_attachment(&path).unwrap_err();
    std::fs::remove_file(path).unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("does not match"));
}

// --- media_to_messages tests ---

#[test]
fn loaded_attachments_preserve_order_types_and_bytes_in_messages() {
    use crate::agent::runner::media_to_messages;
    use rig::completion::Message;
    use rig::message::{
        Audio, AudioMediaType, Document, DocumentMediaType, DocumentSourceKind, Image,
        ImageMediaType, UserContent,
    };

    let fixtures: [(&str, &[u8]); 3] = [
        ("png", b"\x89PNG\r\n\x1a\npayload"),
        ("wav", b"RIFF\x04\x00\x00\x00WAVE"),
        (
            "pdf",
            b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\n%%EOF\n",
        ),
    ];
    let media: Vec<_> = fixtures
        .iter()
        .map(|(extension, bytes)| {
            let path = std::env::temp_dir().join(format!(
                "mini-agent-media-{}.{}",
                uuid::Uuid::new_v4(),
                extension
            ));
            std::fs::write(&path, bytes).unwrap();
            let result = load_attachment(&path);
            std::fs::remove_file(&path).unwrap();
            let attachment = result.unwrap();
            assert_eq!(attachment.path(), path);
            assert_eq!(attachment.size(), bytes.len());
            attachment
        })
        .collect();
    assert!(matches!(media[0], MediaAttachment::Image { .. }));
    assert!(matches!(media[1], MediaAttachment::Audio { .. }));
    assert!(matches!(media[2], MediaAttachment::Document { .. }));

    let expected = [
        UserContent::Image(Image {
            data: DocumentSourceKind::Raw(fixtures[0].1.to_vec()),
            media_type: Some(ImageMediaType::PNG),
            ..Default::default()
        }),
        UserContent::Audio(Audio {
            data: DocumentSourceKind::Raw(fixtures[1].1.to_vec()),
            media_type: Some(AudioMediaType::WAV),
            ..Default::default()
        }),
        UserContent::Document(Document {
            data: DocumentSourceKind::Raw(fixtures[2].1.to_vec()),
            media_type: Some(DocumentMediaType::PDF),
            ..Default::default()
        }),
    ];
    let messages = media_to_messages(&media);
    assert_eq!(messages.len(), expected.len());
    for (message, expected_content) in messages.into_iter().zip(expected) {
        let Message::User { content } = message else {
            panic!("expected User message, got {message:?}");
        };
        assert_eq!(
            content.into_iter().collect::<Vec<_>>(),
            vec![expected_content]
        );
    }
}

#[test]
fn media_to_messages_empty_vec_returns_empty() {
    use crate::agent::runner::media_to_messages;

    let messages = media_to_messages(&[]);
    assert!(messages.is_empty());
}
