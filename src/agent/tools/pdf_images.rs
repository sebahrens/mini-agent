//! Page images for scanned PDFs (child module of `pdf_text`).
//!
//! The extractor has no renderer, but a scanned page is almost always one
//! image XObject painted over the whole page. This module lifts those images
//! so a vision-capable model can read the page itself: JPEG (`DCTDecode`) data
//! passes through byte for byte, and Flate/LZW/ASCII85/RunLength image data
//! in gray, RGB, CMYK or indexed colour is repacked as PNG. Fax, JBIG2 and
//! JPEG 2000 data cannot be decoded here and are named instead.

use super::{Dict, Document, Obj, PageContent, StreamError};

/// Pixels in one lifted image; bigger bitmaps are named, not attached.
const MAX_IMAGE_PIXELS: u64 = 40_000_000;

/// One image taken from a page.
pub(crate) struct PdfImage {
    pub(crate) mime: &'static str,
    pub(crate) data: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Space {
    Gray,
    Rgb,
    Cmyk,
}

impl Space {
    fn components(self) -> usize {
        match self {
            Space::Gray => 1,
            Space::Rgb => 3,
            Space::Cmyk => 4,
        }
    }
}

enum Colors {
    Direct(Space),
    Indexed { base: Space, palette: Vec<u8> },
}

impl Document {
    /// Images painted on the 1-based `page`,
    /// largest first, at most `max`. `Err` names why none could be lifted.
    pub(crate) fn page_images(
        &self,
        page: usize,
        content: &PageContent,
        max: usize,
    ) -> Result<Vec<PdfImage>, String> {
        let Some((_, resources)) = self.pages.get(page - 1) else {
            return Ok(Vec::new());
        };
        let xobjects = resources
            .as_ref()
            .and_then(|resources| resources.get(b"XObject"))
            .and_then(|xobjects| self.resolve(xobjects).as_dict());
        let mut candidates: Vec<(u64, &Dict, &[u8])> = Vec::new();
        let mut seen: Vec<&[u8]> = Vec::new();
        for name in &content.image_names {
            if seen.contains(&name.as_slice()) {
                continue;
            }
            seen.push(name);
            if let Some(Obj::Stream(dict, raw)) = xobjects
                .and_then(|xobjects| xobjects.get(name))
                .map(|object| self.resolve(object))
            {
                let area = self.number(dict, b"Width") * self.number(dict, b"Height");
                candidates.push((area, dict, raw));
            }
        }
        candidates.sort_by_key(|(area, _, _)| std::cmp::Reverse(*area));
        let mut images = Vec::new();
        let mut reasons = Vec::new();
        for (_, dict, raw) in candidates.into_iter().take(max.max(1) * 2) {
            if images.len() >= max {
                break;
            }
            match self.lift_image(dict, raw) {
                Ok(Some(image)) => images.push(image),
                Ok(None) => {}
                Err(reason) => reasons.push(reason),
            }
        }
        if images.is_empty() {
            return Err(reasons
                .into_iter()
                .next()
                .unwrap_or_else(|| "no usable image on the page".to_owned()));
        }
        Ok(images)
    }

    fn number(&self, dict: &Dict, key: &[u8]) -> u64 {
        dict.get(key)
            .map(|value| self.resolve(value))
            .and_then(Obj::as_num)
            .map_or(0, |value| value.max(0.0) as u64)
    }

    fn lift_image(&self, dict: &Dict, raw: &[u8]) -> Result<Option<PdfImage>, String> {
        let width = self.number(dict, b"Width");
        let height = self.number(dict, b"Height");
        if width == 0 || height == 0 {
            return Ok(None);
        }
        if width.saturating_mul(height) > MAX_IMAGE_PIXELS {
            return Err(format!(
                "image is {width} x {height} pixels, above the {MAX_IMAGE_PIXELS} pixel limit"
            ));
        }
        let describe = |error: StreamError| match error {
            StreamError::Unsupported(filter) => {
                format!("image uses {filter}, which cannot be extracted")
            }
            StreamError::TooLarge => "image data exceeds the decode budget".to_owned(),
            StreamError::Corrupt(why) => format!("image data could not be decoded ({why})"),
        };
        let (data, jpeg) = self.decode_inner(dict, raw, true).map_err(describe)?;
        let (width, height) = (width as u32, height as u32);
        if jpeg {
            if !data.starts_with(&[0xFF, 0xD8]) {
                return Err("DCTDecode data is not a JPEG".to_owned());
            }
            return Ok(Some(PdfImage {
                mime: "image/jpeg",
                data,
                width,
                height,
            }));
        }
        if matches!(dict.get(b"ImageMask"), Some(Obj::Bool(true))) {
            return Ok(None);
        }
        let bits = self.number(dict, b"BitsPerComponent").max(1) as usize;
        if !matches!(bits, 1 | 2 | 4 | 8) {
            return Err(format!("image uses {bits} bits per component"));
        }
        let colors = self
            .colors(dict.get(b"ColorSpace"))
            .ok_or_else(|| "image colour space is not supported".to_owned())?;
        let invert = matches!(
            dict.get(b"Decode").map(|d| self.resolve(d)),
            Some(Obj::Array(items)) if matches!(items.as_slice(), [Obj::Num(a), Obj::Num(b)] if *a > *b)
        );
        let channels = match &colors {
            Colors::Direct(space) => space.components(),
            Colors::Indexed { .. } => 1,
        };
        let stride = (width as usize * channels * bits).div_ceil(8);
        if data.len() < stride * height as usize {
            return Err(format!(
                "image data holds {} of the {} bytes its dimensions need",
                data.len(),
                stride * height as usize
            ));
        }
        let pixels = unpack(
            &data,
            width as usize,
            height as usize,
            stride,
            channels,
            bits,
        );
        let dynamic = match colors {
            Colors::Direct(Space::Gray) => gray(width, height, pixels, bits, invert),
            Colors::Direct(Space::Rgb) => rgb(width, height, pixels, bits, invert),
            Colors::Direct(Space::Cmyk) => {
                let rgb_pixels = pixels
                    .chunks_exact(4)
                    .flat_map(|cmyk| {
                        let k = 255 - u32::from(cmyk[3]);
                        [0, 1, 2].map(|c| ((255 - u32::from(cmyk[c])) * k / 255) as u8)
                    })
                    .collect();
                rgb(width, height, rgb_pixels, 8, false)
            }
            Colors::Indexed { base, palette } => {
                let components = base.components().min(3);
                let mut expanded = Vec::with_capacity(pixels.len() * 3);
                for index in &pixels {
                    let at = usize::from(*index) * base.components();
                    let entry = palette.get(at..at + base.components());
                    match (entry, base) {
                        (Some(entry), Space::Gray) => expanded.extend_from_slice(&[entry[0]; 3]),
                        (Some(entry), _) if components == 3 => {
                            expanded.extend_from_slice(&entry[..3])
                        }
                        _ => expanded.extend_from_slice(&[0, 0, 0]),
                    }
                }
                rgb(width, height, expanded, 8, false)
            }
        }
        .ok_or_else(|| "image data does not match its dimensions".to_owned())?;
        let mut png = Vec::new();
        dynamic
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|error| format!("image could not be re-encoded as PNG ({error})"))?;
        Ok(Some(PdfImage {
            mime: "image/png",
            data: png,
            width,
            height,
        }))
    }

    fn colors(&self, space: Option<&Obj>) -> Option<Colors> {
        let space = self.resolve(space?);
        let simple = |name: &[u8]| match name {
            b"DeviceGray" | b"CalGray" | b"G" => Some(Space::Gray),
            b"DeviceRGB" | b"CalRGB" | b"RGB" => Some(Space::Rgb),
            b"DeviceCMYK" | b"CMYK" => Some(Space::Cmyk),
            _ => None,
        };
        match space {
            Obj::Name(name) => simple(name).map(Colors::Direct),
            Obj::Array(items) => {
                let kind = self.resolve(items.first()?).as_name()?;
                match kind {
                    b"ICCBased" => match self.resolve(items.get(1)?) {
                        Obj::Stream(dict, _) => match self.number(dict, b"N") {
                            1 => Some(Colors::Direct(Space::Gray)),
                            3 => Some(Colors::Direct(Space::Rgb)),
                            4 => Some(Colors::Direct(Space::Cmyk)),
                            _ => None,
                        },
                        _ => None,
                    },
                    b"Indexed" | b"I" => {
                        let base = match self.colors(items.get(1))? {
                            Colors::Direct(space) if space != Space::Cmyk => space,
                            _ => return None,
                        };
                        let palette = match self.resolve(items.get(3)?) {
                            Obj::Str(bytes) => bytes.clone(),
                            Obj::Stream(dict, raw) => self.decode(dict, raw).ok()?,
                            _ => return None,
                        };
                        Some(Colors::Indexed { base, palette })
                    }
                    other => simple(other).map(Colors::Direct),
                }
            }
            _ => None,
        }
    }
}

/// One byte per sample, rows unpadded; samples narrower than 8 bits are
/// scaled to the full 0-255 range unless they index a palette.
fn unpack(
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    channels: usize,
    bits: usize,
) -> Vec<u8> {
    let samples_per_row = width * channels;
    let mut out = Vec::with_capacity(samples_per_row * height);
    for row in data.chunks(stride).take(height) {
        if bits == 8 {
            out.extend_from_slice(&row[..samples_per_row.min(row.len())]);
            continue;
        }
        for sample in 0..samples_per_row {
            let bit = sample * bits;
            let byte = row.get(bit / 8).copied().unwrap_or(0);
            let shift = 8 - bits - (bit % 8);
            out.push((byte >> shift) & ((1u8 << bits) - 1));
        }
    }
    out
}

fn gray(
    width: u32,
    height: u32,
    mut pixels: Vec<u8>,
    bits: usize,
    invert: bool,
) -> Option<image::DynamicImage> {
    if bits < 8 {
        let max = (1u16 << bits) - 1;
        for pixel in &mut pixels {
            *pixel = (u16::from(*pixel) * 255 / max) as u8;
        }
    }
    if invert {
        for pixel in &mut pixels {
            *pixel = 255 - *pixel;
        }
    }
    image::GrayImage::from_raw(width, height, pixels).map(image::DynamicImage::ImageLuma8)
}

fn rgb(
    width: u32,
    height: u32,
    mut pixels: Vec<u8>,
    bits: usize,
    invert: bool,
) -> Option<image::DynamicImage> {
    if bits < 8 {
        let max = (1u16 << bits) - 1;
        for pixel in &mut pixels {
            *pixel = (u16::from(*pixel) * 255 / max) as u8;
        }
    }
    if invert {
        for pixel in &mut pixels {
            *pixel = 255 - *pixel;
        }
    }
    image::RgbImage::from_raw(width, height, pixels).map(image::DynamicImage::ImageRgb8)
}

#[cfg(test)]
mod tests {
    use super::super::pdf_filters::ascii85_encode;
    use super::super::test_pdf::{Content, build_with, first_extra_object};
    use super::*;
    use std::io::Write;

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// A one-page PDF whose page paints image `/Im0` with the given
    /// dictionary entries and encoded data.
    fn scan(entries: &str, data: &[u8]) -> Document {
        let mut object = format!(
            "<< /Type /XObject /Subtype /Image {entries} /Length {} >>\nstream\n",
            data.len()
        )
        .into_bytes();
        object.extend_from_slice(data);
        object.extend_from_slice(b"\nendstream");
        let pdf = build_with(
            &[Content::Plain("q 100 0 0 100 0 0 cm /Im0 Do Q".into())],
            false,
            &format!("/XObject << /Im0 {} 0 R >>", first_extra_object(1)),
            vec![object],
        );
        Document::parse(&pdf).unwrap()
    }

    fn lift(entries: &str, data: &[u8]) -> Result<PdfImage, String> {
        let document = scan(entries, data);
        let content = document.page_content(0).unwrap();
        assert_eq!(content.images, 1);
        assert!(!content.text_operators);
        document
            .page_images(1, &content, 4)
            .map(|mut images| images.remove(0))
    }

    fn decode_png(image: &PdfImage) -> image::DynamicImage {
        assert_eq!(image.mime, "image/png");
        image::load_from_memory(&image.data).unwrap()
    }

    #[test]
    fn jpeg_pages_pass_through_byte_for_byte() {
        let jpeg = include_bytes!("fixtures/read-image64/3x2.jpg");
        let image = lift(
            "/Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode",
            jpeg,
        )
        .unwrap();
        assert_eq!(image.mime, "image/jpeg");
        assert_eq!(image.data, jpeg);
        assert_eq!((image.width, image.height), (3, 2));
        // A flate wrapper around the JPEG is undone first.
        let wrapped = lift(
            "/Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter [/FlateDecode /DCTDecode]",
            &deflate(jpeg),
        )
        .unwrap();
        assert_eq!(wrapped.data, jpeg);
    }

    #[test]
    fn gray_and_bilevel_bitmaps_become_png() {
        let gray = lift(
            "/Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
            &deflate(&[0, 100, 200, 255]),
        )
        .unwrap();
        assert_eq!(decode_png(&gray).to_luma8().into_raw(), [0, 100, 200, 255]);
        let bilevel =
            "/Width 8 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /FlateDecode";
        let image = lift(bilevel, &deflate(&[0xF0, 0x0F])).unwrap();
        assert_eq!(
            decode_png(&image).to_luma8().into_raw(),
            [
                255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255
            ]
        );
        let inverted = lift(&format!("{bilevel} /Decode [1 0]"), &deflate(&[0xF0, 0x0F])).unwrap();
        assert_eq!(decode_png(&inverted).to_luma8().into_raw()[0], 0);
    }

    #[test]
    fn colour_spaces_and_chained_filters_become_png() {
        let rgb = lift(
            "/Width 1 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter [/ASCII85Decode /FlateDecode]",
            &ascii85_encode(&deflate(&[255, 0, 0, 0, 0, 255])),
        )
        .unwrap();
        assert_eq!(
            decode_png(&rgb).to_rgb8().into_raw(),
            [255, 0, 0, 0, 0, 255]
        );
        let indexed = lift(
            "/Width 4 /Height 1 /BitsPerComponent 2 /ColorSpace [/Indexed /DeviceRGB 3 <ff0000 00ff00 0000ff ffffff>]",
            &[0b0001_1011],
        )
        .unwrap();
        assert_eq!(
            decode_png(&indexed).to_rgb8().into_raw(),
            [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]
        );
        let cmyk = lift(
            "/Width 2 /Height 1 /ColorSpace /DeviceCMYK /BitsPerComponent 8",
            &[0, 0, 0, 0, 0, 0, 0, 255],
        )
        .unwrap();
        assert_eq!(
            decode_png(&cmyk).to_rgb8().into_raw(),
            [255, 255, 255, 0, 0, 0]
        );
    }

    #[test]
    fn undecodable_or_hostile_images_are_named_not_attached() {
        let fax = lift(
            "/Width 8 /Height 8 /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /CCITTFaxDecode",
            b"abc",
        )
        .err()
        .unwrap();
        assert!(fax.contains("CCITTFaxDecode"), "{fax}");
        let short = lift(
            "/Width 100 /Height 100 /ColorSpace /DeviceGray /BitsPerComponent 8",
            &[0; 10],
        )
        .err()
        .unwrap();
        assert!(short.contains("holds 10"), "{short}");
        let huge = lift(
            "/Width 100000 /Height 100000 /ColorSpace /DeviceGray /BitsPerComponent 8",
            &[0; 10],
        )
        .err()
        .unwrap();
        assert!(huge.contains("pixel limit"), "{huge}");
        let deep = lift(
            "/Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 16",
            &[0, 0],
        )
        .err()
        .unwrap();
        assert!(deep.contains("16 bits"), "{deep}");
        let mask = lift(
            "/Width 8 /Height 1 /ImageMask true /BitsPerComponent 1",
            &[0xFF],
        )
        .err()
        .unwrap();
        assert!(mask.contains("no usable image"), "{mask}");
        let bad_jpeg = lift(
            "/Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode",
            b"not a jpeg",
        )
        .err()
        .unwrap();
        assert!(bad_jpeg.contains("not a JPEG"), "{bad_jpeg}");
        // Bytes after a JPEG stage are corrupt filter chains, not images.
        let chained = lift(
            "/Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter [/DCTDecode /FlateDecode]",
            b"x",
        );
        assert!(chained.is_err());
    }
}
