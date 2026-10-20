//! Loopback HTTP/1.1 fixture for tests that run a one-shot fake server.
//!
//! Replaces the per-file `read_request`/`respond` boilerplate that split the
//! header block, parsed `Content-Length` and wrote a `Connection: close`
//! reply. Tests keep their own assertions over the returned [`Request`].
//! Byte-level transport tests (malformed frames, slow drip, chunking) should
//! keep writing to the socket directly.

use serde_json::Value;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;

/// One parsed request: the raw head, its lower-cased copy, and the body bytes.
pub(crate) struct Request {
    /// Request line plus headers exactly as received, including the final CRLFCRLF.
    pub(crate) head: String,
    /// ASCII-lower-cased `head`, for substring assertions on names and values.
    pub(crate) headers: String,
    pub(crate) body: Vec<u8>,
}

impl Request {
    fn parse(head: Vec<u8>) -> Self {
        let head = String::from_utf8(head).unwrap();
        let headers = head.to_ascii_lowercase();
        Self {
            head,
            headers,
            body: Vec::new(),
        }
    }

    /// Declared `Content-Length`, if the request has one.
    fn content_length(&self) -> Option<usize> {
        self.headers.lines().find_map(|line| {
            line.strip_prefix("content-length:")
                .map(|value| value.trim().parse().unwrap())
        })
    }

    /// The body parsed as JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

/// Formats a complete `Connection: close` response with an exact `Content-Length`.
pub(crate) fn response_bytes(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Tokio variant for `#[tokio::test]` fake servers.
pub(crate) mod asynchronous {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Binds an ephemeral loopback listener and returns it with `http://addr`.
    pub(crate) async fn bind() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (listener, base)
    }

    /// Reads the request line and headers only (no body is consumed).
    pub(crate) async fn read_head(socket: &mut TcpStream) -> Request {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < MAX_HEAD);
            head.push(socket.read_u8().await.unwrap());
        }
        Request::parse(head)
    }

    /// Reads one request including a `Content-Length` body.
    pub(crate) async fn read_request(socket: &mut TcpStream) -> Request {
        let mut request = read_head(socket).await;
        let length = request.content_length().unwrap();
        assert!(length < MAX_BODY);
        request.body = vec![0; length];
        socket.read_exact(&mut request.body).await.unwrap();
        request
    }

    /// Accepts the next connection and reads one request from it.
    pub(crate) async fn accept_request(listener: &TcpListener) -> (TcpStream, Request) {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        (socket, request)
    }

    /// Writes a `Connection: close` response with the given status text and body.
    pub(crate) async fn respond(
        socket: &mut TcpStream,
        status: &str,
        content_type: &str,
        body: &str,
    ) {
        socket
            .write_all(&response_bytes(status, content_type, body))
            .await
            .unwrap();
    }

    /// `200 OK` with `text/event-stream`.
    pub(crate) async fn respond_sse(socket: &mut TcpStream, body: &str) {
        respond(socket, "200 OK", "text/event-stream", body).await;
    }

    /// `200 OK` with `application/json`.
    pub(crate) async fn respond_json(socket: &mut TcpStream, body: &str) {
        respond(socket, "200 OK", "application/json", body).await;
    }
}

/// Blocking variant for fake servers on plain threads.
pub(crate) mod blocking {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Reads one request including a `Content-Length` body (limits are the caller's
    /// `max_body`; the head is capped at 64 KiB).
    pub(crate) fn read_request(socket: &mut TcpStream, max_body: usize) -> Request {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < MAX_HEAD);
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let mut request = Request::parse(head);
        let length = request.content_length().unwrap();
        assert!(length <= max_body);
        request.body = vec![0; length];
        socket.read_exact(&mut request.body).unwrap();
        request
    }

    /// Writes a `Connection: close` response and flushes.
    pub(crate) fn respond(socket: &mut TcpStream, status: &str, content_type: &str, body: &str) {
        socket
            .write_all(&response_bytes(status, content_type, body))
            .unwrap();
        socket.flush().unwrap();
    }
}
