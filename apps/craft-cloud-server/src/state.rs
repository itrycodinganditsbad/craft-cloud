//! Everything a request handler needs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use craft_cloud_meta::Meta;
use craft_cloud_proto::DeviceId;
use craft_cloud_store::Store;
use tokio::sync::Semaphore;

use crate::auth::hash_password;
use crate::error::ApiError;
use crate::notify::Notifier;
use crate::ratelimit::Limiter;

/// Manifest uploads processed at once, server-wide.
pub const MANIFEST_UPLOADS_AT_ONCE: usize = 4;

/// Where the server reads the time of day (milliseconds since the Unix epoch): the system clock, or a simulated one
/// in tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct AppState {
    pub meta: Meta,
    pub store: Store,
    pub trusted_proxies: Vec<IpAddr>,
    pub logins: Limiter,
    pub notifier: Notifier,
    pub manifest_uploads: Semaphore,
    pub(crate) touches: Mutex<HashMap<DeviceId, i64>>,
    /// Verified against when the username doesn't exist, so both cases cost the same.
    pub dummy_hash: String,
    /// The time of day for everything a request records (a device's last use, a stored blob, a commit).
    clock: Clock,
}

impl AppState {
    pub fn new(meta: Meta, store: Store, trusted_proxies: Vec<IpAddr>) -> Result<Self, ApiError> {
        Ok(Self {
            meta,
            store,
            trusted_proxies,
            logins: Limiter::new(Duration::from_secs(300)),
            notifier: Notifier::default(),
            manifest_uploads: Semaphore::new(MANIFEST_UPLOADS_AT_ONCE),
            touches: Mutex::new(HashMap::new()),
            dummy_hash: hash_password("craft-cloud dummy password")?,
            clock: Arc::new(now_ms),
        })
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
