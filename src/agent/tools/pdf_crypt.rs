//! PDF standard security handler, opened with the empty user password.
//!
//! Many real PDFs only restrict permissions (printing, copying) and open in any
//! viewer without a password. This module derives the file key for those
//! documents (RC4-40/128 revisions 2-3, RC4/AESV2 revision 4, AESV3 revisions
//! 5 and 6) and decrypts stream data. Files whose user password is not empty
//! report [`CryptError::PasswordRequired`]; this reader never guesses or asks
//! for a password. Only streams are decrypted: text comes from content
//! streams, ToUnicode maps and object streams, never from encrypted strings.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use md5::Digest as _;
use sha2::Digest as _;

/// How one class of data (streams or strings) is encrypted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Method {
    Identity,
    Rc4,
    AesV2,
    AesV3,
}

/// Values read from the `/Encrypt` dictionary and the first trailer `/ID`.
pub(super) struct EncryptParams {
    pub(super) filter: Vec<u8>,
    pub(super) version: i64,
    pub(super) revision: i64,
    /// `/Length` in bits (RC4 only); 40 when absent.
    pub(super) length_bits: i64,
    pub(super) owner: Vec<u8>,
    pub(super) user: Vec<u8>,
    pub(super) user_key: Vec<u8>,
    pub(super) permissions: i32,
    pub(super) id0: Vec<u8>,
    pub(super) encrypt_metadata: bool,
    pub(super) stream_method: Method,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CryptError {
    /// The user password is not empty (or the handler data is inconsistent
    /// with an empty one).
    PasswordRequired,
    Unsupported(String),
}

pub(super) struct Decryptor {
    key: Vec<u8>,
    stream_method: Method,
    encrypt_metadata: bool,
}

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = md5::Md5::new();
    for part in parts {
        md5::Digest::update(&mut hasher, part);
    }
    md5::Digest::finalize(hasher).into()
}

pub(super) fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    let mut state: Vec<u8> = (0..=255).collect();
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, usize::from(j));
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|byte| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(state[usize::from(i)]);
            state.swap(usize::from(i), usize::from(j));
            let k = state[usize::from(state[usize::from(i)].wrapping_add(state[usize::from(j)]))];
            byte ^ k
        })
        .collect()
}

/// AES-CBC over whole blocks without padding handling.
fn cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if !data.len().is_multiple_of(16) || iv.len() != 16 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut previous = [0u8; 16];
    previous.copy_from_slice(iv);
    macro_rules! run {
        ($cipher:expr) => {{
            let cipher = $cipher;
            for block in data.chunks_exact(16) {
                let mut work = GenericArray::clone_from_slice(block);
                cipher.decrypt_block(&mut work);
                out.extend(work.iter().zip(previous.iter()).map(|(a, b)| a ^ b));
                previous.copy_from_slice(block);
            }
        }};
    }
    match key.len() {
        16 => run!(Aes128::new_from_slice(key).ok()?),
        32 => run!(Aes256::new_from_slice(key).ok()?),
        _ => return None,
    }
    Some(out)
}

fn cbc_encrypt_128(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if !data.len().is_multiple_of(16) || iv.len() != 16 || key.len() != 16 {
        return None;
    }
    let cipher = Aes128::new_from_slice(key).ok()?;
    let mut out = Vec::with_capacity(data.len());
    let mut previous = [0u8; 16];
    previous.copy_from_slice(iv);
    for block in data.chunks_exact(16) {
        let mut work = GenericArray::clone_from_slice(block);
        for (byte, prior) in work.iter_mut().zip(previous.iter()) {
            *byte ^= prior;
        }
        cipher.encrypt_block(&mut work);
        previous.copy_from_slice(&work);
        out.extend_from_slice(&work);
    }
    Some(out)
}

/// ISO 32000-2 algorithm 2.B (revision 6 hash) for an empty password.
fn hash_r6(salt: &[u8], udata: &[u8]) -> Option<Vec<u8>> {
    let mut input = salt.to_vec();
    input.extend_from_slice(udata);
    let mut k: Vec<u8> = sha2::Sha256::digest(&input).to_vec();
    let mut round = 0usize;
    loop {
        let mut k1 = Vec::with_capacity((k.len() + udata.len()) * 64);
        for _ in 0..64 {
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let e = cbc_encrypt_128(&k[..16], &k[16..32], &k1)?;
        let modulus: usize = e[..16].iter().map(|byte| usize::from(*byte)).sum::<usize>() % 3;
        k = match modulus {
            0 => sha2::Sha256::digest(&e).to_vec(),
            1 => sha2::Sha384::digest(&e).to_vec(),
            _ => sha2::Sha512::digest(&e).to_vec(),
        };
        round += 1;
        let last = usize::from(*e.last()?);
        if round >= 64 && last <= round - 32 {
            break;
        }
        if round > 4096 {
            return None;
        }
    }
    k.truncate(32);
    Some(k)
}

impl Decryptor {
    pub(super) fn open(params: &EncryptParams) -> Result<Self, CryptError> {
        if params.filter != b"Standard" {
            return Err(CryptError::Unsupported(format!(
                "security handler {}",
                String::from_utf8_lossy(&params.filter)
            )));
        }
        let key = match params.revision {
            2..=4 if matches!(params.version, 1 | 2 | 4) => Self::legacy_key(params)?,
            5 | 6 if params.version == 5 => Self::aes256_key(params)?,
            revision => {
                return Err(CryptError::Unsupported(format!(
                    "standard security handler revision {revision} (version {})",
                    params.version
                )));
            }
        };
        Ok(Self {
            key,
            stream_method: params.stream_method,
            encrypt_metadata: params.encrypt_metadata,
        })
    }

    fn legacy_key(params: &EncryptParams) -> Result<Vec<u8>, CryptError> {
        let bytes = if params.version == 4 {
            16
        } else if params.revision == 2 {
            5
        } else {
            usize::try_from(params.length_bits / 8)
                .unwrap_or(5)
                .clamp(5, 16)
        };
        let owner = params.owner.get(..32).ok_or(CryptError::PasswordRequired)?;
        let permissions = params.permissions.to_le_bytes();
        let metadata_flag: &[u8] = if params.revision >= 4 && !params.encrypt_metadata {
            &[0xFF; 4]
        } else {
            &[]
        };
        let mut digest = md5(&[&PAD, owner, &permissions, &params.id0, metadata_flag]);
        if params.revision >= 3 {
            for _ in 0..50 {
                digest = md5(&[&digest[..bytes]]);
            }
        }
        let key = digest[..bytes].to_vec();
        let user = params.user.get(..32).ok_or(CryptError::PasswordRequired)?;
        let matches = if params.revision == 2 {
            rc4(&key, &PAD) == user
        } else {
            let seed = md5(&[&PAD, &params.id0]);
            let mut value = rc4(&key, &seed);
            for round in 1..=19u8 {
                let round_key: Vec<u8> = key.iter().map(|byte| byte ^ round).collect();
                value = rc4(&round_key, &value);
            }
            value[..16] == user[..16]
        };
        if matches {
            Ok(key)
        } else {
            Err(CryptError::PasswordRequired)
        }
    }

    fn aes256_key(params: &EncryptParams) -> Result<Vec<u8>, CryptError> {
        let user = params.user.get(..48).ok_or(CryptError::PasswordRequired)?;
        let user_key = params
            .user_key
            .get(..32)
            .ok_or(CryptError::PasswordRequired)?;
        let (hash, validation_salt, key_salt) = (&user[..32], &user[32..40], &user[40..48]);
        let digest = |salt: &[u8]| -> Result<Vec<u8>, CryptError> {
            if params.revision == 5 {
                Ok(sha2::Sha256::digest(salt).to_vec())
            } else {
                hash_r6(salt, &[]).ok_or(CryptError::PasswordRequired)
            }
        };
        if digest(validation_salt)? != hash {
            return Err(CryptError::PasswordRequired);
        }
        let intermediate = digest(key_salt)?;
        cbc_decrypt(&intermediate, &[0u8; 16], user_key).ok_or(CryptError::PasswordRequired)
    }

    /// Whether a stream of this `/Type` is stored unencrypted.
    pub(super) fn leaves_clear(&self, stream_type: Option<&[u8]>) -> bool {
        match stream_type {
            Some(b"XRef") => true,
            Some(b"Metadata") => !self.encrypt_metadata,
            _ => false,
        }
    }

    /// Decrypt one stream's data; `None` when it is malformed.
    pub(super) fn decrypt_stream(
        &self,
        number: u32,
        generation: u16,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        match self.stream_method {
            Method::Identity => Some(data.to_vec()),
            Method::Rc4 => Some(rc4(&self.object_key(number, generation, false), data)),
            Method::AesV2 => {
                let key = self.object_key(number, generation, true);
                Self::aes_stream(&key, data)
            }
            Method::AesV3 => Self::aes_stream(&self.key, data),
        }
    }

    fn object_key(&self, number: u32, generation: u16, aes: bool) -> Vec<u8> {
        let number = number.to_le_bytes();
        let generation = generation.to_le_bytes();
        let mut parts: Vec<&[u8]> = vec![&self.key, &number[..3], &generation];
        if aes {
            parts.push(b"sAlT");
        }
        let digest = md5(&parts);
        digest[..(self.key.len() + 5).min(16)].to_vec()
    }

    fn aes_stream(key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
        let (iv, body) = data.split_at_checked(16)?;
        let mut plain = cbc_decrypt(key, iv, body)?;
        if let Some(&pad) = plain.last()
            && (1..=16).contains(&pad)
            && usize::from(pad) <= plain.len()
        {
            plain.truncate(plain.len() - usize::from(pad));
        }
        Some(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rc4_matches_published_vectors() {
        assert_eq!(rc4(b"Key", b"Plaintext"), hex("BBF316E8D940AF0AD3"));
        assert_eq!(rc4(b"Wiki", b"pedia"), hex("1021BF0420"));
        assert_eq!(
            rc4(b"Secret", b"Attack at dawn"),
            hex("45A01F645FC35B383552544B9BF5")
        );
    }

    #[test]
    fn md5_matches_published_vectors() {
        assert_eq!(
            md5(&[b"abc"]).to_vec(),
            hex("900150983cd24fb0d6963f7d28e17f72")
        );
        assert_eq!(md5(&[b"a", b"bc"]), md5(&[b"abc"]));
    }

    #[test]
    fn aes_cbc_matches_the_nist_vectors() {
        // NIST SP 800-38A F.2.1 (AES-128 CBC), first two blocks.
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let plain = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let cipher = hex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2");
        assert_eq!(cbc_encrypt_128(&key, &iv, &plain).unwrap(), cipher);
        assert_eq!(cbc_decrypt(&key, &iv, &cipher).unwrap(), plain);
        // NIST SP 800-38A F.2.5 (AES-256 CBC), first block.
        let key = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let cipher = hex("f58c4c04d6e5f1ba779eabfb5f7bfbd6");
        assert_eq!(
            cbc_decrypt(&key, &iv, &cipher).unwrap(),
            hex("6bc1bee22e409f96e93d7e117393172a")
        );
        assert!(cbc_decrypt(&key, &iv, &cipher[..15]).is_none());
        assert!(cbc_decrypt(&key[..20], &iv, &cipher).is_none());
    }

    #[test]
    fn aes_streams_strip_padding_and_reject_short_data() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let mut data = vec![7u8; 16];
        let mut plain = b"hello".to_vec();
        plain.extend_from_slice(&[11u8; 11]);
        data.extend_from_slice(&cbc_encrypt_128(&key, &[7u8; 16], &plain).unwrap());
        assert_eq!(Decryptor::aes_stream(&key, &data).unwrap(), b"hello");
        assert!(Decryptor::aes_stream(&key, &data[..10]).is_none());
        assert!(Decryptor::aes_stream(&key, &data[..20]).is_none());
    }
}
