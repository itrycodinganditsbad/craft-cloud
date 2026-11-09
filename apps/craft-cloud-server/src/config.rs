//! The server's TOML config file.
//!
//! ```toml
//! listen = "127.0.0.1:8080"
//! behind_proxy = false          # true when a TLS reverse proxy forwards to this address
//! trusted_proxies = []          # proxies whose X-Forwarded-For is believed; needed with behind_proxy
//! # ignore_forwarded_for = true # behind a proxy without naming it: every client then counts as the proxy
//! # stored_bytes_per_user = 2_000_000_000_000   # cap on what one account stores, in bytes (unset: no cap)
//! # live_bytes_per_user = 1_073_741_824         # cap on what one account's live documents hold, in bytes (1 GiB)
//! # public_url = "https://cloud.example.com"    # where computers reach the server; `user invite` prints it
//!
//! # [audit]
//! # retention_days = 365        # how long security events are kept (1 to 3650); `audit prune` deletes older ones
//!
//! # [web_admin]                 # the admin pages under /web, off unless enabled; needs behind_proxy and an https public_url
//! # enabled = true
//! # idle_minutes = 480          # a browser session ends after this long unused (5 to 1440)
//! # absolute_hours = 168        # and at the latest after this long (1 to 720)
//! # [retention]
//! # trash_days = 30   # how long a deleted file stays in the trash before the server may remove it (1 to 3650)
//!
//! [database]
//! url = "sqlite:/var/lib/craft-cloud/meta.db"   # or postgres://user:pass@host/db
//!
//! [storage]
//! kind = "disk"
//! path = "/var/lib/craft-cloud/blobs"
//! # capacity = "1 TiB"          # what the store may hold, for the admin's storage overview (needed for S3 to show a size)
//! # kind = "s3", bucket, prefix, region, endpoint, access_key_id, secret_access_key, allow_http, capacity
//! # kind = "sftp", sftp_host, sftp_port, sftp_user, sftp_root, sftp_key_file, sftp_key_passphrase, sftp_host_key or
//! #   sftp_known_hosts (one of them is required), sftp_connections, sftp_timeout_secs, sftp_retries, capacity
//! # compression = "zstd"   # or "none": store blobs plain (what a server before compression can read)
//! # compression_level = 3       # zstd level of new writes, 1 to 19 (an admin can change it while the server runs)
//! # compression_min_saving = 5  # keep a compressed blob only when it is at least this many percent smaller (1 to 50)
//!
//! # [backup]                    # defaults for `backup` and `restore`; see docs/server.md
//! # to = "sftp://backup@files.example.org/craft-cloud"
//! # key_file = "backup-key", host_key = "SHA256:…" (or known_hosts), keep = 7
//!
//! [two_factor]                  # optional: without it, two-factor sign-in can't be set up
//! key_file = "two-factor.key"   # 32 random bytes, made by `init`; relative to this file
//! # key_file_old = "old.key"    # while rotating the key, see `two-factor rekey`
//! ```
//!
//! The server speaks plain HTTP. It refuses to listen anywhere but loopback unless `behind_proxy = true`,
//! so it can't be exposed without TLS by accident.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use craft_cloud_proto::join::{Code, JoinString, PAIRING_CHARS};
use craft_cloud_proto::limits::MAX_CHUNK_BYTES;
use craft_cloud_store::s3::S3Config;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    /// The TOML parser's message and line number. The parser's own text quotes the offending line, which
    /// can hold a secret, so it is not used.
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("listen = {0} is not a loopback address. Craft Cloud speaks plain HTTP; put it behind a TLS reverse proxy and set behind_proxy = true")]
    NotLoopback(SocketAddr),
    #[error(
        "behind_proxy = true needs the proxy's address in trusted_proxies: without it every client shares the proxy's \
         address, so a few wrong passwords lock everyone out of signing in. If that is really meant, set \
         ignore_forwarded_for = true"
    )]
    NoTrustedProxies,
    #[error(
        "ignore_forwarded_for = true only goes with behind_proxy = true and an empty trusted_proxies: with trusted \
         proxies their X-Forwarded-For is believed, and without a proxy there is nothing to ignore. Remove one of them"
    )]
    IgnoreForwardedForMisplaced,
    #[error(
        "stored_bytes_per_user = {0} is less than one chunk ({1} bytes): not even one maximal chunk would fit, and the \
         agent could not tell a full account from a file that is too big. Raise it, or remove it for no cap"
    )]
    StoredBytesCapTooSmall(u64, u32),
    #[error("retention.trash_days = {0} is not between {MIN_TRASH_DAYS} and {MAX_TRASH_DAYS}")]
    TrashDays(u32),
    #[error("public_url = {0:?} is not a server address like https://cloud.example.com (a host, an optional port and path, no query)")]
    PublicUrl(String),
    #[error("storage.{0} is missing (set it in the config or the {1} environment variable)")]
    MissingS3Credential(&'static str, &'static str),
    #[error("storage.{0} is missing (set it in the config or the {1} environment variable)")]
    MissingSftpSetting(&'static str, &'static str),
    #[error("{0}")]
    Sftp(String),
    #[error("audit.retention_days = {0} is out of range: keep security events for {1} to {2} days")]
    AuditRetention(u32, u32, u32),
    #[error(
        "web_admin.enabled = true needs public_url = \"https://host[:port]\" with no path: the browser's cookie is only sent over \
         HTTPS, and the pages check the Origin of every form against that address"
    )]
    WebAdminPublicUrl,
    #[error(
        "web_admin.enabled = true needs behind_proxy = true: the server speaks plain HTTP, and the session cookie is marked Secure, \
         so a TLS reverse proxy must stand in front of it"
    )]
    WebAdminNeedsProxy,
    #[error("web_admin.{0} = {1} is out of range: {2} to {3}")]
    WebAdminRange(&'static str, u32, u32, u32),
}

/// Fewest and most days a deleted node waits in the trash.
pub const MIN_TRASH_DAYS: u32 = 1;
pub const MAX_TRASH_DAYS: u32 = 3650;
/// Days in the trash when `retention.trash_days` is not set.
pub const DEFAULT_TRASH_DAYS: u32 = 30;

/// The limits of the SFTP settings (the store crate checks them again when it connects).
const SFTP_MAX_CONNECTIONS: usize = 16;
const SFTP_MAX_TIMEOUT_SECS: u64 = 300;
const SFTP_MAX_RETRIES: u32 = 8;

/// `[retention]`: how long things are kept. Only the trash today; nothing is removed by it yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// Days a deleted node stays in the trash before the server may remove it for good (shown as `purge_after_ms`).
    #[serde(default = "default_trash_days")]
    pub trash_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self { trash_days: DEFAULT_TRASH_DAYS }
    }
}

fn default_trash_days() -> u32 {
    DEFAULT_TRASH_DAYS
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    #[serde(default)]
    pub behind_proxy: bool,
    #[serde(default)]
    pub trusted_proxies: Vec<IpAddr>,
    /// Lets `behind_proxy = true` run with no `trusted_proxies`: the server then believes no `X-Forwarded-For`, so
    /// every client has the proxy's address and shares one budget of sign-in attempts. Refused with a non-empty
    /// `trusted_proxies` or without `behind_proxy`, where it would mean nothing.
    #[serde(default)]
    pub ignore_forwarded_for: bool,
    /// The most bytes one account may store: the stored size of every chunk and manifest it uploaded, each counted
    /// once. An upload that would go over it is refused (`507 quota_exceeded`); the agent skips that file. Unset (the
    /// default): no cap. This is not yet the per-user quota on files' logical size (that comes with history).
    #[serde(default)]
    pub stored_bytes_per_user: Option<u64>,
    /// The most bytes one account's live documents may hold together: every update in their logs, compacted ones too
    /// (they are kept until retention rules come). An update that would go over it is refused (`quota_exceeded`); the
    /// agent keeps it and tries again later, as for a full document. Snapshots aren't counted here: they are uploads,
    /// under `stored_bytes_per_user`. Default 1 GiB.
    #[serde(default = "default_live_bytes")]
    pub live_bytes_per_user: u64,
    /// The address computers reach the server at, `https://host[:port][/prefix]` (the reverse proxy's). `user invite`
    /// puts it in front of the code it prints. Normalised when the config is read: the host in lower case, no
    /// trailing slash. Unset: `user invite` prints the bare code and says to put the address in front.
    #[serde(default)]
    pub public_url: Option<String>,
    /// The security audit log. The section is optional and its defaults are safe.
    #[serde(default)]
    pub audit: AuditConfig,
    /// The admin pages in the browser (`/web`). Off unless the section says `enabled = true`.
    #[serde(default)]
    pub web_admin: WebAdminConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    /// The key for the secrets of two-factor sign-in. Without it two-factor sign-in can't be set up (`503
    /// two_factor_unavailable`), and a server whose database already holds secrets refuses to start. `init` makes the
    /// key file and writes this section. The server never makes the key itself: a second server on the same database,
    /// or a restore without the key, would otherwise seal secrets nobody can open.
    #[serde(default)]
    pub two_factor: Option<TwoFactorConfig>,
    /// Defaults for the `backup` command (and, with `--to`, a place to keep the key and host key).
    #[serde(default)]
    pub backup: Option<BackupConfig>,
    /// The directory the config file is in, which `key_file` paths are relative to. Set by [`Config::load`], never read
    /// from the file.
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

/// `[two_factor]`: where the key for two-factor secrets is.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TwoFactorConfig {
    /// A file of 32 random bytes (mode 0600; Windows has no equivalent here, keep it out of everyone's reach). A relative
    /// path is relative to the directory of `config.toml`.
    pub key_file: PathBuf,
    /// The key the secrets were sealed with before a rotation: new secrets are sealed with `key_file`, old ones still
    /// open with this one until `two-factor rekey` has rewritten them. Remove it after that.
    #[serde(default)]
    pub key_file_old: Option<PathBuf>,
}

fn default_live_bytes() -> u64 {
    craft_cloud_meta::DEFAULT_LIVE_BYTES_PER_SPACE
}

/// `[audit]`: how the security audit log is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Days a security event is kept: `audit prune` deletes older ones, and a write does so now and then (at most once an
    /// hour). 1 to 3650, default 365. A server that no one writes to keeps expired events until `audit prune` runs.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { retention_days: default_retention_days() }
    }
}

fn default_retention_days() -> u32 {
    365
}

/// `[web_admin]`: the admin pages in the browser and the lifetimes of their sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebAdminConfig {
    /// Serve the admin pages under `/web`. Default off: a server that was not told to speak to browsers does not. Needs
    /// `behind_proxy = true` and an `https://` `public_url` without a path.
    #[serde(default)]
    pub enabled: bool,
    /// A session ends when it was not used for this many minutes: 5 to 1440, default 480 (8 hours).
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u32,
    /// A session ends this many hours after sign-in, however busy it is: 1 to 720, default 168 (7 days).
    #[serde(default = "default_absolute_hours")]
    pub absolute_hours: u32,
}

impl Default for WebAdminConfig {
    fn default() -> Self {
        Self { enabled: false, idle_minutes: default_idle_minutes(), absolute_hours: default_absolute_hours() }
    }
}

fn default_idle_minutes() -> u32 {
    8 * 60
}

fn default_absolute_hours() -> u32 {
    7 * 24
}

impl WebAdminConfig {
    /// The idle lifetime of a session in milliseconds.
    pub fn idle_ms(&self) -> i64 {
        i64::from(self.idle_minutes) * 60_000
    }

    /// The absolute lifetime of a session in milliseconds.
    pub fn absolute_ms(&self) -> i64 {
        i64::from(self.absolute_hours) * 3_600_000
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
}

impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseConfig").field("url", &redact_url(&self.url)).finish()
    }
}

/// The URL with its user name and password replaced by `<redacted>`. The scheme, host, port and path stay.
/// Everything up to the last `@` is treated as credentials, so a password containing `@` or `/` still
/// doesn't show. A URL without `@` is returned as is.
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else { return url.to_owned() };
    let authority_start = scheme_end + 3;
    let rest = &url[authority_start..];
    match rest.rfind('@') {
        Some(at) => format!("{}<redacted>{}", &url[..authority_start], &rest[at..]),
        None => url.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum StorageConfig {
    Disk {
        path: PathBuf,
        /// How much the store may hold, for the admin's storage overview. Unset: none is shown (the overview has the disk's own size besides).
        #[serde(default)]
        capacity: Option<Capacity>,
        #[serde(default)]
        compression: Compression,
        #[serde(default)]
        compression_level: CompressionLevel,
        #[serde(default)]
        compression_min_saving: MinSaving,
    },
    S3 {
        /// How much the bucket may hold, for the admin's storage overview. S3 has no size to ask for, so unset: none is shown.
        #[serde(default)]
        capacity: Option<Capacity>,
        bucket: String,
        #[serde(default)]
        prefix: String,
        #[serde(default = "default_region")]
        region: String,
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default)]
        access_key_id: Option<String>,
        #[serde(default)]
        secret_access_key: Option<Secret>,
        #[serde(default)]
        allow_http: bool,
        #[serde(default)]
        compression: Compression,
        #[serde(default)]
        compression_level: CompressionLevel,
        #[serde(default)]
        compression_min_saving: MinSaving,
    },
    /// Blobs on a file server reached over SFTP (needs the `sftp` feature, on in the default build).
    Sftp {
        /// How much the remote directory may hold, for the admin's storage overview. SFTP has no portable free-space
        /// query, so unset: none is shown.
        #[serde(default)]
        capacity: Option<Capacity>,
        sftp_host: String,
        #[serde(default = "default_sftp_port")]
        sftp_port: u16,
        sftp_user: String,
        /// The directory that belongs to Craft Cloud: absolute, or relative to the account's home directory.
        sftp_root: String,
        /// The private key (Ed25519 or ECDSA). A relative path is relative to the directory of `config.toml`. Unset:
        /// the `CRAFT_CLOUD_SFTP_KEY_FILE` environment variable.
        #[serde(default)]
        sftp_key_file: Option<PathBuf>,
        /// The key's passphrase, if it has one. Unset: the `CRAFT_CLOUD_SFTP_KEY_PASSPHRASE` environment variable, if set.
        #[serde(default)]
        sftp_key_passphrase: Option<Secret>,
        /// The server's host key: a `SHA256:…` fingerprint or a public key line. The connection is refused when the
        /// server presents another key; there is no mode without a pinned key.
        #[serde(default)]
        sftp_host_key: Option<String>,
        /// A `known_hosts` file whose entries for the host are pinned. A relative path is relative to `config.toml`.
        #[serde(default)]
        sftp_known_hosts: Option<PathBuf>,
        /// Connections open at once, 1 to 16 (default 4).
        #[serde(default)]
        sftp_connections: Option<usize>,
        /// Seconds a connect or a request may take, 1 to 300 (default 30).
        #[serde(default)]
        sftp_timeout_secs: Option<u64>,
        /// Retries of a request that failed for a reason that may pass, 0 to 8 (default 3).
        #[serde(default)]
        sftp_retries: Option<u32>,
        #[serde(default)]
        compression: Compression,
        #[serde(default)]
        compression_level: CompressionLevel,
        #[serde(default)]
        compression_min_saving: MinSaving,
    },
}

fn default_sftp_port() -> u16 {
    22
}

/// `[backup]`: where `backup` copies the server to and how it signs in. Every key can also be given on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    /// `sftp://user@host[:port]/directory` (`/~/directory` for one in the account's home).
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    #[serde(default)]
    pub key_passphrase: Option<Secret>,
    #[serde(default)]
    pub host_key: Option<String>,
    #[serde(default)]
    pub known_hosts: Option<PathBuf>,
    /// How many database generations stay on the backup server (default 7).
    #[serde(default)]
    pub keep: Option<u32>,
}

/// A size in bytes written as `"1 TiB"`: a whole number or one with one decimal, then B, KB, MB, GB, TB (powers of 1000)
/// or KiB, MiB, GiB, TiB (powers of 1024), in any case, with or without a space. Or a plain positive integer, in bytes.
/// At least one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity(pub u64);

impl Capacity {
    /// Reads `text` as a size, or says what is wrong with it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "storage.capacity = {text:?} is not a size like \"1 TiB\" or \"500 GB\" (a number with at most one decimal, then B, KB, MB, GB, TB, KiB, MiB, GiB or TiB)"
            )
        };
        let trimmed = text.trim();
        // A plain whole number is bytes.
        if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
            let bytes: u64 = trimmed.parse().map_err(|_| format!("storage.capacity = {text:?} is too big (the most is about 16 EiB)"))?;
            return if bytes == 0 {
                Err(format!("storage.capacity = {text:?} is empty: give the size the store may hold, like \"1 TiB\""))
            } else {
                Ok(Self(bytes))
            };
        }
        let split = trimmed.find(|c: char| !(c.is_ascii_digit() || c == '.')).ok_or_else(bad)?;
        let (number, unit) = trimmed.split_at(split);
        let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
            "b" => 1,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "tb" => 1_000_000_000_000,
            "kib" => 1 << 10,
            "mib" => 1 << 20,
            "gib" => 1 << 30,
            "tib" => 1 << 40,
            _ => return Err(bad()),
        };
        let (whole, tenth) = match number.split_once('.') {
            Some((whole, tenth)) if tenth.len() == 1 => (whole, tenth),
            Some(_) => return Err(bad()),
            None => (number, "0"),
        };
        if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !tenth.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let whole: u64 = whole.parse().map_err(|_| bad())?;
        let tenth: u64 = tenth.parse().map_err(|_| bad())?;
        // The decimal is a tenth of the unit, rounded down to a byte.
        let bytes = whole
            .checked_mul(multiplier)
            .and_then(|b| b.checked_add(multiplier / 10 * tenth + (multiplier % 10) * tenth / 10))
            .ok_or_else(|| format!("storage.capacity = {text:?} is too big (the most is about 16 EiB)"))?;
        if bytes == 0 {
            return Err(format!("storage.capacity = {text:?} is empty: give the size the store may hold, like \"1 TiB\""));
        }
        Ok(Self(bytes))
    }
}

impl<'de> Deserialize<'de> for Capacity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Capacity;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a size like \"1 TiB\" or a number of bytes")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Capacity, E> {
                Capacity::parse(text).map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, bytes: u64) -> Result<Capacity, E> {
                Capacity::parse(&bytes.to_string()).map_err(E::custom)
            }

            fn visit_i64<E: serde::de::Error>(self, bytes: i64) -> Result<Capacity, E> {
                Capacity::parse(&bytes.to_string()).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// The zstd level of new writes (`compression_level` in `[storage]`): 1 to 19, 3 by default. 20 to 22 are refused:
/// they use windows the store's reader does not accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionLevel(pub u8);

impl Default for CompressionLevel {
    fn default() -> Self {
        Self(craft_cloud_store::DEFAULT_ZSTD_LEVEL)
    }
}

impl<'de> Deserialize<'de> for CompressionLevel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = i64::deserialize(deserializer)?;
        match u8::try_from(n) {
            Ok(level) if (craft_cloud_store::MIN_ZSTD_LEVEL..=craft_cloud_store::MAX_ZSTD_LEVEL).contains(&level) => Ok(Self(level)),
            _ => Err(serde::de::Error::custom(format!(
                "compression_level = {n} is not a zstd level this server writes: use a whole number from {} to {}",
                craft_cloud_store::MIN_ZSTD_LEVEL,
                craft_cloud_store::MAX_ZSTD_LEVEL
            ))),
        }
    }
}

/// How much smaller a compressed blob must be to be kept (`compression_min_saving` in `[storage]`): 1 to 50 percent,
/// 5 by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinSaving(pub u8);

impl Default for MinSaving {
    fn default() -> Self {
        Self(craft_cloud_store::DEFAULT_MIN_SAVING_PERCENT)
    }
}

impl<'de> Deserialize<'de> for MinSaving {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = i64::deserialize(deserializer)?;
        match u8::try_from(n) {
            Ok(percent) if (1..=50).contains(&percent) => Ok(Self(percent)),
            _ => Err(serde::de::Error::custom(format!("compression_min_saving = {n} is not a percentage from 1 to 50"))),
        }
    }
}

/// How blobs are written to storage (`compression` in `[storage]`). Reading understands both either way, so turning
/// it off later leaves compressed blobs readable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    /// zstd where it saves at least 5% (the default).
    #[default]
    Zstd,
    /// Every blob plain.
    None,
}

impl From<Compression> for craft_cloud_store::Compression {
    fn from(compression: Compression) -> Self {
        match compression {
            Compression::Zstd => craft_cloud_store::Compression::Zstd,
            Compression::None => craft_cloud_store::Compression::None,
        }
    }
}

fn default_region() -> String {
    "us-east-1".to_owned()
}

/// A string that never shows in `Debug` output.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.to_owned(), source })?;
        let mut config = Self::parse(&text)?;
        // A relative path to the config file has a parent too, possibly empty: that is the working directory.
        config.dir = Some(path.parent().map(Path::to_owned).unwrap_or_default());
        Ok(config)
    }

    /// `path` of a `[two_factor]` key file as the server opens it: relative paths are relative to the config file's
    /// directory (when the config was loaded from a file).
    pub fn key_path(&self, path: &Path) -> PathBuf {
        match (&self.dir, path.is_absolute()) {
            (Some(dir), false) => dir.join(path),
            _ => path.to_owned(),
        }
    }

    /// The SFTP settings of `storage.kind = "sftp"`, with the key from the config or the environment
    /// (`CRAFT_CLOUD_SFTP_KEY_FILE`, `CRAFT_CLOUD_SFTP_KEY_PASSPHRASE`). `None` for another kind of storage.
    #[cfg(feature = "sftp")]
    pub fn sftp_storage(&self) -> Result<Option<craft_cloud_store::sftp::SftpConfig>, ConfigError> {
        use craft_cloud_store::sftp::{DEFAULT_CONNECTIONS, DEFAULT_RETRIES, DEFAULT_TIMEOUT_SECS, SftpConfig};
        let StorageConfig::Sftp {
            sftp_host,
            sftp_port,
            sftp_user,
            sftp_root,
            sftp_key_file,
            sftp_key_passphrase,
            sftp_host_key,
            sftp_known_hosts,
            sftp_connections,
            sftp_timeout_secs,
            sftp_retries,
            ..
        } = &self.storage
        else {
            return Ok(None);
        };
        let key_file = match sftp_key_file {
            Some(path) => self.key_path(path),
            None => PathBuf::from(
                std::env::var_os("CRAFT_CLOUD_SFTP_KEY_FILE").ok_or(ConfigError::MissingSftpSetting("sftp_key_file", "CRAFT_CLOUD_SFTP_KEY_FILE"))?,
            ),
        };
        let key_passphrase = match sftp_key_passphrase {
            Some(secret) => Some(secret.0.clone()),
            None => std::env::var("CRAFT_CLOUD_SFTP_KEY_PASSPHRASE").ok(),
        };
        Ok(Some(SftpConfig {
            host: sftp_host.clone(),
            port: *sftp_port,
            user: sftp_user.clone(),
            root: sftp_root.clone(),
            key_file,
            key_passphrase,
            host_key: sftp_host_key.clone(),
            known_hosts: sftp_known_hosts.as_ref().map(|path| self.key_path(path)),
            connections: sftp_connections.unwrap_or(DEFAULT_CONNECTIONS),
            timeout: std::time::Duration::from_secs(sftp_timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
            retries: sftp_retries.unwrap_or(DEFAULT_RETRIES),
        }))
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(describe_toml_error(text, &e)))?;
        if !config.behind_proxy && !config.listen.ip().is_loopback() {
            return Err(ConfigError::NotLoopback(config.listen));
        }
        if config.behind_proxy && config.trusted_proxies.is_empty() && !config.ignore_forwarded_for {
            return Err(ConfigError::NoTrustedProxies);
        }
        // A setting whose name says more than it does is refused rather than half-honoured.
        if config.ignore_forwarded_for && (!config.behind_proxy || !config.trusted_proxies.is_empty()) {
            return Err(ConfigError::IgnoreForwardedForMisplaced);
        }
        // The agent counts less than one chunk of room as a full account (it pauses instead of skipping the file),
        // so a cap under that would make every refusal account-level.
        if let Some(cap) = config.stored_bytes_per_user
            && cap < u64::from(MAX_CHUNK_BYTES)
        {
            return Err(ConfigError::StoredBytesCapTooSmall(cap, MAX_CHUNK_BYTES));
        }
        if !(craft_cloud_meta::MIN_RETENTION_DAYS..=craft_cloud_meta::MAX_RETENTION_DAYS).contains(&config.audit.retention_days) {
            return Err(ConfigError::AuditRetention(
                config.audit.retention_days,
                craft_cloud_meta::MIN_RETENTION_DAYS,
                craft_cloud_meta::MAX_RETENTION_DAYS,
            ));
        }
        let web = &config.web_admin;
        for (name, value, low, high) in [("idle_minutes", web.idle_minutes, 5, 1440), ("absolute_hours", web.absolute_hours, 1, 720)] {
            if !(low..=high).contains(&value) {
                return Err(ConfigError::WebAdminRange(name, value, low, high));
            }
        }
        if !(MIN_TRASH_DAYS..=MAX_TRASH_DAYS).contains(&config.retention.trash_days) {
            return Err(ConfigError::TrashDays(config.retention.trash_days));
        }
        config.storage.check_sftp()?;
        let mut config = config;
        if let Some(url) = &config.public_url {
            config.public_url = Some(public_url(url)?);
        }
        if config.web_admin.enabled {
            // The pages' address is an origin: https, a host and maybe a port, and no path to put in front of `/web`.
            let origin_only = config.public_url.as_deref().and_then(|url| url.strip_prefix("https://")).is_some_and(|rest| !rest.contains('/'));
            if !origin_only {
                return Err(ConfigError::WebAdminPublicUrl);
            }
            if !config.behind_proxy {
                return Err(ConfigError::WebAdminNeedsProxy);
            }
        }
        Ok(config)
    }
}

/// `public_url` checked and normalised as a join string's server part (so what `user invite` prints parses back).
fn public_url(url: &str) -> Result<String, ConfigError> {
    let url = url.trim();
    let refused = || ConfigError::PublicUrl(url.to_owned());
    if !url.to_ascii_lowercase().starts_with("https://") && !url.to_ascii_lowercase().starts_with("http://") {
        return Err(refused());
    }
    let probe = Code::parse("00000000", PAIRING_CHARS).ok_or_else(refused)?;
    let parsed = JoinString::parse(&format!("{}/{}", url.trim_end_matches('/'), probe.as_str())).map_err(|_| refused())?;
    Ok(parsed.server)
}

/// The parser's message with a line number, without the source text.
fn describe_toml_error(text: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            let line = text.get(..span.start).unwrap_or("").matches('\n').count() + 1;
            format!("{} (line {line})", e.message())
        }
        None => e.message().to_owned(),
    }
}

impl StorageConfig {
    /// The size the config says the store may hold, in bytes.
    pub fn capacity(&self) -> Option<u64> {
        match self {
            StorageConfig::Disk { capacity, .. } | StorageConfig::S3 { capacity, .. } | StorageConfig::Sftp { capacity, .. } => capacity.map(|c| c.0),
        }
    }

    /// How blobs are written, whichever backend holds them.
    pub fn compression(&self) -> Compression {
        match self {
            StorageConfig::Disk { compression, .. } | StorageConfig::S3 { compression, .. } | StorageConfig::Sftp { compression, .. } => *compression,
        }
    }

    /// The zstd level new writes use unless an admin changed it while the server runs.
    pub fn compression_level(&self) -> u8 {
        match self {
            StorageConfig::Disk { compression_level, .. }
            | StorageConfig::S3 { compression_level, .. }
            | StorageConfig::Sftp { compression_level, .. } => compression_level.0,
        }
    }

    /// How much smaller (percent) a compressed blob must be to be kept.
    pub fn compression_min_saving(&self) -> u8 {
        match self {
            StorageConfig::Disk { compression_min_saving, .. }
            | StorageConfig::S3 { compression_min_saving, .. }
            | StorageConfig::Sftp { compression_min_saving, .. } => compression_min_saving.0,
        }
    }

    /// The SFTP settings are in range and name a host key to pin. A missing key file is found when the store opens, since
    /// the environment may supply it.
    fn check_sftp(&self) -> Result<(), ConfigError> {
        let StorageConfig::Sftp { sftp_host, sftp_user, sftp_host_key, sftp_known_hosts, sftp_connections, sftp_timeout_secs, sftp_retries, .. } =
            self
        else {
            return Ok(());
        };
        let refuse = |message: String| Err(ConfigError::Sftp(message));
        if sftp_host.trim().is_empty() || sftp_user.trim().is_empty() {
            return refuse("storage.sftp_host and storage.sftp_user must not be empty".into());
        }
        if sftp_host_key.is_none() && sftp_known_hosts.is_none() {
            return refuse(
                "storage.kind = \"sftp\" needs a pinned host key: set sftp_host_key (a SHA256: fingerprint or a public key line) or sftp_known_hosts.                  There is no mode that connects without checking the server"
                    .into(),
            );
        }
        if let Some(n) = sftp_connections
            && !(1..=SFTP_MAX_CONNECTIONS).contains(n)
        {
            return refuse(format!("storage.sftp_connections = {n} is out of range: 1 to {SFTP_MAX_CONNECTIONS}"));
        }
        if let Some(n) = sftp_timeout_secs
            && !(1..=SFTP_MAX_TIMEOUT_SECS).contains(n)
        {
            return refuse(format!("storage.sftp_timeout_secs = {n} is out of range: 1 to {SFTP_MAX_TIMEOUT_SECS}"));
        }
        if let Some(n) = sftp_retries
            && *n > SFTP_MAX_RETRIES
        {
            return refuse(format!("storage.sftp_retries = {n} is out of range: 0 to {SFTP_MAX_RETRIES}"));
        }
        Ok(())
    }

    /// The S3 settings, with credentials from the config or else the standard `AWS_ACCESS_KEY_ID` /
    /// `AWS_SECRET_ACCESS_KEY` environment variables. `None` for disk storage.
    pub fn s3(&self) -> Result<Option<S3Config>, ConfigError> {
        let StorageConfig::S3 { bucket, prefix, region, endpoint, access_key_id, secret_access_key, allow_http, .. } = self else {
            return Ok(None);
        };
        let access_key_id = match access_key_id {
            Some(key) => key.clone(),
            None => std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| ConfigError::MissingS3Credential("access_key_id", "AWS_ACCESS_KEY_ID"))?,
        };
        let secret_access_key = match secret_access_key {
            Some(secret) => secret.0.clone(),
            None => {
                std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| ConfigError::MissingS3Credential("secret_access_key", "AWS_SECRET_ACCESS_KEY"))?
            }
        };
        Ok(Some(S3Config {
            bucket: bucket.clone(),
            prefix: prefix.clone(),
            region: region.clone(),
            endpoint: endpoint.clone(),
            access_key_id,
            secret_access_key,
            allow_http: *allow_http,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISK: &str = r#"
listen = "127.0.0.1:8080"
[database]
url = "sqlite:/tmp/meta.db"
[storage]
kind = "disk"
path = "/tmp/blobs"
"#;

    #[test]
    fn a_disk_config_parses() {
        let c = Config::parse(DISK).unwrap();
        assert_eq!(c.listen, "127.0.0.1:8080".parse().unwrap());
        assert!(!c.behind_proxy && c.trusted_proxies.is_empty());
        assert_eq!(
            c.storage,
            StorageConfig::Disk {
                path: "/tmp/blobs".into(),
                capacity: None,
                compression: Compression::Zstd,
                compression_level: CompressionLevel(3),
                compression_min_saving: MinSaving(5)
            }
        );
        assert_eq!(c.storage.s3().unwrap(), None);
        assert_eq!(c.public_url, None);
    }

    #[test]
    fn the_capacity_is_optional_and_reads_sizes() {
        assert_eq!(Config::parse(DISK).unwrap().storage.capacity(), None, "a config from before the option still loads");
        let with = |value: &str| {
            Config::parse(&DISK.replace(
                "path = \"/tmp/blobs\"",
                &format!(
                    "path = \"/tmp/blobs\"
capacity = {value}"
                ),
            ))
        };
        let bytes = |value: &str| with(value).unwrap().storage.capacity();
        assert_eq!(bytes("\"1 TiB\""), Some(1 << 40));
        assert_eq!(bytes("1000"), Some(1000), "an integer is bytes");
        assert_eq!(bytes("\"1000\""), Some(1000));
        assert_eq!(bytes("1_099_511_627_776"), Some(1 << 40));
        assert_eq!(bytes("\"1TiB\""), Some(1 << 40));
        assert_eq!(bytes("\"500 gb\""), Some(500_000_000_000));
        assert_eq!(bytes("\"2 TB\""), Some(2_000_000_000_000));
        assert_eq!(bytes("\"1.5 TB\""), Some(1_500_000_000_000));
        assert_eq!(bytes("\"0.5 GiB\""), Some(1 << 29));
        assert_eq!(bytes("\"1.5 KiB\""), Some(1536));
        assert_eq!(bytes("\"64 B\""), Some(64));
        assert_eq!(bytes("\"3 KB\""), Some(3000));
        assert_eq!(bytes("\"2 MiB\""), Some(2 << 20));
        assert_eq!(bytes("\"1 MB\""), Some(1_000_000));
        assert_eq!(bytes("\"1 KiB\""), Some(1024));
        assert_eq!(bytes("\"1 GB\""), Some(1_000_000_000));
        assert_eq!(bytes("\"1 GiB\""), Some(1 << 30));
        for bad in [
            "\"\"",
            "\"1.5\"",
            "0",
            "-5",
            "1.5",
            "\"-5\"",
            "\"TiB\"",
            "\"1 XB\"",
            "\"1.25 TB\"",
            "\"1. TB\"",
            "\".5 TB\"",
            "\"-1 TB\"",
            "\"1e3 GB\"",
            "\"0 B\"",
            "\"0.0 TB\"",
            "\"99999999 TiB\"",
            "true",
        ] {
            assert!(matches!(with(bad), Err(ConfigError::Parse(_))), "{bad}");
        }
        let shown = with("\"1 XB\"").unwrap_err().to_string();
        assert!(shown.contains("storage.capacity") && shown.contains("1 XB") && shown.contains("line"), "{shown}");
        // S3 takes it too; without it S3 shows no size.
        let s3 = "listen = \"127.0.0.1:8080\"
[database]
url = \"sqlite:/tmp/meta.db\"
[storage]
kind = \"s3\"
bucket = \"b\"
";
        assert_eq!(Config::parse(s3).unwrap().storage.capacity(), None);
        assert_eq!(
            Config::parse(&format!(
                "{s3}capacity = \"10 TB\"
"
            ))
            .unwrap()
            .storage
            .capacity(),
            Some(10_000_000_000_000)
        );
    }

    #[test]
    fn the_public_url_is_normalised_or_refused() {
        let with = |url: &str| Config::parse(&format!("public_url = {url:?}\n{DISK}"));
        assert_eq!(with("https://Cloud.Example.com/").unwrap().public_url.as_deref(), Some("https://cloud.example.com"));
        assert_eq!(with(" https://example.org:8443/cloud ").unwrap().public_url.as_deref(), Some("https://example.org:8443/cloud"));
        for bad in ["cloud.example.com", "ftp://x", "https://", "https://a b", "https://x/?q=1", "https://ana@x"] {
            assert!(matches!(with(bad), Err(ConfigError::PublicUrl(_))), "{bad}");
        }
    }

    #[test]
    fn plain_http_on_a_public_address_needs_behind_proxy() {
        let public = DISK.replace("127.0.0.1:8080", "0.0.0.0:8080");
        assert!(matches!(Config::parse(&public), Err(ConfigError::NotLoopback(_))));
        let proxied = format!("behind_proxy = true\ntrusted_proxies = [\"10.0.0.1\"]\n{public}");
        let c = Config::parse(&proxied).unwrap();
        assert_eq!(c.trusted_proxies, vec!["10.0.0.1".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn the_stored_bytes_cap_is_off_unless_set() {
        assert_eq!(Config::parse(DISK).unwrap().stored_bytes_per_user, None);
        let capped = format!("stored_bytes_per_user = 2_000_000_000_000\n{DISK}");
        assert_eq!(Config::parse(&capped).unwrap().stored_bytes_per_user, Some(2_000_000_000_000));
        assert!(matches!(Config::parse(&format!("stored_bytes_per_user = -1\n{DISK}")), Err(ConfigError::Parse(_))));
        // Under one chunk the agent would count every refusal as a full account: refused at load.
        assert!(matches!(Config::parse(&format!("stored_bytes_per_user = 4194303\n{DISK}")), Err(ConfigError::StoredBytesCapTooSmall(4_194_303, _))));
        assert_eq!(Config::parse(&format!("stored_bytes_per_user = 4194304\n{DISK}")).unwrap().stored_bytes_per_user, Some(4_194_304));
        assert_eq!(Config::parse(DISK).unwrap().live_bytes_per_user, 1 << 30, "1 GiB by default");
        assert_eq!(Config::parse(&format!("live_bytes_per_user = 5_000_000\n{DISK}")).unwrap().live_bytes_per_user, 5_000_000);
        assert!(matches!(Config::parse(&format!("live_bytes_per_user = -1\n{DISK}")), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn behind_a_proxy_the_proxy_must_be_named_or_ignoring_it_said_out_loud() {
        let unnamed = format!("behind_proxy = true\n{DISK}");
        assert!(matches!(Config::parse(&unnamed), Err(ConfigError::NoTrustedProxies)));
        let said = format!("behind_proxy = true\nignore_forwarded_for = true\n{DISK}");
        let c = Config::parse(&said).unwrap();
        assert!(c.behind_proxy && c.ignore_forwarded_for && c.trusted_proxies.is_empty());
        // Without a proxy there is nothing to name.
        assert!(!Config::parse(DISK).unwrap().ignore_forwarded_for);
    }

    #[test]
    fn ignore_forwarded_for_is_refused_where_it_would_not_do_what_it_says() {
        // Named proxies' X-Forwarded-For is believed, so the flag would ignore nothing.
        let named = format!("behind_proxy = true\nignore_forwarded_for = true\ntrusted_proxies = [\"10.0.0.1\"]\n{DISK}");
        assert!(matches!(Config::parse(&named), Err(ConfigError::IgnoreForwardedForMisplaced)));
        // Without a proxy there is nothing to ignore.
        let no_proxy = format!("ignore_forwarded_for = true\n{DISK}");
        assert!(matches!(Config::parse(&no_proxy), Err(ConfigError::IgnoreForwardedForMisplaced)));
    }

    #[test]
    fn the_audit_section_is_optional_and_checked() {
        assert_eq!(Config::parse(DISK).unwrap().audit.retention_days, 365, "a year by default");
        let with = |section: &str| Config::parse(&format!("{DISK}\n{section}\n"));
        assert_eq!(with("[audit]").unwrap().audit.retention_days, 365);
        assert_eq!(with("[audit]\nretention_days = 30").unwrap().audit.retention_days, 30);
        for days in [1, 3650] {
            assert_eq!(with(&format!("[audit]\nretention_days = {days}")).unwrap().audit.retention_days, days);
        }
        for days in ["0", "3651", "-1", "\"30\"", "1.5"] {
            assert!(with(&format!("[audit]\nretention_days = {days}")).is_err(), "{days}");
        }
        assert!(matches!(with("[audit]\nretention_days = 0"), Err(ConfigError::AuditRetention(0, 1, 3650))));
        assert!(matches!(with("[audit]\nkeep = 5"), Err(ConfigError::Parse(_))), "unknown keys are refused");
    }

    #[test]
    fn the_web_admin_is_off_by_default_and_checked_when_on() {
        let c = Config::parse(DISK).unwrap();
        assert_eq!(c.web_admin, WebAdminConfig { enabled: false, idle_minutes: 480, absolute_hours: 168 }, "off, 8 hours idle, 7 days at most");
        assert_eq!((c.web_admin.idle_ms(), c.web_admin.absolute_ms()), (8 * 3_600_000, 7 * 24 * 3_600_000));
        let proxied = DISK.replace("127.0.0.1:8080", "0.0.0.0:8080");
        let with = |top: &str, section: &str| Config::parse(&format!("{top}\n{proxied}\n[web_admin]\n{section}\n"));
        let proxy = "behind_proxy = true\ntrusted_proxies = [\"10.0.0.1\"]";
        let good = format!("{proxy}\npublic_url = \"https://cloud.example.com\"");
        let c = with(&good, "enabled = true\nidle_minutes = 30\nabsolute_hours = 12").unwrap();
        assert_eq!((c.web_admin.enabled, c.web_admin.idle_minutes, c.web_admin.absolute_hours), (true, 30, 12));
        // Each missing piece of the setup is refused with its own message.
        assert!(matches!(with(proxy, "enabled = true"), Err(ConfigError::WebAdminPublicUrl)), "no public_url");
        for url in ["http://cloud.example.com", "https://cloud.example.com/cloud"] {
            let bad = format!("{proxy}\npublic_url = \"{url}\"");
            assert!(matches!(with(&bad, "enabled = true"), Err(ConfigError::WebAdminPublicUrl)), "{url}");
        }
        let no_proxy = format!("public_url = \"https://cloud.example.com\"\n{DISK}\n[web_admin]\nenabled = true\n");
        assert!(matches!(Config::parse(&no_proxy), Err(ConfigError::WebAdminNeedsProxy)));
        // Off, none of it matters; the lifetimes are always in range; unknown keys are errors.
        assert!(!Config::parse(&format!("{DISK}\n[web_admin]\nenabled = false\n")).unwrap().web_admin.enabled);
        for section in ["idle_minutes = 4", "idle_minutes = 1441", "absolute_hours = 0", "absolute_hours = 721"] {
            assert!(matches!(Config::parse(&format!("{DISK}\n[web_admin]\n{section}\n")), Err(ConfigError::WebAdminRange(..))), "{section}");
        }
        assert!(matches!(Config::parse(&format!("{DISK}\n[web_admin]\ncookie = 1\n")), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn the_trash_period_has_a_default_and_bounds() {
        assert_eq!(Config::parse(DISK).unwrap().retention, RetentionConfig { trash_days: 30 });
        // A table has to come after the top-level keys, so the section goes at the end.
        let at_end = |days: &str| Config::parse(&format!("{DISK}\n[retention]\ntrash_days = {days}\n"));
        assert_eq!(at_end("1").unwrap().retention.trash_days, 1);
        assert_eq!(at_end("3650").unwrap().retention.trash_days, 3650);
        for bad in ["0", "3651", "-1", "\"30\"", "1.5"] {
            assert!(at_end(bad).is_err(), "{bad}");
        }
        assert!(matches!(at_end("0"), Err(ConfigError::TrashDays(0))));
        assert!(matches!(at_end("3651"), Err(ConfigError::TrashDays(3651))));
        // Nothing but `trash_days` is accepted in the section, and the section may be empty.
        assert!(matches!(Config::parse(&format!("{DISK}\n[retention]\ngc_wait_hours = 24\n")), Err(ConfigError::Parse(_))));
        assert_eq!(Config::parse(&format!("{DISK}\n[retention]\n")).unwrap().retention.trash_days, 30);
    }

    #[test]
    fn compression_is_zstd_unless_turned_off() {
        assert_eq!(Config::parse(DISK).unwrap().storage.compression(), Compression::Zstd);
        let with = |value: &str| {
            Config::parse(&format!(
                "{DISK}compression = {value}
"
            ))
        };
        assert_eq!(with("\"zstd\"").unwrap().storage.compression(), Compression::Zstd);
        assert_eq!(with("\"none\"").unwrap().storage.compression(), Compression::None);
        assert_eq!(craft_cloud_store::Compression::from(Compression::None), craft_cloud_store::Compression::None);
        let Err(ConfigError::Parse(message)) = with("\"gzip\"") else { panic!("an unknown compression must be refused") };
        assert!(message.contains("gzip") && message.contains("zstd") && message.contains("none"), "{message}");
        assert!(message.contains("line"), "{message}");
        let s3 = "listen = \"127.0.0.1:9000\"
[database]
url = \"sqlite:/tmp/m.db\"
[storage]
kind = \"s3\"
bucket = \"b\"
compression = \"none\"
";
        assert_eq!(Config::parse(s3).unwrap().storage.compression(), Compression::None);
    }

    #[test]
    fn the_compression_level_and_min_saving_have_defaults_and_ranges() {
        let c = Config::parse(DISK).unwrap();
        assert_eq!((c.storage.compression_level(), c.storage.compression_min_saving()), (3, 5));
        let with = |lines: &str| {
            Config::parse(&format!(
                "{DISK}{lines}
"
            ))
        };
        assert_eq!(with("compression_level = 1").unwrap().storage.compression_level(), 1);
        assert_eq!(with("compression_level = 19").unwrap().storage.compression_level(), 19);
        assert_eq!(with("compression_min_saving = 50").unwrap().storage.compression_min_saving(), 50);
        // The level and "none" are independent settings: "none" is read as "none" whatever the level.
        let both = with(
            "compression = \"none\"
compression_level = 19",
        )
        .unwrap();
        assert_eq!((both.storage.compression(), both.storage.compression_level()), (Compression::None, 19));
        for bad in [
            "compression_level = 0",
            "compression_level = 20",
            "compression_level = 22",
            "compression_level = -3",
            "compression_level = 3.5",
            "compression_level = \"high\"",
            "compression_level = 300",
        ] {
            let Err(ConfigError::Parse(message)) = with(bad) else { panic!("{bad} must be refused") };
            assert!(message.contains("line"), "{bad}: {message}");
        }
        let Err(ConfigError::Parse(message)) = with("compression_level = 25") else { panic!("refused") };
        assert!(message.contains("1 to 19") && message.contains("25"), "{message}");
        for bad in ["compression_min_saving = 0", "compression_min_saving = 51", "compression_min_saving = -1"] {
            let Err(ConfigError::Parse(message)) = with(bad) else { panic!("{bad} must be refused") };
            assert!(message.contains("line") && message.contains("1 to 50"), "{bad}: {message}");
        }
        let s3 = "listen = \"127.0.0.1:9000\"
[database]
url = \"sqlite:/tmp/m.db\"
[storage]
kind = \"s3\"
bucket = \"b\"
compression_level = 9
compression_min_saving = 10
";
        let c = Config::parse(s3).unwrap();
        assert_eq!((c.storage.compression_level(), c.storage.compression_min_saving()), (9, 10));
    }

    #[test]
    fn unknown_keys_are_errors() {
        assert!(matches!(Config::parse(&format!("lisen = 1\n{DISK}")), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn the_two_factor_section_is_optional_and_strict() {
        assert_eq!(Config::parse(DISK).unwrap().two_factor, None, "a config from before two-factor sign-in still loads");
        let with = |section: &str| Config::parse(&format!("{DISK}\n[two_factor]\n{section}\n"));
        let c = with("key_file = \"two-factor.key\"").unwrap();
        assert_eq!(c.two_factor, Some(TwoFactorConfig { key_file: "two-factor.key".into(), key_file_old: None }));
        let c = with("key_file = \"new.key\"\nkey_file_old = \"old.key\"").unwrap();
        assert_eq!(c.two_factor.unwrap().key_file_old, Some("old.key".into()));
        assert!(matches!(with("key_file = \"k\"\nkey = \"x\""), Err(ConfigError::Parse(_))), "an unknown key in the section");
        assert!(matches!(with("key_file_old = \"old.key\""), Err(ConfigError::Parse(_))), "key_file is required");
    }

    #[test]
    fn key_files_are_relative_to_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, format!("{DISK}\n[two_factor]\nkey_file = \"two-factor.key\"\n")).unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.key_path(Path::new("two-factor.key")), dir.path().join("two-factor.key"));
        let absolute = dir.path().join("elsewhere.key");
        assert_eq!(c.key_path(&absolute), absolute);
        // A config that was parsed from text has no directory: the path stays as it is.
        assert_eq!(Config::parse(DISK).unwrap().key_path(Path::new("k")), PathBuf::from("k"));
    }

    #[test]
    fn s3_config_hides_its_secret() {
        let text = r#"
listen = "[::1]:9000"
[database]
url = "postgres://cc:topsecret@db/cc"
[storage]
kind = "s3"
bucket = "blobs"
access_key_id = "AKIA"
secret_access_key = "very-secret-value"
"#;
        let c = Config::parse(text).unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("very-secret-value"));
        assert!(!shown.contains("topsecret"));
        assert!(shown.contains("postgres://<redacted>@db/cc"));
        let s3 = c.storage.s3().unwrap().unwrap();
        assert_eq!((s3.bucket.as_str(), s3.region.as_str(), s3.secret_access_key.as_str()), ("blobs", "us-east-1", "very-secret-value"));
    }

    const SFTP: &str = r#"
listen = "127.0.0.1:8080"
[database]
url = "sqlite:/tmp/meta.db"
[storage]
kind = "sftp"
sftp_host = "files.example.org"
sftp_user = "craft"
sftp_root = "/craft/blobs"
sftp_host_key = "SHA256:abc"
"#;

    #[test]
    fn sftp_storage_needs_a_host_key_and_has_safe_defaults() {
        let c = Config::parse(SFTP).unwrap();
        assert_eq!(c.storage.capacity(), None);
        assert_eq!((c.storage.compression_level(), c.storage.compression_min_saving()), (3, 5));
        assert!(matches!(c.storage, StorageConfig::Sftp { sftp_port: 22, sftp_connections: None, .. }));
        // Without a pinned host key there is no mode that connects: the config is refused, naming the setting.
        let unpinned = SFTP.replace("sftp_host_key = \"SHA256:abc\"\n", "");
        let Err(ConfigError::Sftp(message)) = Config::parse(&unpinned) else { panic!("an SFTP store without a pinned host key must be refused") };
        assert!(message.contains("sftp_host_key") && message.contains("sftp_known_hosts") && message.contains("no mode"), "{message}");
        assert!(Config::parse(&unpinned.replace("[storage]", "[storage]\nsftp_known_hosts = \"known_hosts\"")).is_ok());
        // Password sign-in does not exist, and an unknown key is a typo.
        assert!(matches!(Config::parse(&format!("{SFTP}sftp_password = \"x\"\n")), Err(ConfigError::Parse(_))));
        assert!(matches!(Config::parse(&SFTP.replace("sftp_user = \"craft\"\n", "")), Err(ConfigError::Parse(_))), "the user is required");
        for bad in ["sftp_connections = 0", "sftp_connections = 17", "sftp_timeout_secs = 0", "sftp_timeout_secs = 301", "sftp_retries = 9"] {
            assert!(matches!(Config::parse(&format!("{SFTP}{bad}\n")), Err(ConfigError::Sftp(_))), "{bad}");
        }
        for good in ["sftp_connections = 16", "sftp_timeout_secs = 300", "sftp_retries = 0", "capacity = \"4 TiB\"", "compression = \"none\""] {
            assert!(Config::parse(&format!("{SFTP}{good}\n")).is_ok(), "{good}");
        }
    }

    #[test]
    fn sftp_config_hides_its_passphrase() {
        let c = Config::parse(&format!("{SFTP}sftp_key_passphrase = \"correct-horse\"\n")).unwrap();
        assert!(!format!("{c:?}").contains("correct-horse"));
    }

    #[cfg(feature = "sftp")]
    #[test]
    fn sftp_settings_resolve_relative_paths_against_the_config_file() {
        let mut c = Config::parse(&format!(
            "{SFTP}sftp_key_file = \"keys/sftp\"\nsftp_known_hosts = \"hosts\"\nsftp_key_passphrase = \"pw\"\nsftp_retries = 5\n"
        ))
        .unwrap();
        c.dir = Some(PathBuf::from("/etc/craft"));
        let s = c.sftp_storage().unwrap().unwrap();
        assert_eq!((s.host.as_str(), s.port, s.user.as_str(), s.root.as_str()), ("files.example.org", 22, "craft", "/craft/blobs"));
        assert_eq!(s.key_file, Path::new("/etc/craft").join("keys/sftp"));
        assert_eq!(s.known_hosts, Some(Path::new("/etc/craft").join("hosts")));
        assert_eq!(s.key_passphrase.as_deref(), Some("pw"));
        assert_eq!(
            (s.connections, s.retries, s.timeout.as_secs()),
            (craft_cloud_store::sftp::DEFAULT_CONNECTIONS, 5, craft_cloud_store::sftp::DEFAULT_TIMEOUT_SECS)
        );
        assert!(Config::parse(DISK).unwrap().sftp_storage().unwrap().is_none());
    }

    #[cfg(feature = "sftp")]
    #[test]
    fn the_sftp_limits_here_are_the_store_crates() {
        assert_eq!(SFTP_MAX_CONNECTIONS, craft_cloud_store::sftp::MAX_CONNECTIONS);
        assert_eq!(SFTP_MAX_TIMEOUT_SECS, craft_cloud_store::sftp::MAX_TIMEOUT_SECS);
        assert_eq!(SFTP_MAX_RETRIES, craft_cloud_store::sftp::MAX_RETRIES);
    }

    #[test]
    fn urls_without_credentials_print_as_they_are() {
        assert_eq!(redact_url("sqlite:/var/lib/meta.db"), "sqlite:/var/lib/meta.db");
        assert_eq!(redact_url("postgres://db:5432/cc"), "postgres://db:5432/cc");
    }

    #[test]
    fn a_syntax_error_does_not_echo_the_secret_line() {
        let text = r#"
listen = "127.0.0.1:9000"
[database]
url = "sqlite:/tmp/meta.db"
[storage]
kind = "s3"
bucket = "blobs"
secret_access_key = "very-secret-value
"#;
        let err = Config::parse(text).unwrap_err();
        let shown = err.to_string();
        assert!(!shown.contains("very-secret-value"), "{shown}");
        assert!(shown.contains("line"), "{shown}");
    }
}
