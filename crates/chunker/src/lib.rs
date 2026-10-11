//! Content-defined chunking for Craft Cloud: FastCDC with fixed, named parameter sets, and BLAKE3 hashes.
//!
//! Chunk boundaries depend only on the bytes near them, so an edit in the middle of a big file changes
//! only the chunks around the edit. Boundaries for a parameter set must never change: every version
//! records the set that cut it, and dedup only works within one set. A change means a new set with a new
//! name, never an edit to an existing one. `tests/golden.rs` pins `fastcdc-v1`.
//!
//! Clean room: written from the FastCDC papers (Xia et al., USENIX ATC 2016 and IEEE TPDS 2020), not from
//! any existing implementation.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod cdc;
// mod stream;

pub use cdc::{Chunker, FASTCDC_V1, Params, ParamsError};
// pub use stream::{Chunk, ChunkInfo, Summary, chunk_reader};
