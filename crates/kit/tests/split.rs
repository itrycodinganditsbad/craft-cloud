//! The split at its edge: a change whose estimate is exactly [`SPLIT_BYTES`] is one commit; one byte more is two. Every
//! update stays at or under the split size.

mod common;

use common::{App, Notes};
use craft_cloud_kit::raw::Replica;
use craft_cloud_kit::{OP_OVERHEAD, SPLIT_BYTES};

/// The estimate of adding a note is its row's ID in the list (16 bytes) and its cell (65-byte key and the text), each
/// with the per-operation overhead: two notes of `len` bytes make the split size exactly.
fn note_len_for(total: usize) -> usize {
    (total - 2 * (16 + OP_OVERHEAD) - 2 * (65 + OP_OVERHEAD)) / 2
}

#[test]
fn a_change_at_the_split_size_is_one_commit_and_one_byte_more_is_two() {
    assert_eq!(SPLIT_BYTES % 2, 0);
    for (extra, commits) in [(0, 1), (1, 2)] {
        let mut r = Replica::default();
        let base = Notes::new("t", &["first"]);
        r.write::<App>(None, &base).unwrap();
        let len = note_len_for(SPLIT_BYTES);
        let one = base.with_item(1, &"n".repeat(len));
        let two = one.with_item(2, &"m".repeat(len + extra));
        let next = Notes { hint: Some(two.items.iter().skip(1).map(|(id, _)| *id).collect()), ..two };
        let (updates, made) = r.write::<App>(Some(&base), &next).unwrap();
        assert_eq!(made, commits, "{extra} byte(s) over");
        assert!(updates.iter().all(|u| u.len() <= SPLIT_BYTES), "{:?}", updates.iter().map(Vec::len).collect::<Vec<_>>());
    }
}
