//! Whether the blob store answers, for the admin's server overview (`GET /admin/server`).
//!
//! The probe asks the store for the size of one object that never exists (a chunk whose ID is 32 zero bytes, which
//! no content hashes to). A store that answers "no such object" is reachable, and its credentials and bucket or
//! directory are right; anything else, or no answer within [`PROBE_TIMEOUT`], is a store that is not. The probe
//! writes nothing, so it needs no write permission and leaves nothing behind. It does not prove that a write would
//! succeed (a full disk is `refusing_uploads`' business), only that the store is there.
//!
//! The answer is kept for [`CACHE_FOR`], so an admin page that reloads, or several admins, don't hammer S3.

use std::future::Future;
use std::time::Duration;

use craft_cloud_proto::ChunkId;
use craft_cloud_store::{BlobKey, Store, StoreError};
use tokio::sync::Mutex;
use tokio::time::Instant;

/// How long the store gets to answer.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an answer is kept.
pub const CACHE_FOR: Duration = Duration::from_secs(30);

/// What the last probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCheck {
    pub ok: bool,
    /// When the probe ran, in milliseconds since the Unix epoch.
    pub checked_ms: i64,
}

#[derive(Default)]
pub struct StoreHealth {
    last: Mutex<Option<(Instant, StoreCheck)>>,
}

impl StoreHealth {
    /// The last probe if it is younger than [`CACHE_FOR`], else a new one at `now_ms`. Never an error: a store that
    /// fails is `ok: false`.
    ///
    /// `root` is the disk store's directory: a store whose directory is gone answers "no such object" for every key, so
    /// the probe also checks that the directory is there. `None` for S3 and memory.
    pub async fn check(&self, store: &Store, root: Option<&std::path::Path>, now_ms: i64) -> StoreCheck {
        let key = BlobKey::chunk(&ChunkId::from_bytes([0; 32]));
        let probe = async {
            if let Some(root) = root
                && !tokio::fs::metadata(root).await.is_ok_and(|meta| meta.is_dir())
            {
                return Err(StoreError::Backend("the store's directory is not there".into()));
            }
            store.stored_size(key).await
        };
        self.run(probe, now_ms, CACHE_FOR, PROBE_TIMEOUT).await
    }

    /// [`StoreHealth::check`] with the question as a future, and the times as arguments (the tests' seam). `probe` is
    /// only polled when the last answer is stale.
    async fn run(&self, probe: impl Future<Output = Result<u64, StoreError>>, now_ms: i64, cache_for: Duration, timeout: Duration) -> StoreCheck {
        // Held while probing: concurrent page loads wait for the one probe instead of each making their own.
        let mut last = self.last.lock().await;
        if let Some((at, check)) = *last
            && at.elapsed() < cache_for
        {
            return check;
        }
        let ok = match tokio::time::timeout(timeout, probe).await {
            Ok(Ok(_) | Err(StoreError::NotFound(_))) => true,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "the blob store didn't answer the health probe");
                false
            }
            Err(_) => {
                tracing::warn!("the blob store didn't answer the health probe in time");
                false
            }
        };
        let check = StoreCheck { ok, checked_ms: now_ms };
        *last = Some((Instant::now(), check));
        check
    }
}

#[cfg(test)]
mod tests {
    use craft_cloud_store::memory::MemoryBackend;

    use super::*;

    fn down() -> Result<u64, StoreError> {
        Err(StoreError::Backend("down".into()))
    }

    #[tokio::test]
    async fn a_store_that_says_no_such_object_is_fine() {
        let check = StoreHealth::default().check(&Store::new(MemoryBackend::default()), None, 7).await;
        assert_eq!(check, StoreCheck { ok: true, checked_ms: 7 });
    }

    #[tokio::test]
    async fn a_failing_or_silent_store_is_not_ok_and_never_an_error() {
        let failing = StoreHealth::default().run(async { down() }, 1, CACHE_FOR, PROBE_TIMEOUT).await;
        assert_eq!(failing, StoreCheck { ok: false, checked_ms: 1 });
        let silent = StoreHealth::default().run(std::future::pending(), 2, CACHE_FOR, Duration::from_millis(50)).await;
        assert_eq!(silent, StoreCheck { ok: false, checked_ms: 2 });
    }

    #[tokio::test]
    async fn the_answer_is_kept_for_a_while() {
        let health = StoreHealth::default();
        health.run(async { down() }, 1, CACHE_FOR, PROBE_TIMEOUT).await;
        // Within its time the probe is not even polled.
        let polled = std::cell::Cell::new(false);
        let again = health
            .run(
                async {
                    polled.set(true);
                    Ok(0)
                },
                99,
                CACHE_FOR,
                PROBE_TIMEOUT,
            )
            .await;
        assert_eq!((again, polled.get()), (StoreCheck { ok: false, checked_ms: 1 }, false));
        // Past its time it asks again.
        let later = health.run(async { Ok(0) }, 100, Duration::ZERO, PROBE_TIMEOUT).await;
        assert_eq!(later, StoreCheck { ok: true, checked_ms: 100 });
    }
}
