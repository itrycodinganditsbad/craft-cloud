//! Hard limits every server and agent enforces. Anything from the network is checked against these
//! before it is used or allocated for.

/// Largest chunk, in bytes.
pub const MAX_CHUNK_BYTES: u32 = 4 * 1024 * 1024;
/// Largest file, in bytes (256 GiB).
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024 * 1024;
/// Most chunks one manifest may list: a largest file cut into the smallest chunks `fastcdc-v1` makes
/// (256 KiB), so 1,048,576.
pub const MAX_CHUNKS_PER_MANIFEST: u32 = 1024 * 1024;
/// Largest encoded manifest, in bytes: magic (4), name length (1), longest params name (32), size (8), file
/// hash (32), chunk count (4) and [`MAX_CHUNKS_PER_MANIFEST`] entries of 36 bytes. The `PUT /manifests` body
/// is capped with it.
pub const MAX_MANIFEST_BYTES: usize = 4 + 1 + 32 + 8 + 32 + 4 + (MAX_CHUNKS_PER_MANIFEST as usize) * 36;
/// Longest file or directory name, in UTF-8 bytes after NFC normalisation.
pub const MAX_NAME_BYTES: usize = 255;
/// Most operations in one commit batch.
pub const MAX_BATCH_OPS: usize = 1000;
/// Largest commit batch body, in bytes of JSON.
pub const MAX_BATCH_JSON_BYTES: usize = 1024 * 1024;
/// Deepest allowed folder nesting (the root is depth 0). Deeper creates and moves are refused with TooDeep.
pub const MAX_TREE_DEPTH: usize = 256;
