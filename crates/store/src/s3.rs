//! S3-compatible backend (AWS, MinIO, Garage, Backblaze B2, Cloudflare R2 …) through `object_store`.
//! A PUT is atomic, so no temporary objects are needed.

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::{Backend, StoreError};

/// Where and how to reach the bucket. Credentials come from the server's config, never from clients.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    pub bucket: String,
    /// Key prefix inside the bucket, e.g. `craft-cloud`; empty for the bucket root.
    pub prefix: String,
    pub region: String,
    /// Custom endpoint for non-AWS services, e.g. `http://127.0.0.1:9000`.
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Allow `http://` endpoints (local MinIO). Off for anything reachable from the internet.
    pub allow_http: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

pub struct S3Backend {
    store: AmazonS3,
    prefix: String,
}

impl S3Backend {
    pub fn new(config: &S3Config) -> Result<Self, StoreError> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_allow_http(config.allow_http);
        if let Some(endpoint) = &config.endpoint {
            builder = builder.with_endpoint(endpoint).with_virtual_hosted_style_request(false);
        }
        let store = builder.build().map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(Self { store, prefix: config.prefix.trim_matches('/').to_owned() })
    }

    fn object_path(&self, path: &str) -> Path {
        if self.prefix.is_empty() { Path::from(path) } else { Path::from(format!("{}/{path}", self.prefix)) }
    }
}

fn backend_error(e: object_store::Error) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// The path of a listed key relative to the configured prefix, or `None` when the key is outside the
/// listing prefix (`boundary`, which ends in `/`) or outside the backend's root (`root_boundary`).
fn relative_path(full: &str, boundary: &str, root_boundary: &str) -> Option<String> {
    if !full.starts_with(boundary) {
        return None;
    }
    full.strip_prefix(root_boundary).map(str::to_owned)
}

/// `object_store` parses every listed key as a `Path`, and one key it cannot represent (an empty segment,
/// as in `chunks//a`) fails the whole ListObjectsV2 response page it arrived in and ends the stream. Such a
/// key cannot be reported as foreign from here, and the objects of that page are not visible either, so the
/// listing stops with an error that says why. A key with a trailing slash (`chunks/a/`) is not an error:
/// `object_store` strips the slash and the object appears as `chunks/a`.
fn list_error(error: object_store::Error) -> StoreError {
    match error {
        object_store::Error::InvalidPath { source } => {
            StoreError::Backend(format!("S3 listing contains a key that object_store cannot represent, so the listing cannot continue: {source}"))
        }
        other => backend_error(other),
    }
}

#[async_trait]
impl Backend for S3Backend {
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError> {
        self.store.put(&self.object_path(path), PutPayload::from(data)).await.map_err(backend_error)?;
        Ok(())
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError> {
        self.get_raw_limited(path, usize::MAX).await
    }

    async fn get_raw_limited(&self, path: &str, max_bytes: usize) -> Result<Option<Bytes>, StoreError> {
        match self.store.get(&self.object_path(path)).await {
            Ok(result) => {
                let limit = max_bytes.saturating_add(1);
                let mut body = result.into_stream();
                let mut bytes = Vec::with_capacity(limit.min(1024 * 1024));
                while bytes.len() < limit {
                    let Some(part) = body.try_next().await.map_err(backend_error)? else { break };
                    let take = part.len().min(limit.saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&part[..take]);
                }
                Ok(Some(Bytes::from(bytes)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(backend_error(e)),
        }
    }

    async fn size_raw(&self, path: &str) -> Result<Option<u64>, StoreError> {
        match self.store.head(&self.object_path(path)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(backend_error(e)),
        }
    }

    async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
        match self.store.delete(&self.object_path(path)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(backend_error(e)),
        }
    }

    async fn list_raw(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>, StoreError> {
        let object_prefix = self.object_path(prefix);
        let normalized_prefix = object_prefix.as_ref().to_owned();
        let boundary = if normalized_prefix.is_empty() { String::new() } else { format!("{normalized_prefix}/") };
        let normalized_root = if self.prefix.is_empty() { String::new() } else { Path::from(self.prefix.as_str()).as_ref().to_owned() };
        let root_boundary = if normalized_root.is_empty() { String::new() } else { format!("{normalized_root}/") };
        let offset = after
            .map(|path| {
                let full_path = if normalized_root.is_empty() { path.to_owned() } else { format!("{normalized_root}/{path}") };
                Path::parse(full_path).map_err(|error| StoreError::Backend(error.to_string()))
            })
            .transpose()?;
        let mut stream = if let Some(offset) = &offset {
            self.store.list_with_offset(Some(&object_prefix), offset)
        } else {
            self.store.list(Some(&object_prefix))
        };
        // ListObjectsV2 returns keys in ascending UTF-8 binary order, which is the order `list_raw` promises.
        // The stream is read only until `limit` results are collected, then dropped, so a page costs at most
        // the S3 requests that cover `limit` keys. A key that is not strictly greater than the one before it
        // (a service that lists out of order, such as S3 Express directory buckets) is an error: with an
        // unsorted listing a bounded page cannot be correct, and draining the namespace is not an option.
        let mut paths: Vec<String> = Vec::new();
        while paths.len() < limit {
            let meta = match stream.try_next().await {
                Ok(Some(meta)) => meta,
                Ok(None) => break,
                Err(error) => return Err(list_error(error)),
            };
            let Some(relative) = relative_path(meta.location.as_ref(), &boundary, &root_boundary) else {
                continue;
            };
            if after.is_some_and(|cursor| relative.as_str() <= cursor) {
                continue;
            }
            if paths.last().is_some_and(|previous| relative.as_str() <= previous.as_str()) {
                return Err(StoreError::Backend("S3 listing is not in ascending key order".into()));
            }
            paths.push(relative);
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod debug_tests {
    use super::{S3Backend, S3Config, list_error, relative_path};
    use crate::StoreError;

    #[test]
    fn relative_path_keeps_the_prefix_boundary_exact() {
        assert_eq!(relative_path("root/chunks/ab", "root/chunks/", "root/").as_deref(), Some("chunks/ab"));
        assert_eq!(relative_path("root/chunksx/ab", "root/chunks/", "root/"), None);
        assert_eq!(relative_path("chunks/ab", "chunks/", "").as_deref(), Some("chunks/ab"));
        assert_eq!(relative_path("other/chunks/ab", "root/chunks/", "root/"), None);
    }

    #[test]
    fn keys_object_store_cannot_parse_give_a_specific_error() {
        let source = object_store::path::Path::parse("chunks//a").unwrap_err();
        let message = match list_error(object_store::Error::InvalidPath { source }) {
            StoreError::Backend(message) => message,
            other => panic!("unexpected {other:?}"),
        };
        assert!(message.contains("cannot represent"), "{message}");
        // A trailing slash is normalised away by object_store, before this crate sees the key.
        assert_eq!(object_store::path::Path::parse("chunks/a/").unwrap().as_ref(), "chunks/a");
    }

    #[test]
    fn object_prefix_uses_object_store_path_normalization() {
        let config = S3Config {
            bucket: "b".into(),
            prefix: "a//b%bar".into(),
            region: "us-east-1".into(),
            endpoint: None,
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            allow_http: false,
        };
        let backend = S3Backend::new(&config).unwrap();
        assert_eq!(backend.object_path("chunks").as_ref(), "a/b%25bar/chunks");
    }

    #[test]
    fn debug_output_hides_the_secret_key() {
        let config = S3Config {
            bucket: "b".into(),
            prefix: String::new(),
            region: "us-east-1".into(),
            endpoint: None,
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "super-secret-value".into(),
            allow_http: false,
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("super-secret-value"), "{shown}");
        assert!(shown.contains("<redacted>") && shown.contains("AKIDEXAMPLE"));
    }
}
