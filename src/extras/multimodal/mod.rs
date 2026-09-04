use std::path::{Path, PathBuf};

/// Maximum file size for media attachments: 20 MB.
pub const MAX_MEDIA_BYTES: u64 = 20 * 1024 * 1024;

/// Represents a media file attached to a user message.
/// The raw bytes are held in memory and converted to rig message content
/// when the message is submitted.
#[derive(Debug, Clone)]
pub enum MediaAttachment {
    Image {
        path: PathBuf,
        data: Vec<u8>,
        mime: String,
    },
    Audio {
        path: PathBuf,
        data: Vec<u8>,
        mime: String,
    },
    Document {
        path: PathBuf,
        data: Vec<u8>,
        mime: String,
    },
}

impl MediaAttachment {
    pub fn size(&self) -> usize {
        match self {
            MediaAttachment::Image { data, .. }
            | MediaAttachment::Audio { data, .. }
            | MediaAttachment::Document { data, .. } => data.len(),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            MediaAttachment::Image { path, .. }
            | MediaAttachment::Audio { path, .. }
            | MediaAttachment::Document { path, .. } => path,
        }
    }
}

/// Check whether a file extension indicates multi-modal media (not text).
/// Returns the MIME type string if recognized, `None` otherwise.
pub fn detect_media(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "m4a" => Some("audio/mp4"),
        "aac" => Some("audio/aac"),
        "pdf" => Some("application/pdf"),
        _ => None,
    }
}

fn sniff_media(data: &[u8]) -> Option<&'static str> {
    let pdf_offset = data
        .iter()
        .take(1024)
        .position(|byte| !byte.is_ascii_whitespace());
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"ID3")
        || (data.len() >= 2 && data[0] == 0xff && data[1] & 0xe0 == 0xe0 && data[1] & 0x06 != 0)
    {
        Some("audio/mpeg")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if data.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if data.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if data.len() >= 12 && &data[4..8] == b"ftyp" {
        Some("audio/mp4")
    } else if data.len() >= 2 && data[0] == 0xff && data[1] & 0xf6 == 0xf0 {
        Some("audio/aac")
    } else if pdf_offset.is_some_and(|offset| data[offset..].starts_with(b"%PDF-")) {
        Some("application/pdf")
    } else {
        None
    }
}

/// Load a media file from disk. The caller must have already verified the
/// path exists and is a file. Returns an error if the file is too large.
pub fn load_attachment(path: &Path) -> std::io::Result<MediaAttachment> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_MEDIA_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "file too large: {} (max {} bytes)",
                path.display(),
                MAX_MEDIA_BYTES
            ),
        ));
    }

    let data = std::fs::read(path)?;
    let extension_mime = detect_media(path).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown media type: {}", path.display()),
        )
    })?;
    let sniffed_mime = sniff_media(&data).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("media signature is not recognized: {}", path.display()),
        )
    })?;
    if sniffed_mime != extension_mime {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "media signature {} does not match extension type {}: {}",
                sniffed_mime,
                extension_mime,
                path.display()
            ),
        ));
    }
    let mime = sniffed_mime.to_string();

    // We already know the mime from detect_media — dispatch on the prefix.
    let path = path.to_path_buf();
    Ok(if mime.starts_with("image/") {
        MediaAttachment::Image { path, data, mime }
    } else if mime.starts_with("audio/") {
        MediaAttachment::Audio { path, data, mime }
    } else {
        MediaAttachment::Document { path, data, mime }
    })
}
