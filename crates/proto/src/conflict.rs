//! Conflict copy names: `name (conflict, <device>, 2026-10-11 14-03).ext`.

use crate::limits::MAX_NAME_BYTES;
use crate::name::{Name, NameError};

/// Longest device label kept in a conflict name, in bytes.
const MAX_DEVICE_BYTES: usize = 64;

/// The minute a conflict was found, in the user's local time. The caller reads the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictStamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
}

/// The name for a conflict copy of `original`.
///
/// Files keep their extension (the part after the last dot, unless that dot starts the name); directories
/// have none. `counter` is 1 for the first conflict copy in a minute and 2, 3 … for more, which adds `, 2`.
/// The device label is cleaned to the name rules and capped at 64 bytes. If the result would pass
/// 255 bytes, the original stem is shortened at a character boundary; the extension and the suffix never
/// are. Fails only when even an empty stem is too long (an extension of 200+ bytes).
pub fn conflict_name(original: &Name, is_dir: bool, device: &str, stamp: ConflictStamp, counter: u32) -> Result<Name, NameError> {
    let full = original.as_str();
    let (stem, ext) = match full.rfind('.') {
        Some(dot) if !is_dir && dot > 0 => full.split_at(dot),
        _ => (full, ""),
    };
    let ConflictStamp { year, month, day, hour, minute } = stamp;
    let repeat = if counter > 1 { format!(", {counter}") } else { String::new() };
    let suffix = format!(" (conflict, {}, {year:04}-{month:02}-{day:02} {hour:02}-{minute:02}{repeat})", clean_device(device));
    let budget = MAX_NAME_BYTES.checked_sub(suffix.len()).and_then(|b| b.checked_sub(ext.len()));
    let Some(budget) = budget else {
        return Err(NameError::TooLong { len: suffix.len().saturating_add(ext.len()) });
    };
    Name::new(&format!("{}{suffix}{ext}", truncate(stem, budget)))
}

/// The device label with everything a name can't hold (and the separators `(`, `)` and `,`) replaced by `_`.
fn clean_device(device: &str) -> String {
    let cleaned: String = device
        .chars()
        .map(|ch| if ch.is_control() || matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' | '(' | ')' | ',') { '_' } else { ch })
        .collect();
    let trimmed = truncate(cleaned.trim_matches(|c| c == ' ' || c == '.'), MAX_DEVICE_BYTES).trim_end_matches([' ', '.']);
    if trimmed.is_empty() { "device".to_owned() } else { trimmed.to_owned() }
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    s.get(..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: ConflictStamp = ConflictStamp { year: 2026, month: 10, day: 11, hour: 14, minute: 3 };

    fn conflict(name: &str, is_dir: bool, device: &str, counter: u32) -> String {
        conflict_name(&Name::new(name).unwrap(), is_dir, device, STAMP, counter).unwrap().as_str().to_owned()
    }

    #[test]
    fn keeps_the_extension_after_the_suffix() {
        assert_eq!(conflict("Shot 010.psd", false, "laptop", 1), "Shot 010 (conflict, laptop, 2026-10-11 14-03).psd");
        assert_eq!(conflict("archive.tar.gz", false, "laptop", 1), "archive.tar (conflict, laptop, 2026-10-11 14-03).gz");
    }

    #[test]
    fn dotfiles_extensionless_files_and_directories_get_the_suffix_at_the_end() {
        assert_eq!(conflict(".gitignore", false, "pc", 1), ".gitignore (conflict, pc, 2026-10-11 14-03)");
        assert_eq!(conflict("Makefile", false, "pc", 1), "Makefile (conflict, pc, 2026-10-11 14-03)");
        assert_eq!(conflict("Shots.v2", true, "pc", 1), "Shots.v2 (conflict, pc, 2026-10-11 14-03)");
    }

    #[test]
    fn a_second_conflict_in_the_same_minute_gets_a_counter() {
        assert_eq!(conflict("a.txt", false, "pc", 2), "a (conflict, pc, 2026-10-11 14-03, 2).txt");
    }

    #[test]
    fn device_labels_are_cleaned() {
        assert_eq!(conflict("a.txt", false, "Ana's PC (work): 2/3", 1), "a (conflict, Ana's PC _work__ 2_3, 2026-10-11 14-03).txt");
        assert_eq!(conflict("a.txt", false, "  ...  ", 1), "a (conflict, device, 2026-10-11 14-03).txt");
        assert_eq!(conflict("a.txt", false, "tab\there", 1), "a (conflict, tab_here, 2026-10-11 14-03).txt");
        let long = conflict("a.txt", false, &"é".repeat(40), 1);
        assert!(long.contains(&format!(", {}, ", "é".repeat(32))), "{long}");
    }

    #[test]
    fn long_names_lose_stem_bytes_not_the_extension() {
        let original = format!("{}.psd", "日".repeat(82)); // 246 + 4 = 250 bytes
        let out = conflict(&original, false, "laptop", 1);
        assert!(out.len() <= MAX_NAME_BYTES, "{}", out.len());
        assert!(out.ends_with(" (conflict, laptop, 2026-10-11 14-03).psd"), "{out}");
        assert!(out.starts_with("日日日"));
    }

    #[test]
    fn an_absurd_extension_is_an_error_not_a_panic() {
        let original = format!("a.{}", "x".repeat(240));
        let err = conflict_name(&Name::new(&original).unwrap(), false, "pc", STAMP, 1).unwrap_err();
        assert!(matches!(err, NameError::TooLong { .. }));
    }
}
