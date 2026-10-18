//! One HTTP/1.1 request to the agent's local API and its answer, over a plain loopback TCP connection: written out by
//! hand (the agent is the only peer, and it answers with a `Content-Length`), read with `httparse`, every size capped.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use craft_cloud_proto::local::MAX_LOCAL_JSON_BYTES;

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

/// An answer: status and body.
#[derive(Debug)]
pub(crate) struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends `method path` with the token and an optional JSON body to the agent at `addr`; the answer must come within
/// `timeout`.
pub(crate) fn request(addr: SocketAddr, token: &str, method: &str, path: &str, body: Option<&[u8]>, timeout: Duration) -> Result<Answer, Error> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| Error::NotRunning(format!("can't reach it: {e}")))?;
    stream.set_read_timeout(Some(timeout)).map_err(io)?;
    stream.set_write_timeout(Some(timeout)).map_err(io)?;
    let body = body.unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.write_all(body).map_err(io)?;
    stream.flush().map_err(io)?;
    read_answer(&mut stream)
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

/// Reads one answer (the connection closes after it).
pub(crate) fn read_answer(stream: &mut impl Read) -> Result<Answer, Error> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let (status, length, head_len) = loop {
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
                for header in response.headers.iter() {
                    if header.name.eq_ignore_ascii_case("content-length") {
                        let text = std::str::from_utf8(header.value).map_err(|_| Error::Protocol("a bad Content-Length".into()))?;
                        length = Some(text.trim().parse::<usize>().map_err(|_| Error::Protocol("a bad Content-Length".into()))?);
                    } else if header.name.eq_ignore_ascii_case("transfer-encoding") {
                        return Err(Error::Protocol("the agent's answer is chunked, which this client doesn't read".into()));
                    }
                }
                break (status, length, head_len);
            }
            Ok(httparse::Status::Partial) if buf.len() <= MAX_HEAD_BYTES => continue,
            Ok(httparse::Status::Partial) => return Err(Error::Protocol("the answer's head is too long".into())),
            Err(e) => return Err(Error::Protocol(format!("the answer doesn't parse: {e}"))),
        }
    };
    if length.is_some_and(|n| n > MAX_LOCAL_JSON_BYTES) {
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
        if body.len() > MAX_LOCAL_JSON_BYTES {
            return Err(Error::Protocol("the answer is too long".into()));
        }
    }
    if let Some(n) = length {
        if body.len() < n {
            return Err(Error::Protocol("the answer was cut short".into()));
        }
        body.truncate(n);
    }
    Ok(Answer { status, body })
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
    }
}
