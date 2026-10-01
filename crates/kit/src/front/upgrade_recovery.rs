//! Putting back the `Upgrade` and `Connection` request headers a reverse proxy dropped.
//!
//! Laravel Cloud's edge forwards a WebSocket handshake to the application without them: the
//! request arrives with `Sec-WebSocket-Key` and `Sec-WebSocket-Version` but no `Upgrade:
//! websocket` and no `Connection: upgrade` (probed 2026-10-01; the same edge answered a Rails
//! upgrade with 520 on 2026-09-27). hyper decides whether a connection may be upgraded from those
//! two headers alone, so without them there is no `OnUpgrade` extension and Action Cable can never
//! take the socket, however tolerant its own handshake check is. The bytes have to be put back
//! before hyper parses them.
//!
//! This is off unless `RECOVER_UPGRADE_HEADERS` is set, and it is not Thruster's: it exists only
//! for that edge. It rewrites the head of an HTTP/1 request that carries `Sec-WebSocket-Key`
//! without `Upgrade`, and otherwise passes every byte through untouched. Anything it cannot frame
//! with certainty — a chunked body, a head over `MAX_HEAD`, an upgrade it has already recovered —
//! turns it off for the rest of the connection rather than guessing.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The most a request head may be before the connection is left alone. nginx's own default
/// `large_client_header_buffers` total is smaller than this.
const MAX_HEAD: usize = 64 * 1024;

const INSERTED: &[u8] = b"Upgrade: websocket\r\nConnection: Upgrade\r\n";

enum State {
    /// Reading a request head into `head`.
    Head,
    /// Passing a request body of this many bytes through.
    Body(u64),
    /// Done rewriting this connection: every byte goes through as it arrives.
    Passthrough,
}

/// Wraps a connection's IO, rewriting request heads on the way in (see the module docs).
pub struct RecoverUpgrade<I> {
    inner: I,
    state: State,
    /// The head being read, or bytes already rewritten and waiting for the reader.
    head: Vec<u8>,
    ready: Vec<u8>,
    taken: usize,
}

impl<I> RecoverUpgrade<I> {
    pub fn new(inner: I) -> Self {
        Self { inner, state: State::Head, head: Vec::new(), ready: Vec::new(), taken: 0 }
    }

    /// Moves what has been rewritten into the caller's buffer. True when it wrote something.
    fn drain(&mut self, buf: &mut ReadBuf<'_>) -> bool {
        let pending = &self.ready[self.taken..];
        if pending.is_empty() {
            return false;
        }
        let n = pending.len().min(buf.remaining());
        buf.put_slice(&pending[..n]);
        self.taken += n;
        if self.taken == self.ready.len() {
            self.ready.clear();
            self.taken = 0;
        }
        true
    }

    /// Hands `bytes` to the reader as they are, and stops rewriting this connection.
    fn pass_on(&mut self, bytes: Vec<u8>) {
        self.ready = bytes;
        self.taken = 0;
        self.state = State::Passthrough;
    }

    /// Takes the complete head ending at `end` out of the buffer, rewrites it if it is a stripped
    /// handshake, and keeps whatever followed it: this body's bytes go out behind the head, and
    /// anything past the body starts the next head.
    fn take_head(&mut self, end: usize) {
        let rest = self.head.split_off(end);
        let head = std::mem::take(&mut self.head);
        let recovered = recover(&head);
        // An upgrade this just recovered becomes a tunnel, and so does a body it cannot frame.
        let Some(length) = (if recovered.is_some() { None } else { body_length(&head) }) else {
            let mut out = recovered.unwrap_or(head);
            out.extend_from_slice(&rest);
            self.pass_on(out);
            return;
        };
        let body = usize::try_from(length).unwrap_or(usize::MAX).min(rest.len());
        let mut out = recovered.unwrap_or(head);
        out.extend_from_slice(&rest[..body]);
        self.head = rest[body..].to_vec();
        self.ready = out;
        self.taken = 0;
        self.state = match length - body as u64 {
            0 => State::Head,
            left => State::Body(left),
        };
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for RecoverUpgrade<I> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.drain(buf) {
                return Poll::Ready(Ok(()));
            }
            match this.state {
                State::Passthrough => return Pin::new(&mut this.inner).poll_read(cx, buf),
                State::Body(remaining) => {
                    // A body's bytes are never rewritten, but the next head still has to be
                    // found, so a read may not run past this body's end.
                    let read = if remaining >= buf.remaining() as u64 {
                        let before = buf.filled().len();
                        ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
                        (buf.filled().len() - before) as u64
                    } else {
                        let mut scratch = [0u8; 8 * 1024];
                        let limit = usize::try_from(remaining).unwrap_or(usize::MAX).min(scratch.len()).min(buf.remaining());
                        let mut incoming = ReadBuf::new(&mut scratch[..limit]);
                        ready!(Pin::new(&mut this.inner).poll_read(cx, &mut incoming))?;
                        buf.put_slice(incoming.filled());
                        incoming.filled().len() as u64
                    };
                    this.state = match remaining - read {
                        0 => State::Head,
                        left => State::Body(left),
                    };
                    return Poll::Ready(Ok(()));
                }
                State::Head => {
                    // Whatever is already buffered may hold the whole head, and reading again
                    // would block waiting for a request that has already arrived.
                    if let Some(end) = find_head_end(&this.head) {
                        this.take_head(end);
                        continue;
                    }
                    let mut scratch = [0u8; 8 * 1024];
                    let mut incoming = ReadBuf::new(&mut scratch);
                    ready!(Pin::new(&mut this.inner).poll_read(cx, &mut incoming))?;
                    if incoming.filled().is_empty() {
                        // End of stream: hand over whatever partial head was read.
                        let head = std::mem::take(&mut this.head);
                        if head.is_empty() {
                            return Poll::Ready(Ok(()));
                        }
                        this.pass_on(head);
                        continue;
                    }
                    this.head.extend_from_slice(incoming.filled());
                    if find_head_end(&this.head).is_none() && this.head.len() > MAX_HEAD {
                        let head = std::mem::take(&mut this.head);
                        this.pass_on(head);
                    }
                }
            }
        }
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for RecoverUpgrade<I> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The offset just past the head's blank line, once the whole head has arrived.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n").map(|start| start + 4)
}

/// The head with `Upgrade` and `Connection` put back, or `None` when it isn't a stripped handshake.
fn recover(head: &[u8]) -> Option<Vec<u8>> {
    if !head.starts_with(b"GET ") {
        return None;
    }
    let mut lines = head.split(|byte| *byte == b'\n').skip(1);
    if !lines.any(|line| starts_with_name(line, b"sec-websocket-key:")) {
        return None;
    }
    let named = |name: &[u8]| head.split(|byte| *byte == b'\n').skip(1).any(|line| starts_with_name(line, name));
    if named(b"upgrade:") || named(b"connection:") {
        return None;
    }
    let blank = head.len().checked_sub(2)?;
    let mut out = Vec::with_capacity(head.len() + INSERTED.len());
    out.extend_from_slice(&head[..blank]);
    out.extend_from_slice(INSERTED);
    out.extend_from_slice(b"\r\n");
    Some(out)
}

fn starts_with_name(line: &[u8], name: &[u8]) -> bool {
    line.len() >= name.len() && line[..name.len()].eq_ignore_ascii_case(name)
}

/// The request body's length, or `None` when it cannot be known from the head alone.
fn body_length(head: &[u8]) -> Option<u64> {
    let mut length = 0;
    for line in head.split(|byte| *byte == b'\n').skip(1) {
        if starts_with_name(line, b"transfer-encoding:") || starts_with_name(line, b"upgrade:") {
            return None;
        }
        if starts_with_name(line, b"content-length:") {
            let value = std::str::from_utf8(&line[b"content-length:".len()..]).ok()?;
            length = value.trim().parse().ok()?;
        }
    }
    Some(length)
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    async fn rewritten(input: &[u8]) -> Vec<u8> {
        let mut reader = RecoverUpgrade::new(std::io::Cursor::new(input.to_vec()));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        out
    }

    const STRIPPED: &[u8] =
        b"GET /cable HTTP/1.1\r\nHost: example.com\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";

    #[tokio::test]
    async fn puts_back_the_headers_the_proxy_dropped() {
        let out = rewritten(STRIPPED).await;
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Upgrade: websocket\r\n"), "{text}");
        assert!(text.contains("Connection: Upgrade\r\n"), "{text}");
        assert!(text.ends_with("Sec-WebSocket-Version: 13\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"), "{text}");
    }

    #[tokio::test]
    async fn arrives_the_same_when_the_headers_are_already_there() {
        let intact =
            b"GET /cable HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: k\r\n\r\n";
        assert_eq!(rewritten(intact).await, intact);
    }

    #[tokio::test]
    async fn leaves_an_ordinary_request_alone() {
        let plain = b"GET /rooms/1 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert_eq!(rewritten(plain).await, plain);
    }

    #[tokio::test]
    async fn finds_a_handshake_after_a_request_with_a_body() {
        let mut input = b"POST /messages HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        input.extend_from_slice(STRIPPED);
        let text = String::from_utf8(rewritten(&input).await).unwrap();
        assert!(text.starts_with("POST /messages"), "{text}");
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("Upgrade: websocket\r\n"), "{text}");
    }

    #[tokio::test]
    async fn gives_up_on_a_chunked_body_rather_than_guessing() {
        let mut input = b"POST /messages HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec();
        input.extend_from_slice(STRIPPED);
        let out = rewritten(&input).await;
        assert_eq!(out, input, "a chunked body stops the rewriting instead of framing it wrong");
    }

    #[tokio::test]
    async fn a_body_split_across_reads_still_frames_the_next_head() {
        // The body arrives partly in the head's read and partly after it.
        let mut input = b"POST /m HTTP/1.1\r\nContent-Length: 11\r\n\r\nhello".to_vec();
        input.extend_from_slice(b" world");
        input.extend_from_slice(STRIPPED);
        let text = String::from_utf8(rewritten(&input).await).unwrap();
        assert!(text.contains("hello world"), "{text}");
        assert!(text.contains("Upgrade: websocket\r\n"), "{text}");
    }
}
