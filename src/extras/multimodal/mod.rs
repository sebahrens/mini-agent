use std::io::Read;
use std::path::{Path, PathBuf};

/// Maximum file size for media attachments: 20 MiB.
pub const MAX_MEDIA_BYTES: u64 = 20 * 1024 * 1024;

/// Runtime-only clipboard display metadata. The image itself is persisted as
/// canonical user content, never a pathname to reread.
#[derive(Debug, Clone)]
pub struct ClipboardImageInfo {
    pub width: u32,
    pub height: u32,
}

/// Represents a media file attached to a user message.
/// The raw bytes are held in memory and converted to rig message content
/// when the message is submitted.
#[derive(Debug, Clone)]
pub enum MediaAttachment {
    Image {
        path: PathBuf,
        data: Vec<u8>,
        mime: String,
        clipboard: Option<ClipboardImageInfo>,
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

/// Edge of one vision patch (Claude's published `ceil(w/28) * ceil(h/28)`
/// formula, about `w * h / 750` for typical sizes).
const IMAGE_PATCH_PX: u64 = 28;
/// Highest per-image charge (Claude's high-resolution tier): providers
/// downscale larger images.
const MAX_IMAGE_PREFLIGHT_TOKENS: u64 = 4_784;
/// Conservative per-page charge for a PDF (page text plus its page image).
const PDF_PAGE_PREFLIGHT_TOKENS: u64 = 3_000;
/// Flat estimate kept for audio and for content whose size is unknown.
const DEFAULT_ATTACHMENT_PREFLIGHT_TOKENS: u64 = 2_048;

/// Number of `/Type /Page` objects (not `/Pages`) in a PDF, a cheap lower
/// bound of its page count without parsing the document.
fn pdf_page_count(data: &[u8]) -> u64 {
    let mut pages = 0u64;
    let mut index = 0usize;
    while let Some(offset) = data[index..]
        .windows(5)
        .position(|window| window == b"/Type")
    {
        let mut cursor = index + offset + 5;
        while data.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if data[cursor..].starts_with(b"/Page")
            && !data.get(cursor + 5).is_some_and(|byte| *byte == b's')
        {
            pages += 1;
        }
        index = cursor;
    }
    pages
}

impl MediaAttachment {
    /// Local token estimate of this attachment for the context preflight
    /// (mini-agent-d6x7s): images by their header dimensions (patch formula,
    /// capped at the high-resolution maximum), PDFs per page, other documents
    /// by their text, audio and unknown content at a flat 2,048.
    pub(crate) fn preflight_tokens(&self) -> u64 {
        match self {
            MediaAttachment::Image { data, .. } => {
                match crate::agent::tools::read_image::inspect(data) {
                    Ok(Some(info)) => (u64::from(info.width).div_ceil(IMAGE_PATCH_PX)
                        * u64::from(info.height).div_ceil(IMAGE_PATCH_PX))
                    .clamp(1, MAX_IMAGE_PREFLIGHT_TOKENS),
                    // Unknown dimensions: four pixels per byte, densely.
                    _ => (data.len() as u64)
                        .saturating_mul(4)
                        .div_ceil(IMAGE_PATCH_PX * IMAGE_PATCH_PX)
                        .clamp(
                            DEFAULT_ATTACHMENT_PREFLIGHT_TOKENS,
                            MAX_IMAGE_PREFLIGHT_TOKENS,
                        ),
                }
            }
            MediaAttachment::Document { data, mime, .. } if mime == "application/pdf" => {
                pdf_page_count(data)
                    .max(1)
                    .saturating_mul(PDF_PAGE_PREFLIGHT_TOKENS)
            }
            MediaAttachment::Document { data, .. } => {
                crate::session::Session::estimate_tokens(&String::from_utf8_lossy(data))
                    .max(DEFAULT_ATTACHMENT_PREFLIGHT_TOKENS)
            }
            MediaAttachment::Audio { .. } => DEFAULT_ATTACHMENT_PREFLIGHT_TOKENS,
        }
    }

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
        #[cfg(feature = "pdf")]
        "pdf" => Some("application/pdf"),
        _ => None,
    }
}

fn sniff_media(data: &[u8]) -> Option<&'static str> {
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
    } else if cfg!(feature = "pdf")
        && data
            .iter()
            .take(1024)
            .position(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|offset| data[offset..].starts_with(b"%PDF-"))
    {
        Some("application/pdf")
    } else {
        None
    }
}

/// Load a regular media file, limiting bytes read even if it grows while open.
/// Symlinks to regular files are supported. Metadata and content are read from
/// the same handle so a later pathname replacement cannot switch the input.
pub fn load_attachment(path: &Path) -> std::io::Result<MediaAttachment> {
    let not_regular = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        )
    };
    if !std::fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    #[cfg(test)]
    tests::interpose(path, tests::ReadStage::BeforeOpen);

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A regular file may be replaced with a FIFO after the preflight.
        // Open without waiting for a writer, then inspect the actual handle.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let too_large = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "file too large: {} (max {} bytes)",
                path.display(),
                MAX_MEDIA_BYTES
            ),
        )
    };
    if meta.len() > MAX_MEDIA_BYTES {
        return Err(too_large());
    }
    #[cfg(test)]
    tests::interpose(path, tests::ReadStage::BeforeRead);
    let mut data = Vec::new();
    file.take(MAX_MEDIA_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_MEDIA_BYTES {
        return Err(too_large());
    }
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
        MediaAttachment::Image {
            path,
            data,
            mime,
            clipboard: None,
        }
    } else if mime.starts_with("audio/") {
        MediaAttachment::Audio { path, data, mime }
    } else {
        MediaAttachment::Document { path, data, mime }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone, Copy, PartialEq)]
    pub(super) enum ReadStage {
        BeforeOpen,
        BeforeRead,
    }

    type ReadInterposition = (PathBuf, ReadStage, Box<dyn FnOnce()>);
    thread_local! {
        static READ_INTERPOSITION: std::cell::RefCell<Option<ReadInterposition>> = const {
            std::cell::RefCell::new(None)
        };
    }

    pub(super) fn interpose(path: &Path, stage: ReadStage) {
        let action = READ_INTERPOSITION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|(target, point, _)| target == path && *point == stage)
            {
                slot.take().map(|(_, _, action)| action)
            } else {
                None
            }
        });
        if let Some(action) = action {
            action();
        }
    }

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("mini-agent-media-bounds-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            READ_INTERPOSITION.with(|slot| slot.borrow_mut().take());
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\npayload";

    fn write_sized_png(path: &Path, size: u64) {
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(PNG).unwrap();
        file.set_len(size).unwrap();
    }

    #[test]
    fn attachment_read_bounds_and_open_handle_survive_file_changes() {
        for case in [
            "exact_limit",
            "already_oversized",
            "grows_after_metadata",
            "path_replaced",
            "directory",
            #[cfg(unix)]
            "symlink",
        ] {
            let root = TempRoot::new();
            let path = root.0.join("image.png");
            write_sized_png(
                &path,
                match case {
                    "exact_limit" => MAX_MEDIA_BYTES,
                    "already_oversized" => MAX_MEDIA_BYTES + 1,
                    _ => PNG.len() as u64,
                },
            );
            if case == "directory" {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            #[cfg(unix)]
            if case == "symlink" {
                let target = path.with_extension("original");
                std::fs::rename(&path, &target).unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            if matches!(case, "grows_after_metadata" | "path_replaced") {
                let changed = path.clone();
                READ_INTERPOSITION.with(|slot| {
                    *slot.borrow_mut() = Some((
                        path.clone(),
                        ReadStage::BeforeRead,
                        Box::new(move || {
                            if case == "path_replaced" {
                                std::fs::rename(&changed, changed.with_extension("old")).unwrap();
                                write_sized_png(&changed, MAX_MEDIA_BYTES + 1);
                            } else {
                                std::fs::OpenOptions::new()
                                    .write(true)
                                    .open(changed)
                                    .unwrap()
                                    .set_len(MAX_MEDIA_BYTES + 1)
                                    .unwrap();
                            }
                        }),
                    ));
                });
            }
            let result = load_attachment(&path);
            match case {
                "exact_limit" => assert_eq!(result.unwrap().size() as u64, MAX_MEDIA_BYTES),
                "path_replaced" | "symlink" => {
                    let MediaAttachment::Image { data, .. } = result.unwrap() else {
                        panic!("expected image");
                    };
                    assert_eq!(data, PNG, "must read the already-open file");
                }
                "directory" => {
                    let error = result.unwrap_err();
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
                    assert!(error.to_string().contains("not a regular file"));
                }
                _ => {
                    let error = match result {
                        Err(error) => error,
                        Ok(attachment) => panic!("{case}: accepted {} bytes", attachment.size()),
                    };
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{case}");
                    assert!(
                        error.to_string().contains("file too large"),
                        "{case}: {error}"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path is NUL-terminated and lives through this call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn attachment_rejects_fifo_before_and_during_open_without_waiting_for_writer() {
        for replace_before_open in [false, true] {
            let root = TempRoot::new();
            let path = root.0.join("pipe.png");
            if replace_before_open {
                std::fs::write(&path, PNG).unwrap();
            } else {
                make_fifo(&path);
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                if replace_before_open {
                    let changed = path.clone();
                    READ_INTERPOSITION.with(|slot| {
                        *slot.borrow_mut() = Some((
                            path.clone(),
                            ReadStage::BeforeOpen,
                            Box::new(move || {
                                std::fs::remove_file(&changed).unwrap();
                                make_fifo(&changed);
                            }),
                        ));
                    });
                }
                let _ = tx.send(load_attachment(&path));
            });
            let result = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("attachment open blocked on a FIFO without a writer");
            worker.join().unwrap();
            let error = result.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("not a regular file"), "{error}");
        }
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::MediaAttachment;

    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        let mut chunk = b"IHDR".to_vec();
        chunk.extend_from_slice(&width.to_be_bytes());
        chunk.extend_from_slice(&height.to_be_bytes());
        chunk.extend_from_slice(&[8, 6, 0, 0, 0]);
        let crc = crate::agent::tools::crc::crc32(&chunk);
        bytes.extend_from_slice(&chunk);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes
    }

    fn image(data: Vec<u8>) -> MediaAttachment {
        MediaAttachment::Image {
            path: "shot.png".into(),
            data,
            mime: "image/png".into(),
            clipboard: None,
        }
    }

    // mini-agent-d6x7s: the estimate scales with image size and PDF pages.
    #[test]
    fn preflight_estimate_scales_with_image_dimensions_and_pdf_pages() {
        assert_eq!(image(png(28, 28)).preflight_tokens(), 1);
        assert_eq!(image(png(560, 560)).preflight_tokens(), 400);
        assert_eq!(image(png(4_000, 3_000)).preflight_tokens(), 4_784);
        let pdf = |pages: usize| MediaAttachment::Document {
            path: "doc.pdf".into(),
            data: format!(
                "%PDF-1.7\n1 0 obj << /Type /Pages /Count {pages} >> endobj\n{}",
                "2 0 obj << /Type /Page >> endobj\n".repeat(pages)
            )
            .into_bytes(),
            mime: "application/pdf".into(),
        };
        assert_eq!(pdf(1).preflight_tokens(), 3_000);
        assert_eq!(pdf(12).preflight_tokens(), 36_000);
        let audio = MediaAttachment::Audio {
            path: "a.mp3".into(),
            data: vec![0; 10],
            mime: "audio/mpeg".into(),
        };
        assert_eq!(audio.preflight_tokens(), 2_048);
    }

    // A large image near the window is rejected locally where the flat
    // 2,048-token estimate would have let it through.
    #[test]
    fn large_image_near_the_window_is_rejected_by_the_preflight() {
        let mut session = crate::session::Session::new("anthropic", "claude", 8_000, "");
        session.overhead_tokens = 3_000;
        let reserve = 1_000;
        let text = crate::session::Session::estimate_tokens("describe this");
        let tokens = image(png(4_000, 3_000)).preflight_tokens();
        assert!(!session.is_irreducible_with_pending(reserve, text + 2_048));
        assert!(session.is_irreducible_with_pending(reserve, text + tokens));
    }
}
