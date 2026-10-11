//! Property tests: hostile input never panics, and what is accepted stays valid.

use craft_cloud_proto::limits::MAX_CHUNKS_PER_MANIFEST;
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, ConflictStamp, FileHash, HexIdError, Manifest, ManifestError, Name, conflict_name};
use proptest::prelude::*;

fn manifest_strategy() -> impl Strategy<Value = Manifest> {
    ("[a-z0-9-]{1,32}", any::<[u8; 32]>(), proptest::collection::vec((any::<[u8; 32]>(), 1u32..=4096), 0..20)).prop_map(|(name, hash, chunks)| {
        let chunks: Vec<ChunkRef> = chunks.into_iter().map(|(id, len)| ChunkRef { id: ChunkId::from_bytes(id), len }).collect();
        let size = chunks.iter().map(|c| u64::from(c.len)).sum();
        Manifest::new(ChunkerParamsId::new(&name).unwrap(), size, FileHash::from_bytes(hash), chunks).unwrap()
    })
}

/// The encoding of a valid manifest with no chunks; its last four bytes are the chunk count.
fn empty_manifest_bytes() -> Vec<u8> {
    Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 0, FileHash::from_bytes([0; 32]), vec![]).unwrap().encode()
}

proptest! {
    #[test]
    fn any_string_is_accepted_or_rejected_without_panicking(raw in any::<String>()) {
        if let Ok(name) = Name::new(&raw) {
            // Accepted names are stable: validating the stored form gives the same name.
            prop_assert_eq!(Name::new(name.as_str()).unwrap(), name.clone());
            prop_assert!(name.as_str().len() <= 255);
            let _ = name.fold_key();
        }
    }

    #[test]
    fn conflict_names_are_always_valid(raw in "[^/\\\\<>:\"|?*\\x00-\\x1f]{1,255}", device in any::<String>(), counter in 1u32..1000, is_dir: bool) {
        if let Ok(original) = Name::new(&raw) {
            let stamp = ConflictStamp { year: 2026, month: 10, day: 11, hour: 14, minute: 3 };
            if let Ok(name) = conflict_name(&original, is_dir, &device, stamp, counter) {
                prop_assert!(name.as_str().len() <= 255);
                prop_assert!(name.as_str().contains(" (conflict, "));
                prop_assert_ne!(name.fold_key(), original.fold_key());
            }
        }
    }

    #[test]
    fn valid_manifests_round_trip(m in manifest_strategy()) {
        let bytes = m.encode();
        prop_assert_eq!(Manifest::decode(&bytes), Ok(m.clone()));
        let decoded = Manifest::decode(&bytes).unwrap();
        prop_assert_eq!(decoded.encode(), bytes);
    }

    #[test]
    fn truncated_manifests_are_rejected(m in manifest_strategy(), cut in any::<prop::sample::Index>()) {
        let bytes = m.encode();
        let n = cut.index(bytes.len());
        prop_assert!(Manifest::decode(&bytes[..n]).is_err());
    }

    #[test]
    fn manifests_with_extra_bytes_are_rejected(m in manifest_strategy(), extra in proptest::collection::vec(any::<u8>(), 1..64)) {
        let mut bytes = m.encode();
        bytes.extend_from_slice(&extra);
        prop_assert_eq!(Manifest::decode(&bytes), Err(ManifestError::TrailingBytes { extra: extra.len() }));
    }

    #[test]
    fn corrupted_manifests_never_panic(m in manifest_strategy(), at in any::<prop::sample::Index>(), byte in any::<u8>()) {
        let mut bytes = m.encode();
        let i = at.index(bytes.len());
        bytes[i] = byte;
        if let Ok(d) = Manifest::decode(&bytes) {
            prop_assert_eq!(d.encode(), bytes);
        }
    }

    #[test]
    fn hostile_chunk_counts_are_rejected_without_panicking(count in any::<u32>(), tail in proptest::collection::vec(any::<u8>(), 0..512)) {
        let mut bytes = empty_manifest_bytes();
        let n = bytes.len();
        bytes[n - 4..].copy_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&tail);
        // Each chunk entry is a 32-byte id plus a 4-byte length.
        let needed = usize::try_from(count).unwrap_or(usize::MAX).saturating_mul(36);
        if count > MAX_CHUNKS_PER_MANIFEST {
            prop_assert_eq!(Manifest::decode(&bytes), Err(ManifestError::TooManyChunks { count: u64::from(count) }));
        } else if tail.len() < needed {
            prop_assert_eq!(Manifest::decode(&bytes), Err(ManifestError::Truncated));
        } else {
            let _ = Manifest::decode(&bytes);
        }
    }

    #[test]
    fn lowercase_hex_round_trips(raw in "[0-9a-f]{64}") {
        let id = ChunkId::from_hex(&raw).unwrap();
        prop_assert_eq!(id.to_string(), raw);
    }

    #[test]
    fn uppercase_hex_is_rejected(raw in "[0-9a-f]{10}[A-F][0-9a-fA-F]{53}") {
        let r = ChunkId::from_hex(&raw);
        prop_assert!(matches!(r, Err(HexIdError::NotLowercaseHex { .. })), "got {:?}", r);
    }

    #[test]
    fn wrong_length_hex_is_rejected(raw in "[0-9a-f]{0,63}|[0-9a-f]{65,100}") {
        prop_assert_eq!(ChunkId::from_hex(&raw), Err(HexIdError::WrongLength { len: raw.len() }));
    }

    #[test]
    fn any_string_is_parsed_without_panicking(raw in any::<String>()) {
        if let Ok(id) = ChunkId::from_hex(&raw) {
            prop_assert_eq!(id.to_string(), raw);
        }
    }

    #[test]
    fn multibyte_strings_of_hex_length_are_rejected(s in "[0-9a-f]{62}") {
        // "é" is two bytes, so this is 64 bytes long but not ASCII.
        let raw = format!("é{s}");
        prop_assert_eq!(raw.len(), 64);
        prop_assert_eq!(ChunkId::from_hex(&raw), Err(HexIdError::NotLowercaseHex { index: 0 }));
    }
}
