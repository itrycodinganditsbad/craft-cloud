//! How the engine talks to the server. The agent implements this over HTTP (`/api/v1`); tests and the
//! simulation implement it in-process over the server's own metadata and blob stores.

use async_trait::async_trait;
use bytes::Bytes;
use craft_cloud_proto::api::{CommitResponse, SpaceResponse};
use craft_cloud_proto::{ChangeSet, ChunkId, Conflict, ManifestId, Op, PageCursor};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The batch was refused; nothing applied.
    #[error("{} operation(s) conflict", .0.len())]
    Rejected(Vec<Conflict>),
    /// A manifest lists chunks the space doesn't have.
    #[error("{} chunk(s) missing", .0.len())]
    MissingChunks(Vec<ChunkId>),
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
}
