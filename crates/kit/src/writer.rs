//! Writing one local change into Loro: the mappings write through a [`Writer`], which counts what they write and
//! commits before a commit's export would pass [`SPLIT_BYTES`] (a 5 MB paste becomes several commits, each well under
//! the agent's 1 MiB cap, one undo step together). Each commit's export is taken at once ([`export_since`]); an export
//! that is still over the limit (an undo of a big delete, which Loro makes as one commit) is cut by counter range, so
//! no update the kit sends is over the cap.

use loro::{IdSpan, LoroDoc, LoroMap, LoroValue, ValueOrContainer, VersionVector};

use crate::{KitError, Value};

/// A commit's estimated export is kept at or under this (the agent refuses an update over 1 MiB).
pub const SPLIT_BYTES: usize = 512 * 1024;
/// What one operation adds to the estimate beside its payload (IDs, positions, keys).
pub const OP_OVERHEAD: usize = 32;
/// The agent's cap on one update.
pub const MAX_UPDATE: usize = craft_cloud_api::limits::MAX_UPDATE_BYTES;

/// The origin of the kit's commits of local changes (the undo manager records them).
pub const LOCAL_ORIGIN: &str = "craft-cloud-kit:local";
/// The origin of the kit's own repairs (a normalised merge, a save mark): never an undo step.
pub const KIT_ORIGIN: &str = "craft-cloud-kit:kit";

/// The updates one change made: the exports of its commits, in order, each at most [`SPLIT_BYTES`] (one operation
/// over it alone, at most [`MAX_UPDATE`]).
pub type Updates = Vec<Vec<u8>>;

/// The write side of one change.
pub struct Writer<'d> {
    pub(crate) doc: &'d LoroDoc,
    origin: &'static str,
    /// The estimate of what the open commit holds.
    spent: usize,
    /// Where the open commit started.
    from: VersionVector,
    out: Updates,
    commits: usize,
}

impl<'d> Writer<'d> {
    pub(crate) fn new(doc: &'d LoroDoc, origin: &'static str) -> Self {
        Self { doc, origin, spent: 0, from: doc.oplog_vv(), out: Vec::new(), commits: 0 }
    }

    /// Room left in the open commit before it is cut.
    pub(crate) fn room(&self) -> usize {
        SPLIT_BYTES.saturating_sub(self.spent).saturating_sub(OP_OVERHEAD)
    }

    /// Counts an operation of `payload` bytes, committing first if it wouldn't fit in the open commit.
    pub(crate) fn spend(&mut self, payload: usize) -> Result<(), KitError> {
        let cost = payload.saturating_add(OP_OVERHEAD);
        if self.spent > 0 && self.spent.saturating_add(cost) > SPLIT_BYTES {
            self.cut()?;
        }
        self.spent = self.spent.saturating_add(cost);
        Ok(())
    }

    /// Commits what is open and takes its export.
    pub(crate) fn cut(&mut self) -> Result<(), KitError> {
        if self.spent == 0 {
            return Ok(());
        }
        self.doc.set_next_commit_origin(self.origin);
        self.doc.commit();
        self.commits += 1;
        export_since(self.doc, &self.from, &mut self.out)?;
        self.from = self.doc.oplog_vv();
        self.spent = 0;
        Ok(())
    }

    /// The change's updates (the last commit made now).
    pub(crate) fn finish(mut self) -> Result<(Updates, usize), KitError> {
        self.cut()?;
        // Anything written without being counted (there should be none) still goes out.
        self.doc.set_next_commit_origin(self.origin);
        self.doc.commit();
        export_since(self.doc, &self.from, &mut self.out)?;
        Ok((self.out, self.commits))
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

/// The export of every change since `from` (all made by this peer: a local change, an undo), cut by counter range
/// where it is over [`SPLIT_BYTES`], appended to `out`.
pub(crate) fn export_since(doc: &LoroDoc, from: &VersionVector, out: &mut Updates) -> Result<(), KitError> {
    let now = doc.oplog_vv();
    let mut spans = Vec::new();
    for (peer, end) in now.iter() {
        let start = from.get(peer).copied().unwrap_or(0);
        if *end > start {
            spans.push(IdSpan::new(*peer, start, *end));
        }
    }
    for span in spans {
        slice(doc, span, out)?;
    }
    Ok(())
}

fn slice(doc: &LoroDoc, span: IdSpan, out: &mut Updates) -> Result<(), KitError> {
    let bytes = doc.export(loro::ExportMode::updates_in_range(vec![span]))?;
    let (start, end) = (span.counter.start, span.counter.end);
    if bytes.len() <= SPLIT_BYTES || end - start <= 1 {
        if bytes.len() > MAX_UPDATE {
            return Err(KitError::TooLarge { what: "one operation", len: bytes.len(), max: MAX_UPDATE });
        }
        out.push(bytes);
        return Ok(());
    }
    let mid = start + (end - start) / 2;
    slice(doc, IdSpan::new(span.peer, start, mid), out)?;
    slice(doc, IdSpan::new(span.peer, mid, end), out)
}
