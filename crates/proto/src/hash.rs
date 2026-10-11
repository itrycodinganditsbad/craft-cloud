//! 32-byte content identifiers, written as 64 lowercase hex digits.
//!
//! A [`ChunkId`] is **opaque** to the protocol. Today it is the BLAKE3 hash of the chunk's bytes; with
//! end-to-end encryption it may become a keyed hash. Only the chunker and the chunk verifier may assume
//! how it was made. [`FileHash`] is the BLAKE3 hash of a whole file, checked after a download is
//! assembled. [`ManifestId`] is the BLAKE3 hash of a manifest's canonical encoding.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a hex ID was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HexIdError {
    #[error("expected 64 hex digits, got {len} bytes")]
    WrongLength { len: usize },
    #[error("byte {index} is not a lowercase hex digit")]
    NotLowercaseHex { index: usize },
}

fn nibble(c: u8, index: usize) -> Result<u8, HexIdError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(HexIdError::NotLowercaseHex { index }),
    }
}

/// Parses exactly 64 lowercase hex digits. Uppercase is rejected so every ID has one spelling.
fn parse_hex32(s: &str) -> Result<[u8; 32], HexIdError> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return Err(HexIdError::WrongLength { len: bytes.len() });
    }
    let mut out = [0u8; 32];
    let (pairs, _) = bytes.as_chunks::<2>();
    for (i, (slot, [hi, lo])) in out.iter_mut().zip(pairs).enumerate() {
        *slot = (nibble(*hi, i * 2)? << 4) | nibble(*lo, i * 2 + 1)?;
    }
    Ok(out)
}

fn write_hex(bytes: &[u8; 32], f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

macro_rules! hash32 {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub fn from_hex(s: &str) -> Result<Self, HexIdError> {
                parse_hex32(s).map(Self)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write_hex(&self.0, f)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}(", stringify!($name))?;
                write_hex(&self.0, f)?;
                write!(f, ")")
            }
        }

        impl std::str::FromStr for $name {
            type Err = HexIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::from_hex(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
                Self::from_hex(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

hash32!(
    /// Names one chunk. Opaque: see the module docs.
    ChunkId
);
hash32!(
    /// BLAKE3 of a whole file's bytes.
    FileHash
);
hash32!(
    /// BLAKE3 of a manifest's canonical encoding.
    ManifestId
);

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn bytes() -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = u8::try_from(i).unwrap();
        }
        b
    }

    #[test]
    fn hex_round_trip() {
        let id = ChunkId::from_hex(HEX).unwrap();
        assert_eq!(id.as_bytes(), &bytes());
        assert_eq!(id.to_string(), HEX);
        assert_eq!(format!("{id:?}"), format!("ChunkId({HEX})"));
    }

    #[test]
    fn rejects_bad_hex() {
        assert_eq!(ChunkId::from_hex(""), Err(HexIdError::WrongLength { len: 0 }));
        assert_eq!(ChunkId::from_hex(&HEX[..63]), Err(HexIdError::WrongLength { len: 63 }));
        assert_eq!(ChunkId::from_hex(&format!("{HEX}00")), Err(HexIdError::WrongLength { len: 66 }));
        let upper = HEX.replace('a', "A");
        assert_eq!(ChunkId::from_hex(&upper), Err(HexIdError::NotLowercaseHex { index: 21 }));
        let mut g = HEX.to_owned();
        g.replace_range(0..1, "g");
        assert_eq!(ChunkId::from_hex(&g), Err(HexIdError::NotLowercaseHex { index: 0 }));
        // 32 two-byte characters: 64 bytes long, but not hex. Must not panic on the char boundaries.
        let wide = "é".repeat(32);
        assert_eq!(wide.len(), 64);
        assert_eq!(ChunkId::from_hex(&wide), Err(HexIdError::NotLowercaseHex { index: 0 }));
    }

    #[test]
    fn json_is_a_hex_string() {
        let id = FileHash::from_bytes(bytes());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{HEX}\""));
        assert_eq!(serde_json::from_str::<FileHash>(&json).unwrap(), id);
        assert!(serde_json::from_str::<FileHash>("\"abc\"").is_err());
        assert!(serde_json::from_str::<FileHash>("42").is_err());
    }
}
