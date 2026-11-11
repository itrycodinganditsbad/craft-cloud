//! How the engine talks to the server. The agent implements this over HTTP (`/api/v1`); tests and the
//! simulation implement it in-process over the server's own metadata and blob stores.

use async_trait::async_trait;
use bytes::Bytes;
use craft_cloud_proto::api::{CommitResponse, SpaceResponse};
use craft_cloud_proto::{ChangeSet, ChunkId, Conflict, ManifestId, NodeId, Op, PageCursor, SaveRecord, VersionId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The batch was refused; nothing applied.
    #[error("{} operation(s) conflict", .0.len())]
    Rejected(Vec<Conflict>),
    /// A manifest lists chunks the space doesn't have.
    #[error("{} chunk(s) missing", .0.len())]
    MissingChunks(Vec<ChunkId>),
    /// A chunk or manifest would take the account over what the server lets it store; nothing was stored. `used` is
    /// what it stores now, `limit` what it may. The engine skips the file while room is left and stops the pass once
    /// less than a chunk's worth is ([`Config::quota_floor`](crate::Config::quota_floor)).
    #[error("the account's storage on the server is full or nearly ({used} of {limit} bytes used); the admin can allow more")]
    QuotaExceeded { used: u64, limit: u64 },
    /// The server itself is full and refuses every new upload (`server_full`; the admin set `refuse_when_full`). Not the
    /// account's quota: no file would fit, whatever its size, so the engine ends the pass and the agent pauses until the
    /// admin has freed space or raised the capacity.
    #[error("The server is full; the admin needs to free space or raise its capacity")]
    ServerFull,
    /// The server refused a write because the account's role in this space doesn't allow one (`403 read_only`: a
    /// viewer, or a space that was archived). Nothing was stored. The engine switches the space to read-only mode
    /// ([`Engine::set_read_only`](crate::Engine::set_read_only)) and keeps every local change.
    #[error("this space is read-only for you: your role in it doesn't allow changes")]
    ReadOnly,
    /// The change feed can't continue from our sequence number; list the whole tree.
    #[error("the change feed needs a full listing")]
    ResyncRequired,
    #[error("not found")]
    NotFound,
    /// The device was signed out or revoked.
    #[error("not signed in")]
    Unauthorized,
    /// Try again later: network down, server busy, timeouts.
    #[error("server unavailable: {0}")]
    Unavailable(String),
    /// The server answered something this client doesn't understand.
    #[error("protocol error: {0}")]
    Protocol(String),
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn space(&self) -> Result<SpaceResponse, TransportError>;
    async fn tree(&self, after: Option<PageCursor>) -> Result<ChangeSet, TransportError>;
    async fn changes(&self, since: u64, after: Option<PageCursor>) -> Result<ChangeSet, TransportError>;
    async fn missing_chunks(&self, ids: &[ChunkId]) -> Result<Vec<ChunkId>, TransportError>;
    async fn put_chunk(&self, id: &ChunkId, data: Bytes) -> Result<(), TransportError>;
    async fn get_chunk(&self, id: &ChunkId) -> Result<Bytes, TransportError>;
    async fn put_manifest(&self, id: &ManifestId, data: Bytes) -> Result<(), TransportError>;
    async fn get_manifest(&self, id: &ManifestId) -> Result<Bytes, TransportError>;
    async fn commit(&self, ops: &[Op]) -> Result<CommitResponse, TransportError>;
    /// Asks the server to add to this space the chunks of `ids` that another space (the one the transport was told to take
    /// from) has and the account may read, so that their bytes need not be sent again. Returns the ids this space still lacks,
    /// which the engine uploads. The default claims nothing: every id is still missing. An answer that is a refusal of the claim
    /// itself (no such space, no such route) also claims nothing and is no error; a full quota is `QuotaExceeded`.
    async fn claim_chunks(&self, ids: &[ChunkId]) -> Result<Vec<ChunkId>, TransportError> {
        Ok(ids.to_vec())
    }
    /// Adds a save record to `node`'s version `version`, which went up without one (`POST /saves`, appended and
    /// idempotent). A server from before late records: `NotFound`, as this default says.
    async fn attach_save(&self, node: NodeId, version: VersionId, save: &SaveRecord) -> Result<(), TransportError> {
        let _ = (node, version, save);
        Err(TransportError::NotFound)
    }
}
