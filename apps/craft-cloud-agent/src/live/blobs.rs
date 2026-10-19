//! Snapshots of live documents travel as files' content does, never in a frame: cut into chunks named by a manifest
//! ([`cut`]), uploaded through the chunk API before the frame that names the manifest ([`upload`]: with that API's
//! body limits, hash checks and the account's storage cap), and downloaded the same way ([`download`]: every chunk
//! checked against its ID, the whole against the manifest's size and hash). The replica cuts and names them
//! ([`super::replica::Replica::take_uploads`], [`super::replica::Replica::take_fetches`]); the hub moves the bytes.

use std::collections::BTreeSet;

use bytes::Bytes;
use craft_cloud_chunker::{Chunker, FASTCDC_V1, chunk_reader};
use craft_cloud_proto::live::MAX_SNAPSHOT_BYTES;
use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, DocId, FileHash, Manifest, ManifestId};
use craft_cloud_sync::transport::{Transport, TransportError};

/// Content's chunks, each once, with their IDs.
pub type Chunks = Vec<(ChunkId, Vec<u8>)>;

/// How many chunk IDs one "which are missing" question names.
const MISSING_QUERY_IDS: usize = 1000;

/// A snapshot to upload before a frame names it: its manifest and its chunks (each once).
#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    /// The document it is a snapshot of (frames of that document wait for it).
    pub doc: DocId,
    pub manifest: Manifest,
    pub chunks: Vec<(ChunkId, Vec<u8>)>,
}

impl std::fmt::Debug for Upload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Upload({}, manifest {}, {} bytes)", self.doc, self.manifest.id(), self.manifest.size())
    }
}

impl Upload {
    pub fn id(&self) -> ManifestId {
        self.manifest.id()
    }

    /// The snapshot again, from its chunks (for tests that look inside).
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in self.manifest.chunks() {
            if let Some((_, data)) = self.chunks.iter().find(|(id, _)| *id == chunk.id) {
                out.extend_from_slice(data);
            }
        }
        out
    }
}

/// `bytes` (a snapshot of `doc`) cut into chunks as a file's content is, with the manifest that names them.
pub fn cut(doc: DocId, bytes: &[u8]) -> Result<Upload, String> {
    if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(format!("a snapshot is 1 byte to {MAX_SNAPSHOT_BYTES} bytes; this one is {}", bytes.len()));
    }
    let (manifest, chunks) = cut_content(bytes)?;
    Ok(Upload { doc, manifest, chunks })
}

/// `bytes` (not empty) cut into chunks as a file's content is (`fastcdc-v1`), each chunk once, with the manifest that
/// names them: the same bytes always give the same manifest. Snapshots and apps' blobs both go up this way.
pub fn cut_content(bytes: &[u8]) -> Result<(Manifest, Chunks), String> {
    let chunker = Chunker::new(FASTCDC_V1).map_err(|e| e.to_string())?;
    let mut chunks: Vec<(ChunkId, Vec<u8>)> = Vec::new();
    let mut seen = BTreeSet::new();
    let summary = chunk_reader::<_, std::io::Error, _>(&chunker, bytes, |chunk| {
        let id = ChunkId::from_bytes(chunk.hash);
        if seen.insert(id) {
            chunks.push((id, chunk.data.to_vec()));
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    let refs = summary.chunks.iter().map(|c| ChunkRef { id: ChunkId::from_bytes(c.hash), len: c.len }).collect();
    let params = ChunkerParamsId::new(FASTCDC_V1.name).map_err(|e| e.to_string())?;
    let manifest = Manifest::new(params, summary.size, FileHash::from_bytes(summary.file_hash), refs).map_err(|e| e.to_string())?;
    Ok((manifest, chunks))
}

/// Uploads a snapshot: the chunks the server lacks, then the manifest (and, if the server says it lacks some of them
/// after all, those once more).
pub async fn upload<T: Transport + ?Sized>(transport: &T, up: &Upload) -> Result<(), TransportError> {
    upload_content(transport, &up.manifest, &up.chunks).await
}

/// Uploads content cut by [`cut_content`]: the chunks the server lacks, then the manifest.
pub async fn upload_content<T: Transport + ?Sized>(transport: &T, manifest: &Manifest, chunks: &[(ChunkId, Vec<u8>)]) -> Result<(), TransportError> {
    let id = manifest.id();
    let ids: Vec<ChunkId> = chunks.iter().map(|(id, _)| *id).collect();
    let mut missing = BTreeSet::new();
    for batch in ids.chunks(MISSING_QUERY_IDS) {
        missing.extend(transport.missing_chunks(batch).await?);
    }
    for (id, data) in chunks {
        if missing.contains(id) {
            transport.put_chunk(id, Bytes::from(data.clone())).await?;
        }
    }
    let encoded = Bytes::from(manifest.encode());
    match transport.put_manifest(&id, encoded.clone()).await {
        Err(TransportError::MissingChunks(lost)) => {
            for lost_id in lost {
                let (_, data) = chunks
                    .iter()
                    .find(|(c, _)| *c == lost_id)
                    .ok_or_else(|| TransportError::Protocol(format!("the server asked for chunk {lost_id}, which the content doesn't have")))?;
                transport.put_chunk(&lost_id, Bytes::from(data.clone())).await?;
            }
            transport.put_manifest(&id, encoded).await
        }
        other => other,
    }
}

/// Downloads the snapshot `id` names: its manifest (checked against `id`), then each chunk (checked against its ID and
/// length), the whole checked against the manifest's size (at most [`MAX_SNAPSHOT_BYTES`]) and hash.
pub async fn download<T: Transport + ?Sized>(transport: &T, id: &ManifestId) -> Result<Vec<u8>, TransportError> {
    download_content(transport, id, MAX_SNAPSHOT_BYTES as u64).await
}

/// Downloads the content `id` names, as [`download`], of at most `max` bytes.
pub async fn download_content<T: Transport + ?Sized>(transport: &T, id: &ManifestId, max: u64) -> Result<Vec<u8>, TransportError> {
    let bad = |what: &str| TransportError::Protocol(format!("the content {id} doesn't read: {what}"));
    let manifest = Manifest::decode(&transport.get_manifest(id).await?).map_err(|e| bad(&e.to_string()))?;
    if manifest.id() != *id {
        return Err(bad("its manifest isn't the one asked for"));
    }
    if manifest.size() == 0 || manifest.size() > max {
        return Err(bad("its size is over the limit"));
    }
    let mut out = Vec::with_capacity(usize::try_from(manifest.size()).unwrap_or(0));
    for chunk in manifest.chunks() {
        let data = transport.get_chunk(&chunk.id).await?;
        if data.len() != chunk.len as usize || ChunkId::from_bytes(*blake3::hash(&data).as_bytes()) != chunk.id {
            return Err(bad("a chunk isn't the one named"));
        }
        out.extend_from_slice(&data);
    }
    if out.len() as u64 != manifest.size() || FileHash::from_bytes(*blake3::hash(&out).as_bytes()) != *manifest.file_hash() {
        return Err(bad("the whole isn't the one named"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot is cut as a file is (the same chunks for the same bytes), each chunk once, and reads back whole.
    #[test]
    fn a_snapshot_is_cut_into_chunks_and_a_manifest() {
        let doc = DocId::generate();
        let bytes: Vec<u8> = (0..9_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let up = cut(doc, &bytes).unwrap();
        assert!(up.chunks.len() > 1, "{} chunks", up.chunks.len());
        assert_eq!(up.manifest.size(), bytes.len() as u64);
        assert_eq!(up.bytes(), bytes);
        assert_eq!(cut(doc, &bytes).unwrap().id(), up.id(), "the same manifest every time");
        let repeated = vec![7u8; 9_000_000];
        let up = cut(doc, &repeated).unwrap();
        assert!(up.chunks.len() < up.manifest.chunks().len(), "a chunk the snapshot has twice goes up once");
        assert_eq!(up.bytes(), repeated);
        assert!(cut(doc, &[]).is_err());
        assert!(cut(doc, &vec![1; MAX_SNAPSHOT_BYTES + 1]).is_err());
    }
}
