//! Join codes and join strings: how a computer joins a Craft Cloud without a password.
//!
//! - **Codes** are random Crockford base32 (`0-9` and the letters without `I`, `L`, `O` and `U`, 5 bits a
//!   character): an add-a-computer code (a pairing, made by a signed-in device) has [`PAIRING_CHARS`] and lasts
//!   [`PAIRING_TTL_MS`]; an invite (made by the server's admin) has [`INVITE_CHARS`] and lasts [`INVITE_TTL_MS`].
//!   Both are good for one use. The server keeps only their [`Code::hash`].
//! - **On input** case doesn't matter, `O` counts as `0`, `I` and `L` as `1`, and dashes and spaces are ignored.
//!   They are shown in groups of four with dashes: `K7M4-Q9TX`.
//! - **A join string** is the server's address plus the code, `cloud.example.com/K7M4-Q9TX` (a short code can't name
//!   the server); the link form is `https://cloud.example.com/join/K7M4-Q9TX`. `https://` is assumed when no scheme
//!   is given; `http://` is kept only when typed (the agent accepts it only for a server on the same machine).
//!
//! A code is a secret until it is used: its `Debug` output is redacted, and it is never logged.

/// Characters of an add-a-computer code (40 bits).
pub const PAIRING_CHARS: usize = 8;
/// Characters of an invite code (80 bits).
pub const INVITE_CHARS: usize = 16;
/// How long an add-a-computer code works: 10 minutes.
pub const PAIRING_TTL_MS: i64 = 600_000;
/// How long an invite code works: 7 days.
pub const INVITE_TTL_MS: i64 = 7 * 86_400_000;
/// Longest join string accepted, in bytes (a server address with a path prefix, and a code).
pub const MAX_JOIN_BYTES: usize = 1024;

/// Crockford's base32 alphabet, in value order.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A join code, normalised: upper case, no dashes or spaces, only [`ALPHABET`] characters. Its `Debug` output is
/// redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Code(String);

impl Code {
    /// A new random code of `chars` characters, from the system's random source.
    pub fn generate(chars: usize) -> Result<Code, getrandom::Error> {
        let mut bytes = vec![0u8; chars];
        getrandom::fill(&mut bytes)?;
        // 256 is a multiple of 32, so the low five bits of a random byte are uniform.
        let code = bytes.iter().filter_map(|b| ALPHABET.get(usize::from(b & 31)).map(|&c| char::from(c))).collect();
        Ok(Code(code))
    }

    /// `text` as a code of exactly `chars` characters, forgiving case, `O` for `0`, `I` and `L` for `1`, dashes and
    /// spaces. `None` for any other character (`U` included) or another length.
    pub fn parse(text: &str, chars: usize) -> Option<Code> {
        let mut code = String::with_capacity(chars);
        for c in text.chars() {
            if c == '-' || c.is_whitespace() {
                continue;
            }
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c if c.is_ascii() && ALPHABET.contains(&(c as u8)) => c,
                _ => return None,
            };
            if code.len() == chars {
                return None;
            }
            code.push(c);
        }
        (code.len() == chars).then_some(Code(code))
    }

    /// `text` as either kind of code: [`PAIRING_CHARS`] or [`INVITE_CHARS`] characters.
    pub fn parse_any(text: &str) -> Option<Code> {
        Code::parse(text, PAIRING_CHARS).or_else(|| Code::parse(text, INVITE_CHARS))
    }

    /// The normalised code, without dashes (what is sent to the server).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The code as shown to people: groups of four joined by dashes, `K7M4-Q9TX`.
    pub fn display(&self) -> String {
        let mut shown = String::with_capacity(self.0.len() + self.0.len() / 4);
        for (i, c) in self.0.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                shown.push('-');
            }
            shown.push(c);
        }
        shown
    }

    /// The BLAKE3 hash of the normalised code: all the server keeps of it.
    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(self.0.as_bytes()).as_bytes()
    }
}

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Code(<redacted>)")
    }
}

/// Why a join string was refused. The messages are for people.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoinParseError {
    #[error("enter a join string, like cloud.example.com/K7M4-Q9TX")]
    Empty,
    #[error("that is too long for a join string")]
    TooLong,
    #[error("put the server's address in front of the code, like cloud.example.com/K7M4-Q9TX")]
    NoServer,
    #[error("the code is missing after the server's address, like cloud.example.com/K7M4-Q9TX")]
    NoCode,
    #[error("the code must be 8 or 16 letters and digits, like K7M4-Q9TX")]
    BadCode,
    #[error("the server's address in the join string isn't valid")]
    BadServer,
    #[error("a join string starts with the server's address, or with https://")]
    BadScheme,
}

/// A parsed join string: the server and the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinString {
    /// The server's address, `https://host[:port][/prefix]` (the host in lower case, no trailing slash).
    pub server: String,
    pub code: Code,
}

impl JoinString {
    /// A join string for `server` (an address as [`JoinString::server`] holds it).
    pub fn new(server: impl Into<String>, code: Code) -> JoinString {
        JoinString { server: server.into(), code }
    }

    /// `text` as typed or pasted: `[scheme://]host[:port][/prefix][/join]/code[/]`, with surrounding spaces, a
    /// code of either length and the input aliases of [`Code::parse`]. A last path name `join` before the code is the
    /// link form's, never part of the server's prefix.
    pub fn parse(text: &str) -> Result<JoinString, JoinParseError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(JoinParseError::Empty);
        }
        if text.len() > MAX_JOIN_BYTES {
            return Err(JoinParseError::TooLong);
        }
        let (scheme, rest) = match text.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("https") => ("https", rest),
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => ("http", rest),
            Some(_) => return Err(JoinParseError::BadScheme),
            None => ("https", text),
        };
        let rest = rest.trim_end_matches('/');
        let Some((mut before, code)) = rest.rsplit_once('/') else {
            return Err(if Code::parse_any(rest).is_some() { JoinParseError::NoServer } else { JoinParseError::NoCode });
        };
        if code.eq_ignore_ascii_case("join") {
            return Err(JoinParseError::NoCode);
        }
        let code = Code::parse_any(code).ok_or(JoinParseError::BadCode)?;
        if let Some((server, last)) = before.rsplit_once('/')
            && last.eq_ignore_ascii_case("join")
        {
            before = server;
        }
        let (authority, prefix) = match before.find('/') {
            Some(at) => before.split_at(at),
            None => (before, ""),
        };
        let authority = authority_of(authority).ok_or(JoinParseError::BadServer)?;
        let prefix_ok = prefix.split('/').skip(1).all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.chars().any(|c| c.is_whitespace() || c.is_control() || "?#\\".contains(c))
        });
        // A prefix ending in `join` would read as the link form once written out again.
        let ambiguous = prefix.rsplit('/').next().is_some_and(|last| last.eq_ignore_ascii_case("join"));
        if !prefix_ok || ambiguous {
            return Err(JoinParseError::BadServer);
        }
        Ok(JoinString { server: format!("{scheme}://{authority}{prefix}"), code })
    }

    /// The short form people type: `host[:port][/prefix]/K7M4-Q9TX` (with `http://` in front when that is the
    /// scheme, so it reparses the same).
    pub fn join(&self) -> String {
        let base = self.server.strip_prefix("https://").unwrap_or(&self.server);
        format!("{base}/{}", self.code.display())
    }

    /// The link form: `https://host[:port][/prefix]/join/K7M4-Q9TX`.
    pub fn link(&self) -> String {
        format!("{}/join/{}", self.server, self.code.display())
    }
}

/// `host[:port]` checked, with the host in lower case: a name of ASCII letters, digits, `-`, `.` and `_` (an
/// internationalised name in its `xn--` form), or an IPv6 address in brackets; a port of 1 to 65535. No user name or
/// password.
fn authority_of(authority: &str) -> Option<String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (ip, after) = rest.split_once(']')?;
        if ip.is_empty() || !ip.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.') {
            return None;
        }
        let port = if after.is_empty() { None } else { Some(after.strip_prefix(':')?) };
        (format!("[{}]", ip.to_ascii_lowercase()), port)
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        let name_ok = !host.is_empty() && !host.starts_with(['.', '-']) && host.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c));
        if !name_ok {
            return None;
        }
        (host.to_ascii_lowercase(), port)
    };
    match port {
        None => Some(host),
        Some(port) => {
            let number = port.parse::<u16>().ok().filter(|p| *p > 0 && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()))?;
            Some(format!("{host}:{number}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_displays_and_parses_back() {
        for chars in [PAIRING_CHARS, INVITE_CHARS] {
            let code = Code::generate(chars).unwrap();
            let shown = code.display();
            assert_eq!(shown.len(), chars + chars / 4 - 1, "{shown}");
            assert!(shown.split('-').all(|group| group.len() == 4), "{shown}");
            assert_eq!(Code::parse(&shown, chars), Some(code.clone()));
            assert_eq!(code.hash(), *blake3::hash(code.as_str().as_bytes()).as_bytes());
        }
        assert_ne!(Code::generate(PAIRING_CHARS).unwrap(), Code::generate(PAIRING_CHARS).unwrap());
    }

    #[test]
    fn input_aliases_and_case_are_forgiven() {
        let code = Code::parse("K7M4-Q9TX", PAIRING_CHARS).unwrap();
        assert_eq!(code.as_str(), "K7M4Q9TX");
        assert_eq!(code.display(), "K7M4-Q9TX");
        assert_eq!(Code::parse("k7m4 q9tx", PAIRING_CHARS), Some(code.clone()));
        assert_eq!(Code::parse(" k7m4q9tx ", PAIRING_CHARS), Some(code));
        assert_eq!(Code::parse("O1IL-oilo", PAIRING_CHARS).unwrap().as_str(), "01110110");
        assert_eq!(Code::parse("ABCD-EFGH-JKMN-PQRS", INVITE_CHARS).unwrap().display(), "ABCD-EFGH-JKMN-PQRS");
    }

    #[test]
    fn bad_codes_are_refused() {
        for bad in ["", "-", "K7M4-Q9T", "K7M4-Q9TXA", "K7M4-Q9TU", "K7M4_Q9TX", "K7M4-Q9T!", "K7M4-Q9TÄ"] {
            assert_eq!(Code::parse(bad, PAIRING_CHARS), None, "{bad:?}");
        }
        assert_eq!(Code::parse("K7M4-Q9TX", INVITE_CHARS), None, "the wrong length for an invite");
    }

    #[test]
    fn codes_stay_out_of_debug_output() {
        let code = Code::parse("K7M4-Q9TX", PAIRING_CHARS).unwrap();
        assert!(!format!("{code:?}").contains("K7M4"));
        let join = JoinString::parse("cloud.example.com/K7M4-Q9TX").unwrap();
        assert!(!format!("{join:?}").contains("K7M4"), "{join:?}");
    }

    /// Join strings as people type or paste them.
    #[test]
    fn join_strings_parse_as_typed() {
        let pairing = "K7M4Q9TX";
        let invite = "ABCDEFGHJKMNPQRS";
        let table: &[(&str, &str, &str)] = &[
            ("Cloud.Shadows.CH/k7m4 q9tx", "https://cloud.shadows.ch", pairing),
            ("cloud.shadows.ch/K7M4-Q9TX", "https://cloud.shadows.ch", pairing),
            ("https://cloud.shadows.ch/join/K7M4-Q9TX/", "https://cloud.shadows.ch", pairing),
            ("HTTPS://cloud.shadows.ch/JOIN/K7M4-Q9TX", "https://cloud.shadows.ch", pairing),
            ("cloud.shadows.ch:8443/K7M4Q9TX", "https://cloud.shadows.ch:8443", pairing),
            ("https://example.org/cloud/K7M4-Q9TX", "https://example.org/cloud", pairing),
            ("https://example.org/cloud/join/K7M4-Q9TX", "https://example.org/cloud", pairing),
            ("example.org/Cloud/Sync/K7M4-Q9TX", "https://example.org/Cloud/Sync", pairing),
            ("cloud.shadows.ch/k7m4-q9tx", "https://cloud.shadows.ch", pairing),
            ("  cloud.shadows.ch/K7M4-Q9TX\n", "https://cloud.shadows.ch", pairing),
            ("cloud.shadows.ch/K7M4-Q9TX ", "https://cloud.shadows.ch", pairing),
            ("cloud.shadows.ch/0000-1111", "https://cloud.shadows.ch", "00001111"),
            ("cloud.shadows.ch/OOOO-ILil", "https://cloud.shadows.ch", "00001111"),
            ("http://127.0.0.1:8080/K7M4-Q9TX", "http://127.0.0.1:8080", pairing),
            ("http://[::1]:8080/join/K7M4-Q9TX", "http://[::1]:8080", pairing),
            ("cloud.shadows.ch/ABCD-EFGH-JKMN-PQRS", "https://cloud.shadows.ch", invite),
            ("https://cloud.shadows.ch/join/abcd efgh jkmn pqrs", "https://cloud.shadows.ch", invite),
        ];
        for (input, server, code) in table {
            let parsed = JoinString::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(parsed.server, *server, "{input:?}");
            assert_eq!(parsed.code.as_str(), *code, "{input:?}");
        }
    }

    #[test]
    fn garbage_join_strings_get_a_clear_error() {
        let table: &[(&str, JoinParseError)] = &[
            ("", JoinParseError::Empty),
            ("   ", JoinParseError::Empty),
            ("K7M4-Q9TX", JoinParseError::NoServer),
            ("cloud.shadows.ch", JoinParseError::NoCode),
            ("cloud.shadows.ch/", JoinParseError::NoCode),
            ("https://cloud.shadows.ch/join/", JoinParseError::NoCode),
            ("cloud.shadows.ch/K7M4-Q9T", JoinParseError::BadCode),
            ("cloud.shadows.ch/K7M4-Q9TXA", JoinParseError::BadCode),
            ("cloud.shadows.ch/K7M4-Q9TU", JoinParseError::BadCode),
            ("cloud.shadows.ch/K7M4-Q9T?", JoinParseError::BadCode),
            ("ftp://cloud.shadows.ch/K7M4-Q9TX", JoinParseError::BadScheme),
            ("https:/cloud.shadows.ch/K7M4-Q9TX", JoinParseError::BadServer),
            ("/K7M4-Q9TX", JoinParseError::BadServer),
            ("ana@cloud.shadows.ch/K7M4-Q9TX", JoinParseError::BadServer),
            ("cloud shadows.ch/K7M4-Q9TX", JoinParseError::BadServer),
            ("cloud.shadows.ch:99999/K7M4-Q9TX", JoinParseError::BadServer),
            ("cloud.shadows.ch:/K7M4-Q9TX", JoinParseError::BadServer),
            ("cloud.shadows.ch//K7M4-Q9TX", JoinParseError::BadServer),
            ("cloud.shadows.ch/a b/K7M4-Q9TX", JoinParseError::BadServer),
            ("cloud.shadows.ch/K7M4-Q9TX?x=1", JoinParseError::BadCode),
            ("cloud.shadows.ch/x?y/K7M4-Q9TX", JoinParseError::BadServer),
            ("clöud.shadows.ch/K7M4-Q9TX", JoinParseError::BadServer),
            ("example.org/join/join/K7M4-Q9TX", JoinParseError::BadServer),
        ];
        for (input, want) in table {
            assert_eq!(JoinString::parse(input).as_ref().map(|j| j.server.as_str()), Err(want), "{input:?}");
            assert!(!JoinParseError::to_string(want).is_empty());
        }
        assert!(JoinString::parse(&format!("cloud.shadows.ch/{}", "a".repeat(MAX_JOIN_BYTES))).is_err(), "too long");
    }

    #[test]
    fn join_and_link_forms_reparse_to_the_same_value() {
        for input in [
            "Cloud.Shadows.CH/k7m4 q9tx",
            "cloud.shadows.ch:8443/K7M4Q9TX",
            "https://example.org/cloud/K7M4-Q9TX",
            "http://127.0.0.1:8080/K7M4-Q9TX",
            "cloud.shadows.ch/ABCD-EFGH-JKMN-PQRS",
        ] {
            let parsed = JoinString::parse(input).unwrap();
            assert_eq!(JoinString::parse(&parsed.join()).unwrap(), parsed, "{}", parsed.join());
            assert_eq!(JoinString::parse(&parsed.link()).unwrap(), parsed, "{}", parsed.link());
        }
        let parsed = JoinString::parse("Cloud.Shadows.CH/k7m4 q9tx").unwrap();
        assert_eq!(parsed.join(), "cloud.shadows.ch/K7M4-Q9TX");
        assert_eq!(parsed.link(), "https://cloud.shadows.ch/join/K7M4-Q9TX");
        let local = JoinString::parse("http://127.0.0.1:8080/K7M4-Q9TX").unwrap();
        assert_eq!(local.join(), "http://127.0.0.1:8080/K7M4-Q9TX", "plain HTTP stays spelled out");
        let built = JoinString::new("https://example.org/cloud", Code::parse("K7M4Q9TX", PAIRING_CHARS).unwrap());
        assert_eq!(built.join(), "example.org/cloud/K7M4-Q9TX");
    }
}
