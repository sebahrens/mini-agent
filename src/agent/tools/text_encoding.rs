//! Byte-order-mark detected UTF-16 text, decoded for the read and edit tools
//! and re-encoded on publication so the file keeps its encoding and BOM.
//! Only files that start with a UTF-16 BOM qualify; anything else that is not
//! UTF-8 still fails closed, because a lossy decode would corrupt it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE with BOM",
            Self::Utf16Be => "UTF-16BE with BOM",
        }
    }
}

/// Decodes a file that starts with a UTF-16 byte-order mark. `None` when
/// there is no UTF-16 BOM; an error when the payload is not valid UTF-16
/// (odd length or an unpaired surrogate).
pub(crate) fn decode_utf16_bom(bytes: &[u8]) -> Option<Result<(String, TextEncoding), String>> {
    let encoding = match bytes {
        [0xFF, 0xFE, ..] => TextEncoding::Utf16Le,
        [0xFE, 0xFF, ..] => TextEncoding::Utf16Be,
        _ => return None,
    };
    let payload = &bytes[2..];
    if !payload.len().is_multiple_of(2) {
        return Some(Err(
            "the UTF-16 payload has an odd number of bytes".to_string()
        ));
    }
    let units = payload.chunks_exact(2).map(|pair| match encoding {
        TextEncoding::Utf16Le => u16::from_le_bytes([pair[0], pair[1]]),
        _ => u16::from_be_bytes([pair[0], pair[1]]),
    });
    let mut text = String::with_capacity(payload.len() / 2);
    for (index, unit) in char::decode_utf16(units).enumerate() {
        match unit {
            Ok(ch) => text.push(ch),
            Err(_) => {
                return Some(Err(format!(
                    "invalid UTF-16 (unpaired surrogate near code unit {index})"
                )));
            }
        }
    }
    Some(Ok((text, encoding)))
}

/// Encodes `text` in `encoding`, writing the BOM for UTF-16.
pub(crate) fn encode(text: &str, encoding: TextEncoding) -> Vec<u8> {
    match encoding {
        TextEncoding::Utf8 => text.as_bytes().to_vec(),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let mut out = Vec::with_capacity(2 + text.len() * 2);
            let little = encoding == TextEncoding::Utf16Le;
            out.extend_from_slice(if little { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            for unit in text.encode_utf16() {
                out.extend_from_slice(&if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_round_trips_with_bom_and_rejects_invalid_payloads() {
        for encoding in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
            let text = "line one\r\nüñí 🙂\n";
            let bytes = encode(text, encoding);
            let (decoded, detected) = decode_utf16_bom(&bytes).unwrap().unwrap();
            assert_eq!((decoded.as_str(), detected), (text, encoding));
        }
        assert!(decode_utf16_bom(b"plain").is_none());
        assert!(decode_utf16_bom(&[0xEF, 0xBB, 0xBF, b'a']).is_none());
        assert!(decode_utf16_bom(&[0xFF, 0xFE, b'a']).unwrap().is_err());
        // Lone high surrogate.
        assert!(
            decode_utf16_bom(&[0xFF, 0xFE, 0x00, 0xD8])
                .unwrap()
                .is_err()
        );
        assert_eq!(encode("x", TextEncoding::Utf8), b"x");
    }
}
