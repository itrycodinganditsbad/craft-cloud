//! The server's TOML config file.
//!
//! ```toml
//! listen = "127.0.0.1:8080"
//! behind_proxy = false          # true when a TLS reverse proxy forwards to this address
//! trusted_proxies = []          # proxies whose X-Forwarded-For is believed
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

use craft_cloud_store::s3::S3Config;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("listen = {0} is not a loopback address. Craft Cloud speaks plain HTTP; put it behind a TLS reverse proxy and set behind_proxy = true")]
    NotLoopback(SocketAddr),
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
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
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
        let config: Config = toml::from_str(text)?;
        if !config.behind_proxy && !config.listen.ip().is_loopback() {
            return Err(ConfigError::NotLoopback(config.listen));
        }
        Ok(config)
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
    fn unknown_keys_are_errors() {
        assert!(matches!(Config::parse(&format!("lisen = 1\n{DISK}")), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn s3_config_hides_its_secret() {
        let text = r#"
listen = "[::1]:9000"
[database]
url = "postgres://cc@localhost/cc"
[storage]
kind = "s3"
bucket = "blobs"
access_key_id = "AKIA"
secret_access_key = "very-secret-value"
"#;
        let c = Config::parse(text).unwrap();
        assert!(!format!("{c:?}").contains("very-secret-value"));
        let s3 = c.storage.s3().unwrap().unwrap();
        assert_eq!((s3.bucket.as_str(), s3.region.as_str(), s3.secret_access_key.as_str()), ("blobs", "us-east-1", "very-secret-value"));
    }
}
