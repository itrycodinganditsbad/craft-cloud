//! A model element's properties as a Loro map: what the app's codec writes ([`Props`], only what changed) and reads
//! back ([`PropsRead`]). Keys starting with `$` are the kit's own.

use loro::{LoroMap, ValueOrContainer};

use crate::writer::Writer;
use crate::{KitError, Value};

/// Checks an app's property key: 1 to 128 bytes, not starting with `$` (the kit's) or `¶` (paragraph marks).
pub(crate) fn check_key(key: &str) -> Result<(), KitError> {
    if key.is_empty() || key.len() > 128 || key.starts_with('$') || key.starts_with('¶') {
        return Err(KitError::Invalid(format!("{key:?} isn't a property key (1-128 bytes, not starting with '$' or '¶')")));
    }
    Ok(())
}

/// The write side of one element's properties: set what the element has; what it no longer has goes when the writer
/// is done. A value equal to the stored one writes nothing.
pub struct Props<'a, 'd> {
    pub(crate) map: LoroMap,
    pub(crate) w: &'a mut Writer<'d>,
    pub(crate) written: Vec<String>,
}

impl<'a, 'd> Props<'a, 'd> {
    pub(crate) fn new(map: LoroMap, w: &'a mut Writer<'d>) -> Self {
        Self { map, w, written: Vec::new() }
    }

    /// Sets the property `key`.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) -> Result<(), KitError> {
        check_key(key)?;
        let value = value.into();
        self.w.set(&self.map, key, &value)?;
        self.written.push(key.to_owned());
        Ok(())
    }

    /// Deletes the properties not set (the kit's own keys stay).
    pub(crate) fn finish(self) -> Result<(), KitError> {
        let mut stale = Vec::new();
        self.map.for_each(|key, _| {
            if !key.starts_with('$') && !self.written.iter().any(|w| w == key) {
                stale.push(key.to_owned());
            }
        });
        for key in stale {
            self.w.delete(&self.map, &key)?;
        }
        Ok(())
    }
}

/// One element's properties as stored.
#[derive(Debug, Clone)]
pub struct PropsRead {
    pub(crate) map: LoroMap,
}

impl PropsRead {
    /// The property `key`, if it holds a value an app writes.
    pub fn get(&self, key: &str) -> Option<Value> {
        match self.map.get(key)? {
            ValueOrContainer::Value(v) => Value::from_loro(&v),
            ValueOrContainer::Container(_) => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.as_str().map(str::to_owned))
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.as_i64())
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.as_f64())
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }

    pub fn bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.get(key).and_then(|v| v.as_bytes().map(<[u8]>::to_vec))
    }

    pub fn chunk(&self, key: &str) -> Option<crate::ChunkRef> {
        self.get(key).and_then(|v| v.as_chunk())
    }

    /// Every app property, sorted by key.
    pub fn all(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        self.map.for_each(|key, value| {
            if key.starts_with('$') {
                return;
            }
            if let ValueOrContainer::Value(v) = value
                && let Some(v) = Value::from_loro(&v)
            {
                out.push((key.to_owned(), v));
            }
        });
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}
