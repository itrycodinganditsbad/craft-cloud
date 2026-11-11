//! Everything a request handler needs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::storage_tools::CompressionSettings;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use craft_cloud_meta::{Meta, UserId};
use craft_cloud_proto::DeviceId;
use craft_cloud_store::Store;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::audit::AuditExportLimits;
use crate::auth::hash_password;
use crate::error::ApiError;
use crate::live::LiveHub;
use crate::notify::Notifier;
use crate::ratelimit::Limiter;
use crate::secretbox::SecretBox;
use crate::storage_info::StorageInfo;

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
    /// A live document's log above its snapshot is compacted once it holds more than this many updates…
    pub compact_updates: u64,
    /// …or more than this many bytes.
    pub compact_bytes: u64,
    /// A device asked to compact that doesn't answer within this long is skipped, and another one asked later.
    pub compact_timeout: Duration,
    /// Presence messages one connection may send per document per second (more are dropped).
    pub presence_per_sec: u32,
    /// How often a live connection is pinged, and its account's access and device read again.
    pub live_ping: Duration,
    /// How long a live connection trusts its last look at its account's access before it relays updates or presence, or
    /// accepts presence (a removal made by another process is found within this time).
    pub live_recheck: Duration,
    /// A look at a connection's account that failed (the database could not be read) is repeated this often; the answer
    /// is never taken as "still allowed". The connection relays nothing and accepts no presence meanwhile…
    pub live_retry: Duration,
    /// …and ends with an `internal` error (not `access_changed`) once this many looks in a row failed
    /// ([`crate::live::LOOKUP_FAILURES_MAX`]) or this long has passed since the first.
    pub live_patience: Duration,
    /// All the frames of one catch-up page together must be sent within this long, or the connection ends.
    pub live_page_send: Duration,
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
            compact_updates: craft_cloud_proto::live::COMPACT_UPDATES,
            compact_bytes: craft_cloud_proto::live::COMPACT_BYTES,
            compact_timeout: crate::live::COMPACT_TIMEOUT,
            presence_per_sec: craft_cloud_proto::live::PRESENCE_PER_SEC,
            live_ping: crate::live::PING_EVERY,
            live_recheck: crate::live::RECHECK_EVERY,
            live_retry: crate::live::LOOKUP_RETRY,
            live_patience: crate::live::LOOKUP_PATIENCE,
            live_page_send: crate::live::PAGE_SEND_TIMEOUT,
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
    /// Password change tries per account, in 15-minute windows ([`crate::account`]).
    pub password_changes: Limiter,
    /// Invitations to team spaces per account, in one-minute windows ([`crate::spaces`]).
    pub space_invites: Limiter,
    /// Wrong second factors per account and per client address, in five-minute windows ([`crate::two_factor`]).
    pub second_factors: Limiter,
    /// Sign-in attempts to the admin pages per client address and per account, in five-minute windows ([`crate::web`]).
    pub web_logins: Limiter,
    /// The admin pages' settings; `None`: they are off and `/web` does not exist.
    pub web: Option<crate::web::WebSettings>,
    pub updates: crate::updates::UpdateSettings,
    pub(crate) update_cache: Mutex<Option<(std::time::Instant, Option<crate::updates::UpdateInfo>)>>,
    /// Seals and opens the secrets of two-factor sign-in; `None`: no key is configured, so it can't be set up.
    pub two_factor: Option<Arc<SecretBox>>,
    pub notifier: Notifier,
    /// The live service's connections and subscriptions ([`crate::live`]).
    pub live: LiveHub,
    pub limits: Limits,
    /// How the audit export is cut into pages and bounded ([`crate::audit`]).
    pub audit_export: AuditExportLimits,
    /// The address computers reach the server at (`public_url` in the config), for the join links of the admin API.
    pub public_url: Option<String>,
    /// Chunk and manifest bodies being received, per user ([`Limits::bodies_per_user`]).
    pub bodies: PerUser,
    /// Manifests being decoded and checked, per user ([`Limits::manifests_per_user`]).
    pub manifests: PerUser,
    /// Password checks running, server-wide ([`Limits::password_checks`]).
    pub password_checks: Arc<Semaphore>,
    pub(crate) touches: Mutex<HashMap<DeviceId, i64>>,
    /// Verified against when the username doesn't exist, so both cases cost the same.
    pub dummy_hash: String,
    /// Days a deleted node stays in the trash before the server may remove it (`retention.trash_days`).
    pub trash_days: u32,
    /// Where to measure the room the server has, and the `capacity` the admin set (the admin's storage overview).
    pub storage_info: StorageInfo,
    /// Refuses new uploads while the server is full (`[storage] refuse_when_full`); `None`: off, the default.
    pub full: Option<crate::full::FullGuard>,
    /// When the server started, in milliseconds since the Unix epoch (the admin's overview).
    pub started_ms: i64,
    /// The blob store's last health probe, kept for 30 seconds ([`crate::health`]).
    pub store_health: crate::health::StoreHealth,
    /// The compression settings of the config file, for the admin's storage overview ([`CompressionSettings`]).
    pub compression: CompressionSettings,
    /// The overview's compression totals, kept for a short while: they sum the whole blobs table.
    pub(crate) compression_totals: Mutex<Option<(i64, craft_cloud_meta::StorageTotals)>>,
    /// Held while an admin's compression level is committed and applied to the store, so that concurrent changes leave
    /// the store at the level the database ends with ([`crate::admin`]).
    pub(crate) compression_change: tokio::sync::Mutex<()>,
    /// The time of day for everything a request records (a device's last use, a stored blob, a commit).
    clock: Clock,
}

impl AppState {
    /// Reads configured update status at most once per minute; file errors never fail an admin page. The file is
    /// read on a blocking thread and not while the cache is locked, and a read that does not finish in two seconds
    /// counts as no status.
    pub async fn update_info(&self) -> Option<crate::updates::UpdateInfo> {
        {
            let cache = self.update_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, value)) = &*cache
                && at.elapsed() < Duration::from_secs(60)
            {
                return value.clone();
            }
        }
        let settings = self.updates.clone();
        let read = tokio::task::spawn_blocking(move || crate::updates::read(&settings));
        let value = match tokio::time::timeout(Duration::from_secs(2), read).await {
            Ok(Ok(value)) => value,
            _ => None,
        };
        let mut cache = self.update_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache = Some((std::time::Instant::now(), value.clone()));
        value
    }

    pub fn new(meta: Meta, store: Store, trusted_proxies: Vec<IpAddr>) -> Result<Self, ApiError> {
        let compression = CompressionSettings::of(&store);
        let limits = Limits::default();
        Ok(Self {
            meta,
            store,
            trusted_proxies,
            logins: Limiter::new(Duration::from_secs(300)),
            joins: Limiter::new(Duration::from_secs(60)),
            password_changes: Limiter::new(Duration::from_secs(900)),
            space_invites: Limiter::new(Duration::from_secs(60)),
            second_factors: Limiter::new(Duration::from_secs(300)),
            web_logins: Limiter::new(Duration::from_secs(300)),
            web: None,
            updates: crate::updates::UpdateSettings::default(),
            update_cache: Mutex::new(None),
            two_factor: None,
            notifier: Notifier::default(),
            live: LiveHub::default(),
            bodies: PerUser::new(limits.bodies_per_user),
            manifests: PerUser::new(limits.manifests_per_user),
            password_checks: Arc::new(Semaphore::new(limits.password_checks)),
            limits,
            audit_export: AuditExportLimits::default(),
            public_url: None,
            touches: Mutex::new(HashMap::new()),
            dummy_hash: hash_password("craft-cloud dummy password")?,
            trash_days: 30,
            compression,
            compression_totals: Mutex::new(None),
            compression_change: tokio::sync::Mutex::new(()),
            storage_info: StorageInfo::default(),
            full: None,
            started_ms: now_ms(),
            store_health: crate::health::StoreHealth::default(),
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

    /// The state with other bounds for the audit export (tests use small ones).
    pub fn with_audit_export(mut self, limits: AuditExportLimits) -> Self {
        self.audit_export = limits;
        self
    }

    /// The state with the key for two-factor secrets (`None`: none is configured and two-factor sign-in can't be set up).
    pub fn with_two_factor(mut self, secrets: Option<SecretBox>) -> Self {
        self.two_factor = secrets.map(Arc::new);
        self
    }

    /// The state with the admin pages on (`None`: off, the default): their address, session lifetimes and the like.
    pub fn with_web_admin(mut self, web: Option<crate::web::WebSettings>) -> Self {
        self.web = web;
        self
    }

    /// The local update status file and links shown to administrators.
    pub fn with_updates(mut self, updates: crate::updates::UpdateSettings) -> Self {
        self.updates = updates;
        self
    }

    /// The state with the server's public address (`public_url`), which the admin API puts in the join links it makes.
    pub fn with_public_url(mut self, public_url: Option<String>) -> Self {
        self.public_url = public_url;
        self
    }

    /// The state with another trash period in days (`retention.trash_days`; the default is 30).
    pub fn with_trash_days(mut self, days: u32) -> Self {
        self.trash_days = days;
        self
    }

    /// The state with the places to measure and the configured capacity (`[storage] capacity`). The default measures nothing.
    /// Takes the level an admin set while the server ran (kept in the database) over the config's, for new writes. The
    /// config's `compression = "none"` still wins: the store then writes nothing compressed whatever the level.
    pub async fn load_compression_level(&self) -> Result<(), craft_cloud_meta::MetaError> {
        if let Some((value, _)) = self.meta.server_value(crate::storage_tools::LEVEL_KEY).await? {
            // The same range the overview shows: a stored value outside it (a hand-edited row) is ignored, not clamped.
            match value.parse::<u8>().ok().filter(|l| (craft_cloud_store::MIN_ZSTD_LEVEL..=craft_cloud_store::MAX_ZSTD_LEVEL).contains(l)) {
                Some(level) => self.store.set_level(level),
                None => tracing::warn!(value = %value, "the stored compression level is not 1 to 19 and is ignored"),
            }
        }
        Ok(())
    }

    pub fn with_storage_info(mut self, info: StorageInfo) -> Self {
        self.storage_info = info;
        self
    }

    /// The state that refuses uploads while it is full (`None`: it never does, the default).
    pub fn with_full_guard(mut self, guard: Option<crate::full::FullGuard>) -> Self {
        self.full = guard;
        self
    }

    /// Refuses an upload of `incoming` bytes with `507 server_full` when the server is full and was told to refuse.
    pub async fn check_server_room(&self, incoming: u64) -> Result<(), ApiError> {
        match &self.full {
            Some(guard) => guard.check(&self.meta, incoming).await,
            None => Ok(()),
        }
    }

    /// Counts bytes just stored toward the room the server believes it has left.
    pub fn note_stored(&self, bytes: u64) {
        if let Some(guard) = &self.full {
            guard.note_written(bytes);
        }
    }

    /// The state with another clock: a simulation's, so that what requests record and when a device's last use is
    /// written again follow simulated time, never the real one. A test seam (the `test-support` feature).
    #[cfg(feature = "test-support")]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        // The store judges a browser session's expiry inside an admin change by the same simulated time.
        self.meta = self.meta.clone().with_clock(clock.clone());
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
