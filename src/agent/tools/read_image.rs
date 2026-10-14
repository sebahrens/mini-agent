//! Bounded format-header inspection for image reads.
//!
//! This module reads dimensions without decompressing pixels. A valid header
//! does not establish that the complete image stream is decodable; the selected
//! provider still validates the image. Admission must also enforce the current
//! model, read permission, regular-handle and serialized-envelope limits.

/// Captured from the actual typed completion model when its tools are built.
/// Unknown model capabilities preserve normal provider error handling.
#[cfg(feature = "multimodal")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ImageReadSupport {
    known_image_input: Option<bool>,
    gemini: bool,
    enabled: bool,
}
#[cfg(feature = "multimodal")]
impl ImageReadSupport {
    pub(crate) fn captured(provider: &str, model: &str, gemini: bool) -> Self {
        Self {
            known_image_input: crate::models_catalog::model_image_input(provider, model),
            gemini,
            enabled: true,
        }
    }
    pub(crate) fn check(self, format: ImageFormat) -> Result<(), &'static str> {
        if !self.enabled {
            return Err("Image reads require a captured image-capable model transport");
        }
        if self.known_image_input == Some(false) {
            return Err("Model has no vision: this model is declared text-only");
        }
        if self.gemini && format == ImageFormat::Gif {
            return Err(
                "The Gemini transport does not support GIF image input; use PNG, JPEG or WebP",
            );
        }
        Ok(())
    }
}

#[cfg(feature = "multimodal")]
pub(crate) struct ImageRead {
    pub(crate) mime: &'static str,
    pub(crate) data: String,
}
#[cfg(feature = "multimodal")]
impl ImageRead {
    pub(crate) fn output(self, summary: &str) -> String {
        serde_json::json!({"response": summary, "parts":[{"type":"image","mimeType":self.mime,"data":self.data}]}).to_string()
    }
}

pub(crate) const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 8_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    /// Identify the file from its bytes, independent of its name or extension.
    pub(crate) fn from_magic(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some(Self::Webp)
        } else {
            None
        }
    }

    pub(crate) fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ImageMetadata {
    pub(crate) format: ImageFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes: usize,
}

pub(crate) fn inspect(bytes: &[u8]) -> Result<Option<ImageMetadata>, &'static str> {
    let Some(format) = ImageFormat::from_magic(bytes) else {
        return Ok(None);
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image exceeds the 5 MiB input limit");
    }
    let (width, height) = match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::Gif => gif_dimensions(bytes),
        ImageFormat::Webp => webp_dimensions(bytes),
    }
    .ok_or("Image dimensions are missing or its format header is malformed")?;
    if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err("Image dimensions must be between 1 and 8000 pixels on each side");
    }
    Ok(Some(ImageMetadata {
        format,
        width,
        height,
        bytes: bytes.len(),
    }))
}

fn be_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?))
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn le_u24(bytes: &[u8]) -> Option<u32> {
    let bytes = bytes.get(..3)?;
    Some(u32::from(bytes[0]) | u32::from(bytes[1]) << 8 | u32::from(bytes[2]) << 16)
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..33)?;
    if be_u32(&header[8..])? != 13 || &header[12..16] != b"IHDR" {
        return None;
    }
    if super::crc::crc32(&header[12..29]) != be_u32(&header[29..])? {
        return None;
    }
    // Only the mandatory IHDR is inspected; later chunks are not inflated.
    Some((be_u32(&header[16..])?, be_u32(&header[20..])?))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let screen = bytes.get(..13)?;
    if screen[10] & 0x80 != 0 {
        let table_bytes = 3usize.checked_mul(1usize << ((screen[10] & 7) + 1))?;
        bytes.get(13..13usize.checked_add(table_bytes)?)?;
    }
    Some((
        u32::from(u16::from_le_bytes(screen[6..8].try_into().ok()?)),
        u32::from(u16::from_le_bytes(screen[8..10].try_into().ok()?)),
    ))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut cursor = 2usize;
    while cursor < bytes.len() {
        if *bytes.get(cursor)? != 0xff {
            return None;
        }
        while bytes.get(cursor) == Some(&0xff) {
            cursor = cursor.checked_add(1)?;
        }
        let marker = *bytes.get(cursor)?;
        cursor = cursor.checked_add(1)?;
        match marker {
            // A scan or end before a frame header cannot supply dimensions.
            0x00 | 0xd8 | 0xd9 | 0xda => return None,
            0x01 | 0xd0..=0xd7 => continue,
            _ => {}
        }
        let length = usize::from(be_u16(bytes.get(cursor..)?)?);
        if length < 2 {
            return None;
        }
        let end = cursor.checked_add(length)?;
        let segment = bytes.get(cursor..end)?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let components = usize::from(*segment.get(7)?);
            if !(1..=4).contains(&components) || length != 8 + 3 * components {
                return None;
            }
            return Some((
                u32::from(be_u16(segment.get(5..)?)?),
                u32::from(be_u16(segment.get(3..)?)?),
            ));
        }
        cursor = end;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let declared = usize::try_from(le_u32(bytes.get(4..)?)?).ok()?;
    if declared.checked_add(8)? != bytes.len() {
        return None;
    }
    let mut cursor = 12usize;
    while cursor < bytes.len() {
        let chunk_header = bytes.get(cursor..cursor.checked_add(8)?)?;
        let length = usize::try_from(le_u32(&chunk_header[4..])?).ok()?;
        let start = cursor.checked_add(8)?;
        let end = start.checked_add(length)?;
        let data = bytes.get(start..end)?;
        // RIFF chunks include one padding byte when their size is odd.
        let next = end.checked_add(length & 1)?;
        bytes.get(..next)?;
        match &chunk_header[..4] {
            b"VP8X" if length == 10 => {
                return Some((le_u24(&data[4..])? + 1, le_u24(&data[7..])? + 1));
            }
            b"VP8 " => {
                let header = data.get(..10)?;
                if header[0] & 1 != 0 || &header[3..6] != b"\x9d\x01\x2a" {
                    return None;
                }
                let width = u16::from_le_bytes(header[6..8].try_into().ok()?) & 0x3fff;
                let height = u16::from_le_bytes(header[8..10].try_into().ok()?) & 0x3fff;
                return Some((u32::from(width), u32::from(height)));
            }
            b"VP8L" => {
                if *data.first()? != 0x2f {
                    return None;
                }
                let packed = le_u32(data.get(1..)?)?;
                return Some(((packed & 0x3fff) + 1, ((packed >> 14) & 0x3fff) + 1));
            }
            _ => cursor = next,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: [(ImageFormat, &[u8]); 6] = [
        (
            ImageFormat::Png,
            include_bytes!("fixtures/read-image64/3x2.png"),
        ),
        (
            ImageFormat::Jpeg,
            include_bytes!("fixtures/read-image64/3x2.jpg"),
        ),
        (
            ImageFormat::Gif,
            include_bytes!("fixtures/read-image64/3x2.gif"),
        ),
        (
            ImageFormat::Webp,
            include_bytes!("fixtures/read-image64/3x2-lossy.webp"),
        ),
        (
            ImageFormat::Webp,
            include_bytes!("fixtures/read-image64/3x2-lossless.webp"),
        ),
        (
            ImageFormat::Webp,
            include_bytes!("fixtures/read-image64/3x2-alpha.webp"),
        ),
    ];

    #[test]
    fn image_read64_headers_cover_real_png_jpeg_gif_and_webp_variants() {
        for (format, bytes) in FIXTURES {
            let metadata = inspect(bytes).unwrap().unwrap();
            assert_eq!(metadata.format, format);
            assert_eq!((metadata.width, metadata.height), (3, 2));
            assert_eq!(metadata.bytes, bytes.len());
            assert_eq!(ImageFormat::from_magic(bytes), Some(format));
            assert!(format.mime().starts_with("image/"));
        }
        assert_eq!(inspect(b"plain UTF-8 text").unwrap(), None);
        assert_eq!(inspect(b"RIFF\x04\0\0\0WAVE").unwrap(), None);
    }

    #[test]
    fn image_read64_limits_bad_headers_and_segment_lengths_fail_without_decoding() {
        let png = FIXTURES[0].1;
        let mut exact = png.to_vec();
        exact.resize(MAX_IMAGE_BYTES, 0);
        assert_eq!(inspect(&exact).unwrap().unwrap().bytes, MAX_IMAGE_BYTES);
        exact.push(0);
        assert!(inspect(&exact).unwrap_err().contains("5 MiB"));

        for width in [0, 8_001, u32::MAX] {
            let mut bytes = png.to_vec();
            bytes[16..20].copy_from_slice(&width.to_be_bytes());
            let checksum = super::super::crc::crc32(&bytes[12..29]);
            bytes[29..33].copy_from_slice(&checksum.to_be_bytes());
            assert!(inspect(&bytes).unwrap_err().contains("dimensions"));
        }
        let mut corrupted = png.to_vec();
        corrupted[16] ^= 1;
        assert!(inspect(&corrupted).is_err());

        for bytes in [
            &b"\xff\xd8\xff\xe0\0\x01"[..],
            &b"\xff\xd8\xff\xe0\xff\xff\0"[..],
            &b"\xff\xd8\xff\xda\0\x02"[..],
            &b"\xff\xd8\xff\xff\xff"[..],
            &b"GIF89a\0\0\0\0\0\0\0"[..],
            &b"RIFF\xff\xff\xff\xffWEBPVP8 \xff\xff\xff\xff"[..],
        ] {
            assert!(
                inspect(bytes).is_err(),
                "accepted malformed header: {bytes:?}"
            );
        }
        for (_, bytes) in FIXTURES {
            for end in 0..bytes.len() {
                // Header inspection may accept a complete header with a truncated
                // compressed stream; none of these prefixes may panic or decode.
                let _ = inspect(&bytes[..end]);
            }
        }
    }
}
