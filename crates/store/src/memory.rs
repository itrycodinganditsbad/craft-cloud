//! An in-memory backend for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, StoreError};

/// Clones share the same objects, so a test can keep a handle to the backend it gave a `Store`.
#[derive(Default, Clone)]
pub struct MemoryBackend {
    objects: Arc<Mutex<HashMap<String, Bytes>>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Bytes>>, StoreError> {
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

    async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
        self.lock()?.remove(path);
        Ok(())
    }
}
