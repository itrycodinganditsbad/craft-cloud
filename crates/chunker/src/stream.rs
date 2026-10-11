//! Chunking a stream, with a hash per chunk and one for the whole file.

use std::io::{self, Read};

use crate::cdc::Chunker;

/// One chunk, handed to the callback while its bytes are still in the buffer.
#[derive(Debug)]
pub struct Chunk<'a> {
    /// Where the chunk starts in the file.
    pub offset: u64,
    pub data: &'a [u8],
    /// BLAKE3 of `data`.
    pub hash: [u8; 32],
}

/// What [`chunk_reader`] keeps of each chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkInfo {
    pub hash: [u8; 32],
    pub len: u32,
}

/// A whole file, chunked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub size: u64,
    /// BLAKE3 of the whole file.
    pub file_hash: [u8; 32],
    pub chunks: Vec<ChunkInfo>,
}

/// Reads `reader` to the end, cutting it with `chunker` and calling `on_chunk` for every chunk in order.
///
/// Holds at most one maximum-size chunk (`max` bytes) in memory. The boundaries are the same however the reader
/// splits its reads (short reads and `Interrupted` are handled). Stops at the first error from the reader
/// or the callback.
pub fn chunk_reader<R, E, F>(chunker: &Chunker, mut reader: R, mut on_chunk: F) -> Result<Summary, E>
where
    R: Read,
    E: From<io::Error>,
    F: FnMut(Chunk<'_>) -> Result<(), E>,
{
    let max = chunker.params().max;
    let mut buf: Vec<u8> = Vec::with_capacity(max);
    let mut file_hasher = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut offset: u64 = 0;
    let mut eof = false;
    loop {
        if !eof {
            eof = fill(&mut reader, &mut buf, max)?;
        }
        let cut = chunker.cut_point(&buf);
        if cut == 0 {
            break;
        }
        let data = buf.get(..cut).ok_or_else(|| io::Error::other("chunk cut past the buffer"))?;
        let hash = *blake3::hash(data).as_bytes();
        file_hasher.update(data);
        on_chunk(Chunk { offset, data, hash })?;
        let len = u32::try_from(cut).map_err(|_| io::Error::other("chunk longer than 4 GiB"))?;
        chunks.push(ChunkInfo { hash, len });
        offset = offset.checked_add(u64::from(len)).ok_or_else(|| io::Error::other("file longer than 2^64 bytes"))?;
        buf.drain(..cut);
    }
    Ok(Summary { size: offset, file_hash: *file_hasher.finalize().as_bytes(), chunks })
}

/// Reads until `buf` holds `want` bytes or the reader ends. Returns whether it ended.
///
/// The spare area is zeroed once per call, not once per `read`; `buf` is cut back to the bytes actually read
/// on every exit.
fn fill<R: Read>(reader: &mut R, buf: &mut Vec<u8>, want: usize) -> io::Result<bool> {
    let mut filled = buf.len();
    if filled >= want {
        return Ok(false);
    }
    buf.resize(want, 0);
    let result = loop {
        let Some(spare) = buf.get_mut(filled..).filter(|spare| !spare.is_empty()) else {
            break Ok(false);
        };
        let spare_len = spare.len();
        match reader.read(spare) {
            Ok(0) => break Ok(true),
            Ok(n) => filled = filled.saturating_add(n.min(spare_len)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    buf.truncate(filled);
    result
}
