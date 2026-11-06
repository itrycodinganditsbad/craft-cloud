//! An in-memory backend for tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, StoreError};

/// Clones share the same objects, so a test can keep a handle to the backend it gave a `Store`.
#[derive(Default, Clone)]
pub struct MemoryBackend {
    objects: Arc<Mutex<BTreeMap<String, Bytes>>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Bytes>>, StoreError> {
        self.objects.lock().map_err(|_| StoreError::Backend("memory store lock poisoned".into()))
    }

    /// Replaces an object's raw bytes (format byte included), for tests that simulate damage.
    pub fn overwrite_raw(&self, path: &str, raw: Bytes) -> Result<(), StoreError> {
        self.lock()?.insert(path.to_owned(), raw);
        Ok(())
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError> {
        self.lock()?.insert(path.to_owned(), data);
        Ok(())
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError> {
        Ok(self.lock()?.get(path).cloned())
    }

    async fn get_raw_limited(&self, path: &str, max_bytes: usize) -> Result<Option<Bytes>, StoreError> {
        Ok(self.lock()?.get(path).map(|bytes| bytes.slice(..bytes.len().min(max_bytes.saturating_add(1)))))
    }

    async fn size_raw(&self, path: &str) -> Result<Option<u64>, StoreError> {
        Ok(self.lock()?.get(path).map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX)))
    }

    async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
        self.lock()?.remove(path);
        Ok(())
    }

    async fn list_raw(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>, StoreError> {
        let objects = self.lock()?;
        let boundary = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
        // Start at the later of the prefix boundary and `after`, and stop at the first key outside the
        // boundary: keys of later namespaces are never visited, and the mutex is held for at most `limit` steps.
        let start = match after {
            Some(path) if path >= boundary.as_str() => std::ops::Bound::Excluded(path.to_owned()),
            _ => std::ops::Bound::Included(boundary.clone()),
        };
        Ok(objects
            .range((start, std::ops::Bound::Unbounded))
            .map(|(path, _)| path)
            .take_while(|path| path.starts_with(&boundary))
            .take(limit)
            .cloned()
            .collect())
    }
}
