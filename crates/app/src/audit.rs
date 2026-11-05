//! The security audit log through the agent's local API: the signed-in account's own activity
//! ([`Client::account_activity`], for every account), and for an admin the whole log ([`Client::admin_audit`]) and its export
//! ([`Client::admin_audit_export`]). The server decides who may read what; this crate only asks. Every call asks the server:
//! call them **off the UI thread**.
//!
//! **The export is streamed**: the agent passes the server's NDJSON through as it comes (chunked), and this client writes each
//! line to the writer it is given as it reads it, with nothing held but one line (at most [`MAX_LINE_BYTES`]) and a small
//! buffer. It counts the export as whole only with the server's last line, `{"complete":true,"exported":n}`, when `n` is the
//! number of event lines it saw; a stream that ends without that line, breaks in the middle, or writes a line this crate can't
//! read is an error ([`Error::Protocol`] or [`Error::Io`]), and what was written to the writer is the caller's to throw away.
//! `{"truncated":true,…}` is a good export of the newest events that is not the whole log ([`ExportOutcome::truncated`]).

use std::io::{BufRead, Read, Write};

use craft_cloud_api::api::{AuditEvent, AuditExportEnd, AuditPage, valid_audit_action};
use craft_cloud_api::local::paths;

use crate::{Client, Error, Uuid, http};

/// The longest line of an export this client reads, in bytes (an event is far smaller: its details are capped at 2 KiB).
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// The most events one export may hold before this client gives up (the server's own limit is 100,000).
pub const MAX_EXPORT_EVENTS: u64 = 1_000_000;

/// Which events a list or an export asks for. A `None` leaves the filter out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// The `next` of the page before (opaque, given back as it came); none for the first page. Lists only.
    pub cursor: Option<String>,
    /// Only events before this time (milliseconds since the Unix epoch); it limits the first page, a cursor wins over it.
    pub before_ms: Option<i64>,
    /// Events on the page, 1 to 200 (an account's own activity: 100); none: the server's default (50). Lists only.
    pub limit: Option<u32>,
    /// Only events about this account (an admin's filter).
    pub user: Option<Uuid>,
    /// Only this event name, or a prefix ending in a dot like `auth.` ([`craft_cloud_api::api::AUDIT_ACTIONS`]; an admin's filter).
    pub action: Option<String>,
}

impl AuditQuery {
    /// What is wrong with the query, before anything is sent: an action that is not in the closed list, a time before 1970, a
    /// limit outside 1 to 200.
    pub fn check(&self) -> Result<(), Error> {
        let bad = |message: &str| Error::Refused { code: craft_cloud_api::api::codes::INVALID.to_owned(), message: message.to_owned(), name_taken: None };
        if self.action.as_deref().is_some_and(|a| !valid_audit_action(a)) {
            return Err(bad("action is an event name like auth.login, or a prefix like auth."));
        }
        if self.before_ms.is_some_and(|t| t < 0) {
            return Err(bad("before_ms is a time in milliseconds"));
        }
        if self.limit.is_some_and(|l| !(1..=200).contains(&l)) {
            return Err(bad("limit is 1 to 200"));
        }
        Ok(())
    }

    /// `path` with the query string.
    fn path(&self, path: &str) -> String {
        let mut pairs = Vec::new();
        if let Some(cursor) = self.cursor.as_deref().filter(|c| !c.is_empty()) {
            pairs.push(format!("cursor={}", crate::admin::encode(cursor)));
        }
        if let Some(before) = self.before_ms {
            pairs.push(format!("before_ms={before}"));
        }
        if let Some(limit) = self.limit {
            pairs.push(format!("limit={limit}"));
        }
        if let Some(user) = self.user {
            pairs.push(format!("user={user}"));
        }
        if let Some(action) = self.action.as_deref().filter(|a| !a.is_empty()) {
            pairs.push(format!("action={}", crate::admin::encode(action)));
        }
        if pairs.is_empty() { path.to_owned() } else { format!("{path}?{}", pairs.join("&")) }
    }
}

/// How an export ended, when it ended with the server's last line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportOutcome {
    /// Event lines written.
    pub events: u64,
    /// The log holds more events than an export does (the server's `truncated` line): these are the newest ones.
    pub truncated: bool,
}

impl Client {
    /// The signed-in account, as the server says it: its role and whether it has two-factor sign-in.
    pub fn account(&self) -> Result<craft_cloud_api::api::AccountInfo, Error> {
        self.call("GET", paths::ACCOUNT, None, http::SLOW_TIMEOUT)
    }

    /// A page of the signed-in account's own security events, newest first. Any account may; `user` and `action` of `query`
    /// are not sent (they are the admin's filters).
    pub fn account_activity(&self, query: &AuditQuery) -> Result<AuditPage, Error> {
        let own = AuditQuery { user: None, action: None, ..query.clone() };
        own.check()?;
        self.call("GET", &own.path(paths::ACCOUNT_ACTIVITY), None, http::SLOW_TIMEOUT)
    }

    /// A page of the whole security log, newest first, filtered (an admin's: a member is `Refused { code: "forbidden" }`).
    pub fn admin_audit(&self, query: &AuditQuery) -> Result<AuditPage, Error> {
        query.check()?;
        self.call("GET", &query.path(paths::ADMIN_AUDIT), None, http::SLOW_TIMEOUT)
    }

    /// Writes the log, filtered by `query.user`, `query.action` and `query.before_ms`, to `out` as NDJSON (one event a line,
    /// the server's own text), reading the stream as it comes (see the module's docs). Whole only when it returns
    /// `Ok`; on any `Err`, `out` holds an unfinished export. A refusal before the stream starts is the usual `Refused`.
    pub fn admin_audit_export(&self, query: &AuditQuery, out: &mut dyn Write) -> Result<ExportOutcome, Error> {
        let export = AuditQuery { cursor: None, limit: None, ..query.clone() };
        export.check()?;
        let streamed = http::request_stream(self.addr(), self.token(), "GET", &export.path(paths::ADMIN_AUDIT_EXPORT), http::SLOW_TIMEOUT)?;
        match streamed {
            http::Streamed::Refused(answer) => Err(crate::refusal(answer).err().unwrap_or_else(|| Error::Protocol("the export was refused".into()))),
            http::Streamed::Body(body) => copy_export(body, out),
        }
    }
}

/// Reads `body` (the NDJSON of an export) line by line into `out`.
fn copy_export(body: impl Read, out: &mut dyn Write) -> Result<ExportOutcome, Error> {
    let mut lines = std::io::BufReader::with_capacity(8 * 1024, body);
    let mut events = 0u64;
    let mut end: Option<AuditExportEnd> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        // One more byte than the longest line, to tell a line that is too long from one that just fits.
        let read =
            (&mut lines).take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut line).map_err(|e| Error::Io(format!("the export broke off: {e}")))?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            return if read > MAX_LINE_BYTES {
                Err(Error::Protocol("the export has a line that is too long".into()))
            } else {
                Err(Error::Protocol("the export was cut off in the middle of a line".into()))
            };
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.is_empty() {
            continue;
        }
        if end.is_some() {
            return Err(Error::Protocol("the export goes on after its last line".into()));
        }
        let value: serde_json::Value =
            serde_json::from_slice(text).map_err(|e| Error::Protocol(format!("the export has a line that doesn't read: {e}")))?;
        if value.get("exported").is_some() {
            end = Some(serde_json::from_value(value).map_err(|e| Error::Protocol(format!("the export's last line doesn't read: {e}")))?);
            continue;
        }
        // An event: read as one, so a stream of anything else is no export; what is written is the server's own line.
        serde_json::from_value::<AuditEvent>(value).map_err(|e| Error::Protocol(format!("the export has an event that doesn't read: {e}")))?;
        events = events.saturating_add(1);
        if events > MAX_EXPORT_EVENTS {
            return Err(Error::Protocol("the export has more events than this client takes".into()));
        }
        out.write_all(text).and_then(|()| out.write_all(b"\n")).map_err(|e| Error::Io(format!("can't write the export: {e}")))?;
    }
    out.flush().map_err(|e| Error::Io(format!("can't write the export: {e}")))?;
    let Some(end) = end else {
        return Err(Error::Protocol(format!("the export was cut off after {events} events: it has no last line")));
    };
    if end.exported != events {
        return Err(Error::Protocol(format!("the export says {} events and has {events}", end.exported)));
    }
    match (end.complete, end.truncated) {
        (Some(true), _) => Ok(ExportOutcome { events, truncated: false }),
        (_, Some(true)) => Ok(ExportOutcome { events, truncated: true }),
        _ => Err(Error::Protocol("the export's last line says neither complete nor truncated".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(n: u128) -> String {
        format!(
            r#"{{"id":"{}","at_ms":{n},"actor_user":null,"actor_device":null,"action":"auth.login","target_kind":"","target_id":null,"outcome":"success","details":{{}},"count":1}}"#,
            Uuid::from_u128(n)
        )
    }

    fn copy(text: &str) -> (Result<ExportOutcome, Error>, String) {
        let mut out = Vec::new();
        let result = copy_export(text.as_bytes(), &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_query_is_written_as_a_query_string_and_checked_first() {
        let q =
            AuditQuery { cursor: Some("YWJj".into()), before_ms: Some(5), limit: Some(20), user: Some(Uuid::nil()), action: Some("auth.".into()) };
        assert_eq!(
            q.path("/v1/admin/audit"),
            "/v1/admin/audit?cursor=YWJj&before_ms=5&limit=20&user=00000000-0000-0000-0000-000000000000&action=auth."
        );
        assert_eq!(AuditQuery::default().path("/v1/admin/audit"), "/v1/admin/audit");
        assert!(q.check().is_ok());
        for bad in [
            AuditQuery { action: Some("auth.x".into()), ..AuditQuery::default() },
            AuditQuery { before_ms: Some(-1), ..AuditQuery::default() },
            AuditQuery { limit: Some(0), ..AuditQuery::default() },
            AuditQuery { limit: Some(201), ..AuditQuery::default() },
        ] {
            assert!(matches!(bad.check(), Err(Error::Refused { ref code, .. }) if code == "invalid"), "{bad:?}");
        }
    }

    #[test]
    fn a_whole_export_is_copied_line_by_line() {
        let text = format!("{}\n{}\n{{\"complete\":true,\"exported\":2}}\n", event(2), event(1));
        let (result, out) = copy(&text);
        assert_eq!(result.unwrap(), ExportOutcome { events: 2, truncated: false });
        assert_eq!(out, format!("{}\n{}\n", event(2), event(1)), "the end line is not part of the export");
        let (result, _) = copy("{\"complete\":true,\"exported\":0}\n");
        assert_eq!(result.unwrap(), ExportOutcome { events: 0, truncated: false });
    }

    #[test]
    fn a_truncated_export_is_good_but_not_whole() {
        let text = format!("{}\n{{\"truncated\":true,\"exported\":1}}\n", event(1));
        assert_eq!(copy(&text).0.unwrap(), ExportOutcome { events: 1, truncated: true });
    }

    #[test]
    fn a_stream_that_is_cut_off_or_wrong_is_no_export() {
        // No last line (the server broke off, or the connection did).
        let (result, _) = copy(&format!("{}\n{}\n", event(2), event(1)));
        assert!(matches!(result, Err(Error::Protocol(ref m)) if m.contains("cut off after 2 events")), "{result:?}");
        // Cut in the middle of a line.
        let (result, _) = copy(&format!("{}\n{}", event(2), &event(1)[..20]));
        assert!(matches!(result, Err(Error::Protocol(ref m)) if m.contains("middle of a line")), "{result:?}");
        // A last line that disagrees with what came.
        let (result, _) = copy(&format!("{}\n{{\"complete\":true,\"exported\":2}}\n", event(1)));
        assert!(matches!(result, Err(Error::Protocol(ref m)) if m.contains("says 2 events and has 1")), "{result:?}");
        // Neither complete nor truncated; lines after the end; lines that are not events; a line that is too long.
        let (result, _) = copy("{\"exported\":0}\n");
        assert!(result.is_err());
        let (result, _) = copy(&format!("{{\"complete\":true,\"exported\":0}}\n{}\n", event(1)));
        assert!(matches!(result, Err(Error::Protocol(ref m)) if m.contains("goes on")), "{result:?}");
        let (result, _) = copy("{\"hello\":1}\n{\"complete\":true,\"exported\":0}\n");
        assert!(matches!(result, Err(Error::Protocol(_))), "{result:?}");
        let (result, _) = copy("not json\n");
        assert!(matches!(result, Err(Error::Protocol(_))), "{result:?}");
        let long = format!("{}\n", "x".repeat(MAX_LINE_BYTES + 10));
        let (result, _) = copy(&long);
        assert!(matches!(result, Err(Error::Protocol(ref m)) if m.contains("too long")), "{result:?}");
    }

    /// A reader that breaks after what it has (a connection that dropped): the export fails with an I/O error; a writer that
    /// fails (a full disk) fails it too.
    #[test]
    fn a_broken_connection_or_a_full_disk_fails_the_export() {
        struct Breaks(std::io::Cursor<Vec<u8>>);
        impl Read for Breaks {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.0.read(buf)? {
                    0 => Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")),
                    n => Ok(n),
                }
            }
        }
        let mut out = Vec::new();
        let result = copy_export(Breaks(std::io::Cursor::new(format!("{}\n", event(1)).into_bytes())), &mut out);
        assert!(matches!(result, Err(Error::Io(ref m)) if m.contains("broke off")), "{result:?}");
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let result = copy_export(format!("{}\n", event(1)).as_bytes(), &mut Full);
        assert!(matches!(result, Err(Error::Io(ref m)) if m.contains("can't write")), "{result:?}");
    }

    /// One answer from a listener of this test: the bytes it writes (the request is read first), then it closes.
    fn serve_once(answer: Vec<u8>) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut chunk = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).unwrap();
                seen.extend_from_slice(&chunk[..n]);
            }
            stream.write_all(&answer).unwrap();
        });
        addr
    }

    fn chunk(text: &str) -> Vec<u8> {
        format!("{:x}\r\n{text}\r\n", text.len()).into_bytes()
    }

    fn exported(answer: Vec<u8>) -> (Result<ExportOutcome, Error>, String) {
        let addr = serve_once(answer);
        let mut out = Vec::new();
        let streamed = http::request_stream(addr, "token", "GET", "/v1/admin/audit/export", std::time::Duration::from_secs(5)).unwrap();
        let result = match streamed {
            http::Streamed::Body(body) => copy_export(body, &mut out),
            http::Streamed::Refused(answer) => Err(crate::refusal(answer).unwrap_err()),
        };
        (result, String::from_utf8(out).unwrap())
    }

    fn head_again() -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\n\r\n".to_vec()
    }

    /// Over a real connection: a chunked export that ends properly, one whose connection drops in the middle (no
    /// terminating chunk: a failed export, whatever came before), and a refusal before it starts.
    #[test]
    fn an_export_over_a_connection_is_whole_only_when_it_ends_properly() {
        let head = b"HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\n\r\n".to_vec();
        let mut whole = head.clone();
        whole.extend(chunk(&format!("{}\n", event(2))));
        whole.extend(chunk(&format!("{}\n{{\"complete\":true,\"exported\":2}}\n", event(1))));
        whole.extend(b"0\r\n\r\n");
        let (result, out) = exported(whole);
        assert_eq!(result.unwrap(), ExportOutcome { events: 2, truncated: false });
        assert_eq!(out, format!("{}\n{}\n", event(2), event(1)));

        // The last line came, but the stream was cut before its terminating chunk: not whole (the end is not the server's).
        let mut cut = head.clone();
        cut.extend(chunk(&format!("{}\n{{\"complete\":true,\"exported\":1}}\n", event(1))));
        let (result, _) = exported(cut);
        assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
        // Cut off in the middle of a chunk.
        let mut broken = head.clone();
        broken.extend(b"ff\r\n");
        broken.extend(event(1).as_bytes());
        let (result, out) = exported(broken);
        assert!(result.is_err(), "{result:?}");
        assert!(out.is_empty(), "no half line is written: {out:?}");
        // Whole lines but no last line, and a clean end: still no export.
        let mut no_end = head;
        no_end.extend(chunk(&format!("{}\n", event(1))));
        no_end.extend(b"0\r\n\r\n");
        let (result, _) = exported(no_end);
        assert!(matches!(result, Err(Error::Protocol(ref m)) if m.contains("no last line")), "{result:?}");
        // The last chunk, then the connection ends before the body's last line break, or inside a trailer: not complete.
        for tail in [&b"0\r\n"[..], &b"0\r\nX-Trailer: y"[..], &b"0\r\nX-Trailer: y\r\n"[..]] {
            let mut cut_at_the_end = head_again();
            cut_at_the_end.extend(chunk(&format!("{}\n{{\"complete\":true,\"exported\":1}}\n", event(1))));
            cut_at_the_end.extend(tail);
            let (result, _) = exported(cut_at_the_end);
            assert!(matches!(result, Err(Error::Io(_))), "{tail:?}: {result:?}");
        }
        // A refusal: the agent's JSON, with the server's wait.
        let body = r#"{"error":"rate_limited","message":"too many tries"}"#;
        let refused = format!("HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        let (result, _) = exported(refused.into_bytes());
        assert!(matches!(&result, Err(e) if e.code() == Some("rate_limited") && e.retry_after_secs() == Some(30)), "{result:?}");
    }
}
