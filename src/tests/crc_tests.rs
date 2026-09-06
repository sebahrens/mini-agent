use crate::agent::tools::crc::{Crc32, crc32, crc32_hex};

#[test]
fn test_crc32_empty() {
    assert_eq!(crc32(b""), 0x00000000);
}

#[test]
fn test_crc32_hello() {
    assert_eq!(crc32(b"hello"), 0x3610A686);
    assert_eq!(crc32_hex(b"hello"), "3610a686");
}

#[test]
fn test_crc32_deterministic() {
    assert_eq!(crc32(b"same string"), 0xE78408EA);
}

#[test]
fn test_crc32_different() {
    let a = crc32(b"hello");
    let b = crc32(b"world");
    assert_ne!(a, b);
}

#[test]
fn incremental_crc32_matches_one_shot_hashing() {
    let mut crc = Crc32::new();
    crc.update(b"hello");
    crc.update(b" ");
    crc.update(b"world");

    assert_eq!(crc.finalize(), crc32(b"hello world"));
}
