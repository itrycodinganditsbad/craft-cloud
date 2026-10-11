//! Property tests: hostile input never panics, and what is accepted stays valid.

use craft_cloud_proto::{ChunkId, ConflictStamp, Manifest, Name, conflict_name};
use proptest::prelude::*;

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
    fn any_bytes_decode_or_fail_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(manifest) = Manifest::decode(&bytes) {
            prop_assert_eq!(manifest.encode(), bytes);
        }
    }

    #[test]
    fn any_string_parses_as_a_chunk_id_or_fails_without_panicking(raw in any::<String>()) {
        if let Ok(id) = ChunkId::from_hex(&raw) {
            prop_assert_eq!(id.to_string(), raw);
        }
    }
}
