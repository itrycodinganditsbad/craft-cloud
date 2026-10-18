//! Everything a request handler needs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use craft_cloud_meta::{Meta, UserId};
use craft_cloud_proto::DeviceId;
use craft_cloud_store::Store;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::hash_password;
use crate::error::ApiError;
use crate::live::LiveHub;
use crate::notify::Notifier;
use crate::ratelimit::Limiter;

/// How much one user, or one request, may hold on the server at once. Each user's limits are their own, so a
/// stalled or greedy client only ever slows its own account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Chunk and manifest bodies one user may be sending at once; more wait their turn before their bodies are
    /// read, so they hold no buffered bytes meanwhile.
    pub bodies_per_user: usize,
    /// Manifests one user may have decoded and checked at once. Taken once the body is in, so a body that is slow
    /// to arrive doesn't hold it.
    pub manifests_per_user: usize,
    /// Password checks (Argon2id, 19 MiB each) the whole server runs at once. A sign-in beyond that is answered
    /// `429 rate_limited` at once rather than queued.
    pub password_checks: usize,
    /// How long a request body (chunk, manifest or JSON) may take to arrive: this much, plus `body_time_per_mib` for every MiB the
    /// route allows. A body that takes longer is answered `408` and its slot goes to the next request.
    pub body_time_base: Duration,
    pub body_time_per_mib: Duration,
    /// Live frames (updates, creates, subscribes) one connection may send at once…
    pub live_burst: u32,
    /// …refilled at this many per second; over it the answer is `rate_limited`.
    pub live_per_sec: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bodies_per_user: 8,
            manifests_per_user: 4,
            password_checks: 4,
            body_time_base: Duration::from_secs(60),
            body_time_per_mib: Duration::from_secs(30),
            live_burst: crate::live::UPDATE_BURST,
            live_per_sec: crate::live::UPDATES_PER_SEC,
        }
    }
}

impl Limits {
    /// The time a body of up to `max_bytes` bytes may take to arrive. A 4 MiB chunk gets 3 minutes; the largest
    /// manifest (about 37 MiB) about 20, which is still 35 KB/s.
    pub fn body_time(&self, max_bytes: usize) -> Duration {
        let mib = u32::try_from((max_bytes as u64).div_ceil(1024 * 1024)).unwrap_or(u32::MAX);
        self.body_time_base.saturating_add(self.body_time_per_mib.saturating_mul(mib))
    }
}

/// A semaphore per user, made on first use: at most `per_user` permits for each user at once.
pub struct PerUser {
    per_user: usize,
    users: Mutex<HashMap<UserId, Arc<Semaphore>>>,
}

impl PerUser {
    fn new(per_user: usize) -> Self {
        Self { per_user, users: Mutex::new(HashMap::new()) }
    }

    /// Waits for one of `user`'s permits.
    pub async fn acquire(&self, user: UserId) -> Result<OwnedSemaphorePermit, ApiError> {
        let semaphore = {
            let mut users = self.users.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            users.entry(user).or_insert_with(|| Arc::new(Semaphore::new(self.per_user))).clone()
        };
        semaphore.acquire_owned().await.map_err(|e| {
            tracing::error!(error = %e, "a per-user semaphore closed");
            ApiError::internal()
        })
    }
}

/// Where the server reads the time of day (milliseconds since the Unix epoch): the system clock, or a simulated one
/// in tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct AppState {
    pub meta: Meta,
    pub store: Store,
    pub trusted_proxies: Vec<IpAddr>,
    pub logins: Limiter,
    /// Failed join-code redeems per client address, in one-minute windows ([`crate::join`]).
    pub joins: Limiter,
    pub notifier: Notifier,
    /// The live service's connections and subscriptions ([`crate::live`]).
    pub live: LiveHub,
    pub limits: Limits,
    /// Chunk and manifest bodies being received, per user ([`Limits::bodies_per_user`]).
    pub bodies: PerUser,
    /// Manifests being decoded and checked, per user ([`Limits::manifests_per_user`]).
    pub manifests: PerUser,
    /// Password checks running, server-wide ([`Limits::password_checks`]).
    pub password_checks: Arc<Semaphore>,
    pub(crate) touches: Mutex<HashMap<DeviceId, i64>>,
    /// Verified against when the username doesn't exist, so both cases cost the same.
    pub dummy_hash: String,
    /// The time of day for everything a request records (a device's last use, a stored blob, a commit).
    clock: Clock,
}

impl AppState {
    pub fn new(meta: Meta, store: Store, trusted_proxies: Vec<IpAddr>) -> Result<Self, ApiError> {
        let limits = Limits::default();
        Ok(Self {
            meta,
            store,
            trusted_proxies,
            logins: Limiter::new(Duration::from_secs(300)),
            joins: Limiter::new(Duration::from_secs(60)),
            notifier: Notifier::default(),
            live: LiveHub::default(),
            bodies: PerUser::new(limits.bodies_per_user),
            manifests: PerUser::new(limits.manifests_per_user),
            password_checks: Arc::new(Semaphore::new(limits.password_checks)),
            limits,
            touches: Mutex::new(HashMap::new()),
            dummy_hash: hash_password("craft-cloud dummy password")?,
            clock: Arc::new(now_ms),
        })
    }

    /// The state with other [`Limits`] (tests use small ones).
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.bodies = PerUser::new(limits.bodies_per_user);
        self.manifests = PerUser::new(limits.manifests_per_user);
        self.password_checks = Arc::new(Semaphore::new(limits.password_checks));
        self.limits = limits;
        self
    }

    /// The state with another clock: a simulation's, so that what requests record and when a device's last use is
    /// written again follow simulated time, never the real one. A test seam (the `test-support` feature).
    #[cfg(feature = "test-support")]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Milliseconds since the Unix epoch, by the state's clock.
    pub fn now_ms(&self) -> i64 {
        (self.clock)()
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_time_grows_with_the_routes_limit() {
        let limits = Limits::default();
        assert_eq!(limits.body_time(4 * 1024 * 1024), Duration::from_secs(180));
        assert_eq!(limits.body_time(craft_cloud_proto::limits::MAX_MANIFEST_BYTES), Duration::from_secs(60 + 30 * 37));
        assert_eq!(limits.body_time(0), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn each_user_has_their_own_permits() {
        let slots = PerUser::new(2);
        let (ana, bob) = (UserId(uuid::Uuid::now_v7()), UserId(uuid::Uuid::now_v7()));
        let _first = slots.acquire(ana).await.unwrap();
        let second = slots.acquire(ana).await.unwrap();
        // Ana's third waits…
        assert!(tokio::time::timeout(Duration::from_millis(50), slots.acquire(ana)).await.is_err());
        // …while Bob gets his at once.
        let _bob = tokio::time::timeout(Duration::from_millis(50), slots.acquire(bob)).await.unwrap().unwrap();
        // One of Ana's ends: her next goes ahead.
        drop(second);
        let _third = tokio::time::timeout(Duration::from_millis(50), slots.acquire(ana)).await.unwrap().unwrap();
    }
}
