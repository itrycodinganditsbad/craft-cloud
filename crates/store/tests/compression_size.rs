//! How much compression saves on disk, measured on this repository's own text (`docs/` and `crates/`). Ignored by
//! default; run it with
//! `cargo test -p craft-cloud-store --release --test compression_size -- --ignored --nocapture`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use craft_cloud_proto::ChunkId;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::{BlobKey, Compression, Store};

/// Every file under `dir`, in a stable order.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The sizes of every file under `dir`, added up.
fn bytes_under(dir: &Path) -> u64 {
    let mut all = Vec::new();
    files(dir, &mut all);
    all.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not a check"]
async fn compression_saves_space_on_text() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    files(&repo.join("docs"), &mut sources);
    files(&repo.join("crates"), &mut sources);
    // One stream of all the files, cut into 1 MiB blobs (the chunker's average size).
    let mut text = Vec::new();
    for path in &sources {
        text.extend_from_slice(&std::fs::read(path).unwrap());
    }
    let blobs: Vec<&[u8]> = text.chunks(1 << 20).collect();
    let mib = text.len() as f64 / f64::from(1 << 20);

    for compression in [Compression::None, Compression::Zstd] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(DiskBackend::new(dir.path()).unwrap()).with_compression(compression);
        let keys: Vec<BlobKey> = blobs.iter().map(|b| BlobKey::chunk(&ChunkId::from_bytes(*blake3::hash(b).as_bytes()))).collect();
        let started = Instant::now();
        for (key, blob) in keys.iter().zip(&blobs) {
            store.put(*key, blob).await.unwrap();
        }
        let put = started.elapsed();
        let started = Instant::now();
        for (key, blob) in keys.iter().zip(&blobs) {
            assert_eq!(&store.get(*key).await.unwrap()[..], *blob);
        }
        let get = started.elapsed();
        let on_disk = bytes_under(dir.path());
        println!(
            "{compression:?}: {} files, {} bytes in {} blobs, {on_disk} bytes on disk ({:.1}%), put {:.2} ms/MiB, get {:.2} ms/MiB",
            sources.len(),
            text.len(),
            blobs.len(),
            on_disk as f64 * 100.0 / text.len() as f64,
            put.as_secs_f64() * 1000.0 / mib,
            get.as_secs_f64() * 1000.0 / mib,
        );
    }
}
