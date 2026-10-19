//! The chunk API as the server serves it, in-process (what the agent's hub reaches over HTTP to upload and download
//! live documents' snapshots): every chunk and manifest checked and recorded as the API does, under the account's
//! storage cap. Only the blob calls; the rest of the API isn't needed here. Included by `#[path]`.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use craft_cloud_proto::api::{CommitResponse, SpaceResponse};
use craft_cloud_proto::{BlobKind, ChangeSet, ChunkId, Manifest, ManifestId, Op, PageCursor, SpaceId};
use craft_cloud_server::AppState;
use craft_cloud_store::BlobKey;
use craft_cloud_sync::transport::{Transport, TransportError};

pub struct ServerBlobs {
    pub state: Arc<AppState>,
    pub space: SpaceId,
}

fn failed(e: impl std::fmt::Display) -> TransportError {
    TransportError::Unavailable(e.to_string())
}

fn meta_error(e: craft_cloud_meta::MetaError) -> TransportError {
    match e {
        craft_cloud_meta::MetaError::QuotaExceeded { used, limit } => TransportError::QuotaExceeded { used, limit },
        craft_cloud_meta::MetaError::MissingChunks(ids) => TransportError::MissingChunks(ids),
        other => failed(other),
    }
}

#[async_trait]
impl Transport for ServerBlobs {
    async fn space(&self) -> Result<SpaceResponse, TransportError> {
        Err(TransportError::Protocol("not here".into()))
    }

    async fn tree(&self, _: Option<PageCursor>) -> Result<ChangeSet, TransportError> {
        Err(TransportError::Protocol("not here".into()))
    }

    async fn changes(&self, _: u64, _: Option<PageCursor>) -> Result<ChangeSet, TransportError> {
        Err(TransportError::Protocol("not here".into()))
    }

    async fn missing_chunks(&self, ids: &[ChunkId]) -> Result<Vec<ChunkId>, TransportError> {
        self.state.meta.missing_chunks(self.space, ids).await.map_err(meta_error)
    }

    async fn put_chunk(&self, id: &ChunkId, data: Bytes) -> Result<(), TransportError> {
        if *blake3::hash(&data).as_bytes() != *id.as_bytes() {
            return Err(TransportError::Protocol("the chunk isn't its ID".into()));
        }
        self.state.meta.check_room(self.space, BlobKind::Chunk, id.as_bytes(), data.len() as u64 + 1).await.map_err(meta_error)?;
        self.state.store.put(BlobKey::chunk(id), &data).await.map_err(failed)?;
        self.state.meta.add_chunk(self.space, id, data.len() as u64 + 1, 0).await.map_err(meta_error)
    }

    async fn get_chunk(&self, id: &ChunkId) -> Result<Bytes, TransportError> {
        if !self.state.meta.missing_chunks(self.space, &[*id]).await.map_err(meta_error)?.is_empty() {
            return Err(TransportError::NotFound);
        }
        self.state.store.get(BlobKey::chunk(id)).await.map_err(failed)
    }

    async fn put_manifest(&self, id: &ManifestId, data: Bytes) -> Result<(), TransportError> {
        let manifest = Manifest::decode(&data).map_err(|e| TransportError::Protocol(e.to_string()))?;
        if manifest.id() != *id {
            return Err(TransportError::Protocol("the manifest isn't its ID".into()));
        }
        let ids: Vec<ChunkId> = manifest.chunks().iter().map(|c| c.id).collect();
        let missing = self.state.meta.missing_chunks(self.space, &ids).await.map_err(meta_error)?;
        if !missing.is_empty() {
            return Err(TransportError::MissingChunks(missing));
        }
        self.state.meta.check_room(self.space, BlobKind::Manifest, id.as_bytes(), data.len() as u64 + 1).await.map_err(meta_error)?;
        self.state.store.put(BlobKey::manifest(id), &data).await.map_err(failed)?;
        self.state.meta.add_manifest(self.space, &manifest, data.len() as u64 + 1, 0).await.map_err(meta_error)
    }

    async fn get_manifest(&self, id: &ManifestId) -> Result<Bytes, TransportError> {
        if !self.state.meta.has_manifest(self.space, id).await.map_err(meta_error)? {
            return Err(TransportError::NotFound);
        }
        self.state.store.get(BlobKey::manifest(id)).await.map_err(failed)
    }

    async fn commit(&self, _: &[Op]) -> Result<CommitResponse, TransportError> {
        Err(TransportError::Protocol("not here".into()))
    }
}
