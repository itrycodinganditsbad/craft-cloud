//! `chunk_reader` behaviour, on a small parameter set so the tests are quick.

mod common;

use std::io;

use common::{Dribble, noise};
use craft_cloud_chunker::{Chunker, Params, Summary, chunk_reader};
use proptest::prelude::*;

const SMALL: Params =
    Params { name: "test-small", gear_context: "craft-cloud test small", min: 64, avg: 256, max: 1024, bits_before_avg: 9, bits_after_avg: 7 };

fn chunk_all(chunker: &Chunker, data: &[u8]) -> Summary {
    chunk_reader::<_, io::Error, _>(chunker, data, |_| Ok(())).unwrap()
}

/// Reference: cut the whole buffer in memory with `cut_point`.
fn lengths_in_memory(chunker: &Chunker, data: &[u8]) -> Vec<u32> {
    let mut rest = data;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let cut = chunker.cut_point(rest);
        out.push(u32::try_from(cut).unwrap());
        rest = &rest[cut..];
    }
    out
}

#[test]
fn empty_input_has_no_chunks_and_the_empty_hash() {
    let c = Chunker::new(SMALL).unwrap();
    let s = chunk_all(&c, &[]);
    assert_eq!(s.size, 0);
    assert!(s.chunks.is_empty());
    assert_eq!(s.file_hash, *blake3::hash(b"").as_bytes());
}

#[test]
fn matches_cutting_the_whole_buffer_in_memory() {
    let c = Chunker::new(SMALL).unwrap();
    let data = noise(100_000, 7);
    let s = chunk_all(&c, &data);
    let lens: Vec<u32> = s.chunks.iter().map(|ch| ch.len).collect();
    assert_eq!(lens, lengths_in_memory(&c, &data));
    assert_eq!(s.size, 100_000);
    assert_eq!(s.file_hash, *blake3::hash(&data).as_bytes());
}

#[test]
fn callback_sees_every_chunk_in_order_with_its_hash() {
    let c = Chunker::new(SMALL).unwrap();
    let data = noise(20_000, 8);
    let mut rebuilt = Vec::new();
    let s = chunk_reader::<_, io::Error, _>(&c, data.as_slice(), |chunk| {
        assert_eq!(chunk.offset, rebuilt.len() as u64);
        assert_eq!(chunk.hash, *blake3::hash(chunk.data).as_bytes());
        rebuilt.extend_from_slice(chunk.data);
        Ok(())
    })
    .unwrap();
    assert_eq!(rebuilt, data);
    assert!(s.chunks.len() > 10);
}

#[test]
fn short_reads_and_interruptions_do_not_move_boundaries() {
    let c = Chunker::new(SMALL).unwrap();
    let data = noise(30_000, 9);
    let whole = chunk_all(&c, &data);
    for step in [1, 7, 63, 64, 65, 1000, 1025] {
        let dribbled = chunk_reader::<_, io::Error, _>(&c, Dribble { data: &data, step, calls: 0 }, |_| Ok(())).unwrap();
        assert_eq!(dribbled, whole, "step {step}");
    }
}

#[test]
fn a_callback_error_stops_the_read() {
    let c = Chunker::new(SMALL).unwrap();
    let data = noise(20_000, 10);
    let mut seen = 0;
    let err = chunk_reader::<_, io::Error, _>(&c, data.as_slice(), |_| {
        seen += 1;
        if seen == 3 { Err(io::Error::other("upload failed")) } else { Ok(()) }
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "upload failed");
    assert_eq!(seen, 3);
}

#[test]
fn a_reader_error_is_returned() {
    struct Broken;
    impl io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("disk gone"))
        }
    }
    let c = Chunker::new(SMALL).unwrap();
    let err = chunk_reader::<_, io::Error, _>(&c, Broken, |_| Ok(())).unwrap_err();
    assert_eq!(err.to_string(), "disk gone");
}

#[test]
fn an_insert_in_the_middle_only_changes_nearby_chunks() {
    let c = Chunker::new(SMALL).unwrap();
    let data = noise(200_000, 11);
    let mut edited = data.clone();
    edited.splice(100_000..100_000, *b"inserted by an edit");
    let before: std::collections::HashSet<[u8; 32]> = chunk_all(&c, &data).chunks.iter().map(|ch| ch.hash).collect();
    let after = chunk_all(&c, &edited);
    let new = after.chunks.iter().filter(|ch| !before.contains(&ch.hash)).count();
    assert!(new <= 3, "{new} of {} chunks changed", after.chunks.len());
}

#[test]
fn a_delete_in_the_middle_only_changes_nearby_chunks() {
    let c = Chunker::new(SMALL).unwrap();
    let data = noise(200_000, 11);
    let mut edited = data.clone();
    edited.drain(100_000..100_019);
    let before: std::collections::HashSet<[u8; 32]> = chunk_all(&c, &data).chunks.iter().map(|ch| ch.hash).collect();
    let after = chunk_all(&c, &edited);
    let new = after.chunks.iter().filter(|ch| !before.contains(&ch.hash)).count();
    assert!(new <= 3, "{new} of {} chunks changed", after.chunks.len());
}

proptest! {
    #[test]
    fn any_input_is_cut_within_limits_and_reassembles(data in proptest::collection::vec(any::<u8>(), 0..6000), step in 1usize..2000) {
        let c = Chunker::new(SMALL).unwrap();
        let mut rebuilt = Vec::new();
        let s = chunk_reader::<_, io::Error, _>(&c, Dribble { data: &data, step, calls: 0 }, |chunk| {
            rebuilt.extend_from_slice(chunk.data);
            Ok(())
        }).unwrap();
        prop_assert_eq!(&rebuilt, &data);
        prop_assert_eq!(s.size, data.len() as u64);
        let n = s.chunks.len();
        for (i, ch) in s.chunks.iter().enumerate() {
            prop_assert!(ch.len as usize <= SMALL.max);
            if i + 1 < n {
                prop_assert!(ch.len as usize > SMALL.min);
            }
        }
    }
}
