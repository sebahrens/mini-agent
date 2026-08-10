/// Encode bytes as stable lowercase hexadecimal without relying on digest output
/// formatting traits.
pub(crate) fn encode_lower(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode_lower;

    #[test]
    fn encodes_empty_and_boundary_bytes_as_lowercase_hex() {
        assert_eq!(encode_lower([]), "");
        assert_eq!(encode_lower([0x00, 0x0f, 0x10, 0xff]), "000f10ff");
    }
}
