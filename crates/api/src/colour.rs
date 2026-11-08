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
    PaletteColour { name: "orange", light: [0xd8, 0x4b, 0x0c], dark: [0xff, 0x9a, 0x5c] },
    PaletteColour { name: "amber", light: [0xa8, 0x6a, 0x00], dark: [0xf5, 0xc0, 0x4a] },
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
