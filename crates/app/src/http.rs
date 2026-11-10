//! One HTTP/1.1 request to the agent's local API and its answer, over a plain loopback TCP connection: written out by
//! hand (the agent is the only peer, and it answers with a `Content-Length`), read with `httparse`, every size capped.

use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use craft_cloud_api::local::MAX_LOCAL_JSON_BYTES;

use crate::Error;

/// Connecting must take no longer (it is this machine).
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// A quick call's answer (status) must come within this: the agent answers it from memory.
pub(crate) const QUICK_TIMEOUT: Duration = Duration::from_secs(2);
/// What the agent's loop answers itself (pause, resume, quit, autostart): quick, but it may be finishing something.
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// A sign-in or sign-out waits for the server (an Argon2 check, the first steps, a revoke): only these get this long.
pub(crate) const SLOW_TIMEOUT: Duration = Duration::from_secs(90);
/// Most bytes of an answer's head.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// An answer: status and body, and the wait in seconds its `Retry-After` header names (a number of seconds; a date isn't read).
#[derive(Debug)]
pub(crate) struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after_secs: Option<u64>,
}

/// Sends `method path` with the token and an optional JSON body to the agent at `addr`; the answer must come within
/// `timeout`.
pub(crate) fn request(addr: SocketAddr, token: &str, method: &str, path: &str, body: Option<&[u8]>, timeout: Duration) -> Result<Answer, Error> {
    request_with(addr, token, method, path, body, "application/json", MAX_LOCAL_JSON_BYTES, timeout)
}

/// [`request`] with a body of `content_type` and an answer of at most `max` bytes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn request_with(
    addr: SocketAddr,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    content_type: &str,
    max: usize,
    timeout: Duration,
) -> Result<Answer, Error> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| Error::NotRunning(format!("can't reach it: {e}")))?;
    stream.set_read_timeout(Some(timeout)).map_err(io)?;
    stream.set_write_timeout(Some(timeout)).map_err(io)?;
    let body = body.unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.write_all(body).map_err(io)?;
    stream.flush().map_err(io)?;
    read_answer_max(&mut stream, max)
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

/// Reads one answer (the connection closes after it).
#[cfg(test)]
pub(crate) fn read_answer(stream: &mut impl Read) -> Result<Answer, Error> {
    read_answer_max(stream, MAX_LOCAL_JSON_BYTES)
}

/// Reads one answer of at most `max` bytes of body (the connection closes after it).
pub(crate) fn read_answer_max(stream: &mut impl Read, max: usize) -> Result<Answer, Error> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let (status, length, head_len, retry_after_secs) = loop {
        let n = stream.read(&mut chunk).map_err(io)?;
        if n == 0 {
            return Err(Error::Protocol("the agent closed the connection before answering".into()));
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut headers);
        match response.parse(&buf) {
            Ok(httparse::Status::Complete(head_len)) => {
                let status = response.code.ok_or_else(|| Error::Protocol("an answer without a status".into()))?;
                let mut length = None;
                let mut retry_after_secs = None;
                for header in response.headers.iter() {
                    if header.name.eq_ignore_ascii_case("content-length") {
                        let text = std::str::from_utf8(header.value).map_err(|_| Error::Protocol("a bad Content-Length".into()))?;
                        length = Some(text.trim().parse::<usize>().map_err(|_| Error::Protocol("a bad Content-Length".into()))?);
                    } else if header.name.eq_ignore_ascii_case("retry-after") {
                        retry_after_secs = std::str::from_utf8(header.value).ok().and_then(|text| text.trim().parse::<u64>().ok());
                    } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
                        return Err(Error::Protocol("the agent's answer is chunked, which this client doesn't read".into()));
                    }
                }
                break (status, length, head_len, retry_after_secs);
            }
            Ok(httparse::Status::Partial) if buf.len() <= MAX_HEAD_BYTES => continue,
            Ok(httparse::Status::Partial) => return Err(Error::Protocol("the answer's head is too long".into())),
            Err(e) => return Err(Error::Protocol(format!("the answer doesn't parse: {e}"))),
        }
    };
    if length.is_some_and(|n| n > max) {
        return Err(Error::Protocol("the answer is too long".into()));
    }
    let mut body = buf.split_off(head_len.min(buf.len()));
    loop {
        if length.is_some_and(|n| body.len() >= n) {
            break;
        }
        let n = stream.read(&mut chunk).map_err(io)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if body.len() > max {
            return Err(Error::Protocol("the answer is too long".into()));
        }
    }
    if let Some(n) = length {
        if body.len() < n {
            return Err(Error::Protocol("the answer was cut short".into()));
        }
        body.truncate(n);
    }
    Ok(Answer { status, body, retry_after_secs })
}

/// What [`request_stream`] got: the agent refused (an answer like any other), or the body of a good answer, to be read as it
/// comes.
pub(crate) enum Streamed {
    Refused(Answer),
    Body(BodyReader<std::io::BufReader<TcpStream>>),
}

/// Most bytes of a refusal's body read from a stream request.
const MAX_REFUSAL_BYTES: usize = 64 * 1024;
/// Longest chunk-size line (hex digits and extensions) read.
const MAX_CHUNK_LINE: usize = 64;

/// `method path` with the token to the agent at `addr`, and its answer read as it comes: for a long body (an audit export)
/// that must not be held whole. A good answer (2xx) is its [`BodyReader`] (chunked, with a `Content-Length`, or to the end of
/// the connection); any other is read whole (at most [`MAX_REFUSAL_BYTES`]) as an [`Answer`]. `timeout` is how long a
/// single read or write may wait, not the whole transfer.
pub(crate) fn request_stream(addr: SocketAddr, token: &str, method: &str, path: &str, timeout: Duration) -> Result<Streamed, Error> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| Error::NotRunning(format!("can't reach it: {e}")))?;
    stream.set_read_timeout(Some(timeout)).map_err(io)?;
    stream.set_write_timeout(Some(timeout)).map_err(io)?;
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.flush().map_err(io)?;
    let mut reader = std::io::BufReader::with_capacity(8 * 1024, stream);
    // The head: lines up to the empty one, at most MAX_HEAD_BYTES.
    let mut raw = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = (&mut reader).take(MAX_HEAD_BYTES as u64).read_until(b'\n', &mut line).map_err(io)?;
        if n == 0 {
            return Err(Error::Protocol("the agent closed the connection before answering".into()));
        }
        raw.extend_from_slice(&line);
        if raw.len() > MAX_HEAD_BYTES {
            return Err(Error::Protocol("the answer's head is too long".into()));
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(&raw) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(Error::Protocol("the answer's head is cut short".into())),
        Err(e) => return Err(Error::Protocol(format!("the answer doesn't parse: {e}"))),
    }
    let status = response.code.ok_or_else(|| Error::Protocol("an answer without a status".into()))?;
    let (mut length, mut chunked, mut retry_after_secs) = (None, false, None);
    for header in response.headers.iter() {
        let text = std::str::from_utf8(header.value).unwrap_or_default().trim();
        if header.name.eq_ignore_ascii_case("content-length") {
            length = Some(text.parse::<u64>().map_err(|_| Error::Protocol("a bad Content-Length".into()))?);
        } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = text.eq_ignore_ascii_case("chunked");
        } else if header.name.eq_ignore_ascii_case("retry-after") {
            retry_after_secs = text.parse::<u64>().ok();
        }
    }
    let mut body = if chunked {
        BodyReader::chunked(reader)
    } else if let Some(n) = length {
        BodyReader::with_length(reader, n)
    } else {
        BodyReader::to_end(reader)
    };
    if (200..300).contains(&status) {
        return Ok(Streamed::Body(body));
    }
    let mut bytes = Vec::new();
    // A refusal's body is small; a longer one is cut (the code and message are at its start).
    let _ = (&mut body).take(MAX_REFUSAL_BYTES as u64).read_to_end(&mut bytes);
    Ok(Streamed::Refused(Answer { status, body: bytes, retry_after_secs }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Framing {
    Length,
    Chunked,
    ToEnd,
}

/// The body of an answer as it arrives, with its framing (chunks, or a length) taken off. A body that ends before its framing
/// says it should is an error, not an end.
pub(crate) struct BodyReader<R> {
    inner: R,
    framing: Framing,
    /// Bytes left of the length, or of the chunk being read.
    left: u64,
    done: bool,
}

impl<R: BufRead> BodyReader<R> {
    pub(crate) fn chunked(inner: R) -> Self {
        Self { inner, framing: Framing::Chunked, left: 0, done: false }
    }

    pub(crate) fn with_length(inner: R, length: u64) -> Self {
        Self { inner, framing: Framing::Length, left: length, done: length == 0 }
    }

    pub(crate) fn to_end(inner: R) -> Self {
        Self { inner, framing: Framing::ToEnd, left: 0, done: false }
    }

    /// The next chunk's size (a line of hex digits, extensions ignored); `0` ends the body, after its trailers.
    fn next_chunk(&mut self) -> std::io::Result<u64> {
        let bad = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_owned());
        let mut line = Vec::new();
        let n = (&mut self.inner).take(MAX_CHUNK_LINE as u64 + 1).read_until(b'\n', &mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the body ended inside its chunks"));
        }
        if line.last() != Some(&b'\n') {
            return Err(bad("a chunk size line is too long or cut off"));
        }
        let text = std::str::from_utf8(&line).map_err(|_| bad("a chunk size isn't text"))?;
        let size = text.trim().split(';').next().unwrap_or_default().trim();
        let size = u64::from_str_radix(size, 16).map_err(|_| bad("a chunk size isn't a number"))?;
        if size == 0 {
            // The trailers, up to the empty line that ends the body: a stream that ends before it, or inside a trailer, is cut
            // off (an error, not an end), and the trailers are bounded as the head is.
            let mut seen = 0usize;
            loop {
                let mut trailer = Vec::new();
                let n = (&mut self.inner).take(MAX_HEAD_BYTES as u64 + 1).read_until(b'\n', &mut trailer)?;
                if n == 0 || trailer.last() != Some(&b'\n') {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the body ended before its last line break"));
                }
                if trailer == b"\r\n" || trailer == b"\n" {
                    break;
                }
                seen = seen.saturating_add(n);
                if seen > MAX_HEAD_BYTES {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the body's trailers are too long"));
                }
            }
        }
        Ok(size)
    }

    /// The line break after a chunk's data.
    fn end_of_chunk(&mut self) -> std::io::Result<()> {
        let mut line = Vec::new();
        (&mut self.inner).take(3).read_until(b'\n', &mut line)?;
        if line == b"\r\n" || line == b"\n" {
            Ok(())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a chunk doesn't end where it should"))
        }
    }
}

impl<R: BufRead> Read for BodyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.done {
            return Ok(0);
        }
        if self.framing == Framing::ToEnd {
            return self.inner.read(buf);
        }
        if self.framing == Framing::Chunked && self.left == 0 {
            let size = self.next_chunk()?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let max = usize::try_from(self.left).unwrap_or(usize::MAX).min(buf.len());
        let n = self.inner.read(buf.get_mut(..max).unwrap_or_default())?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the body is shorter than its framing says"));
        }
        self.left = self.left.saturating_sub(n as u64);
        if self.left == 0 {
            match self.framing {
                Framing::Chunked => self.end_of_chunk()?,
                _ => self.done = true,
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_by_their_length() {
        let text = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 7\r\n\r\n{\"a\":1}trailing";
        let answer = read_answer(&mut &text[..]).unwrap();
        assert_eq!((answer.status, answer.body.as_slice()), (200, &b"{\"a\":1}"[..]));
        let short = b"HTTP/1.1 200 OK\r\ncontent-length: 70\r\n\r\n{}";
        assert!(read_answer(&mut &short[..]).is_err());
        let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert!(read_answer(&mut &chunked[..]).is_err());
        let huge = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", MAX_LOCAL_JSON_BYTES + 1);
        assert!(read_answer(&mut huge.as_bytes()).is_err());
        assert!(read_answer(&mut &b"nonsense\r\n\r\n"[..]).is_err());
        assert!(read_answer(&mut &b""[..]).is_err());
        let no_length = b"HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":\"unauthorized\"}";
        assert_eq!(read_answer(&mut &no_length[..]).unwrap().status, 401);
        let slow = b"HTTP/1.1 429 Too Many Requests
Retry-After: 30
content-length: 2

{}";
        assert_eq!(read_answer(&mut &slow[..]).unwrap().retry_after_secs, Some(30));
        assert_eq!(read_answer(&mut &no_length[..]).unwrap().retry_after_secs, None);
    }

    #[test]
    fn a_retry_after_header_is_kept_when_it_is_a_number_of_seconds() {
        let text = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 840\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(read_answer(&mut &text[..]).unwrap().retry_after_secs, Some(840));
        let date = b"HTTP/1.1 429 Too Many Requests\r\nretry-after: Wed, 21 Oct 2026 07:28:00 GMT\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(read_answer(&mut &date[..]).unwrap().retry_after_secs, None);
        let none = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(read_answer(&mut &none[..]).unwrap().retry_after_secs, None);
    }

    #[test]
    fn bodies_are_read_by_their_framing() {
        let read = |mut r: BodyReader<&[u8]>| {
            let mut out = Vec::new();
            r.read_to_end(&mut out).map(|_| out)
        };
        let chunked = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nTrailer: x\r\n\r\nrest";
        assert_eq!(read(BodyReader::chunked(&chunked[..])).unwrap(), b"hello world");
        assert_eq!(read(BodyReader::with_length(&b"abcdef"[..], 4)).unwrap(), b"abcd");
        assert_eq!(read(BodyReader::to_end(&b"abc"[..])).unwrap(), b"abc");
        // Cut off inside a chunk, between chunks, or before the length: errors, never an end.
        assert!(read(BodyReader::chunked(&b"5\r\nhel"[..])).is_err());
        assert!(read(BodyReader::chunked(&b"5\r\nhello\r\n"[..])).is_err());
        assert!(read(BodyReader::chunked(&b"5\r\nhello"[..])).is_err());
        // The last chunk without the line break that ends the body, or cut inside a trailer: cut off, not complete.
        assert!(read(BodyReader::chunked(&b"5\r\nhello\r\n0\r\n"[..])).is_err());
        assert!(read(BodyReader::chunked(&b"5\r\nhello\r\n0\r\nTrailer: x"[..])).is_err());
        assert!(read(BodyReader::chunked(&b"5\r\nhello\r\n0\r\nTrailer: x\r\n"[..])).is_err());
        assert_eq!(read(BodyReader::chunked(&b"5\r\nhello\r\n0\r\n\r\n"[..])).unwrap(), b"hello");
        let long = format!("5\r\nhello\r\n0\r\n{}\r\n\r\n", "X: y\r\n".repeat(5_000));
        assert!(read(BodyReader::chunked(long.as_bytes())).is_err(), "trailers are bounded");
        assert!(read(BodyReader::with_length(&b"abc"[..], 4)).is_err());
        // Garbage and over-long size lines.
        assert!(read(BodyReader::chunked(&b"zz\r\n"[..])).is_err());
        let long = format!("{}\r\n", "1".repeat(200));
        assert!(read(BodyReader::chunked(long.as_bytes())).is_err());
        assert!(read(BodyReader::chunked(&b"5\r\nhelloXX0\r\n\r\n"[..])).is_err());
    }
}
