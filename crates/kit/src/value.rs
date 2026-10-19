//! The values the mappings carry: stable element IDs, scalar values, chunk references for binary content (with the
//! blob store behind them) and the opaque-value codec.

use std::sync::Arc;

use loro::LoroValue;

use crate::KitError;

/// A model element's stable identity across machines: the app keeps it in its own model (a layer's, a paragraph's, a
/// row's ID). The kit never makes IDs from positions; [`ElemId::new`] makes a random one for apps that have none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElemId(pub u128);

impl ElemId {
    /// A new random ID (from the OS's random source; a time-and-counter mix if that fails).
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            return Self(now ^ (u128::from(n) << 64) ^ u128::from(std::process::id()) << 96);
        }
        Self(u128::from_le_bytes(bytes))
    }

    /// The ID as 32 lowercase hex digits (the key it has in Loro maps).
    pub fn hex(&self) -> String {
        format!("{:032x}", self.0)
    }

    /// 32 lowercase hex digits as an ID.
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        u128::from_str_radix(text, 16).ok().map(Self)
    }

    pub(crate) fn to_loro(self) -> LoroValue {
        LoroValue::from(self.0.to_le_bytes().to_vec())
    }

    pub(crate) fn from_loro(value: &LoroValue) -> Option<Self> {
        match value {
            LoroValue::Binary(bytes) => {
                let array: [u8; 16] = bytes.as_slice().try_into().ok()?;
                Some(Self(u128::from_le_bytes(array)))
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for ElemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Binary content kept out of the live document: its name (the agent's content hash) and length. The bytes go
/// through a [`BlobStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkRef {
    pub hash: [u8; 32],
    pub len: u64,
}

/// Where binary content lives: the agent's `/v1/blobs` ([`AgentBlobs`]), or memory in tests ([`MemoryBlobs`]). Blocking:
/// a get may download, so call it off the UI thread.
pub trait BlobStore: Send + Sync {
    fn put(&self, bytes: &[u8]) -> Result<ChunkRef, KitError>;
    fn get(&self, chunk: &ChunkRef) -> Result<Arc<[u8]>, KitError>;
}

/// The agent's blob store, through `craft-cloud-app`. An agent too old for blobs answers [`KitError::Agent`] with
/// `agent_too_old` in the message.
#[derive(Debug, Clone)]
pub struct AgentBlobs(pub craft_cloud_app::blobs::Blobs);

impl BlobStore for AgentBlobs {
    fn put(&self, bytes: &[u8]) -> Result<ChunkRef, KitError> {
        let stored = self.0.put(bytes).map_err(agent_error)?;
        Ok(ChunkRef { hash: stored.hash, len: stored.len })
    }

    fn get(&self, chunk: &ChunkRef) -> Result<Arc<[u8]>, KitError> {
        Ok(self.0.get(&chunk.hash, chunk.len).map_err(agent_error)?.into())
    }
}

/// An agent's refusal, its code kept in the message (`agent_too_old: …`).
fn agent_error(e: craft_cloud_app::Error) -> KitError {
    match e {
        craft_cloud_app::Error::Refused { code, message } => KitError::Agent(format!("{code}: {message}")),
        other => KitError::from(other),
    }
}

/// Blobs in memory (tests, and apps that run without Craft Cloud), named by BLAKE3.
#[derive(Debug, Default)]
pub struct MemoryBlobs(std::sync::Mutex<std::collections::HashMap<[u8; 32], Arc<[u8]>>>);

impl BlobStore for MemoryBlobs {
    fn put(&self, bytes: &[u8]) -> Result<ChunkRef, KitError> {
        let hash = *blake3::hash(bytes).as_bytes();
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(hash, bytes.into());
        Ok(ChunkRef { hash, len: bytes.len() as u64 })
    }

    fn get(&self, chunk: &ChunkRef) -> Result<Arc<[u8]>, KitError> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&chunk.hash)
            .cloned()
            .ok_or_else(|| KitError::Agent("blob_not_found: no such blob".into()))
    }
}

/// A value that merges as a whole (last writer wins): settings, styles, plugin state.
pub trait OpaqueCodec: Sized {
    fn encode(&self) -> Vec<u8>;
    fn decode(bytes: &[u8]) -> Result<Self, KitError>;
}

impl OpaqueCodec for Vec<u8> {
    fn encode(&self) -> Vec<u8> {
        self.clone()
    }

    fn decode(bytes: &[u8]) -> Result<Self, KitError> {
        Ok(bytes.to_vec())
    }
}

impl OpaqueCodec for String {
    fn encode(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, KitError> {
        String::from_utf8(bytes.to_vec()).map_err(|e| KitError::App(e.to_string()))
    }
}

/// Largest opaque value or property value in a live document, in bytes (bigger content goes in as a [`ChunkRef`]).
pub const MAX_VALUE_BYTES: usize = 256 * 1024;

/// A property or cell value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Chunk(ChunkRef),
}

const CHUNK_HASH: &str = "$chunk";
const CHUNK_LEN: &str = "$len";

impl Value {
    /// Its size as the change-size estimate counts it.
    pub(crate) fn weight(&self) -> usize {
        match self {
            Value::Null | Value::Bool(_) | Value::I64(_) | Value::F64(_) => 9,
            Value::Str(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Chunk(_) => 48,
        }
    }

    /// Refuses a value too big for a live document.
    pub(crate) fn check(&self) -> Result<(), KitError> {
        let len = self.weight();
        if len > MAX_VALUE_BYTES {
            return Err(KitError::TooLarge { what: "a value", len, max: MAX_VALUE_BYTES });
        }
        Ok(())
    }

    pub(crate) fn to_loro(&self) -> LoroValue {
        match self {
            Value::Null => LoroValue::Null,
            Value::Bool(b) => LoroValue::from(*b),
            Value::I64(i) => LoroValue::from(*i),
            Value::F64(f) => LoroValue::from(*f),
            Value::Str(s) => LoroValue::from(s.as_str()),
            Value::Bytes(b) => LoroValue::from(b.clone()),
            Value::Chunk(c) => {
                let mut map = std::collections::HashMap::new();
                map.insert(CHUNK_HASH.to_owned(), LoroValue::from(c.hash.to_vec()));
                map.insert(CHUNK_LEN.to_owned(), LoroValue::from(i64::try_from(c.len).unwrap_or(i64::MAX)));
                LoroValue::from(map)
            }
        }
    }

    /// A Loro value as a property value; `None` for one no app wrote (a list, a container, a broken chunk).
    pub(crate) fn from_loro(value: &LoroValue) -> Option<Value> {
        Some(match value {
            LoroValue::Null => Value::Null,
            LoroValue::Bool(b) => Value::Bool(*b),
            LoroValue::I64(i) => Value::I64(*i),
            LoroValue::Double(f) => Value::F64(*f),
            LoroValue::String(s) => Value::Str(s.to_string()),
            LoroValue::Binary(b) => Value::Bytes(b.to_vec()),
            LoroValue::Map(map) => {
                let hash = match map.get(CHUNK_HASH)? {
                    LoroValue::Binary(b) => <[u8; 32]>::try_from(b.as_slice()).ok()?,
                    _ => return None,
                };
                let len = match map.get(CHUNK_LEN)? {
                    LoroValue::I64(n) => u64::try_from(*n).ok()?,
                    _ => return None,
                };
                Value::Chunk(ChunkRef { hash, len })
            }
            LoroValue::List(_) | LoroValue::Container(_) => return None,
        })
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::I64(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::F64(f) => Some(*f),
            Value::I64(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_chunk(&self) -> Option<ChunkRef> {
        match self {
            Value::Chunk(c) => Some(*c),
            _ => None,
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::I64(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::F64(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Bytes(v)
    }
}

impl From<ChunkRef> for Value {
    fn from(v: ChunkRef) -> Self {
        Value::Chunk(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_values_round_trip() {
        let id = ElemId::new();
        assert_ne!(id, ElemId::new());
        assert_eq!(ElemId::from_hex(&id.hex()), Some(id));
        assert_eq!(ElemId::from_loro(&id.to_loro()), Some(id));
        assert_eq!(ElemId::from_hex("XYZ"), None);
        let chunk = ChunkRef { hash: [3; 32], len: 9 };
        for v in [Value::Null, Value::Bool(true), Value::I64(-3), Value::F64(1.5), Value::from("é"), Value::Bytes(vec![1, 2]), Value::Chunk(chunk)] {
            assert_eq!(Value::from_loro(&v.to_loro()), Some(v));
        }
        assert!(Value::Bytes(vec![0; MAX_VALUE_BYTES]).check().is_ok());
        assert!(Value::Bytes(vec![0; MAX_VALUE_BYTES + 1]).check().is_err());
        let blobs = MemoryBlobs::default();
        let r = blobs.put(b"abc").unwrap();
        assert_eq!(&*blobs.get(&r).unwrap(), b"abc");
    }
}
