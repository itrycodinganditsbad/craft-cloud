//! A file's content: which chunker cut it, its size, its whole-file hash and its chunks in order.
//!
//! Manifests travel in a compact binary encoding (a 20 GB file has about 20,000 chunks, too many for a JSON
//! commit body) and are named by the BLAKE3 hash of that encoding. Version metadata (mtime, executable bit,
//! device) is not part of a manifest, so identical content always has the same [`ManifestId`].
//!
//! Encoding, all integers little-endian:
//!
//! ```text
//! "CCM1"                      magic, 4 bytes
//! u8 n, n bytes               chunker parameter set name (ASCII)
//! u64                         file size
//! 32 bytes                    file hash
//! u32 count                   number of chunks
//! count × (32 bytes, u32)     chunk ID and chunk length
//! ```

use crate::hash::{ChunkId, FileHash, ManifestId};
use crate::limits::{MAX_CHUNK_BYTES, MAX_CHUNKS_PER_MANIFEST, MAX_FILE_BYTES};

const MAGIC: &[u8; 4] = b"CCM1";
const MAX_PARAMS_NAME_BYTES: usize = 32;
const CHUNK_ENTRY_BYTES: usize = 36;

/// Why a manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("the manifest ends early")]
    Truncated,
    #[error("the manifest has {extra} bytes after its last chunk")]
    TrailingBytes { extra: usize },
    #[error("not a manifest (bad magic)")]
    BadMagic,
    #[error("bad chunker parameter set name")]
    BadParamsName,
    #[error("the file is {size} bytes; the limit is {MAX_FILE_BYTES}")]
    FileTooLarge { size: u64 },
    #[error("the manifest lists {count} chunks; the limit is {MAX_CHUNKS_PER_MANIFEST}")]
    TooManyChunks { count: u64 },
    #[error("chunk {index} is {len} bytes; chunks are 1 to {MAX_CHUNK_BYTES} bytes")]
    BadChunkLength { index: usize, len: u32 },
    #[error("the chunks add up to {sum} bytes but the file is {size} bytes")]
    SizeMismatch { sum: u64, size: u64 },
}

/// Names the chunker parameter set that cut a file, like `fastcdc-v1`: 1 to 32 bytes of `a-z`, `0-9`, `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkerParamsId(String);

impl ChunkerParamsId {
    pub fn new(name: &str) -> Result<Self, ManifestError> {
        let ok = (1..=MAX_PARAMS_NAME_BYTES).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok { Ok(Self(name.to_owned())) } else { Err(ManifestError::BadParamsName) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One chunk of a file, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef {
    pub id: ChunkId,
    pub len: u32,
}

/// A file's content. Build it with [`Manifest::new`], which validates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    chunker: ChunkerParamsId,
    size: u64,
    file_hash: FileHash,
    chunks: Vec<ChunkRef>,
}

impl Manifest {
    /// Checks the limits and that the chunk lengths add up to `size`. An empty file has no chunks.
    pub fn new(chunker: ChunkerParamsId, size: u64, file_hash: FileHash, chunks: Vec<ChunkRef>) -> Result<Self, ManifestError> {
        if size > MAX_FILE_BYTES {
            return Err(ManifestError::FileTooLarge { size });
        }
        let count = u64::try_from(chunks.len()).unwrap_or(u64::MAX);
        if count > u64::from(MAX_CHUNKS_PER_MANIFEST) {
            return Err(ManifestError::TooManyChunks { count });
        }
        let mut sum: u64 = 0;
        for (index, chunk) in chunks.iter().enumerate() {
            if chunk.len == 0 || chunk.len > MAX_CHUNK_BYTES {
                return Err(ManifestError::BadChunkLength { index, len: chunk.len });
            }
            sum = sum.saturating_add(u64::from(chunk.len));
        }
        if sum != size {
            return Err(ManifestError::SizeMismatch { sum, size });
        }
        Ok(Self { chunker, size, file_hash, chunks })
    }

    pub fn chunker(&self) -> &ChunkerParamsId {
        &self.chunker
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn file_hash(&self) -> &FileHash {
        &self.file_hash
    }

    pub fn chunks(&self) -> &[ChunkRef] {
        &self.chunks
    }

    /// The canonical encoding (module docs).
    pub fn encode(&self) -> Vec<u8> {
        let name = self.chunker.as_str().as_bytes();
        let mut out = Vec::with_capacity(4 + 1 + name.len() + 8 + 32 + 4 + self.chunks.len().saturating_mul(CHUNK_ENTRY_BYTES));
        out.extend_from_slice(MAGIC);
        // Both casts are guaranteed by construction: names are at most 32 bytes, counts at most 2^20.
        out.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(name);
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(self.file_hash.as_bytes());
        out.extend_from_slice(&u32::try_from(self.chunks.len()).unwrap_or(u32::MAX).to_le_bytes());
        for chunk in &self.chunks {
            out.extend_from_slice(chunk.id.as_bytes());
            out.extend_from_slice(&chunk.len.to_le_bytes());
        }
        out
    }

    /// BLAKE3 of [`Manifest::encode`].
    pub fn id(&self) -> ManifestId {
        ManifestId::from_bytes(*blake3::hash(&self.encode()).as_bytes())
    }

    /// Parses and validates an encoding from an untrusted source. Never allocates more than the input
    /// can justify.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestError> {
        let mut r = Reader { rest: bytes };
        if r.take::<4>()? != *MAGIC {
            return Err(ManifestError::BadMagic);
        }
        let [name_len] = r.take::<1>()?;
        let name = r.slice(usize::from(name_len))?;
        let name = std::str::from_utf8(name).map_err(|_| ManifestError::BadParamsName)?;
        let chunker = ChunkerParamsId::new(name)?;
        let size = u64::from_le_bytes(r.take::<8>()?);
        let file_hash = FileHash::from_bytes(r.take::<32>()?);
        let count = u32::from_le_bytes(r.take::<4>()?);
        if count > MAX_CHUNKS_PER_MANIFEST {
            return Err(ManifestError::TooManyChunks { count: u64::from(count) });
        }
        let count = usize::try_from(count).map_err(|_| ManifestError::TooManyChunks { count: u64::from(count) })?;
        // Check the remaining length before allocating, so a claimed count can't force a big allocation.
        let needed = count.checked_mul(CHUNK_ENTRY_BYTES).ok_or(ManifestError::Truncated)?;
        if r.rest.len() < needed {
            return Err(ManifestError::Truncated);
        }
        let mut chunks = Vec::with_capacity(count);
        for _ in 0..count {
            let id = ChunkId::from_bytes(r.take::<32>()?);
            let len = u32::from_le_bytes(r.take::<4>()?);
            chunks.push(ChunkRef { id, len });
        }
        if !r.rest.is_empty() {
            return Err(ManifestError::TrailingBytes { extra: r.rest.len() });
        }
        Self::new(chunker, size, file_hash, chunks)
    }
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn slice(&mut self, n: usize) -> Result<&'a [u8], ManifestError> {
        if self.rest.len() < n {
            return Err(ManifestError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], ManifestError> {
        let head = self.slice(N)?;
        <[u8; N]>::try_from(head).map_err(|_| ManifestError::Truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ChunkerParamsId {
        ChunkerParamsId::new("fastcdc-v1").unwrap()
    }

    fn chunk(byte: u8, len: u32) -> ChunkRef {
        ChunkRef { id: ChunkId::from_bytes([byte; 32]), len }
    }

    fn sample() -> Manifest {
        Manifest::new(params(), 300, FileHash::from_bytes([9; 32]), vec![chunk(1, 100), chunk(2, 200)]).unwrap()
    }

    #[test]
    fn params_names_are_restricted() {
        assert!(ChunkerParamsId::new("fastcdc-v1").is_ok());
        for bad in ["", "FastCDC", "fast cdc", "fastcdc_v1", &"a".repeat(33)] {
            assert_eq!(ChunkerParamsId::new(bad), Err(ManifestError::BadParamsName), "{bad}");
        }
    }

    #[test]
    fn validates_on_construction() {
        let h = FileHash::from_bytes([0; 32]);
        assert!(Manifest::new(params(), 0, h, vec![]).is_ok());
        assert_eq!(Manifest::new(params(), 1, h, vec![]), Err(ManifestError::SizeMismatch { sum: 0, size: 1 }));
        assert_eq!(Manifest::new(params(), 0, h, vec![chunk(1, 0)]), Err(ManifestError::BadChunkLength { index: 0, len: 0 }));
        assert_eq!(
            Manifest::new(params(), u64::from(MAX_CHUNK_BYTES) + 1, h, vec![chunk(1, MAX_CHUNK_BYTES + 1)]),
            Err(ManifestError::BadChunkLength { index: 0, len: MAX_CHUNK_BYTES + 1 })
        );
        assert_eq!(Manifest::new(params(), MAX_FILE_BYTES + 1, h, vec![]), Err(ManifestError::FileTooLarge { size: MAX_FILE_BYTES + 1 }));
    }

    #[test]
    fn encoding_round_trips_and_is_stable() {
        let m = sample();
        let bytes = m.encode();
        assert_eq!(bytes.len(), 4 + 1 + 10 + 8 + 32 + 4 + 2 * 36);
        assert_eq!(&bytes[..4], b"CCM1");
        assert_eq!(Manifest::decode(&bytes).unwrap(), m);
        assert_eq!(m.id(), ManifestId::from_bytes(*blake3::hash(&bytes).as_bytes()));
    }

    #[test]
    fn decode_rejects_damage() {
        let bytes = sample().encode();
        assert_eq!(Manifest::decode(&bytes[..bytes.len() - 1]), Err(ManifestError::Truncated));
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(Manifest::decode(&extra), Err(ManifestError::TrailingBytes { extra: 1 }));
        let mut magic = bytes.clone();
        magic[0] = b'X';
        assert_eq!(Manifest::decode(&magic), Err(ManifestError::BadMagic));
        assert_eq!(Manifest::decode(&[]), Err(ManifestError::Truncated));
    }

    #[test]
    fn a_huge_claimed_count_is_rejected_without_allocating() {
        let mut bytes = Manifest::new(params(), 0, FileHash::from_bytes([0; 32]), vec![]).unwrap().encode();
        let count_at = bytes.len() - 4;
        bytes[count_at..].copy_from_slice(&MAX_CHUNKS_PER_MANIFEST.to_le_bytes());
        assert_eq!(Manifest::decode(&bytes), Err(ManifestError::Truncated));
        bytes[count_at..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Manifest::decode(&bytes), Err(ManifestError::TooManyChunks { count: u64::from(u32::MAX) }));
    }
}
