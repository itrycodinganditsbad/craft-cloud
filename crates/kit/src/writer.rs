//! Writing one local change into Loro: the mappings write through a [`Writer`], which counts what they write, and the
//! change is one commit, exported whole as one update ([`export_since`]) whatever its size. An update over the agent's
//! 1 MiB frame limit goes in fragments (`craft-cloud-app` cuts it), and every receiver imports it only once it holds
//! all of them, so no machine ever holds part of a change.

use loro::{IdSpan, LoroDoc, LoroMap, LoroValue, ValueOrContainer, VersionVector};

use crate::{KitError, Value};

/// What one operation adds to the estimate beside its payload (IDs, positions, keys).
pub const OP_OVERHEAD: usize = 32;
/// The origin of the kit's commits of local changes (the undo manager records them).
pub const LOCAL_ORIGIN: &str = "craft-cloud-kit:local";
/// The origin of the kit's own repairs (a normalised merge, a save mark): never an undo step.
pub const KIT_ORIGIN: &str = "craft-cloud-kit:kit";

/// The updates one change made: none (nothing changed) or its one update, whole.
pub type Updates = Vec<Vec<u8>>;

/// The write side of one change.
pub struct Writer<'d> {
    pub(crate) doc: &'d LoroDoc,
    origin: &'static str,
    /// The estimate of what the change holds.
    spent: usize,
    /// Where the change started.
    from: VersionVector,
    /// The text this change inserted and deleted, in bytes (what an undo of it would have Loro go through).
    pub(crate) text_bytes: usize,
}

impl<'d> Writer<'d> {
    pub(crate) fn new(doc: &'d LoroDoc, origin: &'static str) -> Self {
        Self { doc, origin, spent: 0, from: doc.oplog_vv(), text_bytes: 0 }
    }

    /// Counts an operation of `payload` bytes.
    pub(crate) fn spend(&mut self, payload: usize) -> Result<(), KitError> {
        self.spent = self.spent.saturating_add(payload.saturating_add(OP_OVERHEAD));
        Ok(())
    }

    /// The change's update (its one commit made now), and how many commits it made (0 or 1).
    pub(crate) fn finish(self) -> Result<(Updates, usize), KitError> {
        self.doc.set_next_commit_origin(self.origin);
        self.doc.commit();
        let mut out = Vec::new();
        export_since(self.doc, &self.from, &mut out)?;
        Ok((out, usize::from(self.spent > 0)))
    }

    /// Sets `key` of `map` to `value` unless it holds that already.
    pub(crate) fn set(&mut self, map: &LoroMap, key: &str, value: &Value) -> Result<bool, KitError> {
        value.check()?;
        let new = value.to_loro();
        if let Some(ValueOrContainer::Value(old)) = map.get(key)
            && old == new
        {
            return Ok(false);
        }
        self.spend(value.weight().saturating_add(key.len()))?;
        map.insert(key, new)?;
        Ok(true)
    }

    /// Sets `key` of `map` to a raw Loro value unless it holds it already.
    pub(crate) fn set_raw(&mut self, map: &LoroMap, key: &str, value: LoroValue, weight: usize) -> Result<bool, KitError> {
        if let Some(ValueOrContainer::Value(old)) = map.get(key)
            && old == value
        {
            return Ok(false);
        }
        self.spend(weight.saturating_add(key.len()))?;
        map.insert(key, value)?;
        Ok(true)
    }

    /// Deletes `key` of `map` if it is there.
    pub(crate) fn delete(&mut self, map: &LoroMap, key: &str) -> Result<bool, KitError> {
        if map.get(key).is_none() {
            return Ok(false);
        }
        self.spend(key.len())?;
        map.delete(key)?;
        Ok(true)
    }
}

/// The export of every change since `from` (all made by this peer: a local change, an undo, a repair) as one update,
/// appended to `out` (nothing when there is none).
pub(crate) fn export_since(doc: &LoroDoc, from: &VersionVector, out: &mut Updates) -> Result<(), KitError> {
    let now = doc.oplog_vv();
    let mut spans = Vec::new();
    for (peer, end) in now.iter() {
        let start = from.get(peer).copied().unwrap_or(0);
        if *end > start {
            spans.push(IdSpan::new(*peer, start, *end));
        }
    }
    if !spans.is_empty() {
        out.push(doc.export(loro::ExportMode::updates_in_range(spans))?);
    }
    Ok(())
}
