use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use uuid::Uuid;

const AUTH_VERSION: &str = "MINI-AGENT-ACP-AUTH/1";
const CHALLENGE_PREFIX: &str = "MINI-AGENT-ACP-AUTH/1 CHALLENGE ";
const RESPONSE_PREFIX: &str = "MINI-AGENT-ACP-AUTH/1 RESPONSE ";
const RESPONSE_DIGEST_LEN: usize = 64;
const SHA256_BLOCK_LEN: usize = 64;
const MAX_RESPONSE_BYTES: usize = 128;
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthError {
    Disconnected,
    Invalid,
    Io,
    Oversized,
    Timeout,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ACP TCP authentication failed")
    }
}

impl std::error::Error for AuthError {}

pub(crate) fn authenticate_peer(stream: &mut TcpStream, api_key: &str) -> Result<(), AuthError> {
    authenticate_peer_with_timeout(stream, api_key, AUTH_TIMEOUT)
}

fn authenticate_peer_with_timeout(
    stream: &mut TcpStream,
    api_key: &str,
    timeout: Duration,
) -> Result<(), AuthError> {
    let started = Instant::now();
    let nonce = Uuid::new_v4().simple().to_string();
    let challenge = format!("{CHALLENGE_PREFIX}{nonce}\n");

    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| AuthError::Io)?;
    stream
        .write_all(challenge.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(classify_io_error)?;

    let remaining = timeout
        .checked_sub(started.elapsed())
        .ok_or(AuthError::Timeout)?;
    let response = read_bounded_line(stream, remaining)?;
    let candidate = response
        .strip_prefix(RESPONSE_PREFIX)
        .ok_or(AuthError::Invalid)?;
    let expected = response_digest(&nonce, api_key);

    if !fixed_time_digest_eq(candidate.as_bytes(), expected.as_bytes()) {
        return Err(AuthError::Invalid);
    }

    stream.set_read_timeout(None).map_err(|_| AuthError::Io)?;
    stream.set_write_timeout(None).map_err(|_| AuthError::Io)?;
    Ok(())
}

fn read_bounded_line(stream: &mut TcpStream, timeout: Duration) -> Result<String, AuthError> {
    let started = Instant::now();
    let mut bytes = Vec::with_capacity(MAX_RESPONSE_BYTES);

    loop {
        let remaining = timeout
            .checked_sub(started.elapsed())
            .ok_or(AuthError::Timeout)?;
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| AuthError::Io)?;

        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return Err(AuthError::Disconnected),
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => {
                if bytes.len() == MAX_RESPONSE_BYTES {
                    return Err(AuthError::Oversized);
                }
                bytes.push(byte[0]);
            }
            Ok(_) => unreachable!("single-byte read returned more than one byte"),
            Err(error) => return Err(classify_io_error(error)),
        }
    }

    String::from_utf8(bytes).map_err(|_| AuthError::Invalid)
}

fn classify_io_error(error: io::Error) -> AuthError {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => AuthError::Timeout,
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe => AuthError::Disconnected,
        _ => AuthError::Io,
    }
}

fn response_digest(nonce: &str, api_key: &str) -> String {
    let mut key_block = [0_u8; SHA256_BLOCK_LEN];
    if api_key.len() > SHA256_BLOCK_LEN {
        let hashed_key = Sha256::digest(api_key.as_bytes());
        key_block[..hashed_key.len()].copy_from_slice(&hashed_key);
    } else {
        key_block[..api_key.len()].copy_from_slice(api_key.as_bytes());
    }

    let mut inner_pad = key_block;
    let mut outer_pad = key_block;
    for byte in &mut inner_pad {
        *byte ^= 0x36;
    }
    for byte in &mut outer_pad {
        *byte ^= 0x5c;
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(AUTH_VERSION.as_bytes());
    inner.update([0]);
    inner.update(nonce.as_bytes());

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    crate::hex::encode_lower(outer.finalize())
}

fn fixed_time_digest_eq(candidate: &[u8], expected: &[u8]) -> bool {
    debug_assert_eq!(expected.len(), RESPONSE_DIGEST_LEN);

    let mut padded = [0_u8; RESPONSE_DIGEST_LEN];
    let copy_len = candidate.len().min(RESPONSE_DIGEST_LEN);
    padded[..copy_len].copy_from_slice(&candidate[..copy_len]);

    let mut difference = candidate.len() ^ RESPONSE_DIGEST_LEN;
    for (actual, wanted) in padded.iter().zip(expected) {
        difference |= usize::from(actual ^ wanted);
    }

    std::hint::black_box(difference) == 0
}

pub(crate) fn read_challenge(stream: &mut TcpStream) -> Result<String, AuthError> {
    let challenge = read_bounded_line(stream, AUTH_TIMEOUT)?;
    challenge
        .strip_prefix(CHALLENGE_PREFIX)
        .filter(|nonce| nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or(AuthError::Invalid)
}

pub(crate) fn send_response(
    stream: &mut TcpStream,
    nonce: &str,
    api_key: &str,
) -> Result<String, AuthError> {
    let response = format!("{RESPONSE_PREFIX}{}\n", response_digest(nonce, api_key));
    stream
        .write_all(response.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(classify_io_error)?;
    Ok(response)
}

pub(crate) fn verify_tcp_authentication() -> anyhow::Result<()> {
    const CHECK_KEY: &str = "installed-binary-authentication-check";

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let address = listener.local_addr()?;
    let server = std::thread::spawn(move || -> Result<[bool; 4], AuthError> {
        let mut outcomes = [false; 4];
        for outcome in &mut outcomes {
            let (mut stream, _) = listener.accept().map_err(|_| AuthError::Io)?;
            *outcome = authenticate_peer(&mut stream, CHECK_KEY).is_ok();
        }
        Ok(outcomes)
    });

    let mut first = TcpStream::connect(address)?;
    let first_nonce = read_challenge(&mut first)?;
    let replay = send_response(&mut first, &first_nonce, CHECK_KEY)?;

    let mut replayed = TcpStream::connect(address)?;
    let _ = read_challenge(&mut replayed)?;
    replayed.write_all(replay.as_bytes())?;
    replayed.flush()?;
    drop(replayed);

    let mut missing = TcpStream::connect(address)?;
    let _ = read_challenge(&mut missing)?;
    drop(missing);

    let mut final_valid = TcpStream::connect(address)?;
    let final_nonce = read_challenge(&mut final_valid)?;
    let _ = send_response(&mut final_valid, &final_nonce, CHECK_KEY)?;

    let outcomes = server
        .join()
        .map_err(|_| anyhow::anyhow!("ACP authentication check thread panicked"))??;
    anyhow::ensure!(
        outcomes == [true, false, false, true],
        "ACP authentication check did not reject missing or replayed credentials"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    #[test]
    fn authenticates_valid_challenge_response() {
        let (mut server, mut client) = connected_pair();
        let handle = std::thread::spawn(move || authenticate_peer(&mut server, "correct-key"));

        let nonce = read_challenge(&mut client).unwrap();
        send_response(&mut client, &nonce, "correct-key").unwrap();

        assert_eq!(handle.join().unwrap(), Ok(()));
    }

    #[test]
    fn rejects_wrong_key_without_exposing_it() {
        let (mut server, mut client) = connected_pair();
        let handle = std::thread::spawn(move || authenticate_peer(&mut server, "correct-key"));

        let nonce = read_challenge(&mut client).unwrap();
        send_response(&mut client, &nonce, "wrong-key").unwrap();

        let error = handle.join().unwrap().unwrap_err();
        assert_eq!(error, AuthError::Invalid);
        assert_eq!(error.to_string(), "ACP TCP authentication failed");
        assert!(!error.to_string().contains("correct-key"));
        assert!(!error.to_string().contains("wrong-key"));
    }

    #[test]
    fn rejects_oversized_response() {
        let (mut server, mut client) = connected_pair();
        let handle = std::thread::spawn(move || authenticate_peer(&mut server, "correct-key"));

        let _ = read_challenge(&mut client).unwrap();
        client.write_all(&[b'x'; MAX_RESPONSE_BYTES + 1]).unwrap();

        assert_eq!(handle.join().unwrap(), Err(AuthError::Oversized));
    }

    #[test]
    fn partial_response_obeys_total_deadline() {
        let (mut server, mut client) = connected_pair();
        let handle = std::thread::spawn(move || {
            authenticate_peer_with_timeout(&mut server, "correct-key", Duration::from_millis(40))
        });

        let _ = read_challenge(&mut client).unwrap();
        client.write_all(RESPONSE_PREFIX.as_bytes()).unwrap();
        client.flush().unwrap();

        assert_eq!(handle.join().unwrap(), Err(AuthError::Timeout));
    }

    #[test]
    fn replayed_response_is_rejected_by_fresh_nonce() {
        let first_nonce = "00000000000000000000000000000000";
        let second_nonce = "11111111111111111111111111111111";
        let replay = response_digest(first_nonce, "correct-key");
        let expected = response_digest(second_nonce, "correct-key");

        assert!(!fixed_time_digest_eq(
            replay.as_bytes(),
            expected.as_bytes()
        ));
    }

    #[test]
    fn response_digest_matches_hmac_sha256_vector() {
        assert_eq!(
            response_digest("00000000000000000000000000000000", "correct-key"),
            "24d1030854a2ee4a50dfd3d9acabddab7cabe8052958b99f77440f3f649a585f"
        );
    }

    #[test]
    fn installed_binary_check_covers_valid_missing_and_replay() {
        verify_tcp_authentication().unwrap();
    }
}
