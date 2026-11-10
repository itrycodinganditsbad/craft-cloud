//! What each zstd level saves and how fast it is, on kinds of data a store holds. Ignored by default; run it with
//! `cargo test -p craft-cloud-store --release --test level_bench -- --ignored --nocapture`.
//!
//! The data is this repository's own text (`docs/` and `crates/`), the programs and libraries of its own build
//! (`CRAFT_CLOUD_BENCH_PROGRAMS`, a directory; skipped when unset), and two synthetic kinds: the float arrays of a
//! 3D mesh and bytes that are already random (what video, archives and photos look like to a compressor). Each is cut
//! into 1 MiB pieces, the chunker's average size. A piece counts as compressed only when it saves the store's default
//! 5%, as the store does, otherwise it is stored plain (plus the format byte). Speeds are one core, in MiB/s of plain
//! data.

use std::path::{Path, PathBuf};
use std::time::Instant;

const PIECE: usize = 1 << 20;
const LEVELS: [i32; 8] = [1, 3, 5, 9, 12, 15, 17, 19];

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn stream(dir: &Path, filter: impl Fn(&Path) -> bool, limit: usize) -> Vec<u8> {
    let mut paths = Vec::new();
    files(dir, &mut paths);
    let mut data = Vec::new();
    for path in paths.iter().filter(|p| filter(p)) {
        if let Ok(bytes) = std::fs::read(path) {
            data.extend_from_slice(&bytes);
        }
        if data.len() >= limit {
            break;
        }
    }
    data.truncate(limit);
    data
}

/// Vertex positions, normals and UVs of a smooth surface as little-endian f32, interleaved like a mesh buffer.
fn mesh(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut i = 0u32;
    while out.len() < len {
        let (u, v) = ((i % 256) as f32 / 255.0, (i / 256 % 256) as f32 / 255.0);
        let (x, y, z) = (u * 2.0 - 1.0, v * 2.0 - 1.0, (u * 6.0).sin() * (v * 6.0).cos() * 0.2);
        for f in [x, y, z, 0.0, 0.0, 1.0, u, v] {
            out.extend_from_slice(&f.to_le_bytes());
        }
        i += 1;
    }
    out.truncate(len);
    out
}

fn random(len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    blake3::Hasher::new().update(b"level bench").finalize_xof().fill(&mut data);
    data
}

struct Row {
    saving: f64,
    compress: f64,
    decompress: f64,
}

fn measure(data: &[u8], level: i32) -> Row {
    let pieces: Vec<&[u8]> = data.chunks(PIECE).collect();
    let (mut logical, mut stored) = (0usize, 0usize);
    let mut compressed_pieces = Vec::new();
    let started = Instant::now();
    for piece in &pieces {
        let out = zstd::bulk::compress(piece, level).unwrap();
        logical += piece.len() + 1;
        // The store's rule: at most 95% of the plain size, else plain.
        if out.len() * 100 <= piece.len() * 95 {
            stored += out.len() + 1;
            compressed_pieces.push((out, piece.len()));
        } else {
            stored += piece.len() + 1;
        }
    }
    let compress = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let mut decoded = 0usize;
    for (out, len) in &compressed_pieces {
        decoded += zstd::bulk::decompress(out, *len).unwrap().len();
    }
    let decompress = started.elapsed().as_secs_f64();
    let mib = data.len() as f64 / (1 << 20) as f64;
    Row {
        saving: 100.0 - stored as f64 * 100.0 / logical as f64,
        compress: mib / compress,
        decompress: if decompress > 0.0 { decoded as f64 / (1 << 20) as f64 / decompress } else { f64::NAN },
    }
}

#[test]
#[ignore = "a measurement, not a check"]
fn levels_on_kinds_of_data() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut kinds: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut text = stream(&repo.join("docs"), |_| true, 24 << 20);
    text.extend(stream(&repo.join("crates"), |p| p.extension().is_some_and(|e| e == "rs" || e == "md" || e == "toml"), 24 << 20));
    kinds.push(("text and code", text));
    if let Some(dir) = std::env::var_os("CRAFT_CLOUD_BENCH_PROGRAMS") {
        let programs = stream(Path::new(&dir), |p| p.extension().is_some_and(|e| e == "exe" || e == "dll" || e == "rlib" || e == "pdb"), 64 << 20);
        if !programs.is_empty() {
            kinds.push(("programs and libraries", programs));
        }
    }
    kinds.push(("3D mesh buffers (synthetic)", mesh(32 << 20)));
    kinds.push(("random bytes (media, archives)", random(16 << 20)));

    println!("| kind | MiB | level | saving | compress MiB/s | decompress MiB/s |");
    println!("|---|---|---|---|---|---|");
    for (name, data) in &kinds {
        for level in LEVELS {
            let row = measure(data, level);
            let decompress = if row.decompress.is_finite() && row.decompress > 0.0 { format!("{:.0}", row.decompress) } else { "-".to_owned() };
            println!("| {name} | {:.0} | {level} | {:.1} % | {:.0} | {decompress} |", data.len() as f64 / (1 << 20) as f64, row.saving, row.compress);
        }
    }
}
