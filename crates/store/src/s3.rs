//! S3-compatible backend (AWS, MinIO, Garage, Backblaze B2, Cloudflare R2 …) through `object_store`.
//! A PUT is atomic, so no temporary objects are needed.

use async_trait::async_trait;
use bytes::Bytes;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{ObjectStoreExt, PutPayload};

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

#[async_trait]
impl Backend for S3Backend {
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError> {
        self.store.put(&self.object_path(path), PutPayload::from(data)).await.map_err(backend_error)?;
        Ok(())
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError> {
        match self.store.get(&self.object_path(path)).await {
            Ok(result) => Ok(Some(result.bytes().await.map_err(backend_error)?)),
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
}

#[cfg(test)]
mod debug_tests {
    use super::S3Config;

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
