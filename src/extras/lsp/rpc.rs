//! Minimal JSON-RPC framing for LSP: `Content-Length`-headed messages over
//! the server's stdio. Hand-rolled to keep the dependency tree at `lsp-types`
//! only. Unit-tested over `tokio::io::duplex` — no live server needed.

use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub(crate) const MAX_HEADER_BYTES: usize = 8 * 1024;
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Writes one framed JSON-RPC message.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
    if body.len() > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "LSP message body too large",
        ));
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(body).await?;
    w.flush().await
}

/// Persistent buffered reader for LSP frames.
///
/// Keeping the buffer across frames avoids one underlying pipe read per header byte and preserves
/// any bytes read ahead from the next frame.
pub struct FrameReader<R> {
    inner: BufReader<R>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
        }
    }

    /// Reads one framed JSON-RPC message. Returns `Ok(None)` on a clean EOF before any header byte
    /// (server exited); an error on EOF mid-message.
    pub async fn read_frame(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        read_frame(&mut self.inner).await
    }
}

async fn read_frame<R: AsyncBufRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut headers = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte).await?;
        if n == 0 {
            if headers.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof inside LSP headers",
            ));
        }
        headers.push(byte[0]);
        if headers.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "LSP header block too large",
            ));
        }
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let headers = std::str::from_utf8(&headers).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LSP headers are not valid UTF-8",
        )
    })?;
    let mut content_length = None;
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("Content-Length") {
            continue;
        }
        if content_length.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "multiple Content-Length headers in LSP frame",
            ));
        }
        content_length = Some(value.trim().parse::<usize>().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid Content-Length in LSP frame",
            )
        })?);
    }
    let content_length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing Content-Length in LSP frame",
        )
    })?;
    if content_length > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "LSP message body too large",
        ));
    }

    let mut body = vec![0u8; content_length];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, ReadBuf};

    use super::*;

    struct CountingReader {
        bytes: Vec<u8>,
        offset: usize,
        reads: Arc<AtomicUsize>,
    }

    impl AsyncRead for CountingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let remaining = &self.bytes[self.offset..];
            let count = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..count]);
            self.offset += count;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn frame_reader_buffers_headers_and_read_ahead_across_frames() {
        let first = br#"{"id":1}"#;
        let second = br#"{"id":2}"#;
        let bytes = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            first.len(),
            std::str::from_utf8(first).unwrap(),
            second.len(),
            std::str::from_utf8(second).unwrap()
        )
        .into_bytes();
        let reads = Arc::new(AtomicUsize::new(0));
        let source = CountingReader {
            bytes,
            offset: 0,
            reads: reads.clone(),
        };
        let mut reader = FrameReader::new(source);

        assert_eq!(reader.read_frame().await.unwrap().unwrap(), first);
        assert_eq!(reader.read_frame().await.unwrap().unwrap(), second);
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "both small frames should be served from one buffered pipe read"
        );
    }
}
