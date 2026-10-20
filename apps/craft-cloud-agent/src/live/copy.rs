//! A live document's local copy: a Loro document that every machine of the account merges from opaque updates, with
//! nothing in it read here. What recents and documents of files share: importing (checked by Loro; a panic inside Loro
//! becomes an error, never a crashed agent), snapshots (whole, or at the version some updates make), and what a log
//! lacks of it, for a push after a restore.

use std::collections::BTreeMap;

use craft_cloud_proto::fragments::{is_bundle, parts};
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

/// Loro frontiers of `(peer, counter)` operation IDs (`None` for a counter past Loro's).
pub fn frontiers(ids: &[(u64, u32)]) -> Option<loro::Frontiers> {
    ids.iter().map(|(peer, counter)| Some(loro::ID::new(*peer, i32::try_from(*counter).ok()?))).collect::<Option<Vec<_>>>().map(loro::Frontiers::from)
}

/// The ranges of operations an update (or snapshot, or a bundle of updates: its slices', merged) holds, read from its
/// header without importing it; `None` for bytes Loro can't read.
pub fn spans_of(bytes: &[u8]) -> Option<Vec<(u64, i32, i32)>> {
    if is_bundle(bytes) {
        let mut spans = Spans::new();
        for part in parts(bytes)? {
            add_spans(&mut spans, &spans_of(part)?);
        }
        return Some(spans.into_iter().flat_map(|(peer, list)| list.into_iter().map(move |(s, e)| (peer, s, e))).collect());
    }
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

#[cfg(test)]
thread_local! {
    static FAIL_ON: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// For tests on this thread: importing `bytes` (alone or in a batch) panics as Loro might.
#[cfg(test)]
pub(crate) fn fail_on(bytes: &[u8]) {
    FAIL_ON.with(|fail| fail.borrow_mut().push(bytes.to_vec()));
}

/// A Loro peer ID this document may use (`u64::MAX` is reserved by Loro).
fn usable_peer(peer: u64) -> u64 {
    if peer == u64::MAX { peer - 1 } else { peer }
}

/// The most bytes of updates one of Loro's batch imports takes ([`LiveCopy::import_batch`]).
const BATCH_GROUP_BYTES: usize = 16 * 1024;
/// A bundle's slice whose import takes this long: the rest goes in as one update ([`LiveCopy::import_any_size`]).
const SLOW_SLICE: std::time::Duration = std::time::Duration::from_millis(50);

/// `updates` in consecutive groups of at most `max` bytes (an update bigger than that alone).
fn groups(updates: &[Vec<u8>], max: usize) -> Vec<&[Vec<u8>]> {
    let mut out = Vec::new();
    let (mut start, mut size) = (0, 0usize);
    for (i, u) in updates.iter().enumerate() {
        if i > start && size.saturating_add(u.len()) > max {
            out.extend(updates.get(start..i));
            start = i;
            size = 0;
        }
        size = size.saturating_add(u.len());
    }
    if start < updates.len() {
        out.extend(updates.get(start..));
    }
    out
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

    /// For tests: updates that "make Loro fail" when imported, alone or in a batch ([`fail_on`]).
    #[cfg(test)]
    fn failing(updates: &[&[u8]]) {
        if FAIL_ON.with(|fail| fail.borrow().iter().any(|f| updates.contains(&f.as_slice()))) {
            panic!("a test's update that makes Loro fail");
        }
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

    /// [`LiveCopy::import`] without the update limit: for a snapshot, or this machine's own stored document. A bundle
    /// (one change as its slices) goes in slice by slice, all of it before this returns; one that doesn't read is
    /// refused whole.
    pub fn import_any_size(&self, bytes: &[u8]) -> Result<Taken, UpdateError> {
        if is_bundle(bytes) {
            let slices = parts(bytes).ok_or_else(|| UpdateError::Malformed("a bundle cut short".into()))?;
            // Every slice is read before any goes in: one that doesn't read refuses the bundle whole, nothing of it in.
            for slice in &slices {
                guarded(|| LoroDoc::decode_import_blob_meta(slice, true).map_err(|e| UpdateError::Malformed(e.to_string())))?;
            }
            let (mut new, mut needs) = (false, Vec::new());
            let mut take = |taken: Taken| match taken {
                Taken::Applied { new: n } => new |= n,
                Taken::Waiting { needs: more } => needs.extend(more),
            };
            let mut i = 0;
            while let Some(slice) = slices.get(i) {
                let started = std::time::Instant::now();
                take(self.import_one(slice)?);
                i += 1;
                // A slice this slow: Loro takes its slow way (edits in the same text concurrent with the change, in
                // one order of the peers, make each slice cost more than the last): the rest goes in as one update.
                let rest = slices.get(i..).unwrap_or_default();
                if rest.len() >= 2
                    && started.elapsed() > SLOW_SLICE
                    && let Ok(one) = self.joined(rest)
                {
                    take(self.import_one(&one)?);
                    break;
                }
            }
            return Ok(if needs.is_empty() { Taken::Applied { new } } else { Taken::Waiting { needs } });
        }
        self.import_one(bytes)
    }

    /// `updates` (which follow on from this copy) as one update, joined on a detached fork (which only takes their
    /// operations).
    fn joined(&self, updates: &[&[u8]]) -> Result<Vec<u8>, UpdateError> {
        self.guard(|doc| {
            let fork = doc.fork();
            fork.detach();
            let before = fork.oplog_vv();
            for u in updates {
                fork.import(u).map_err(|e| UpdateError::Malformed(e.to_string()))?;
            }
            fork.export(ExportMode::updates(&before)).map_err(|e| UpdateError::Malformed(e.to_string()))
        })
    }

    fn import_one(&self, bytes: &[u8]) -> Result<Taken, UpdateError> {
        let status = self.guard(|doc| {
            #[cfg(test)]
            Self::failing(&[bytes]);
            doc.import(bytes).map_err(|e| UpdateError::Malformed(e.to_string()))
        })?;
        let needs: Vec<(u64, i32)> = status.pending.map(|p| p.iter().map(|(peer, (_, end))| (*peer, *end)).collect()).unwrap_or_default();
        let new = status.success.iter().any(|(_, (start, end))| start < end);
        Ok(if needs.is_empty() { Taken::Applied { new } } else { Taken::Waiting { needs } })
    }

    /// Imports many updates at once (a catch-up's), checked by Loro as one import would: much faster than one by one
    /// on a big document. Whether it brought anything new. The caller still takes each update on its own after this
    /// (which is then quick), for what each one did.
    pub fn import_batch(&self, updates: &[Vec<u8>]) -> Result<bool, UpdateError> {
        if updates.iter().any(|u| u.len() > MAX_UPDATE_BYTES) {
            return Err(UpdateError::Invalid("over 1 MiB"));
        }
        let before = self.version()?;
        // (A batch Loro refuses as a whole is taken one by one by the caller, which finds the one at fault.)
        let _ = self.guard(|doc| {
            #[cfg(test)]
            Self::failing(&updates.iter().map(Vec::as_slice).collect::<Vec<_>>());
            // In groups of at most `BATCH_GROUP_BYTES`: Loro's batch import of much text at once takes time growing
            // with its square (a 1 MB paste in pieces: seconds), while small groups keep the batch's speed.
            for group in groups(updates, BATCH_GROUP_BYTES) {
                doc.import_batch(group).map_err(|e| UpdateError::Malformed(e.to_string()))?;
            }
            Ok(())
        });
        Ok(self.version()? != before)
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

    /// Whether every operation `ids` names (`(peer, counter)`: a save's frontiers) is in this copy.
    pub fn holds(&self, ids: &[(u64, u32)]) -> bool {
        self.guard(|doc| {
            let vv = doc.oplog_vv();
            Ok(ids.iter().all(|(peer, counter)| i32::try_from(*counter).is_ok_and(|c| vv.includes_id(loro::ID::new(*peer, c)))))
        })
        .unwrap_or(false)
    }

    /// Whether the document as frontiers `a` name it holds every operation of frontiers `b`; `None` when this copy
    /// lacks one of their operations (it can't tell).
    pub fn includes(&self, a: &[(u64, u32)], b: &[(u64, u32)]) -> Option<bool> {
        let (fa, fb) = (frontiers(a)?, frontiers(b)?);
        self.guard(|doc| Ok(doc.cmp_frontiers(&fa, &fb).ok().map(|o| matches!(o, Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)))))
            .ok()
            .flatten()
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
    ///
    /// (A Loro change is a run of one peer's commits Loro merged, not an app's change, so a big one goes in pieces
    /// here: this is history the log lost, sent up again after a server's restore.)
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
///
/// A change that came in fragments is in the log from its last fragment's row on (its ranges are noted there), and
/// no version is offered between its first fragment's row and its last: a snapshot there would drop its first
/// fragments from the log and keep the rest, and a machine catching up later would never get it whole.
#[derive(Debug, Clone, Default)]
pub struct LogVersion {
    anchor: Option<(u64, Version)>,
    /// The updates taken since the anchor, in order: number and ranges.
    since: Vec<(u64, Ranges)>,
    /// The rows of the changes that came in fragments since the anchor: first and last.
    sets: Vec<(u64, u64)>,
}

/// The ranges of operations of an update: peer, first and end counter.
pub type Ranges = Vec<(u64, i32, i32)>;

/// Most updates [`LogVersion`] keeps past its anchor before it forgets it (and works one out again when it can).
const MAX_SINCE: usize = 100_000;

impl LogVersion {
    /// Anchored at `seq` with `version`.
    pub fn anchored(seq: u64, version: Version) -> Self {
        Self { anchor: Some((seq, version)), since: Vec::new(), sets: Vec::new() }
    }

    /// A change that came in fragments took the rows `first` to `last`.
    pub fn note_set(&mut self, first: u64, last: u64) {
        if self.anchor.is_some() && last > first {
            self.sets.push((first, last));
        }
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
        if upto < *at || self.sets.iter().any(|(first, last)| *first <= upto && upto < *last) {
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
        self.sets.retain(|(_, last)| *last > upto);
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

    fn heads(doc: &LoroDoc) -> Vec<(u64, u32)> {
        doc.oplog_frontiers().iter().map(|id| (id.peer, id.counter as u32)).collect()
    }

    /// Which of two saves is newer, from the copy's operations: one made after the other includes it, two made apart
    /// include neither, and frontiers whose operations aren't here yet can't be told (none) until they are.
    #[test]
    fn saves_are_ordered_by_the_copys_operations() {
        let (x, y) = (LoroDoc::new(), LoroDoc::new());
        x.set_peer_id(1).unwrap();
        y.set_peer_id(2).unwrap();
        let base = text_update(&x, 0, "base");
        y.import(&base).unwrap();
        let first = heads(&x);
        let ux = text_update(&x, 4, " x");
        let uy = text_update(&y, 4, " y");
        let (on_x, on_y) = (heads(&x), heads(&y));
        let copy = LiveCopy::new(9);
        copy.import(&base).unwrap();
        assert!(copy.holds(&first) && !copy.holds(&on_x));
        assert_eq!(copy.includes(&on_x, &first), None, "x's save isn't here yet");
        copy.import(&ux).unwrap();
        copy.import(&uy).unwrap();
        assert_eq!(copy.includes(&on_x, &first), Some(true));
        assert_eq!(copy.includes(&first, &on_x), Some(false));
        assert_eq!(copy.includes(&on_x, &on_y), Some(false), "made apart");
        assert_eq!(copy.includes(&on_y, &on_x), Some(false));
        assert_eq!(copy.includes(&on_x, &on_x), Some(true));
        let both = heads(&{
            x.import(&uy).unwrap();
            x
        });
        assert_eq!(both.len(), 2, "two heads");
        assert_eq!(copy.includes(&both, &on_x), Some(true));
        assert_eq!(copy.includes(&both, &on_y), Some(true));
    }

    /// A batch is imported in small groups: a 2.5 MB paste in pieces (which Loro's batch import takes minutes for in
    /// one go) comes in at once, and the groups never pass the limit but for one big update alone.
    #[test]
    fn a_big_paste_in_a_batch_is_imported_in_small_groups() {
        let sizes = [10usize, 20_000, 3, 9_000, 9_000, 1];
        let updates: Vec<Vec<u8>> = sizes.iter().map(|n| vec![0; *n]).collect();
        let g = groups(&updates, BATCH_GROUP_BYTES);
        assert_eq!(g.iter().map(|g| g.len()).collect::<Vec<_>>(), vec![1, 1, 2, 2]);
        assert!(groups(&[], BATCH_GROUP_BYTES).is_empty());
        let other = LoroDoc::new();
        other.set_peer_id(2).unwrap();
        let pieces: Vec<Vec<u8>> = (0..5).map(|i| text_update(&other, i * 500 * 1024, &"p".repeat(500 * 1024))).collect();
        let copy = LiveCopy::new(1);
        let started = std::time::Instant::now();
        assert!(copy.import_batch(&pieces).unwrap());
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        let back = LoroDoc::new();
        back.import(&copy.snapshot().unwrap()).unwrap();
        assert_eq!(back.get_text("text").len_utf8(), 5 * 500 * 1024, "all of it");
    }

    /// A 4 MiB paste on top of another peer's text: whole in one update, Loro's import takes seconds (time growing
    /// with the square of the text); as a bundle of slices (as the kit sends it), the copy takes it in a moment, and
    /// holds all of it. A bundle's ranges are its slices'.
    #[test]
    fn a_big_change_as_a_bundle_of_slices_imports_at_once() {
        use craft_cloud_proto::fragments::{SLICE_OPS, bundle};
        let importer = LoroDoc::new();
        importer.set_peer_id(40).unwrap();
        text_update(&importer, 0, "import");
        let import = importer.export(ExportMode::Snapshot).unwrap();
        let app = LoroDoc::new();
        app.set_peer_id(50).unwrap();
        app.import(&import).unwrap();
        app.get_text("text").insert(0, &"a".repeat(4 * 1024 * 1024)).unwrap();
        app.commit();
        let end = *app.oplog_vv().get(&50).unwrap();
        let mut slices = Vec::new();
        let mut at = 0;
        while at < end {
            let to = (at + SLICE_OPS).min(end);
            slices.push(app.export(ExportMode::updates_in_range(vec![IdSpan::new(50, at, to)])).unwrap());
            at = to;
        }
        let bundle_bytes = bundle(&slices);
        assert!(is_bundle(&bundle_bytes));
        assert_eq!(spans_of(&bundle_bytes), Some(vec![(50, 0, end)]), "its ranges, merged");
        let copy = LiveCopy::load(&import, 1).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(copy.import_any_size(&bundle_bytes).unwrap(), Taken::Applied { new: true });
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(copy.covers(&[(50, end)]));
        let back = LoroDoc::new();
        back.import(&copy.snapshot().unwrap()).unwrap();
        assert_eq!(back.get_text("text").len_utf8(), 4 * 1024 * 1024 + "import".len());
        // A snapshot of the copy as it is: at once too.
        let started = std::time::Instant::now();
        let at_now = copy.snapshot_at(&copy.version().unwrap()).unwrap();
        assert!(!at_now.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        // A bundle with a slice that doesn't read is refused whole: nothing of its other slices goes in.
        let fresh = LiveCopy::load(&import, 1).unwrap();
        let mut bad: Vec<Vec<u8>> = slices.iter().take(3).cloned().collect();
        bad.push(b"not an update".to_vec());
        assert!(fresh.import_any_size(&bundle(&bad)).is_err());
        assert!(!fresh.covers(&[(50, 1)]), "nothing of it in");
    }

    /// A 600 KB paste's bundle into a copy holding an app's edit of the same text made at the same moment, the edit's
    /// peer below the paste's (the order Loro is slow in, slice after slice: about a minute here): a few slices one
    /// by one, then the rest together, in seconds, and the copy holds both.
    #[test]
    fn a_bundle_into_a_text_edited_at_the_same_moment_goes_in_in_seconds() {
        use craft_cloud_proto::fragments::{SLICE_OPS, bundle};
        let importer = LoroDoc::new();
        importer.set_peer_id(40).unwrap();
        text_update(&importer, 0, "import");
        let import = importer.export(ExportMode::Snapshot).unwrap();
        let typist = LoroDoc::new();
        typist.set_peer_id(10).unwrap();
        typist.import(&import).unwrap();
        let typed = text_update(&typist, 0, "hello ");
        let paster = LoroDoc::new();
        paster.set_peer_id(50).unwrap();
        paster.import(&import).unwrap();
        let paste = "p".repeat(600 * 1024);
        paster.get_text("text").insert(6, &paste).unwrap();
        paster.commit();
        let end = *paster.oplog_vv().get(&50).unwrap();
        let mut slices = Vec::new();
        let mut at = 0;
        while at < end {
            let to = (at + SLICE_OPS).min(end);
            slices.push(paster.export(ExportMode::updates_in_range(vec![IdSpan::new(50, at, to)])).unwrap());
            at = to;
        }
        let copy = LiveCopy::load(&import, 1).unwrap();
        copy.import(&typed).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(copy.import_any_size(&bundle(&slices)).unwrap(), Taken::Applied { new: true });
        assert!(started.elapsed() < std::time::Duration::from_secs(15), "{:?}", started.elapsed());
        let back = LoroDoc::new();
        back.import(&copy.snapshot().unwrap()).unwrap();
        assert_eq!(back.get_text("text").to_string(), format!("hello import{paste}"));
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
