use std::io::Read;
use std::path::{Path, PathBuf};

/// Maximum file size for media attachments: 20 MiB.
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
        MediaAttachment::Image { path, data, mime }
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
