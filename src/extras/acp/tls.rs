//! TLS for the ACP TCP transport.
//!
//! The plaintext TCP transport authenticates only the client, only during the
//! handshake. With `MINI_AGENT_ACP_TLS_CERT` and `MINI_AGENT_ACP_TLS_KEY` set,
//! every accepted connection is first wrapped in TLS (1.2 or newer) using the
//! operator's certificate, and the challenge-response then runs inside that
//! channel in its channel-bound form (see
//! [`crate::acp_auth::channel_bound_response_digest`]): the response covers
//! the SHA-256 of the server certificate the client observed, so a man in the
//! middle presenting any other certificate cannot relay a valid response,
//! even to a client that did not verify the certificate chain. The session
//! that follows is encrypted and integrity-protected by TLS.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::acp_auth::{AuthError, authenticate_tls_peer};

pub(super) const ACP_TLS_CERT_ENV: &str = "MINI_AGENT_ACP_TLS_CERT";
pub(super) const ACP_TLS_KEY_ENV: &str = "MINI_AGENT_ACP_TLS_KEY";

/// Budget for the TLS handshake plus the challenge-response of one peer.
const TLS_AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Server TLS identity and the certificate digest the handshake binds.
pub(super) struct AcpTlsConfig {
    acceptor: tokio_native_tls::TlsAcceptor,
    certificate_sha256: String,
}

impl AcpTlsConfig {
    /// Loads a PEM certificate chain (leaf first) and a PEM PKCS#8 private
    /// key.
    pub(super) fn load(certificate: &Path, key: &Path) -> anyhow::Result<Self> {
        let certificate_pem = std::fs::read(certificate).map_err(|error| {
            anyhow::anyhow!(
                "ACP TLS certificate '{}' is unreadable: {error}",
                certificate.display()
            )
        })?;
        let key_pem = std::fs::read(key).map_err(|error| {
            anyhow::anyhow!("ACP TLS key '{}' is unreadable: {error}", key.display())
        })?;
        Self::from_pem(&certificate_pem, &key_pem)
    }

    pub(super) fn from_pem(certificate_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Self> {
        let leaf = first_pem_certificate(certificate_pem)
            .ok_or_else(|| anyhow::anyhow!("ACP TLS certificate file contains no certificate"))?;
        let leaf_der = native_tls::Certificate::from_pem(leaf.as_bytes())
            .and_then(|certificate| certificate.to_der())
            .map_err(|error| anyhow::anyhow!("ACP TLS certificate is invalid: {error}"))?;
        let identity =
            native_tls::Identity::from_pkcs8(certificate_pem, key_pem).map_err(|error| {
                anyhow::anyhow!(
                    "ACP TLS certificate/key pair is invalid (the key must be PEM PKCS#8): {error}"
                )
            })?;
        let acceptor = native_tls::TlsAcceptor::builder(identity)
            .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
            .build()
            .map_err(|error| anyhow::anyhow!("ACP TLS acceptor could not be built: {error}"))?;
        Ok(Self {
            acceptor: acceptor.into(),
            certificate_sha256: certificate_sha256(&leaf_der),
        })
    }

    #[cfg(test)]
    pub(super) fn certificate_sha256(&self) -> &str {
        &self.certificate_sha256
    }
}

/// Lowercase hex SHA-256 of a DER certificate, as bound by the handshake.
pub(super) fn certificate_sha256(der: &[u8]) -> String {
    crate::hex::encode_lower(Sha256::digest(der))
}

/// The certificate and key paths from the environment: both or neither.
pub(super) fn tls_paths_from_env() -> anyhow::Result<Option<(PathBuf, PathBuf)>> {
    tls_paths(
        std::env::var_os(ACP_TLS_CERT_ENV).filter(|value| !value.is_empty()),
        std::env::var_os(ACP_TLS_KEY_ENV).filter(|value| !value.is_empty()),
    )
}

fn tls_paths(
    certificate: Option<std::ffi::OsString>,
    key: Option<std::ffi::OsString>,
) -> anyhow::Result<Option<(PathBuf, PathBuf)>> {
    match (certificate, key) {
        (None, None) => Ok(None),
        (Some(certificate), Some(key)) => Ok(Some((certificate.into(), key.into()))),
        _ => anyhow::bail!("ACP TLS needs both {ACP_TLS_CERT_ENV} and {ACP_TLS_KEY_ENV}"),
    }
}

fn first_pem_certificate(pem: &[u8]) -> Option<String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let text = std::str::from_utf8(pem).ok()?;
    let start = text.find(BEGIN)?;
    let end = start + text[start..].find(END)? + END.len();
    Some(text[start..end].to_string())
}

pub(super) type TlsStream = tokio_native_tls::TlsStream<tokio::net::TcpStream>;

/// Accepts connections until one completes the TLS handshake and the
/// channel-bound challenge-response. Slow or failing peers never block a
/// valid one: each is handled concurrently, bounded in number and time.
pub(super) async fn accept_tls_peer(
    listener: std::net::TcpListener,
    api_key: String,
    tls: Arc<AcpTlsConfig>,
    max_pending: usize,
) -> std::io::Result<(TlsStream, SocketAddr)> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let api_key: Arc<str> = api_key.into();
    let mut pending = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, peer_addr) = accepted?;
                if pending.len() >= max_pending {
                    tracing::warn!("ACP TLS peer rejected because authentication capacity is full");
                    continue;
                }
                let api_key = api_key.clone();
                let tls = tls.clone();
                pending.spawn(async move {
                    let result = tokio::time::timeout(TLS_AUTH_TIMEOUT, async {
                        let mut stream = tls
                            .acceptor
                            .accept(tcp)
                            .await
                            .map_err(|_| AuthError::Invalid)?;
                        authenticate_tls_peer(&mut stream, &api_key, &tls.certificate_sha256)
                            .await?;
                        Ok::<_, AuthError>(stream)
                    })
                    .await
                    .unwrap_or(Err(AuthError::Timeout));
                    (result, peer_addr)
                });
            }
            Some(joined) = pending.join_next(), if !pending.is_empty() => {
                match joined {
                    Ok((Ok(stream), peer_addr)) => return Ok((stream, peer_addr)),
                    Ok((Err(_), peer_addr)) => {
                        tracing::warn!("ACP TLS peer authentication rejected for {}", peer_addr);
                    }
                    Err(error) => tracing::warn!("ACP TLS authentication task failed: {error}"),
                }
            }
        }
    }
}

/// Adapts a Tokio byte stream to the `futures` I/O traits the ACP transport
/// consumes.
pub(super) struct FuturesIo<T>(pub(super) T);

impl<T: tokio::io::AsyncRead + Unpin> futures::io::AsyncRead for FuturesIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read_buf = tokio::io::ReadBuf::new(buf);
        match Pin::new(&mut self.0).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> futures::io::AsyncWrite for FuturesIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_auth::{
        TLS_CHALLENGE_PREFIX, TLS_RESPONSE_PREFIX, channel_bound_response_digest,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    const CERT: &str = include_str!("testdata/test-only-tls-cert.pem");
    const KEY: &str = include_str!("testdata/test-only-tls-key.pem");

    fn config() -> Arc<AcpTlsConfig> {
        Arc::new(AcpTlsConfig::from_pem(CERT.as_bytes(), KEY.as_bytes()).unwrap())
    }

    /// Connects over TLS without verifying the chain (the channel binding is
    /// what authenticates the server here) and returns the stream plus the
    /// SHA-256 of the certificate the client observed.
    async fn connect(
        address: SocketAddr,
    ) -> (tokio_native_tls::TlsStream<tokio::net::TcpStream>, String) {
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let connector = tokio_native_tls::TlsConnector::from(connector);
        let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        let stream = connector.connect("localhost", tcp).await.unwrap();
        let observed = stream
            .get_ref()
            .peer_certificate()
            .unwrap()
            .expect("server presents a certificate")
            .to_der()
            .unwrap();
        (stream, certificate_sha256(&observed))
    }

    async fn respond(
        stream: &mut tokio_native_tls::TlsStream<tokio::net::TcpStream>,
        api_key: &str,
        binding: &str,
    ) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let nonce = line
            .trim_end()
            .strip_prefix(TLS_CHALLENGE_PREFIX)
            .expect("channel-bound challenge");
        let response = format!(
            "{TLS_RESPONSE_PREFIX}{}\n",
            channel_bound_response_digest(nonce, api_key, binding)
        );
        let stream = reader.into_inner();
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    }

    fn listener() -> (std::net::TcpListener, SocketAddr) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    #[tokio::test]
    async fn tls_peer_with_matching_channel_binding_is_accepted_and_session_is_encrypted() {
        let tls = config();
        let (listener, address) = listener();
        let server = tokio::spawn(accept_tls_peer(listener, "key".into(), tls.clone(), 4));

        let (mut client, observed) = connect(address).await;
        assert_eq!(observed, tls.certificate_sha256());
        respond(&mut client, "key", &observed).await;

        let (mut accepted, _) = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("a valid TLS peer is accepted")
            .unwrap()
            .unwrap();
        // The accepted session carries application data over TLS.
        client.write_all(b"{\"jsonrpc\":\"2.0\"}\n").await.unwrap();
        client.flush().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut accepted)
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line, "{\"jsonrpc\":\"2.0\"}\n");
    }

    #[tokio::test]
    async fn relayed_response_bound_to_another_certificate_is_rejected() {
        let tls = config();
        let (listener, address) = listener();
        let server = tokio::spawn(accept_tls_peer(listener, "key".into(), tls, 4));

        // A man in the middle holds its own certificate, so the client's
        // response is bound to that certificate's digest instead.
        let (mut relayed, _) = connect(address).await;
        respond(
            &mut relayed,
            "key",
            &certificate_sha256(b"attacker certificate"),
        )
        .await;
        let (mut wrong_key, observed) = connect(address).await;
        respond(&mut wrong_key, "other-key", &observed).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(500), server)
                .await
                .is_err(),
            "no peer may authenticate with a wrong binding or key"
        );
    }

    #[tokio::test]
    async fn plaintext_peer_cannot_authenticate_to_a_tls_listener() {
        let tls = config();
        let (listener, address) = listener();
        let server = tokio::spawn(accept_tls_peer(listener, "key".into(), tls, 4));
        let mut plain = tokio::net::TcpStream::connect(address).await.unwrap();
        plain
            .write_all(b"MINI-AGENT-ACP-AUTH/1 RESPONSE 00\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), server)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn futures_adapter_round_trips_split_halves() {
        use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (left, right) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let mut writer = FuturesIo(left_write);
        let mut reader = FuturesIo(right_read);
        writer.write_all(b"frame\n").await.unwrap();
        writer.flush().await.unwrap();
        let mut buffer = [0_u8; 6];
        reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"frame\n");
        writer.close().await.unwrap();
        drop((left_read, right_write));
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "closing the writer ends the reader");
    }

    #[test]
    fn tls_requires_both_certificate_and_key() {
        assert!(tls_paths(None, None).unwrap().is_none());
        assert!(
            tls_paths(Some("cert.pem".into()), Some("key.pem".into()))
                .unwrap()
                .is_some()
        );
        let error = tls_paths(Some("cert.pem".into()), None).unwrap_err();
        assert!(error.to_string().contains(ACP_TLS_KEY_ENV), "{error}");
        assert!(AcpTlsConfig::from_pem(b"not a certificate", KEY.as_bytes()).is_err());
        assert!(AcpTlsConfig::from_pem(CERT.as_bytes(), b"not a key").is_err());
    }
}
