//! A member's colour: which of twelve fixed colours an account is shown in (presence cursors, the member list, the people
//! badge), the same on every machine.
//!
//! The colour is derived, never stored: [`member_colour`] is BLAKE3 of a fixed label and the account's ID, read as a
//! big-endian number, modulo [`PALETTE_LEN`]. Two members of a big space may share a colour; the name tells them apart.
//! [`PALETTE`] holds the twelve colours, each with a value for a light and for a dark background.

use uuid::Uuid;

/// How many colours the palette has.
pub const PALETTE_LEN: usize = 12;

/// One palette colour: a name and an sRGB value for light and for dark backgrounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteColour {
    pub name: &'static str,
    /// `[r, g, b]` for a light background.
    pub light: [u8; 3],
    /// `[r, g, b]` for a dark background.
    pub dark: [u8; 3],
}

/// The twelve colours, by palette index. Fixed: a change would recolour every member everywhere.
pub const PALETTE: [PaletteColour; PALETTE_LEN] = [
    PaletteColour { name: "red", light: [0xc6, 0x28, 0x28], dark: [0xef, 0x7a, 0x7a] },
    PaletteColour { name: "orange", light: [0xb8, 0x4c, 0x00], dark: [0xff, 0x9a, 0x5c] },
    PaletteColour { name: "amber", light: [0x9a, 0x60, 0x00], dark: [0xf5, 0xc0, 0x4a] },
    PaletteColour { name: "lime", light: [0x5a, 0x7d, 0x0a], dark: [0xb5, 0xd9, 0x5a] },
    PaletteColour { name: "green", light: [0x1b, 0x7a, 0x3e], dark: [0x6e, 0xd1, 0x8f] },
    PaletteColour { name: "teal", light: [0x0b, 0x7a, 0x75], dark: [0x5e, 0xd4, 0xcc] },
    PaletteColour { name: "cyan", light: [0x08, 0x77, 0xa6], dark: [0x5c, 0xc8, 0xf0] },
    PaletteColour { name: "blue", light: [0x1f, 0x4f, 0xc4], dark: [0x7f, 0xa6, 0xff] },
    PaletteColour { name: "indigo", light: [0x4a, 0x3a, 0xc8], dark: [0xa0, 0x98, 0xff] },
    PaletteColour { name: "purple", light: [0x8a, 0x2f, 0xb8], dark: [0xcf, 0x93, 0xf0] },
    PaletteColour { name: "pink", light: [0xc2, 0x1f, 0x7a], dark: [0xf5, 0x8a, 0xc4] },
    PaletteColour { name: "slate", light: [0x4a, 0x5a, 0x6a], dark: [0xa8, 0xb6, 0xc4] },
];

/// The palette index (0 to 11) an account is shown in.
pub fn member_colour(user: Uuid) -> u8 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"craft-cloud member colour v1");
    hasher.update(user.as_bytes());
    let digest = hasher.finalize();
    let remainder = digest.as_bytes().iter().fold(0u32, |acc, byte| (acc * 256 + u32::from(*byte)) % PALETTE_LEN as u32);
    // At most 11.
    u8::try_from(remainder).unwrap_or(0)
}

/// The palette colour at `index`; an index past the palette (a newer server) wraps around.
pub fn palette_colour(index: u8) -> PaletteColour {
    PALETTE.get(usize::from(index) % PALETTE_LEN).copied().unwrap_or(PALETTE[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_is_one_of_twelve_and_the_same_every_time() {
        let user = Uuid::now_v7();
        assert!(usize::from(member_colour(user)) < PALETTE_LEN);
        assert_eq!(member_colour(user), member_colour(user));
        let seen: std::collections::BTreeSet<u8> = (0..200).map(|_| member_colour(Uuid::now_v7())).collect();
        assert!(seen.len() > 6, "the colours are spread: {seen:?}");
    }

    /// The derivation is pinned: a change would recolour every member everywhere.
    #[test]
    fn the_derivation_is_pinned() {
        let user = Uuid::from_bytes([7; 16]);
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"craft-cloud member colour v1");
        hasher.update(&[7; 16]);
        let digest = *hasher.finalize().as_bytes();
        // The digest as a big-endian 256-bit number, modulo 12, by long division.
        let mut rem = 0u32;
        for byte in digest {
            rem = (rem * 256 + u32::from(byte)) % 12;
        }
        assert_eq!(u32::from(member_colour(user)), rem);
        assert_eq!(member_colour(Uuid::nil()), 1);
    }

    /// Golden values: the same account has the same colour on every machine and in every release. A change here recolours
    /// every member everywhere.
    #[test]
    fn colours_of_known_accounts_are_pinned() {
        let known = [
            (Uuid::nil(), 1u8),
            (Uuid::from_bytes([7; 16]), 10),
            (Uuid::from_bytes([0xff; 16]), 0),
            (Uuid::from_u128(1), 0),
            (Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef), 3),
        ];
        let got: Vec<u8> = known.iter().map(|(user, _)| member_colour(*user)).collect();
        let want: Vec<u8> = known.iter().map(|(_, colour)| *colour).collect();
        assert_eq!(got, want);
    }

    /// The twelve colours, value for value.
    #[test]
    fn the_palette_values_are_pinned() {
        let want: [(&str, [u8; 3], [u8; 3]); PALETTE_LEN] = [
            ("red", [0xc6, 0x28, 0x28], [0xef, 0x7a, 0x7a]),
            ("orange", [0xb8, 0x4c, 0x00], [0xff, 0x9a, 0x5c]),
            ("amber", [0x9a, 0x60, 0x00], [0xf5, 0xc0, 0x4a]),
            ("lime", [0x5a, 0x7d, 0x0a], [0xb5, 0xd9, 0x5a]),
            ("green", [0x1b, 0x7a, 0x3e], [0x6e, 0xd1, 0x8f]),
            ("teal", [0x0b, 0x7a, 0x75], [0x5e, 0xd4, 0xcc]),
            ("cyan", [0x08, 0x77, 0xa6], [0x5c, 0xc8, 0xf0]),
            ("blue", [0x1f, 0x4f, 0xc4], [0x7f, 0xa6, 0xff]),
            ("indigo", [0x4a, 0x3a, 0xc8], [0xa0, 0x98, 0xff]),
            ("purple", [0x8a, 0x2f, 0xb8], [0xcf, 0x93, 0xf0]),
            ("pink", [0xc2, 0x1f, 0x7a], [0xf5, 0x8a, 0xc4]),
            ("slate", [0x4a, 0x5a, 0x6a], [0xa8, 0xb6, 0xc4]),
        ];
        let got: Vec<_> = PALETTE.iter().map(|c| (c.name, c.light, c.dark)).collect();
        assert_eq!(got, want.to_vec());
    }

    /// Every colour is text a person reads: at least 4.5:1 (WCAG AA) on egui's light panel (#f8f8f8) and white for the light
    /// value, and on its dark panel (#1b1b1b) and black for the dark one.
    #[test]
    fn every_colour_reads_as_text_on_its_background() {
        fn luminance(rgb: [u8; 3]) -> f64 {
            let lin = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
        }
        let contrast = |a: [u8; 3], b: [u8; 3]| {
            let (la, lb) = (luminance(a), luminance(b));
            (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
        };
        for c in PALETTE {
            for bg in [[0xf8; 3], [0xff; 3]] {
                assert!(contrast(c.light, bg) >= 4.5, "{} light on {bg:?}: {}", c.name, contrast(c.light, bg));
            }
            for bg in [[0x1b; 3], [0x00; 3]] {
                assert!(contrast(c.dark, bg) >= 4.5, "{} dark on {bg:?}: {}", c.name, contrast(c.dark, bg));
            }
        }
    }

    #[test]
    fn the_palette_has_twelve_distinct_colours() {
        let names: std::collections::BTreeSet<_> = PALETTE.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), PALETTE_LEN);
        let lights: std::collections::BTreeSet<_> = PALETTE.iter().map(|c| c.light).collect();
        assert_eq!(lights.len(), PALETTE_LEN);
        assert_eq!(palette_colour(3), PALETTE[3]);
        assert_eq!(palette_colour(15), PALETTE[3], "an index past the palette wraps");
    }
}
