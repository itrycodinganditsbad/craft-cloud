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

/// How long the free-space read may take before the filesystem counts as unknown (a dead network mount would
/// otherwise hold every upload).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Warnings that refusing started are at most this far apart, however close to the threshold the server sits.
const WARN_EVERY: Duration = Duration::from_secs(60);

pub struct FullGuard {
    policy: Policy,
    probe: Probe,
    /// How long the cached numbers count as current ([`REFRESH`]; tests use others).
    refresh: Duration,
    probe_timeout: Duration,
    cache: Mutex<Cache>,
    /// Bytes this server stored since the numbers in the cache were read.
    written: AtomicU64,
    /// Whether a smallest upload would be refused now. Set from the real numbers only, never from what a request
    /// declares, so no client can make the server say it is full.
    refusing: AtomicBool,
    /// A free-space read is running (it may be stuck on a dead mount); no second one is started meanwhile.
    probing: Arc<AtomicBool>,
    /// The free space was unknown at the last read, and that was said in the log.
    unknown_logged: AtomicBool,
    /// When the log last said refusing started, and whether it has not yet said it stopped.
    warned: std::sync::Mutex<(Option<Instant>, bool)>,
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
            probe_timeout: PROBE_TIMEOUT,
            cache: Mutex::new(Cache::default()),
            written: AtomicU64::new(0),
            refusing: AtomicBool::new(false),
            probing: Arc::new(AtomicBool::new(false)),
            unknown_logged: AtomicBool::new(false),
            warned: std::sync::Mutex::new((None, false)),
        }
    }

    /// The guard reading the numbers again after `refresh` instead of [`REFRESH`] (tests).
    pub fn with_refresh(mut self, refresh: Duration) -> Self {
        self.refresh = refresh;
        self
    }

    /// The guard waiting `timeout` for the free-space read instead of [`PROBE_TIMEOUT`] (tests).
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Whether the server would refuse even the smallest upload, as of the last time the numbers were looked at.
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

    /// Whether the server is refusing uploads now (for the admin's overview). Reads the numbers if they are stale, so it
    /// turns false soon after space was freed even when no upload has come since.
    pub async fn status(&self, meta: &Meta) -> Result<bool, ApiError> {
        self.evaluate(meta, 0).await?;
        Ok(self.refusing())
    }

    /// Counts bytes the server stored, until the numbers are read again. Called once the upload is recorded in the
    /// database: a read of the stored total that came between the write and the record then still has them counted here,
    /// and one that came after has them in the total (counted twice for up to one refresh, which errs toward refusing).
    pub fn note_written(&self, bytes: u64) {
        self.written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// `Some(room)` when an upload of `incoming` bytes would be refused. Also keeps `refusing` (for a one-byte upload).
    async fn evaluate(&self, meta: &Meta, incoming: u64) -> Result<Option<ServerRoom>, ApiError> {
        let mut cache = self.cache.lock().await;
        if cache.read_at.is_none_or(|at| at.elapsed() >= self.refresh) {
            // Reset first: bytes stored while the numbers are read may then be counted twice, which errs toward refusing.
            self.written.store(0, Ordering::Relaxed);
            cache.stored = meta.admin_server_counts().await?.stored_bytes;
            if self.policy.min_free.is_some() {
                (cache.total, cache.free) = self.read_filesystem().await;
            }
            cache.read_at = Some(Instant::now());
        }
        let written = self.written.load(Ordering::Relaxed);
        let (stored, total, free) = (cache.stored, cache.total, cache.free);
        drop(cache);
        let reason_for = |incoming: u64| {
            let added = written.saturating_add(incoming);
            if self.policy.capacity.is_some_and(|capacity| stored.saturating_add(added) > capacity) {
                Some(Reason::Capacity)
            } else if let (Some(min_free), Some(free)) = (self.policy.min_free, free)
                && free.saturating_sub(added) < min_free
            {
                Some(Reason::FreeSpace)
            } else {
                None
            }
        };
        self.note_state(reason_for(1));
        let room = ServerRoom { free_bytes: free, total_bytes: total, capacity_bytes: self.policy.capacity };
        Ok(reason_for(incoming).map(|_| room))
    }

    /// The store's filesystem: total and free bytes, or unknown when the read fails or takes longer than the timeout.
    async fn read_filesystem(&self) -> (Option<u64>, Option<u64>) {
        let known = if self.probing.swap(true, Ordering::SeqCst) {
            // The last read is still stuck: don't pile up threads behind a dead mount.
            None
        } else {
            let (probe, probing) = (self.probe.clone(), self.probing.clone());
            let read = tokio::task::spawn_blocking(move || {
                let numbers = probe();
                probing.store(false, Ordering::SeqCst);
                numbers
            });
            match tokio::time::timeout(self.probe_timeout, read).await {
                Ok(Ok(numbers)) => Some(numbers),
                _ => None,
            }
        };
        let numbers = known.unwrap_or((None, None));
        // Said once while it lasts: a reserve that can't be checked is not checked.
        if numbers.1.is_none() {
            if !self.unknown_logged.swap(true, Ordering::Relaxed) {
                tracing::warn!("can't read the free space of the store's filesystem: the `min_free` reserve is not checked until it can be");
            }
        } else {
            self.unknown_logged.store(false, Ordering::Relaxed);
        }
        numbers
    }

    /// Logs when refusing starts or stops. A start is said at most once a minute, and a stop only after a start was said.
    fn note_state(&self, reason: Option<Reason>) {
        let was = self.refusing.swap(reason.is_some(), Ordering::Relaxed);
        if was == reason.is_some() {
            return;
        }
        let mut warned = self.warned.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match reason {
            Some(reason) => {
                if warned.0.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    *warned = (Some(Instant::now()), true);
                    match reason {
                        Reason::Capacity => tracing::warn!("the server is full: it holds as much as `capacity` allows; refusing new uploads"),
                        Reason::FreeSpace => {
                            tracing::warn!("the server is full: free space is down to the `min_free` reserve; refusing new uploads")
                        }
                    }
                }
            }
            None => {
                if warned.1 {
                    warned.1 = false;
                    tracing::warn!("the server has room again; accepting uploads");
                }
            }
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

    #[tokio::test]
    async fn what_a_request_declares_never_makes_the_server_say_it_is_full() {
        let meta = meta().await;
        let guard = FullGuard::with_probe(Policy { capacity: Some(100), min_free: None }, Arc::new(|| (None, None)));
        assert!(guard.check(&meta, u64::MAX).await.is_err(), "that upload is refused");
        assert!(!guard.refusing(), "but a one-byte upload would fit, so the server is not full");
        assert!(!guard.status(&meta).await.unwrap());
        assert!(guard.check(&meta, 100).await.is_ok());
    }

    #[tokio::test]
    async fn a_free_space_read_that_takes_too_long_counts_as_unknown() {
        let meta = meta().await;
        // A dead mount: the read outlasts the timeout, and the reserve is then not checked (not refused for).
        let slow: Probe = Arc::new(|| {
            std::thread::sleep(Duration::from_millis(400));
            (Some(1000), Some(0))
        });
        let guard = FullGuard::with_probe(Policy { capacity: None, min_free: Some(500) }, slow).with_probe_timeout(Duration::from_millis(50));
        let started = std::time::Instant::now();
        assert!(guard.check(&meta, 1).await.is_ok());
        assert!(started.elapsed() < Duration::from_millis(300), "{:?}", started.elapsed());
        assert!(!guard.refusing());
    }
}
