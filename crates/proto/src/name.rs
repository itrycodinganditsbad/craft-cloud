//! File and directory names.
//!
//! A [`Name`] is one path segment, NFC-normalised, that can exist on Windows, macOS and Linux alike. The
//! server rejects anything else, so every file syncs to every machine. Two names that differ only by case
//! (Unicode full case folding) or normalisation have the same [`FoldKey`] and can't share a directory.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::limits::MAX_NAME_BYTES;

/// Characters Windows forbids in names (besides control characters, which are rejected separately).
pub(crate) const FORBIDDEN: [char; 9] = ['/', '\\', '<', '>', ':', '"', '|', '?', '*'];

/// The sync agent keeps its own state in `.craft-cloud/` in the sync root; no node may take that name.
const AGENT_DIR: &str = ".craft-cloud";

/// Why a name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("the name is empty")]
    Empty,
    #[error("the name is {len} bytes long; the limit is {MAX_NAME_BYTES}")]
    TooLong { len: usize },
    #[error("`.` and `..` are not names")]
    DotOrDotDot,
    #[error("the name contains {ch:?}, which Windows does not allow")]
    ForbiddenChar { ch: char },
    #[error("the name contains the control character U+{:04X}", u32::from(*ch))]
    ControlChar { ch: char },
    #[error("the name ends with a dot or a space, which Windows does not allow")]
    TrailingDotOrSpace,
    #[error("`{name}` is a reserved name")]
    Reserved { name: String },
}

/// One validated, NFC-normalised path segment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

/// A name's case-folded form: equal keys mean the names collide in a directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FoldKey(String);

impl Name {
    /// Normalises `raw` to NFC and validates it.
    pub fn new(raw: &str) -> Result<Self, NameError> {
        let nfc: String = raw.nfc().collect();
        validate(&nfc)?;
        Ok(Self(nfc))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn fold_key(&self) -> FoldKey {
        FoldKey(fold(&self.0))
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Name {
    type Error = NameError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

/// Canonical caseless form: NFD, Unicode full case folding, then NFC.
fn fold(s: &str) -> String {
    let nfd: String = s.nfd().collect();
    caseless::default_case_fold_str(&nfd).nfc().collect()
}

fn validate(s: &str) -> Result<(), NameError> {
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    if s.len() > MAX_NAME_BYTES {
        return Err(NameError::TooLong { len: s.len() });
    }
    if s == "." || s == ".." {
        return Err(NameError::DotOrDotDot);
    }
    for ch in s.chars() {
        if ch.is_control() {
            return Err(NameError::ControlChar { ch });
        }
        if FORBIDDEN.contains(&ch) {
            return Err(NameError::ForbiddenChar { ch });
        }
    }
    if s.ends_with('.') || s.ends_with(' ') {
        return Err(NameError::TrailingDotOrSpace);
    }
    if is_windows_reserved(s) || fold(s) == AGENT_DIR {
        return Err(NameError::Reserved { name: s.to_owned() });
    }
    Ok(())
}

/// Windows device names, with or without an extension and in any case: `CON`, `con.txt`, `COM1.log`, `lpt³`.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let digit = upper.strip_prefix("COM").or_else(|| upper.strip_prefix("LPT"));
            matches!(digit, Some("0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for ok in ["a", "Shot 010.psd", ".gitignore", "archive.tar.gz", "日本語.txt", " leading space", "con-test", "COM10", "a..b"] {
            assert_eq!(Name::new(ok).unwrap().as_str(), ok, "{ok}");
        }
    }

    #[test]
    fn rejects_what_some_os_cannot_store() {
        assert_eq!(Name::new(""), Err(NameError::Empty));
        assert_eq!(Name::new("."), Err(NameError::DotOrDotDot));
        assert_eq!(Name::new(".."), Err(NameError::DotOrDotDot));
        for ch in FORBIDDEN {
            assert_eq!(Name::new(&format!("a{ch}b")), Err(NameError::ForbiddenChar { ch }));
        }
        assert_eq!(Name::new("a\0b"), Err(NameError::ControlChar { ch: '\0' }));
        assert_eq!(Name::new("tab\there"), Err(NameError::ControlChar { ch: '\t' }));
        assert_eq!(Name::new("del\u{7f}"), Err(NameError::ControlChar { ch: '\u{7f}' }));
        assert_eq!(Name::new("c1\u{85}"), Err(NameError::ControlChar { ch: '\u{85}' }));
        assert_eq!(Name::new("file."), Err(NameError::TrailingDotOrSpace));
        assert_eq!(Name::new("file "), Err(NameError::TrailingDotOrSpace));
    }

    #[test]
    fn rejects_windows_device_names_in_any_form() {
        for bad in ["CON", "con", "Con.txt", "NUL.tar.gz", "aux .log", "COM1", "com9.x", "LPT0", "lpt³", "CONIN$", "conout$.txt"] {
            assert!(matches!(Name::new(bad), Err(NameError::Reserved { .. })), "{bad}");
        }
    }

    #[test]
    fn rejects_the_agent_directory_in_any_case() {
        for bad in [".craft-cloud", ".Craft-Cloud", ".CRAFT-CLOUD"] {
            assert!(matches!(Name::new(bad), Err(NameError::Reserved { .. })), "{bad}");
        }
        assert!(Name::new(".craft-cloud2").is_ok());
    }

    #[test]
    fn length_is_counted_in_utf8_bytes_after_nfc() {
        assert!(Name::new(&"a".repeat(255)).is_ok());
        assert_eq!(Name::new(&"a".repeat(256)), Err(NameError::TooLong { len: 256 }));
        // "é" decomposed is 3 bytes, composed 2: 100 of them are 300 bytes raw but 200 after NFC.
        let decomposed = "e\u{301}".repeat(100);
        assert_eq!(Name::new(&decomposed).unwrap().as_str().len(), 200);
        assert_eq!(Name::new(&"日".repeat(86)), Err(NameError::TooLong { len: 258 }));
    }

    #[test]
    fn normalisation_makes_composed_and_decomposed_equal() {
        let composed = Name::new("caf\u{e9}.psd").unwrap();
        let decomposed = Name::new("cafe\u{301}.psd").unwrap();
        assert_eq!(composed, decomposed);
    }

    #[test]
    fn fold_key_matches_unicode_case_insensitive_names() {
        let same = |a: &str, b: &str| Name::new(a).unwrap().fold_key() == Name::new(b).unwrap().fold_key();
        assert!(same("Straße.psd", "STRASSE.PSD"));
        assert!(same("İ", "i\u{307}"));
        assert!(same("cafe\u{301}.txt", "CAFÉ.TXT"));
        assert!(same("Σίσυφος", "ΣΊΣΥΦΟΣ"));
        assert!(!same("a.psd", "b.psd"));
    }

    #[test]
    fn json_validates_on_the_way_in() {
        let name: Name = serde_json::from_str("\"cafe\\u0301\"").unwrap();
        assert_eq!(name.as_str(), "caf\u{e9}");
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"caf\u{e9}\"");
        assert!(serde_json::from_str::<Name>("\"a/b\"").is_err());
        assert!(serde_json::from_str::<Name>("\"\"").is_err());
    }
}
