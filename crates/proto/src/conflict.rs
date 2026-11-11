//! Conflict copy names: `name (conflict, <device>, 2026-10-11 14-03).ext`.

use crate::limits::MAX_NAME_BYTES;
use crate::name::{FORBIDDEN, Name, NameError};
use unicode_normalization::UnicodeNormalization;

/// Longest device label kept in a conflict name, in bytes.
const MAX_DEVICE_BYTES: usize = 64;

/// The minute a conflict was found, in the user's local time. The caller reads the clock. When the local
/// offset can't be read, the caller gives UTC and sets `utc`, which marks the time with `Z` (`14-03Z`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictStamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub utc: bool,
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
    let ConflictStamp { year, month, day, hour, minute, utc } = stamp;
    let repeat = if counter > 1 { format!(", {counter}") } else { String::new() };
    let zone = if utc { "Z" } else { "" };
    let suffix = format!(" (conflict, {}, {year:04}-{month:02}-{day:02} {hour:02}-{minute:02}{zone}{repeat})", clean_device(device));
    let (stem, ext) = split_extension(full, is_dir);
    // At least one byte of stem must fit beside the extension and the suffix; otherwise there is no extension.
    let (stem, ext, budget) = match MAX_NAME_BYTES.checked_sub(suffix.len()).and_then(|b| b.checked_sub(ext.len())) {
        Some(budget) if budget >= 1 => (stem, ext, budget),
        _ => (full, "", MAX_NAME_BYTES.saturating_sub(suffix.len())),
    };
    Name::new(&format!("{}{suffix}{ext}", truncate(stem, budget)))
}

/// A conflict copy's name taken apart again by [`parse_conflict_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictName {
    /// The original's name as the copy keeps it: the stem (shortened if the copy's name had to be) and the extension.
    pub original: String,
    /// The device label as the name holds it (already cleaned, see [`conflict_name`]).
    pub device: String,
    /// The minute the conflict was found.
    pub at: ConflictStamp,
    /// 1 for the first copy in a minute, 2, 3 … for more.
    pub counter: u32,
}

impl ConflictStamp {
    /// The stamp as milliseconds since the Unix epoch when it is marked UTC (`Z`); `None` for a local time, whose
    /// offset the name does not hold, and for a date that does not exist.
    pub fn unix_ms(&self) -> Option<i64> {
        if self.utc { self.naive_ms() } else { None }
    }

    /// The stamp read as if it were UTC, for ordering copies by when they were found (a local time is off by its
    /// offset at most). `None` for a date that does not exist.
    pub fn naive_ms(&self) -> Option<i64> {
        let Self { year, month, day, hour, minute, .. } = *self;
        let (year, month, day) = (i64::from(year), i64::from(month), i64::from(day));
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
            return None;
        }
        // Days since 1970-01-01 of a civil date (proleptic Gregorian), with the year counted from March.
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some(((days * 24 + i64::from(hour)) * 60 + i64::from(minute)) * 60_000)
    }
}

/// The parts of a conflict copy's name, or `None` when `name` is not one [`conflict_name`] makes.
///
/// The exact inverse of [`conflict_name`]: ` (conflict, <device>, YYYY-MM-DD HH-MM[Z][, n])` with the extension (a dot
/// and at most 16 bytes, no space) after it or nothing. The device holds no `(`, `)` or `,`, so the last such suffix
/// in the name is the one made last. A name that only looks similar (no device, a date that does not exist, a counter
/// below 2 or with a leading zero, text after the closing parenthesis that is not an extension) is `None`. A user's
/// own file can still match; the name alone cannot tell, so callers also look for the original.
///
/// `original` is what the copy kept of the original's name: a stem shortened to fit is not restored.
pub fn parse_conflict_name(name: &str) -> Option<ConflictName> {
    const OPEN: &str = " (conflict, ";
    let at = name.rfind(OPEN)?;
    let stem = name.get(..at)?;
    let rest = name.get(at.checked_add(OPEN.len())?..)?;
    let (device, rest) = rest.split_once(", ")?;
    if device.is_empty() || device.len() > MAX_DEVICE_BYTES || device.contains(['(', ')', ',']) {
        return None;
    }
    // `YYYY-MM-DD HH-MM`, then `Z`, then `, n`, then `)`.
    let date = rest.get(..16)?;
    let after = rest.get(16..)?;
    let b = date.as_bytes();
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || b.get(10) != Some(&b' ') || b.get(13) != Some(&b'-') {
        return None;
    }
    let digits = |from: usize, to: usize| -> Option<u32> {
        let s = date.get(from..to)?;
        if s.bytes().all(|b| b.is_ascii_digit()) { s.parse().ok() } else { None }
    };
    let (year, month, day, hour, minute) = (digits(0, 4)?, digits(5, 7)?, digits(8, 10)?, digits(11, 13)?, digits(14, 16)?);
    let (utc, after) = match after.strip_prefix('Z') {
        Some(after) => (true, after),
        None => (false, after),
    };
    let (counter, ext) = match after.strip_prefix(", ") {
        Some(more) => {
            let (number, ext) = more.split_once(')')?;
            if number.is_empty() || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let counter: u32 = number.parse().ok()?;
            if counter < 2 {
                return None;
            }
            (counter, ext)
        }
        None => (1, after.strip_prefix(')')?),
    };
    if !(ext.is_empty() || (ext.starts_with('.') && ext.len() <= MAX_EXT_BYTES.saturating_add(1) && !ext.contains(' '))) {
        return None;
    }
    let at = ConflictStamp {
        year: u16::try_from(year).ok()?,
        month: u8::try_from(month).ok()?,
        day: u8::try_from(day).ok()?,
        hour: u8::try_from(hour).ok()?,
        minute: u8::try_from(minute).ok()?,
        utc,
    };
    at.naive_ms()?;
    if stem.is_empty() {
        return None;
    }
    Some(ConflictName { original: format!("{stem}{ext}"), device: device.to_owned(), at, counter })
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

    const STAMP: ConflictStamp = ConflictStamp { year: 2026, month: 10, day: 11, hour: 14, minute: 3, utc: false };

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
    fn utc_times_are_marked() {
        let stamp = ConflictStamp { utc: true, ..STAMP };
        let out = conflict_name(&Name::new("a.txt").unwrap(), false, "pc", stamp, 2).unwrap();
        assert_eq!(out.as_str(), "a (conflict, pc, 2026-10-11 14-03Z, 2).txt");
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
    fn parses_what_the_builder_makes() {
        let p = parse_conflict_name("Shot 010 (conflict, laptop, 2026-10-11 14-03).psd").unwrap();
        assert_eq!((p.original.as_str(), p.device.as_str(), p.at, p.counter), ("Shot 010.psd", "laptop", STAMP, 1));
        let p = parse_conflict_name("a (conflict, pc, 2026-10-11 14-03Z, 2).txt").unwrap();
        assert_eq!((p.original.as_str(), p.at, p.counter), ("a.txt", ConflictStamp { utc: true, ..STAMP }, 2));
        assert_eq!(parse_conflict_name(".gitignore (conflict, pc, 2026-10-11 14-03)").unwrap().original, ".gitignore");
        assert_eq!(parse_conflict_name("archive.tar (conflict, laptop, 2026-10-11 14-03).gz").unwrap().original, "archive.tar.gz");
        // A copy of a copy: the last suffix is the one made last.
        let p = parse_conflict_name("a (conflict, x, 2026-10-10 09-00) (conflict, y, 2026-10-11 14-03).txt").unwrap();
        assert_eq!((p.original.as_str(), p.device.as_str()), ("a (conflict, x, 2026-10-10 09-00).txt", "y"));
    }

    #[test]
    fn names_that_only_look_similar_do_not_parse() {
        for name in [
            "plain.txt",
            "a (conflict, pc, 2026-10-11 14-03.txt",
            "a (conflict, , 2026-10-11 14-03).txt",
            "a (conflict, pc, 2026-13-11 14-03).txt",
            "a (conflict, pc, 2026-10-32 14-03).txt",
            "a (conflict, pc, 2026-10-11 24-03).txt",
            "a (conflict, pc, 2026-10-11 14-60).txt",
            "a (conflict, pc, 2026-10-11 14-03, 1).txt",
            "a (conflict, pc, 2026-10-11 14-03, 02).txt",
            "a (conflict, pc, 2026-10-11 14-03, x).txt",
            "a (conflict, pc, 2026-10-11 14-03) trailing",
            "a (conflict, pc, 2026-10-11 14-03).two words",
            "a (conflict, pc, 2026-10-1 14-03).txt",
            "a (conflict, pc, 2026/10/11 14-03).txt",
            "a (Conflict, pc, 2026-10-11 14-03).txt",
            " (conflict, pc, 2026-10-11 14-03).txt",
            "a (conflict, p,c, 2026-10-11 14-03).txt",
            "a (conflict, pc, +026-10-11 14-03).txt",
            "a (conflict, pc, 2026-10-11 14-03é).txt",
        ] {
            assert_eq!(parse_conflict_name(name), None, "{name}");
        }
    }

    #[test]
    fn the_stamp_converts_to_milliseconds() {
        let utc = ConflictStamp { year: 1970, month: 1, day: 1, hour: 0, minute: 1, utc: true };
        assert_eq!(utc.unix_ms(), Some(60_000));
        let stamp = ConflictStamp { year: 2026, month: 10, day: 11, hour: 14, minute: 3, utc: true };
        assert_eq!(stamp.unix_ms(), Some(1_791_727_380_000));
        assert_eq!(STAMP.unix_ms(), None, "a local time has no offset in the name");
        assert_eq!(STAMP.naive_ms(), Some(1_791_727_380_000));
        let leap = ConflictStamp { year: 2024, month: 2, day: 29, hour: 0, minute: 0, utc: true };
        assert_eq!(leap.unix_ms(), Some(1_709_164_800_000));
    }

    proptest::proptest! {
        #[test]
        fn build_then_parse_gives_the_parts_back(
            stem in "[a-zA-Z0-9 ().,_日é-]{1,40}",
            ext in proptest::option::of("[a-zA-Z0-9()日é_-]{1,12}"),
            device in "[a-zA-Z0-9 ().,_日é-]{1,40}",
            year in 1000u16..9999, month in 1u8..=12, day in 1u8..=28, hour in 0u8..24, minute in 0u8..60,
            utc in proptest::bool::ANY, counter in 0u32..1000,
        ) {
            let original = match &ext {
                Some(ext) => format!("{stem}.{ext}"),
                None => stem.clone(),
            };
            let Ok(name) = Name::new(&original) else { return Ok(()) };
            let stamp = ConflictStamp { year, month, day, hour, minute, utc };
            let built = conflict_name(&name, false, &device, stamp, counter).unwrap();
            let parsed = parse_conflict_name(built.as_str()).expect("a built name parses");
            proptest::prop_assert_eq!(parsed.at, stamp);
            proptest::prop_assert_eq!(parsed.counter, counter.max(1));
            proptest::prop_assert_eq!(&parsed.device, &clean_device(&device));
            // Short names are never shortened, so the original comes back exactly as the name holds it.
            proptest::prop_assert_eq!(parsed.original.as_str(), name.as_str());
            // And building again from the parts gives the same name.
            let again = conflict_name(&Name::new(&parsed.original).unwrap(), false, &parsed.device, stamp, counter).unwrap();
            proptest::prop_assert_eq!(again.as_str(), built.as_str());
        }
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
