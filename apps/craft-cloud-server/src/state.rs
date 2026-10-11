//! Everything a request handler needs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
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
        })
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)).unwrap_or(0)
}
