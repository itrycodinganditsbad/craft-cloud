//! Conflict copy names: `name (conflict, <device>, 2026-10-11 14-03).ext`.

use crate::limits::MAX_NAME_BYTES;
use crate::name::{FORBIDDEN, Name, NameError};
use unicode_normalization::UnicodeNormalization;

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

/// Longest extension kept after the suffix, in bytes after the dot.
const MAX_EXT_BYTES: usize = 16;

/// The name for a conflict copy of `original`.
///
/// A file keeps its extension after the suffix: the part after the last dot, when that dot does not start
/// the name and the part is at most 16 bytes with no space in it. Anything else (directories, dotfiles,
/// `Re. notes from the review`) has no extension and gets the suffix at the end. `counter` is 1 for the
/// first conflict copy in a minute and 2, 3 … for more, which adds `, 2`; 0 behaves like 1. The device
/// label is cleaned to the name rules and capped at 64 bytes. If the result would pass 255 bytes, the
/// original stem is shortened at a character boundary; the extension and the suffix never are.
///
/// Never fails for a valid [`Name`]: when the extension and the suffix would leave no room for the stem,
/// the extension is dropped and the whole name is the stem. The `Result` stays for API stability.
pub fn conflict_name(original: &Name, is_dir: bool, device: &str, stamp: ConflictStamp, counter: u32) -> Result<Name, NameError> {
    let full = original.as_str();
    let ConflictStamp { year, month, day, hour, minute } = stamp;
    let repeat = if counter > 1 { format!(", {counter}") } else { String::new() };
    let suffix = format!(" (conflict, {}, {year:04}-{month:02}-{day:02} {hour:02}-{minute:02}{repeat})", clean_device(device));
    let (stem, ext) = split_extension(full, is_dir);
    // At least one byte of stem must fit beside the extension and the suffix; otherwise there is no extension.
    let (stem, ext, budget) = match MAX_NAME_BYTES.checked_sub(suffix.len()).and_then(|b| b.checked_sub(ext.len())) {
        Some(budget) if budget >= 1 => (stem, ext, budget),
        _ => (full, "", MAX_NAME_BYTES.saturating_sub(suffix.len())),
    };
    Name::new(&format!("{}{suffix}{ext}", truncate(stem, budget)))
}

/// Splits `full` into stem and extension (the extension includes its dot); see [`conflict_name`].
fn split_extension(full: &str, is_dir: bool) -> (&str, &str) {
    match full.rfind('.') {
        Some(dot) if !is_dir && dot > 0 => {
            let (stem, ext) = full.split_at(dot);
            if ext.len() <= MAX_EXT_BYTES.saturating_add(1) && !ext.contains(' ') { (stem, ext) } else { (full, "") }
        }
        _ => (full, ""),
    }
}

/// The device label with everything a name can't hold (and the separators `(`, `)` and `,`) replaced by `_`.
fn clean_device(device: &str) -> String {
    let cleaned: String = device
        .nfc()
        .collect::<String>()
        .chars()
        .map(|ch| if ch.is_control() || FORBIDDEN.contains(&ch) || matches!(ch, '(' | ')' | ',') { '_' } else { ch })
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

    fn check_fits(original: &str, device: &str) -> String {
        let out = conflict_name(&Name::new(original).unwrap(), false, device, STAMP, 1).unwrap();
        let out = out.as_str().to_owned();
        assert!(out.len() <= MAX_NAME_BYTES, "{}", out.len());
        assert!(out.contains(&format!(" (conflict, {device}, ")), "{out}");
        out
    }

    #[test]
    fn a_long_tail_after_an_early_dot_is_not_an_extension() {
        let original = format!("Re. notes from the review of the quarterly budget, {}", "and then some more words, ".repeat(8));
        let original = &original[..249.min(original.len())];
        assert_eq!(original.len(), 249);
        let out = check_fits(original.trim_end(), "laptop");
        assert!(out.ends_with(')'), "{out}");
    }

    #[test]
    fn a_long_name_with_an_early_dot_and_a_long_device_fits() {
        let original = format!("2026.10.11 {}", "打ち合わせ議事録".repeat(8));
        let original = Name::new(&original).unwrap();
        assert!(original.as_str().len() > 150);
        let device = "d".repeat(70);
        let out = conflict_name(&original, false, &device, STAMP, 1).unwrap();
        assert!(out.as_str().len() <= MAX_NAME_BYTES);
        assert!(out.as_str().contains(" (conflict, "));
    }

    #[test]
    fn a_255_byte_name_with_a_16_byte_extension_keeps_it() {
        let original = format!("{}.{}", "a".repeat(255 - 17), "e".repeat(16));
        let out = check_fits(&original, "laptop");
        assert!(out.ends_with(&format!(").{}", "e".repeat(16))), "{out}");
    }

    #[test]
    fn a_long_extension_is_dropped_instead_of_failing() {
        let out = check_fits(&format!("a.{}", "x".repeat(240)), "pc");
        assert!(out.ends_with(')'), "{out}");
        let out = check_fits(&format!("a.{}", "x".repeat(17)), "pc");
        assert!(out.ends_with(')'), "{out}");
    }

    #[test]
    fn counter_zero_behaves_like_one() {
        assert_eq!(conflict("a.txt", false, "pc", 0), conflict("a.txt", false, "pc", 1));
    }

    #[test]
    fn nfc_expanding_device_labels_still_fit() {
        let original = format!("{}.psd", "日".repeat(82));
        let out = conflict_name(&Name::new(&original).unwrap(), false, &"\u{958}".repeat(30), STAMP, 1).unwrap();
        let out = out.as_str();
        assert!(out.len() <= MAX_NAME_BYTES, "{}", out.len());
        assert!(out.ends_with(".psd"), "{out}");
        assert!(out.contains(" (conflict, "), "{out}");
    }
}
