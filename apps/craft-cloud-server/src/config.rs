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
//! [database]
//! url = "sqlite:/var/lib/craft-cloud/meta.db"   # or postgres://user:pass@host/db
//!
//! [storage]
//! kind = "disk"
//! path = "/var/lib/craft-cloud/blobs"
//! # kind = "s3", bucket, prefix, region, endpoint, access_key_id, secret_access_key, allow_http
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
    #[error("public_url = {0:?} is not a server address like https://cloud.example.com (a host, an optional port and path, no query)")]
    PublicUrl(String),
    #[error("storage.{0} is missing (set it in the config or the {1} environment variable)")]
    MissingS3Credential(&'static str, &'static str),
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
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
}

fn default_live_bytes() -> u64 {
    craft_cloud_meta::DEFAULT_LIVE_BYTES_PER_SPACE
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
        Self::parse(&text)
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
        let mut config = config;
        if let Some(url) = &config.public_url {
            config.public_url = Some(public_url(url)?);
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
    fn unknown_keys_are_errors() {
        assert!(matches!(Config::parse(&format!("lisen = 1\n{DISK}")), Err(ConfigError::Parse(_))));
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
