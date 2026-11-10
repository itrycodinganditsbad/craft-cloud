//! TOTP (RFC 6238) as authenticator apps speak it: HMAC-SHA-1, six digits, 30-second steps.
//!
//! - A code is accepted for the step the clock is in and the one before and after it (a clock a little off); the step
//!   it matched is handed back, and the database lets an account accept a step only once, and only a later one than the
//!   last (see `craft_cloud_meta`'s two-factor module), so a code can't be used twice.
//! - A code is exactly six ASCII digits. Anything else is refused before it is compared.
//! - Codes are compared in constant time. The secret is 20 random bytes, shown to the person as Base32 without padding.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Bytes of a new secret: 160 bits.
pub const SECRET_BYTES: usize = 20;
/// Digits of a code.
pub const DIGITS: usize = 6;
/// Seconds a code is good for.
pub const STEP_SECS: i64 = 30;
/// What the authenticator app shows as the service's name.
pub const ISSUER: &str = "Craft Cloud";

/// A new random secret.
pub fn generate_secret() -> Result<Zeroizing<[u8; SECRET_BYTES]>, getrandom::Error> {
    let mut secret = Zeroizing::new([0u8; SECRET_BYTES]);
    getrandom::fill(secret.as_mut_slice())?;
    Ok(secret)
}

/// The time step of `now_ms` (milliseconds since the Unix epoch).
pub fn step_at(now_ms: i64) -> i64 {
    now_ms.div_euclid(1000).div_euclid(STEP_SECS)
}

/// Whether `code` has the shape of a code: exactly six ASCII digits.
pub fn is_code(code: &str) -> bool {
    code.len() == DIGITS && code.bytes().all(|b| b.is_ascii_digit())
}

/// The code for time step `step` (HOTP's counter), with `secret`. `None` for a step before the epoch.
pub fn code_for(secret: &[u8], step: i64) -> Option<String> {
    let counter = u64::try_from(step).ok()?;
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret).ok()?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest.last().copied()? & 0x0f);
    let word = digest.get(offset..offset.checked_add(4)?)?;
    let value = u32::from_be_bytes([*word.first()?, *word.get(1)?, *word.get(2)?, *word.get(3)?]) & 0x7fff_ffff;
    Some(format!("{:0width$}", value % 10u32.pow(DIGITS as u32), width = DIGITS))
}

/// The time step `code` is right for at `now_ms`, if it is right for the step of `now_ms` or the one before or after it
/// (the latest if several match, which is all but impossible); `None` for a code that is wrong or is not six digits. All
/// three steps are always computed and compared in constant time.
pub fn verify(secret: &[u8], code: &str, now_ms: i64) -> Option<i64> {
    if !is_code(code) {
        return None;
    }
    let now = step_at(now_ms);
    let mut matched = None;
    for step in [now.saturating_sub(1), now, now.saturating_add(1)] {
        let expected = code_for(secret, step).unwrap_or_default();
        // Equal lengths (six) for a real code; `ct_eq` on slices of different lengths is false.
        if bool::from(expected.as_bytes().ct_eq(code.as_bytes())) && !expected.is_empty() {
            matched = Some(step);
        }
    }
    matched
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 Base32 without padding.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            if let Some(&c) = BASE32.get(((buffer >> bits) & 31) as usize) {
                out.push(char::from(c));
            }
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0
        && let Some(&c) = BASE32.get(((buffer << (5 - bits)) & 31) as usize)
    {
        out.push(char::from(c));
    }
    out
}

/// The inverse of [`base32_encode`] (upper or lower case, no padding, no spaces); `None` for anything else.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let value = BASE32.iter().position(|&b| b == c.to_ascii_uppercase())?;
        buffer = (buffer << 5) | u32::try_from(value).ok()?;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
            buffer &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be the zeros the encoder pads with.
    (buffer == 0).then_some(out)
}

/// `text` with everything but `A-Z a-z 0-9 - _ . ~` percent-encoded (UTF-8 bytes), for a URI's path and query.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `otpauth://` URI an authenticator app reads (from a QR code or typed in): the issuer and the account's username
/// as the label, both percent-encoded.
pub fn otpauth_uri(username: &str, secret_base32: &str) -> String {
    let issuer = percent_encode(ISSUER);
    format!(
        "otpauth://totp/{issuer}:{}?secret={secret_base32}&issuer={issuer}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        percent_encode(username)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238's SHA-1 key; its eight-digit test codes, cut to the last six digits.
    const RFC_KEY: &[u8] = b"12345678901234567890";

    #[test]
    fn the_rfc_6238_vectors_hold_for_six_digits() {
        for (time, code) in [
            (59i64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(code_for(RFC_KEY, step_at(time * 1000)).as_deref(), Some(code), "t = {time}");
        }
        // RFC 4226's own HOTP vectors, counter 0 and 1.
        assert_eq!(code_for(RFC_KEY, 0).as_deref(), Some("755224"));
        assert_eq!(code_for(RFC_KEY, 1).as_deref(), Some("287082"));
        assert_eq!(code_for(RFC_KEY, -1), None);
    }

    #[test]
    fn the_window_is_one_step_either_way_and_reports_the_matched_step() {
        let now_ms = 1_111_111_109_000;
        let step = step_at(now_ms);
        for (offset, accepted) in [(-2, false), (-1, true), (0, true), (1, true), (2, false)] {
            let code = code_for(RFC_KEY, step + offset).unwrap();
            assert_eq!(verify(RFC_KEY, &code, now_ms), accepted.then_some(step + offset), "offset {offset}");
        }
        // Just inside and just outside the step's edge.
        let edge = step_at(1_111_111_109_000) * STEP_SECS * 1000;
        assert_eq!(step_at(edge), step);
        assert_eq!(step_at(edge - 1), step - 1);
    }

    #[test]
    fn only_six_ascii_digits_are_codes() {
        for bad in ["", "12345", "1234567", "12 456", "١٢٣٤٥٦", "12345a", " 123456", "123456 ", "+12345"] {
            assert!(!is_code(bad), "{bad:?}");
            assert_eq!(verify(RFC_KEY, bad, 59_000), None, "{bad:?}");
        }
        assert!(is_code("000000"));
        assert_eq!(verify(RFC_KEY, "287082", 59_000), Some(1));
        assert_eq!(verify(RFC_KEY, "287083", 59_000), None);
    }

    #[test]
    fn base32_round_trips_and_matches_the_rfc() {
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b"fo"), "MZXQ");
        assert_eq!(base32_encode(b"foo"), "MZXW6");
        assert_eq!(base32_encode(b"foob"), "MZXW6YQ");
        assert_eq!(base32_encode(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_encode(RFC_KEY), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        for text in ["", "MY", "MZXQ", "MZXW6", "MZXW6YQ", "MZXW6YTB", "MZXW6YTBOI"] {
            assert_eq!(base32_encode(&base32_decode(text).unwrap()), text);
        }
        assert_eq!(base32_decode("mzxw6ytboi").as_deref(), Some(b"foobar".as_slice()));
        assert_eq!(base32_decode("MZXW6YTB1"), None, "1 is not in the alphabet");
        assert_eq!(base32_decode("MZXW6Y=="), None, "no padding");
        assert_eq!(base32_decode("MZ"), None, "the leftover bits must be zero");
        let secret = generate_secret().unwrap();
        assert_eq!(base32_decode(&base32_encode(secret.as_slice())).unwrap(), secret.to_vec());
        assert_eq!(base32_encode(secret.as_slice()).len(), 32);
    }

    #[test]
    fn the_otpauth_uri_encodes_the_label() {
        let uri = otpauth_uri("ana", "GEZDGNBVGY3TQOJQ");
        assert_eq!(uri, "otpauth://totp/Craft%20Cloud:ana?secret=GEZDGNBVGY3TQOJQ&issuer=Craft%20Cloud&algorithm=SHA1&digits=6&period=30");
        let odd = otpauth_uri("a.b_c-d~e", "AAAA");
        assert!(odd.contains(":a.b_c-d~e?"), "{odd}");
        // Not a real username today, but the encoder must hold for whatever one comes to be.
        let wild = otpauth_uri("a b&c=d/é?#:", "AAAA");
        assert!(wild.contains(":a%20b%26c%3Dd%2F%C3%A9%3F%23%3A?"), "{wild}");
        assert_eq!(wild.matches('?').count(), 1);
        assert_eq!(wild.matches('#').count(), 0);
    }
}
