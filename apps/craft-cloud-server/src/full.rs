//! A server that refuses new uploads when it is full (`[storage] refuse_when_full`, off by default).
//!
//! An upload (a chunk or a manifest, which is also how a live document's snapshot goes up) is refused with `507
//! server_full` when either holds, after counting the bytes it would add:
//!
//! - the store's filesystem would be left with less free space than `min_free` (a disk store only; S3 has no free
//!   space to measure), or
//! - the bytes the server stores would pass `capacity` (a disk or S3 store, when `capacity` is set).
//!
//! Reads, deletes, trash, garbage collection, live updates (they live in the database, not the blob store) and the
//! admin's calls are never refused, so an admin can free space while the server is full.
//!
//! The numbers are cached: they are read again at most every [`REFRESH`], and the bytes the server wrote itself since
//! are added in between, so the hot path costs a lock and a few additions. The state flips in the log at `warn` when
//! refusing starts and stops, with the reason and no names.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use craft_cloud_meta::Meta;
use craft_cloud_proto::api::ServerRoom;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::ApiError;
use crate::space::StorageInfo;

/// The free space a disk store keeps when `min_free` is not set.
pub const DEFAULT_MIN_FREE: u64 = 1 << 30;

/// How long the stored total and the free space count as current.
pub const REFRESH: Duration = Duration::from_secs(5);

/// What the admin configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The most the store may hold.
    pub capacity: Option<u64>,
    /// The free space to keep on the store's filesystem (disk only; `None`: not checked).
    pub min_free: Option<u64>,
}

/// Total and free bytes of the store's filesystem, `(None, None)` when unknown. Blocking. The tests' seam: they
/// replace the filesystem with numbers they choose.
pub type Probe = Arc<dyn Fn() -> (Option<u64>, Option<u64>) + Send + Sync>;

/// Why the server is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Capacity,
    FreeSpace,
}

#[derive(Default)]
struct Cache {
    read_at: Option<Instant>,
    stored: u64,
    total: Option<u64>,
    free: Option<u64>,
}

pub struct FullGuard {
    policy: Policy,
    probe: Probe,
    /// How long the cached numbers count as current ([`REFRESH`]; tests use others).
    refresh: Duration,
    cache: Mutex<Cache>,
    /// Bytes this server stored since the numbers in the cache were read.
    written: AtomicU64,
    refusing: AtomicBool,
    /// The size of the last upload that was refused, which [`FullGuard::status`] asks about again.
    refused: AtomicU64,
}

impl FullGuard {
    /// A guard for the store `info` describes: the filesystem of its directory is measured when there is one.
    pub fn new(policy: Policy, info: &StorageInfo) -> Self {
        let info = info.clone();
        Self::with_probe(policy, Arc::new(move || info.store_filesystem()))
    }

    /// A guard that reads the filesystem through `probe` (tests).
    pub fn with_probe(policy: Policy, probe: Probe) -> Self {
        Self {
            policy,
            probe,
            refresh: REFRESH,
            cache: Mutex::new(Cache::default()),
            written: AtomicU64::new(0),
            refusing: AtomicBool::new(false),
            refused: AtomicU64::new(0),
        }
    }

    /// The guard reading the numbers again after `refresh` instead of [`REFRESH`] (tests).
    pub fn with_refresh(mut self, refresh: Duration) -> Self {
        self.refresh = refresh;
        self
    }

    /// Whether the server refused the last upload it was asked about, or would refuse one now ([`FullGuard::status`]).
    pub fn refusing(&self) -> bool {
        self.refusing.load(Ordering::Relaxed)
    }

    /// Refuses an upload of `incoming` bytes when the server is full.
    pub async fn check(&self, meta: &Meta, incoming: u64) -> Result<(), ApiError> {
        match self.evaluate(meta, incoming).await? {
            None => Ok(()),
            Some(room) => Err(ApiError::server_full(room)),
        }
    }

    /// Whether the server is refusing uploads now (for the admin's overview): it would still refuse the upload it last
    /// refused, or, if it has refused none, one of a single byte. Reads the numbers if they are stale, so it turns false
    /// soon after space was freed even when no upload has come since.
    pub async fn status(&self, meta: &Meta) -> Result<bool, ApiError> {
        let asked = if self.refusing() { self.refused.load(Ordering::Relaxed).max(1) } else { 1 };
        Ok(self.evaluate(meta, asked).await?.is_some())
    }

    /// Counts bytes the server stored, until the numbers are read again.
    pub fn note_written(&self, bytes: u64) {
        self.written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// `Some(room)` when an upload of `incoming` bytes would be refused.
    async fn evaluate(&self, meta: &Meta, incoming: u64) -> Result<Option<ServerRoom>, ApiError> {
        let mut cache = self.cache.lock().await;
        if cache.read_at.is_none_or(|at| at.elapsed() >= self.refresh) {
            // Reset first: a write that lands while the numbers are read is then counted twice, never lost.
            self.written.store(0, Ordering::Relaxed);
            cache.stored = meta.admin_server_counts().await?.stored_bytes;
            if self.policy.min_free.is_some() {
                let probe = self.probe.clone();
                (cache.total, cache.free) = tokio::task::spawn_blocking(move || probe()).await.unwrap_or((None, None));
            }
            cache.read_at = Some(Instant::now());
        }
        let added = self.written.load(Ordering::Relaxed).saturating_add(incoming);
        let reason = if self.policy.capacity.is_some_and(|capacity| cache.stored.saturating_add(added) > capacity) {
            Some(Reason::Capacity)
        } else if let (Some(min_free), Some(free)) = (self.policy.min_free, cache.free)
            && free.saturating_sub(added) < min_free
        {
            Some(Reason::FreeSpace)
        } else {
            None
        };
        let room = ServerRoom { free_bytes: cache.free, total_bytes: cache.total, capacity_bytes: self.policy.capacity };
        drop(cache);
        if reason.is_some() {
            self.refused.store(incoming, Ordering::Relaxed);
        }
        self.note_state(reason);
        Ok(reason.map(|_| room))
    }

    /// Logs when refusing starts or stops.
    fn note_state(&self, reason: Option<Reason>) {
        let was = self.refusing.swap(reason.is_some(), Ordering::Relaxed);
        match (was, reason) {
            (false, Some(Reason::Capacity)) => tracing::warn!("the server is full: it holds as much as `capacity` allows; refusing new uploads"),
            (false, Some(Reason::FreeSpace)) => {
                tracing::warn!("the server is full: free space is down to the `min_free` reserve; refusing new uploads")
            }
            (true, None) => tracing::warn!("the server has room again; accepting uploads"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn meta() -> Meta {
        Meta::open("sqlite::memory:").await.unwrap()
    }

    fn probe(free: Arc<AtomicU64>) -> Probe {
        Arc::new(move || (Some(1000), Some(free.load(Ordering::Relaxed))))
    }

    #[tokio::test]
    async fn the_reserve_refuses_the_write_that_would_cut_into_it_and_the_numbers_are_cached() {
        let meta = meta().await;
        let free = Arc::new(AtomicU64::new(300));
        let guard =
            FullGuard::with_probe(Policy { capacity: None, min_free: Some(200) }, probe(free.clone())).with_refresh(Duration::from_millis(200));
        assert!(guard.check(&meta, 100).await.is_ok(), "300 - 100 = 200 stays at the reserve");
        guard.note_written(100);
        // 200 free by what the server wrote itself, though the filesystem was not read again.
        assert!(guard.check(&meta, 1).await.is_err());
        assert!(guard.refusing());
        assert!(guard.status(&meta).await.unwrap(), "the admin's overview says so too");
        // The disk is read again after the refresh time, not before.
        free.store(900, Ordering::Relaxed);
        assert!(guard.check(&meta, 1).await.is_err(), "still the cached numbers");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!guard.status(&meta).await.unwrap(), "and stops saying so once space is back, with no upload needed");
        assert!(guard.check(&meta, 1).await.is_ok());
        assert!(!guard.refusing());
    }

    #[tokio::test]
    async fn the_capacity_counts_what_is_stored_and_what_comes() {
        let meta = meta().await;
        let guard = FullGuard::with_probe(Policy { capacity: Some(10), min_free: None }, Arc::new(|| (None, None)));
        assert!(guard.check(&meta, 10).await.is_ok(), "exactly the capacity fits");
        let err = guard.check(&meta, 11).await.unwrap_err();
        assert_eq!(err.code, "server_full");
        assert_eq!(err.server.unwrap().capacity_bytes, Some(10));
    }

    #[tokio::test]
    async fn an_unknown_filesystem_never_refuses() {
        let meta = meta().await;
        let guard = FullGuard::with_probe(Policy { capacity: None, min_free: Some(u64::MAX) }, Arc::new(|| (None, None)));
        assert!(guard.check(&meta, 1 << 40).await.is_ok());
    }
}
