//! Pins `fastcdc-v1` forever. If one of these fails, the change broke dedup for every stored file: undo
//! it, or add a new parameter set with a new name instead.
//!
//! The expected values were recorded once with `cargo test -p craft-cloud-chunker --test golden -- --ignored --nocapture`.

mod common;

use std::io;

use common::noise;
use craft_cloud_chunker::{Chunker, FASTCDC_V1, chunk_reader};

const GOLDEN_SEED: u64 = 0x0123_4567_89ab_cdef;
const GOLDEN_LEN: usize = 24 * 1024 * 1024;

fn gear_table_hash(c: &Chunker) -> String {
    let mut bytes = Vec::with_capacity(256 * 8);
    for word in c.gear() {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    blake3::hash(&bytes).to_hex().to_string()
}

fn golden_lengths(c: &Chunker) -> Vec<u32> {
    let data = noise(GOLDEN_LEN, GOLDEN_SEED);
    chunk_reader::<_, io::Error, _>(c, data.as_slice(), |_| Ok(())).unwrap().chunks.iter().map(|ch| ch.len).collect()
}

#[test]
#[ignore = "prints the values to paste into the constants below"]
fn print_golden_values() {
    let c = Chunker::new(FASTCDC_V1).unwrap();
    println!("GEAR_FIRST = {:#018x}", c.gear()[0]);
    println!("GEAR_TABLE_BLAKE3 = {}", gear_table_hash(&c));
    println!("LENGTHS = {:?}", golden_lengths(&c));
}

const GEAR_FIRST: u64 = 0x159065feca9a0d46;
const GEAR_TABLE_BLAKE3: &str = "5fc1ed2087058701d2fd6372367747b6c3d0b25f532e4a3a8d35ad02298f3022";
const LENGTHS: [u32; 23] = [
    424416, 1622747, 1204543, 1660689, 1817166, 1201988, 1208594, 448390, 1151589, 1246460, 576674, 1112312, 584795, 1080959, 656852, 1708288,
    1098386, 1125835, 638136, 466381, 1967879, 1608442, 554303,
];

#[test]
fn fastcdc_v1_parameters_are_unchanged() {
    assert_eq!(FASTCDC_V1.name, "fastcdc-v1");
    assert_eq!(FASTCDC_V1.gear_context, "craft-cloud 2026-10-11 fastcdc-v1 gear table");
    assert_eq!((FASTCDC_V1.min, FASTCDC_V1.avg, FASTCDC_V1.max), (256 * 1024, 1024 * 1024, 4 * 1024 * 1024));
    assert_eq!((FASTCDC_V1.bits_before_avg, FASTCDC_V1.bits_after_avg), (21, 19));
}

#[test]
fn fastcdc_v1_gear_table_is_unchanged() {
    let c = Chunker::new(FASTCDC_V1).unwrap();
    assert_eq!(c.gear()[0], GEAR_FIRST);
    assert_eq!(gear_table_hash(&c), GEAR_TABLE_BLAKE3);
}

#[test]
fn fastcdc_v1_boundaries_are_unchanged() {
    let c = Chunker::new(FASTCDC_V1).unwrap();
    let lengths = golden_lengths(&c);
    assert_eq!(lengths, LENGTHS);
    assert_eq!(lengths.iter().map(|&l| u64::from(l)).sum::<u64>(), GOLDEN_LEN as u64);
}

#[test]
fn fastcdc_v1_averages_about_one_mebibyte() {
    let c = Chunker::new(FASTCDC_V1).unwrap();
    let data = noise(64 * 1024 * 1024, 42);
    let s = chunk_reader::<_, io::Error, _>(&c, data.as_slice(), |_| Ok(())).unwrap();
    let avg = s.size / s.chunks.len() as u64;
    assert!((768 * 1024..=1536 * 1024).contains(&avg), "average chunk {avg} bytes over {} chunks", s.chunks.len());
}
