//! A live document's local copy: a Loro document that every machine of the account merges from opaque updates, with
//! nothing in it read here. What recents and documents of files share: importing (checked by Loro; a panic inside Loro
//! becomes an error, never a crashed agent), snapshots (whole, or at the version some updates make), and what a log
//! lacks of it, for a push after a restore.

use std::collections::BTreeMap;

use craft_cloud_proto::live::MAX_UPDATE_BYTES;
use loro::{ExportMode, IdSpan, LoroDoc, VersionVector};

/// Why an update or a stored document was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("the update doesn't import: {0}")]
    Malformed(String),
    #[error("the recents document is of version {0}; this agent reads version {RECENTS_VERSION}", RECENTS_VERSION = super::recents::RECENTS_VERSION)]
    Newer(i64),
    #[error("the update breaks a limit: {0}")]
    Invalid(&'static str),
    /// Loro panicked on it: the document may be half-changed and is read back from its file.
    #[error("Loro failed on the update")]
    Panicked,
}

/// What an import brought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// Applied; whether it held anything this copy didn't have.
    Applied { new: bool },
    /// Not wholly applied: some of it depends on updates this copy doesn't have yet. Loro keeps that part and applies
    /// it as soon as they come; `needs` is how far each peer's operations must reach for it to be in
    /// ([`LiveCopy::covers`]).
    Waiting { needs: Vec<(u64, i32)> },
}

/// Ranges of each Loro peer's operations (`[start, end)` counters), as a log is known to hold them.
pub type Spans = BTreeMap<u64, Vec<(i32, i32)>>;

/// A version: how far each peer's operations reach.
pub type Version = BTreeMap<u64, i32>;

/// The ranges of operations an update (or snapshot) holds, read from its header without importing it; `None` for
/// bytes Loro can't read.
pub fn spans_of(bytes: &[u8]) -> Option<Vec<(u64, i32, i32)>> {
    let meta = guarded(|| LoroDoc::decode_import_blob_meta(bytes, true).map_err(|e| UpdateError::Malformed(e.to_string()))).ok()?;
    Some(meta.partial_end_vv.iter().map(|(peer, end)| (*peer, meta.partial_start_vv.get(peer).copied().unwrap_or(0), *end)).collect())
}

/// Adds `ranges` to `spans`, merging what touches.
pub fn add_spans(spans: &mut Spans, ranges: &[(u64, i32, i32)]) {
    for &(peer, start, end) in ranges {
        if start >= end {
            continue;
        }
        let list = spans.entry(peer).or_default();
        list.push((start, end));
        list.sort_unstable();
        let mut merged: Vec<(i32, i32)> = Vec::with_capacity(list.len());
        for &(s, e) in list.iter() {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        *list = merged;
    }
}

/// Runs `f`, turning a panic inside Loro into [`UpdateError::Panicked`] (never into a crashed agent).
pub(crate) fn guarded<T>(f: impl FnOnce() -> Result<T, UpdateError>) -> Result<T, UpdateError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(Err(UpdateError::Panicked))
}

/// A Loro peer ID this document may use (`u64::MAX` is reserved by Loro).
fn usable_peer(peer: u64) -> u64 {
    if peer == u64::MAX { peer - 1 } else { peer }
}

/// One live document's copy on this machine.
pub struct LiveCopy {
    pub(crate) doc: LoroDoc,
    /// Loro panicked in a call on this document: its lock is poisoned, so the document is read back from its file and
    /// this one let go of without dropping it (dropping a poisoned Loro document panics again).
    poisoned: std::cell::Cell<bool>,
}

impl std::fmt::Debug for LiveCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveCopy").field("peer", &self.doc.peer_id()).finish()
    }
}

impl LiveCopy {
    /// An empty document, editing as Loro peer `peer` (unique per machine and run).
    pub fn new(peer: u64) -> Self {
        let doc = LoroDoc::new();
        // A fresh document has no history, so any peer ID is accepted.
        let _ = doc.set_peer_id(usable_peer(peer));
        Self { doc, poisoned: std::cell::Cell::new(false) }
    }

    /// Runs `f` on the document under [`guarded`]; a panic marks it poisoned. Every Loro call on a document goes
    /// through here (it may hold any machine's data).
    pub(crate) fn guard<T>(&self, f: impl FnOnce(&LoroDoc) -> Result<T, UpdateError>) -> Result<T, UpdateError> {
        if self.poisoned.get() {
            return Err(UpdateError::Panicked);
        }
        let result = guarded(|| f(&self.doc));
        if matches!(result, Err(UpdateError::Panicked)) {
            self.poisoned.set(true);
        }
        result
    }

    /// Whether Loro panicked in a call on this document ([`LiveCopy::guard`]): it must be read back from its file, and
    /// this one never dropped.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.get()
    }

    /// Marks the document as one Loro failed in (for tests: a real Loro panic can't be made on demand).
    #[cfg(test)]
    pub(crate) fn poison(&self) {
        self.poisoned.set(true);
    }

    /// A stored document (a Loro snapshot), editing as `peer`. Of any size: the 1 MiB limit is on updates from the
    /// server, not on this machine's own document.
    pub fn load(snapshot: &[u8], peer: u64) -> Result<Self, UpdateError> {
        let copy = Self::new(peer);
        match copy.import_any_size(snapshot)? {
            Taken::Applied { .. } => Ok(copy),
            Taken::Waiting { .. } => Err(UpdateError::Malformed("a stored document that needs more history".into())),
        }
    }

    /// Merges an update from elsewhere, checked by Loro. Refused only when Loro can't decode it, or it is over the
    /// update limit: those are the same on every machine. A refused update leaves the document as it was.
    pub fn import(&self, bytes: &[u8]) -> Result<Taken, UpdateError> {
        if bytes.len() > MAX_UPDATE_BYTES {
            return Err(UpdateError::Invalid("over 1 MiB"));
        }
        self.import_any_size(bytes)
    }

    /// [`LiveCopy::import`] without the update limit: for a snapshot, or this machine's own stored document.
    pub fn import_any_size(&self, bytes: &[u8]) -> Result<Taken, UpdateError> {
        let status = self.guard(|doc| doc.import(bytes).map_err(|e| UpdateError::Malformed(e.to_string())))?;
        let needs: Vec<(u64, i32)> = status.pending.map(|p| p.iter().map(|(peer, (_, end))| (*peer, *end)).collect()).unwrap_or_default();
        let new = status.success.iter().any(|(_, (start, end))| start < end);
        Ok(if needs.is_empty() { Taken::Applied { new } } else { Taken::Waiting { needs } })
    }

    /// Whether the document has any operations (Loro drops the updates waiting in a document without any when a
    /// snapshot comes in: the caller imports them again).
    pub fn has_history(&self) -> bool {
        self.guard(|doc| Ok(!doc.oplog_vv().is_empty())).unwrap_or(false)
    }

    /// The Loro peer this document edits as (a plain field read: no lock).
    pub fn peer(&self) -> u64 {
        self.doc.peer_id()
    }

    /// Whether every peer's operations here reach as far as `needs` says (a waiting update is then in).
    pub fn covers(&self, needs: &[(u64, i32)]) -> bool {
        self.guard(|doc| {
            let vv = doc.oplog_vv();
            Ok(needs.iter().all(|(peer, end)| vv.get(peer).is_some_and(|have| have >= end)))
        })
        .unwrap_or(false)
    }

    /// How far each peer's operations reach in this copy.
    pub fn version(&self) -> Result<Version, UpdateError> {
        self.guard(|doc| Ok(doc.oplog_vv().iter().map(|(peer, end)| (*peer, *end)).collect()))
    }

    /// The whole document (a Loro snapshot): to store it, or to send it up again to a server that lost it.
    pub fn snapshot(&self) -> Result<Vec<u8>, UpdateError> {
        self.guard(|doc| doc.export(ExportMode::Snapshot).map_err(|e| UpdateError::Malformed(e.to_string())))
    }

    /// A snapshot of the document as it was at `version` (its history up to there, and the state then; nothing
    /// later). `Invalid` if this copy doesn't hold every operation `version` names.
    pub fn snapshot_at(&self, version: &Version) -> Result<Vec<u8>, UpdateError> {
        self.guard(|doc| {
            let have = doc.oplog_vv();
            if version.iter().any(|(peer, end)| *end > 0 && have.get(peer).is_none_or(|h| h < end)) {
                return Err(UpdateError::Invalid("the copy doesn't hold that version"));
            }
            let vv: VersionVector = version.iter().filter(|(_, end)| **end > 0).map(|(peer, end)| (*peer, *end)).collect();
            let frontiers = doc.vv_to_frontiers(&vv);
            doc.export(ExportMode::SnapshotAt { version: std::borrow::Cow::Owned(frontiers) }).map_err(|e| UpdateError::Malformed(e.to_string()))
        })
    }

    /// The changes (Loro commits: peer, first and end counter) of this document that `has` doesn't wholly hold, in
    /// causal order: by Lamport time (a change's dependencies all have a smaller one), then peer. Any prefix of it,
    /// sent after what `has` holds, applies on its own.
    pub fn missing(&self, has: &Spans) -> Result<Vec<(u64, i32, i32)>, UpdateError> {
        self.guard(|doc| Ok(Self::missing_in(doc, has)))
    }

    fn missing_in(doc: &LoroDoc, has: &Spans) -> Vec<(u64, i32, i32)> {
        let mut out = Vec::new();
        let ends: Vec<(u64, i32)> = doc.oplog_vv().iter().map(|(peer, end)| (*peer, *end)).collect();
        for (peer, end) in ends {
            let mut at = 0;
            while at < end {
                let Some(change) = doc.get_change(loro::ID::new(peer, at)) else { break };
                let (start, stop) = (change.id.counter, change.id.counter.saturating_add(i32::try_from(change.len).unwrap_or(i32::MAX)));
                if stop <= at {
                    break;
                }
                // The pieces of the change `has` lacks (a change may be partly sent: a big one goes in pieces), each
                // at the Lamport time of its first operation.
                let mut from = start;
                for &(s, e) in has.get(&peer).map(Vec::as_slice).unwrap_or_default() {
                    if e <= from || s >= stop {
                        continue;
                    }
                    if s > from {
                        out.push((change.lamport.saturating_add(u32::try_from(from - start).unwrap_or(0)), peer, from, s));
                    }
                    from = from.max(e);
                }
                if from < stop {
                    out.push((change.lamport.saturating_add(u32::try_from(from - start).unwrap_or(0)), peer, from, stop));
                }
                at = stop;
            }
        }
        out.sort_unstable();
        out.into_iter().map(|(_, peer, start, stop)| (peer, start, stop)).collect()
    }

    /// `changes` (whole Loro commits) as one update.
    /// Each peer's pieces must follow on from each other ([`LiveCopy::export_prefix`] keeps them so): Loro panics on a
    /// range export of one peer's operations with a gap.
    pub fn export_changes(&self, changes: &[(u64, i32, i32)]) -> Result<Vec<u8>, UpdateError> {
        let mut spans: BTreeMap<u64, (i32, i32)> = BTreeMap::new();
        for &(peer, start, end) in changes {
            let span = spans.entry(peer).or_insert((start, end));
            span.0 = span.0.min(start);
            span.1 = span.1.max(end);
        }
        let spans: Vec<IdSpan> = spans.into_iter().map(|(peer, (start, end))| IdSpan::new(peer, start, end)).collect();
        self.guard(|doc| doc.export(ExportMode::updates_in_range(spans)).map_err(|e| UpdateError::Malformed(e.to_string())))
    }

    /// The longest start of `changes` that fits in `max` bytes as one update (at least one change; `Invalid` if that
    /// one doesn't fit in `hard_max`), with how many changes it holds.
    pub fn export_prefix(&self, changes: &[(u64, i32, i32)], max: usize, hard_max: usize) -> Result<(Vec<u8>, usize), UpdateError> {
        let Some(&(peer, start, end)) = changes.first() else { return Err(UpdateError::Invalid("nothing to send")) };
        // The most operations of the first change that fit, by doubling from one and then halving the gap, so no
        // export is much bigger than a part (a big change isn't exported whole for every piece of it). A change too big
        // for a part goes in pieces: its operations depend on the ones before them, so a start of it applies once
        // those before it are in.
        let total = end - start;
        let (mut ops, mut one) = (0, Vec::new());
        let (mut try_ops, mut over) = (1, None);
        while over.is_none() && ops < total {
            let n = try_ops.min(total);
            let bytes = self.export_changes(&[(peer, start, start + n)])?;
            if bytes.len() <= max {
                (ops, one) = (n, bytes);
                try_ops = n.saturating_mul(2);
            } else {
                over = Some(n);
            }
        }
        if let Some(mut high) = over {
            while high - ops > 1 {
                let mid = ops + (high - ops) / 2;
                let bytes = self.export_changes(&[(peer, start, start + mid)])?;
                if bytes.len() <= max {
                    (ops, one) = (mid, bytes);
                } else {
                    high = mid;
                }
            }
        }
        if ops == 0 {
            // One operation over a part: sent alone if it fits an update at all.
            let bytes = self.export_changes(&[(peer, start, start + 1)])?;
            return if bytes.len() <= hard_max { Ok((bytes, 1)) } else { Err(UpdateError::Invalid("one operation is larger than an update may be")) };
        }
        if ops < total {
            return Ok((one, 1));
        }
        // Only as far as each peer's pieces follow on from each other (a piece after a gap waits for a later part).
        let mut ends: BTreeMap<u64, i32> = BTreeMap::new();
        let mut usable = 0;
        for &(peer, start, end) in changes {
            if ends.get(&peer).is_some_and(|last| *last != start) {
                break;
            }
            ends.insert(peer, end);
            usable += 1;
        }
        let changes = &changes[..usable.max(1)];
        // The most changes that fit: doubling, then halving the gap.
        let (mut fits, mut best) = (1, one);
        let mut too_many = None;
        let mut try_n = 2;
        while too_many.is_none() && fits < changes.len() {
            let n = try_n.min(changes.len());
            let bytes = self.export_changes(&changes[..n])?;
            if bytes.len() <= max {
                (fits, best) = (n, bytes);
                try_n = n * 2;
            } else {
                too_many = Some(n);
            }
        }
        if let Some(mut high) = too_many {
            while high - fits > 1 {
                let mid = fits + (high - fits) / 2;
                let bytes = self.export_changes(&changes[..mid])?;
                if bytes.len() <= max {
                    (fits, best) = (mid, bytes);
                } else {
                    high = mid;
                }
            }
        }
        Ok((best, fits))
    }
}

/// What the server's log holds of a document, as a version, at any sequence number from an anchor on: so the agent can
/// make a snapshot of exactly the updates 1..=`upto` when the server asks for one, without keeping the updates.
///
/// The anchor is a number with the version of everything the log holds up to it (from the server's snapshot, from 0
/// after a catch-up from the start, or worked out from the copy when nothing else is in it); every update taken after
/// it adds its ranges. A version with a gap (an update whose history comes later in the log) is no version at all:
/// no snapshot can be made there.
#[derive(Debug, Clone, Default)]
pub struct LogVersion {
    anchor: Option<(u64, Version)>,
    /// The updates taken since the anchor, in order: number and ranges.
    since: Vec<(u64, Ranges)>,
}

/// The ranges of operations of an update: peer, first and end counter.
pub type Ranges = Vec<(u64, i32, i32)>;

/// Most updates [`LogVersion`] keeps past its anchor before it forgets it (and works one out again when it can).
const MAX_SINCE: usize = 100_000;

impl LogVersion {
    /// Anchored at `seq` with `version`.
    pub fn anchored(seq: u64, version: Version) -> Self {
        Self { anchor: Some((seq, version)), since: Vec::new() }
    }

    /// Nothing known.
    pub fn unknown() -> Self {
        Self::default()
    }

    pub fn is_known(&self) -> bool {
        self.anchor.is_some()
    }

    /// Update `seq`, holding `ranges`, is in the log (taken in order after what was noted before).
    pub fn note(&mut self, seq: u64, ranges: Vec<(u64, i32, i32)>) {
        let Some((at, _)) = &self.anchor else { return };
        if seq <= *at {
            return;
        }
        if self.since.len() >= MAX_SINCE {
            *self = Self::unknown();
            return;
        }
        self.since.push((seq, ranges));
    }

    /// The version the log's updates up to `upto` make, if it is known and has no gap.
    pub fn at(&self, upto: u64) -> Option<Version> {
        let (at, base) = self.anchor.as_ref()?;
        if upto < *at {
            return None;
        }
        let mut spans = Spans::new();
        let base: Vec<(u64, i32, i32)> = base.iter().map(|(peer, end)| (*peer, 0, *end)).collect();
        add_spans(&mut spans, &base);
        for (_, ranges) in self.since.iter().take_while(|(seq, _)| *seq <= upto) {
            add_spans(&mut spans, ranges);
        }
        // Each peer's operations must run from its first one without a hole.
        let mut version = Version::new();
        for (peer, list) in spans {
            match list.as_slice() {
                [(0, end)] => {
                    version.insert(peer, *end);
                }
                [] => {}
                _ => return None,
            }
        }
        Some(version)
    }

    /// The version at `upto` is `version` (a snapshot was made there): what came before is forgotten.
    pub fn rebase(&mut self, upto: u64, version: Version) {
        self.since.retain(|(seq, _)| *seq > upto);
        self.anchor = Some((upto, version));
    }
}

/// The version of a snapshot (or update) from its header: how far each peer's operations reach.
pub fn version_of(bytes: &[u8]) -> Option<Version> {
    let ranges = spans_of(bytes)?;
    let mut version = Version::new();
    for (peer, _, end) in ranges {
        let reach = version.entry(peer).or_insert(0);
        *reach = (*reach).max(end);
    }
    Some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_update(doc: &LoroDoc, at: usize, s: &str) -> Vec<u8> {
        let before = doc.oplog_vv();
        doc.get_text("text").insert(at, s).unwrap();
        doc.commit();
        doc.export(ExportMode::updates(&before)).unwrap()
    }

    /// An update Loro refuses leaves the copy as it was (Ruling: validation without a scratch copy).
    #[test]
    fn a_refused_update_leaves_the_copy_as_it_was() {
        let copy = LiveCopy::new(1);
        let other = LoroDoc::new();
        other.set_peer_id(2).unwrap();
        copy.import(&text_update(&other, 0, "hello")).unwrap();
        let before = copy.snapshot().unwrap();
        let good = text_update(&other, 5, " world");
        // Damaged: a byte in the middle flipped (Loro's checksum catches it), cut short, junk, empty.
        let mut flipped = good.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0x55;
        for bad in [flipped, good[..good.len() / 2].to_vec(), vec![1, 2, 3, 4, 5], Vec::new()] {
            assert!(copy.import(&bad).is_err(), "{bad:?}");
            assert_eq!(copy.snapshot().unwrap(), before, "unchanged");
            assert!(!copy.is_poisoned());
        }
        assert_eq!(copy.import(&good).unwrap(), Taken::Applied { new: true });
        assert_eq!(copy.doc.get_text("text").to_string(), "hello world");
        assert_eq!(copy.import(&good).unwrap(), Taken::Applied { new: false }, "a repeat brings nothing new");
        assert!(copy.import(&vec![0; MAX_UPDATE_BYTES + 1]).is_err());
    }

    /// A snapshot at a version holds exactly what the log held there, and nothing later.
    #[test]
    fn a_snapshot_at_a_version_holds_nothing_later() {
        let a = LoroDoc::new();
        a.set_peer_id(1).unwrap();
        let copy = LiveCopy::new(9);
        let mut log = LogVersion::anchored(0, Version::new());
        let u1 = text_update(&a, 0, "one ");
        let u2 = text_update(&a, 4, "two ");
        let u3 = text_update(&a, 8, "three");
        for (seq, u) in [(1, &u1), (2, &u2), (3, &u3)] {
            copy.import(u).unwrap();
            log.note(seq, spans_of(u).unwrap());
        }
        let at2 = log.at(2).unwrap();
        let snapshot = copy.snapshot_at(&at2).unwrap();
        let fresh = LiveCopy::load(&snapshot, 5).unwrap();
        assert_eq!(fresh.doc.get_text("text").to_string(), "one two ");
        fresh.import(&u3).unwrap();
        assert_eq!(fresh.doc.get_text("text").to_string(), "one two three", "the rest of the log applies after it");
        // A version the copy doesn't hold is refused.
        let mut beyond = at2.clone();
        beyond.insert(1, 1000);
        assert!(matches!(copy.snapshot_at(&beyond), Err(UpdateError::Invalid(_))));
        // A log with a gap (an update whose history comes after it) has no version there.
        let b = LoroDoc::new();
        b.set_peer_id(2).unwrap();
        let b1 = text_update(&b, 0, "x");
        let b2 = text_update(&b, 1, "y");
        let mut gap = LogVersion::anchored(0, Version::new());
        gap.note(1, spans_of(&b2).unwrap());
        gap.note(2, spans_of(&b1).unwrap());
        assert_eq!(gap.at(1), None, "b2 without b1");
        assert!(gap.at(2).is_some(), "both");
        assert_eq!(gap.at(0), Some(Version::new()));
        // Rebased: what came before is gone, what came after stays.
        log.rebase(2, at2.clone());
        assert_eq!(log.at(1), None);
        assert_eq!(log.at(2), Some(at2));
        assert!(log.at(3).is_some());
        assert_eq!(version_of(&snapshot).unwrap().get(&1), Some(&8));
    }
}
