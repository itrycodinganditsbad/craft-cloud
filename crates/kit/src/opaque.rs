//! Opaque values: one key of the root map `$opaque` per part, the value's bytes, last writer wins.

use loro::{LoroDoc, LoroValue, ValueOrContainer};

use crate::mapping::OpaqueSource;
use crate::writer::Writer;
use crate::{KitError, MAX_VALUE_BYTES};

/// The root map of every opaque part.
pub(crate) const ROOT: &str = "$opaque";

/// Writes `new`'s value unless `old` has the same key, or the stored bytes are the same.
pub(crate) fn write(w: &mut Writer<'_>, key: &str, old: Option<&dyn OpaqueSource>, new: &dyn OpaqueSource) -> Result<(), KitError> {
    if let (Some(old), Some(k)) = (old, new.key())
        && old.key() == Some(k)
    {
        return Ok(());
    }
    let map = w.doc.get_map(ROOT);
    match new.encode()? {
        Some(bytes) => {
            if bytes.len() > MAX_VALUE_BYTES {
                return Err(KitError::TooLarge { what: "an opaque value", len: bytes.len(), max: MAX_VALUE_BYTES });
            }
            let weight = bytes.len();
            w.set_raw(&map, key, LoroValue::from(bytes), weight)?;
        }
        None => {
            w.delete(&map, key)?;
        }
    }
    Ok(())
}

/// The value as stored (`None`: there is none, or it isn't bytes).
pub(crate) fn read(doc: &LoroDoc, key: &str) -> Option<Vec<u8>> {
    match doc.get_map(ROOT).get(key)? {
        ValueOrContainer::Value(LoroValue::Binary(bytes)) => Some(bytes.to_vec()),
        _ => None,
    }
}

/// An [`OpaqueSource`] over a value with an [`crate::OpaqueCodec`]; `key` is the value's address when the app shares
/// it by `Arc` (unchanged values are then skipped without encoding them).
pub struct Opaque<'a, T: crate::OpaqueCodec> {
    pub value: Option<&'a T>,
    pub key: Option<usize>,
}

impl<T: crate::OpaqueCodec> OpaqueSource for Opaque<'_, T> {
    fn key(&self) -> Option<usize> {
        self.key
    }

    fn encode(&self) -> Result<Option<Vec<u8>>, KitError> {
        Ok(self.value.map(crate::OpaqueCodec::encode))
    }
}
