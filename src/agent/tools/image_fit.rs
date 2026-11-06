//! Downscale and re-encode images that exceed a byte or side limit.
//!
//! Used by image reads and user attachments so a Retina screenshot or phone
//! photo is resized instead of refused. Decoding is bounded (side and
//! allocation limits) and only happens for oversized input; images already
//! inside the limits are passed through byte-for-byte. A refusal remains only
//! for undecodable or unsupported data.

use std::io::Cursor;

use super::read_image::ImageFormat;

/// Longest edge, in pixels, of a downscaled image (provider-friendly size).
pub(crate) const FIT_LONG_EDGE: u32 = 2_000;
/// Largest encoded input accepted for downscaling.
pub(crate) const MAX_FIT_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DECODE_SIDE: u32 = 30_000;
const MAX_DECODE_ALLOC: u64 = 768 * 1024 * 1024;
const JPEG_QUALITY: u8 = 85;

#[derive(Debug)]
pub(crate) struct Fitted {
    pub(crate) data: Vec<u8>,
    pub(crate) format: ImageFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) original_width: u32,
    pub(crate) original_height: u32,
}

impl Fitted {
    /// Human-readable note for tool results and attachment labels.
    pub(crate) fn note(&self) -> String {
        format!(
            "resized from {} x {} to {} x {} ({}, {} bytes) to fit the image limits",
            self.original_width,
            self.original_height,
            self.width,
            self.height,
            self.format.mime(),
            self.data.len()
        )
    }
}

fn image_format(format: ImageFormat) -> image::ImageFormat {
    match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Webp => image::ImageFormat::WebP,
    }
}

fn encode(image: &image::DynamicImage, png: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    if png {
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|error| format!("Image could not be re-encoded: {error}"))?;
    } else {
        let rgb = image.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .encode_image(&rgb)
            .map_err(|error| format!("Image could not be re-encoded: {error}"))?;
    }
    Ok(out)
}

/// Decode `bytes` and downscale/re-encode it until it is at most `max_bytes`
/// and no side exceeds `max_side` (and [`FIT_LONG_EDGE`]). Images with
/// transparency stay PNG while that fits; others become JPEG.
pub(crate) fn fit(bytes: &[u8], max_bytes: usize, max_side: u32) -> Result<Fitted, String> {
    let format = ImageFormat::from_magic(bytes)
        .ok_or("Unsupported image format; use PNG, JPEG, GIF or WebP")?;
    if bytes.len() > MAX_FIT_INPUT_BYTES {
        return Err(format!(
            "Image is larger than the {} MiB downscaling input limit",
            MAX_FIT_INPUT_BYTES / (1024 * 1024)
        ));
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image_format(format));
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|error| format!("Image could not be decoded for resizing: {error}"))?;
    let (original_width, original_height) = (decoded.width(), decoded.height());
    let has_alpha = decoded.color().has_alpha();
    let mut long_edge = FIT_LONG_EDGE
        .min(max_side)
        .min(original_width.max(original_height))
        .max(1);
    for attempt in 0..6 {
        let scale = f64::from(long_edge) / f64::from(original_width.max(original_height));
        let width = ((f64::from(original_width) * scale).round() as u32).max(1);
        let height = ((f64::from(original_height) * scale).round() as u32).max(1);
        let resized = if (width, height) == (original_width, original_height) {
            decoded.clone()
        } else {
            decoded.resize_exact(width, height, image::imageops::FilterType::Triangle)
        };
        // Keep transparency as PNG while it fits; fall back to JPEG after.
        let png = has_alpha && attempt < 2;
        let data = encode(&resized, png)?;
        if data.len() <= max_bytes {
            return Ok(Fitted {
                data,
                format: if png {
                    ImageFormat::Png
                } else {
                    ImageFormat::Jpeg
                },
                width,
                height,
                original_width,
                original_height,
            });
        }
        long_edge = ((f64::from(long_edge) * 0.7) as u32).max(64);
    }
    Err(format!(
        "Image could not be reduced below {max_bytes} bytes"
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A noisy RGB PNG that compresses badly, so it is large on disk.
    pub(crate) fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x2545_f491_u32;
        let image = image::RgbImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn oversized_images_are_downscaled_and_garbage_is_refused() {
        let png = noisy_png(3000, 2000);
        let fitted = fit(&png, 2 * 1024 * 1024, 8_000).unwrap();
        assert_eq!(
            (fitted.original_width, fitted.original_height),
            (3000, 2000)
        );
        assert!(fitted.width <= FIT_LONG_EDGE && fitted.height <= FIT_LONG_EDGE);
        assert!(fitted.data.len() <= 2 * 1024 * 1024);
        assert_eq!(ImageFormat::from_magic(&fitted.data), Some(fitted.format));
        assert!(fitted.note().starts_with("resized from 3000 x 2000 to "));

        let mut broken = png[..64].to_vec();
        broken.extend_from_slice(&[0; 64]);
        assert!(
            fit(&broken, 1024, 8_000)
                .unwrap_err()
                .contains("could not be decoded")
        );
        assert!(
            fit(b"not an image", 1024, 8_000)
                .unwrap_err()
                .contains("Unsupported")
        );
    }
}
