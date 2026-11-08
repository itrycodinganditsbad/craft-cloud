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
//! # [spaces]                   # team spaces
//! # team_live_bytes = 4_294_967_296   # cap on what one team space's live documents hold, in bytes (4 GiB)
//! # [retention]
//! # trash_days = 30   # how long a deleted file stays in the trash before the server may remove it (1 to 3650)
//!
//! [database]
//! url = "sqlite:/var/lib/craft-cloud/meta.db"   # or postgres://user:pass@host/db
//!
//! [storage]
//! kind = "disk"
//! path = "/var/lib/craft-cloud/blobs"
//! # kind = "s3", bucket, prefix, region, endpoint, access_key_id, secret_access_key, allow_http
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
    /// Limits of team spaces. The section is optional.
    #[serde(default)]
    pub spaces: SpacesConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    /// The key for the secrets of two-factor sign-in. Without it two-factor sign-in can't be set up (`503
    /// two_factor_unavailable`), and a server whose database already holds secrets refuses to start. `init` makes the
    /// key file and writes this section. The server never makes the key itself: a second server on the same database,
    /// or a restore without the key, would otherwise seal secrets nobody can open.
    #[serde(default)]
    pub two_factor: Option<TwoFactorConfig>,
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

/// `[spaces]`: limits of team spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacesConfig {
    /// The most bytes one team space's live documents may hold together, as `live_bytes_per_user` does for a personal
    /// space: every update in their logs, compacted ones too. An update that would go over it is refused
    /// (`quota_exceeded`). Default 4 GiB.
    #[serde(default = "default_team_live_bytes")]
    pub team_live_bytes: u64,
}

impl Default for SpacesConfig {
    fn default() -> Self {
        Self { team_live_bytes: default_team_live_bytes() }
    }
}

fn default_team_live_bytes() -> u64 {
    craft_cloud_meta::DEFAULT_TEAM_LIVE_BYTES
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
    },
    S3 {
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
    },
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
    /// The S3 settings, with credentials from the config or else the standard `AWS_ACCESS_KEY_ID` /
    /// `AWS_SECRET_ACCESS_KEY` environment variables. `None` for disk storage.
    pub fn s3(&self) -> Result<Option<S3Config>, ConfigError> {
        let StorageConfig::S3 { bucket, prefix, region, endpoint, access_key_id, secret_access_key, allow_http } = self else {
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
        assert_eq!(c.storage, StorageConfig::Disk { path: "/tmp/blobs".into() });
        assert_eq!(c.storage.s3().unwrap(), None);
        assert_eq!(c.public_url, None);
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
        // A team space has a cap of its own (4 GiB by default) in the optional [spaces] section.
        assert_eq!(Config::parse(DISK).unwrap().spaces.team_live_bytes, 4 << 30, "4 GiB by default");
        assert_eq!(Config::parse(&format!("{DISK}\n[spaces]\nteam_live_bytes = 7_000_000\n")).unwrap().spaces.team_live_bytes, 7_000_000);
        assert!(matches!(Config::parse(&format!("{DISK}\n[spaces]\nteam_live_bytes = -1\n")), Err(ConfigError::Parse(_))));
        assert!(matches!(Config::parse(&format!("{DISK}\n[spaces]\nteam_stored = 1\n")), Err(ConfigError::Parse(_))), "unknown keys are refused");
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
