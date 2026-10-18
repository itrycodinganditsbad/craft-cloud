//! An app's recents document: a Loro document that every machine of the account merges, and the list apps see.
//!
//! **Layout.** The root map `entries` holds one entry per add, under a key of its own (16 random bytes as hex, never
//! reused), its value the entry as JSON. The root map `schema` holds `{kind: "recents", version: 1}`. One key per add
//! rather than one per file: a Loro map is last-writer-wins per key (by Lamport time, then peer), so if entries were
//! keyed by file, one machine pruning a file's old entry could win over another machine adding that same file anew at
//! the same moment, and the new add would be lost. With a key per add, a removal only ever names a key its machine has
//! seen, so it is causally after that key's insert and can't race any other add.
//!
//! **The list** ([`Recents::view`], [`view_of`]) is computed from the entries alone, the same way everywhere: newest
//! first (by `opened`, then by key), an entry hidden when a newer one names the same file (the same node; for an
//! entry without a node, the same path), and at most [`MAX_RECENTS`] of what is left. **Pruning**: the machine that
//! adds an entry removes, in the same commit, every entry the list no longer shows, so the document stays near the
//! cap. A removal can't drop an entry a merged state would show: an entry removed for being superseded or past the
//! cap here has a newer entry for the same file, or [`MAX_RECENTS`] newer files, wherever the updates meet.
//!
//! **Remote updates** ([`Recents::import`]) are refused only when Loro can't decode them or they are over the update
//! limit, the same on every machine. Entries that don't read or break the rules are hidden by the list, never removed
//! and never a reason to refuse an update: whatever depends on what a machine already has would let machines disagree.
//! What a document may grow to is bounded by the server's quota per document.
//! (`MAX_ENTRY_JSON` and the rules of `check_entry` are this agent's reading of an entry, nothing more.)

use std::collections::HashSet;

use craft_cloud_proto::NodeId;
use craft_cloud_proto::live::MAX_UPDATE_BYTES;
use craft_cloud_proto::local::{MAX_MACHINE_BYTES, MAX_RECENTS, MAX_TITLE_BYTES, RecentEntry, recent_path};
use loro::{ExportMode, IdSpan, LoroDoc, LoroValue, ValueOrContainer};

/// The key of one add: random, never reused.
pub type AddKey = [u8; 16];

/// The document's format, in its `schema` map.
pub const RECENTS_VERSION: i64 = 1;

/// Longest entry, in bytes of JSON.
const MAX_ENTRY_JSON: usize = 16 * 1024;

const ENTRIES: &str = "entries";
const SCHEMA: &str = "schema";

/// Why an update or a stored document was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("the recents update doesn't import: {0}")]
    Malformed(String),
    #[error("the recents document is of version {0}; this agent reads version {RECENTS_VERSION}")]
    Newer(i64),
    #[error("the recents update breaks a limit: {0}")]
    Invalid(&'static str),
    /// Loro panicked on it: the document may be half-changed and is read back from its file.
    #[error("Loro failed on the recents update")]
    Panicked,
}

fn check_entry(entry: &RecentEntry) -> Result<(), UpdateError> {
    if recent_path(&entry.path).as_deref() != Some(entry.path.as_str()) {
        return Err(UpdateError::Invalid("a path that isn't a clean relative path"));
    }
    if entry.title.len() > MAX_TITLE_BYTES || entry.title.chars().any(char::is_control) {
        return Err(UpdateError::Invalid("a title too long or with control characters"));
    }
    if entry.machine.len() > MAX_MACHINE_BYTES || entry.machine.chars().any(char::is_control) {
        return Err(UpdateError::Invalid("a machine name too long or with control characters"));
    }
    Ok(())
}

fn hex(key: &AddKey) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// A Loro peer ID this document may use (`u64::MAX` is reserved by Loro).
fn usable_peer(peer: u64) -> u64 {
    if peer == u64::MAX { peer - 1 } else { peer }
}

/// One app's recents document.
pub struct Recents {
    doc: LoroDoc,
    /// Loro panicked in a call on this document: its lock is poisoned, so the document is read back from its file and
    /// this one let go of without dropping it (dropping a poisoned Loro document panics again).
    poisoned: std::cell::Cell<bool>,
}

impl std::fmt::Debug for Recents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recents").field("entries", &self.len()).finish()
    }
}

/// What names a file in the list: its node, or its path while it has none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum File<'a> {
    Node(NodeId),
    Path(&'a str),
}

fn file_of(entry: &RecentEntry) -> File<'_> {
    match entry.node {
        Some(node) => File::Node(node),
        None => File::Path(&entry.path),
    }
}

/// Entries newest first, each with its key and whether the list shows it.
fn ranked(mut all: Vec<(String, RecentEntry)>) -> Vec<(String, RecentEntry, bool)> {
    all.sort_by(|(ka, a), (kb, b)| b.opened_ms.cmp(&a.opened_ms).then_with(|| kb.cmp(ka)));
    let mut seen: HashSet<File<'_>> = HashSet::new();
    let mut flags = Vec::with_capacity(all.len());
    let mut shown = 0usize;
    for (_, entry) in &all {
        let show = seen.insert(file_of(entry)) && shown < MAX_RECENTS;
        if show {
            shown += 1;
        }
        flags.push(show);
    }
    all.into_iter().zip(flags).map(|((k, e), show)| (k, e, show)).collect()
}

/// The list the rules make of these entries (by their add keys): newest first, one per file, at most [`MAX_RECENTS`].
pub fn view_of(entries: impl IntoIterator<Item = (AddKey, RecentEntry)>) -> Vec<RecentEntry> {
    let all = entries.into_iter().map(|(k, e)| (hex(&k), e)).collect();
    ranked(all).into_iter().filter(|(_, _, show)| *show).map(|(_, e, _)| e).collect()
}

/// An entry's value as the list reads it: `None` for one that doesn't read or breaks the rules (another agent's
/// bug, a newer agent's format, or a hostile device). Such an entry is never shown, and never removed.
fn read_entry(value: &ValueOrContainer) -> Option<RecentEntry> {
    match value {
        ValueOrContainer::Value(LoroValue::String(json)) if json.len() <= MAX_ENTRY_JSON => {
            serde_json::from_str::<RecentEntry>(json.as_str()).ok().filter(|e| check_entry(e).is_ok())
        }
        _ => None,
    }
}

/// The entries of a document that read, by key, and the keys of those that don't.
fn entries_of(doc: &LoroDoc) -> (Vec<(String, RecentEntry)>, Vec<String>) {
    let map = doc.get_map(ENTRIES);
    let (mut good, mut bad) = (Vec::with_capacity(map.len()), Vec::new());
    map.for_each(|key, value| match read_entry(&value) {
        Some(entry) if key.len() == 32 => good.push((key.to_owned(), entry)),
        _ => bad.push(key.to_owned()),
    });
    (good, bad)
}

/// What an import did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Imported {
    /// Applied; whether the list changed.
    Applied { changed: bool },
    /// Not wholly applied: some of it depends on updates this document doesn't have yet. Loro keeps that part and
    /// applies it as soon as they come; `needs` is how far each peer's operations must reach for it to be in
    /// ([`Recents::covers`]).
    Waiting { needs: Vec<(u64, i32)>, changed: bool },
}

/// Ranges of each Loro peer's operations (`[start, end)` counters), as a log is known to hold them.
pub type Spans = std::collections::BTreeMap<u64, Vec<(i32, i32)>>;

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
fn guarded<T>(f: impl FnOnce() -> Result<T, UpdateError>) -> Result<T, UpdateError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(Err(UpdateError::Panicked))
}

impl Recents {
    /// An empty document, editing as Loro peer `peer` (unique per machine and run).
    pub fn new(peer: u64) -> Self {
        let doc = LoroDoc::new();
        // A fresh document has no history, so any peer ID is accepted.
        let _ = doc.set_peer_id(usable_peer(peer));
        Self { doc, poisoned: std::cell::Cell::new(false) }
    }

    /// Runs `f` on the document under [`guarded`]; a panic marks it poisoned. Every Loro call on a document goes
    /// through here (it may hold any machine's data).
    fn guard<T>(&self, f: impl FnOnce(&LoroDoc) -> Result<T, UpdateError>) -> Result<T, UpdateError> {
        if self.poisoned.get() {
            return Err(UpdateError::Panicked);
        }
        let result = guarded(|| f(&self.doc));
        if matches!(result, Err(UpdateError::Panicked)) {
            self.poisoned.set(true);
        }
        result
    }

    /// Whether Loro panicked in a call on this document ([`Recents::guard`]): it must be read back from its file, and
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
        let recents = Self::new(peer);
        match recents.import_any_size(snapshot)? {
            Imported::Applied { .. } => Ok(recents),
            Imported::Waiting { .. } => Err(UpdateError::Malformed("a stored document that needs more history".into())),
        }
    }

    /// Merges an update (or a snapshot) from elsewhere. Refused only when Loro can't decode it, or it is over the
    /// update limit: those are the same on every machine. Nothing else about it is judged here, since anything that
    /// depends on what a machine already has (which value wins a key, how many entries there are) would let machines
    /// disagree about what they took, and their later updates would wait on each other for good. Entries that don't
    /// read, or break the rules, are only ever hidden by the list ([`read_entry`]).
    pub fn import(&self, bytes: &[u8]) -> Result<Imported, UpdateError> {
        if bytes.len() > MAX_UPDATE_BYTES {
            return Err(UpdateError::Invalid("over 1 MiB"));
        }
        self.import_any_size(bytes)
    }

    /// [`Recents::import`] without the update limit: for this machine's own stored document (or a simulation merging
    /// machines' documents), never for an update from the server.
    pub fn import_any_size(&self, bytes: &[u8]) -> Result<Imported, UpdateError> {
        let before = self.view();
        let status = self.guard(|doc| doc.import(bytes).map_err(|e| UpdateError::Malformed(e.to_string())))?;
        let changed = self.view() != before;
        let needs: Vec<(u64, i32)> = status.pending.map(|p| p.iter().map(|(peer, (_, end))| (*peer, *end)).collect()).unwrap_or_default();
        Ok(if needs.is_empty() { Imported::Applied { changed } } else { Imported::Waiting { needs, changed } })
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

    /// The whole document (a Loro snapshot): to store it, or to send it up again to a server that lost it.
    pub fn snapshot(&self) -> Result<Vec<u8>, UpdateError> {
        self.guard(|doc| doc.export(ExportMode::Snapshot).map_err(|e| UpdateError::Malformed(e.to_string())))
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
    /// Each peer's pieces must follow on from each other ([`Recents::export_prefix`] keeps them so): Loro panics on a
    /// range export of one peer's operations with a gap.
    pub fn export_changes(&self, changes: &[(u64, i32, i32)]) -> Result<Vec<u8>, UpdateError> {
        let mut spans: std::collections::BTreeMap<u64, (i32, i32)> = std::collections::BTreeMap::new();
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
        let mut ends: std::collections::BTreeMap<u64, i32> = std::collections::BTreeMap::new();
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

    /// The list apps see: newest first, one entry per file, at most [`MAX_RECENTS`].
    pub fn view(&self) -> Vec<RecentEntry> {
        let entries = self.guard(|doc| Ok(entries_of(doc).0)).unwrap_or_default();
        ranked(entries).into_iter().filter(|(_, _, show)| *show).map(|(_, e, _)| e).collect()
    }

    /// The newest entry for the file `node` (or, without one, `path`), if the list shows it.
    pub fn current(&self, node: Option<NodeId>, path: &str) -> Option<RecentEntry> {
        let want = match node {
            Some(node) => File::Node(node),
            None => File::Path(path),
        };
        self.view().into_iter().find(|e| file_of(e) == want)
    }

    /// The document's version if a newer agent wrote it (its `schema` map, as merged: the same on every machine once
    /// they have the same updates). Updates are taken all the same (refusing them would leave machines apart); this
    /// agent only stops adding to such a document.
    pub fn newer(&self) -> Option<i64> {
        self.guard(|doc| {
            Ok(match doc.get_map(SCHEMA).get("version") {
                Some(ValueOrContainer::Value(LoroValue::I64(version))) if version > RECENTS_VERSION => Some(version),
                _ => None,
            })
        })
        .unwrap_or(None)
    }

    /// Adds `entry` under `key`, removes what the list would no longer show with it, in one commit, and returns that
    /// commit as a Loro update. A document a newer agent wrote is refused ([`UpdateError::Newer`]).
    pub fn add(&self, key: AddKey, entry: &RecentEntry) -> Result<Vec<u8>, UpdateError> {
        if let Some(version) = self.newer() {
            return Err(UpdateError::Newer(version));
        }
        let json = serde_json::to_string(entry).map_err(|e| UpdateError::Malformed(e.to_string()))?;
        self.guard(|doc| {
            let before = doc.oplog_vv();
            let fail = |e: loro::LoroError| UpdateError::Malformed(e.to_string());
            let schema = doc.get_map(SCHEMA);
            if schema.get("version").is_none() {
                schema.insert("kind", "recents").map_err(fail)?;
                schema.insert("version", RECENTS_VERSION).map_err(fail)?;
            }
            let entries = doc.get_map(ENTRIES);
            entries.insert(&hex(&key), json).map_err(fail)?;
            // What the list no longer shows: superseded entries and those past the cap. An entry that doesn't read is
            // only hidden, never removed: another agent version may read it (and its rules must not remove this one's).
            let (good, _) = entries_of(doc);
            for gone in ranked(good).into_iter().filter(|(_, _, show)| !show).map(|(k, _, _)| k) {
                entries.delete(&gone).map_err(fail)?;
            }
            doc.commit();
            doc.export(ExportMode::updates(&before)).map_err(|e| UpdateError::Malformed(e.to_string()))
        })
    }

    /// Every entry that reads, shown or not, by its key (hex): what the list is made from.
    pub fn readable(&self) -> Vec<(String, RecentEntry)> {
        self.guard(|doc| Ok(entries_of(doc).0)).unwrap_or_default()
    }

    /// How many entries the document holds (shown or not yet pruned).
    pub fn len(&self) -> usize {
        self.guard(|doc| Ok(doc.get_map(ENTRIES).len())).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(node: Option<NodeId>, path: &str, opened_ms: i64) -> RecentEntry {
        RecentEntry { node, path: path.into(), title: path.into(), opened_ms, saved_ms: None, machine: "pc".into() }
    }

    fn key(n: u32) -> AddKey {
        let mut k = [0u8; 16];
        k[..4].copy_from_slice(&n.to_be_bytes());
        k
    }

    #[test]
    fn a_file_is_listed_once_newest_first() {
        let node = NodeId::generate();
        let r = Recents::new(1);
        r.add(key(1), &entry(Some(node), "a.docx", 10)).unwrap();
        r.add(key(2), &entry(None, "b.docx", 20)).unwrap();
        r.add(key(3), &entry(Some(node), "renamed.docx", 30)).unwrap();
        let paths: Vec<String> = r.view().into_iter().map(|e| e.path).collect();
        assert_eq!(paths, vec!["renamed.docx", "b.docx"]);
        assert_eq!(r.current(Some(node), "x").map(|e| e.opened_ms), Some(30));
        assert_eq!(r.len(), 2, "the older entry for the same node was removed");
    }

    #[test]
    fn the_list_is_capped_and_pruned_by_the_adder() {
        let r = Recents::new(1);
        for i in 0..150u32 {
            r.add(key(i), &entry(None, &format!("f{i}.txt"), i64::from(i))).unwrap();
        }
        let view = r.view();
        assert_eq!(view.len(), MAX_RECENTS);
        assert_eq!(view.first().map(|e| e.path.as_str()), Some("f149.txt"));
        assert_eq!(view.last().map(|e| e.path.as_str()), Some("f50.txt"));
        assert_eq!(r.len(), MAX_RECENTS, "the oldest were removed");
    }

    /// Two machines adding at once, each pruning on its own, meet on the same list, in any order and with repeats.
    #[test]
    fn concurrent_adds_merge_to_one_list_in_any_order() {
        let base = Recents::new(1);
        for i in 0..100u32 {
            base.add(key(i), &entry(None, &format!("f{i}.txt"), i64::from(i))).unwrap();
        }
        let snapshot = base.snapshot().unwrap();
        let (a, b, c) = (Recents::load(&snapshot, 2).unwrap(), Recents::load(&snapshot, 3).unwrap(), Recents::load(&snapshot, 4).unwrap());
        let ua = a.add(key(1000), &entry(None, "a.txt", 500)).unwrap();
        let ub = b.add(key(1001), &entry(None, "b.txt", 501)).unwrap();
        assert_eq!(a.import(&ub).unwrap(), Imported::Applied { changed: true });
        b.import(&ua).unwrap();
        c.import(&ub).unwrap();
        c.import(&ua).unwrap();
        assert_eq!(c.import(&ub).unwrap(), Imported::Applied { changed: false }, "a repeat changes nothing");
        assert_eq!(a.view(), b.view());
        assert_eq!(a.view(), c.view());
        assert_eq!(a.view().len(), MAX_RECENTS);
        let paths: Vec<String> = a.view().into_iter().map(|e| e.path).take(2).collect();
        assert_eq!(paths, vec!["b.txt", "a.txt"]);
        // An update whose history isn't here yet waits in Loro, and applies once the history came.
        let d = Recents::new(5);
        let Imported::Waiting { needs, .. } = d.import(&ua).unwrap() else { panic!("not waiting") };
        assert!(d.view().is_empty() && !d.covers(&needs));
        assert!(!d.has_history());
        d.import(&snapshot).unwrap();
        // Loro drops what waits when a snapshot comes into a document without history; imported again, it applies.
        assert!(!d.covers(&needs), "Loro kept it after all: the replica need not import held updates again");
        d.import(&ua).unwrap();
        assert!(d.covers(&needs), "in with its history");
        assert_eq!(d.view().first().map(|e| e.path.as_str()), Some("a.txt"));
    }

    /// A commit from `from` that sets `key(k)` to `json`.
    fn honest(from: &LoroDoc, k: u32, json: String) -> Vec<u8> {
        let vv = from.oplog_vv();
        from.get_map(ENTRIES).insert(&hex(&key(k)), json).unwrap();
        from.commit();
        from.export(ExportMode::updates(&vv)).unwrap()
    }

    /// A device's update with an entry that breaks the rules, then a good one depending on it. Refusing the first would
    /// leave the second waiting in Loro for good (that device stalled): the first is taken, its entry never shown, and
    /// the second applies.
    #[test]
    fn a_bad_entry_doesnt_stall_its_device() {
        let r = Recents::new(1);
        let other = LoroDoc::new();
        let _ = other.set_peer_id(9);
        let u1 = honest(&other, 1, serde_json::to_string(&entry(None, "../bad.txt", 1)).unwrap());
        let u2 = honest(&other, 2, serde_json::to_string(&entry(None, "good.txt", 2)).unwrap());
        assert_eq!(r.import(&u1).unwrap(), Imported::Applied { changed: false }, "taken, not shown");
        assert_eq!(r.import(&u2).unwrap(), Imported::Applied { changed: true });
        assert_eq!(r.view().iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["good.txt"]);
        // The next add removes only what the list doesn't show of what reads; the entry that
        // doesn't read stays (another agent version may read it).
        r.add(key(3), &entry(None, "mine.txt", 3)).unwrap();
        assert_eq!(r.len(), 3);
    }

    /// An add removes only readable entries the list doesn't show, never entries it can't read (of the wrong shape, or
    /// a newer format): another agent version may read them.
    #[test]
    fn an_add_keeps_entries_it_cant_read() {
        let r = Recents::new(1);
        let other = LoroDoc::new();
        let _ = other.set_peer_id(9);
        let _ = honest(&other, 1, "{\"a newer format\": true}".to_owned());
        r.import(&other.export(ExportMode::Snapshot).unwrap()).unwrap();
        for i in 2..=(MAX_RECENTS as u32 + 5) {
            r.add(key(i), &entry(None, &format!("f{i}.txt"), i64::from(i))).unwrap();
        }
        assert_eq!(r.len(), MAX_RECENTS + 1, "the cap's 100 and the one it can't read");
        assert!(r.doc.get_map(ENTRIES).get(&hex(&key(1))).is_some());
    }

    /// An update that depends on one not here yet waits in Loro and comes in with its
    /// history. Its shape is never a reason to refuse it (that would depend on what a machine has): an entry of the
    /// wrong shape is only hidden.
    #[test]
    fn a_waiting_update_comes_in_with_its_history() {
        let r = Recents::new(1);
        let other = LoroDoc::new();
        let _ = other.set_peer_id(9);
        let u1 = honest(&other, 1, serde_json::to_string(&entry(None, "good.txt", 1)).unwrap());
        // Of the wrong shape: a value that isn't an entry string at all.
        let vv = other.oplog_vv();
        other.get_map(ENTRIES).insert(&hex(&key(2)), 42i64).unwrap();
        other.commit();
        let u2 = other.export(ExportMode::updates(&vv)).unwrap();
        let Imported::Waiting { needs, .. } = r.import(&u2).unwrap() else { panic!("u2 first: waits") };
        assert_eq!(r.import(&u1).unwrap(), Imported::Applied { changed: true });
        assert!(r.covers(&needs), "u2 came in with u1");
        assert_eq!((r.len(), r.view().len()), (2, 1), "u2's entry is hidden");
        // Unrelated updates and local adds go on.
        let third = LoroDoc::new();
        let _ = third.set_peer_id(10);
        let u3 = honest(&third, 3, serde_json::to_string(&entry(None, "later.txt", 3)).unwrap());
        assert_eq!(r.import(&u3).unwrap(), Imported::Applied { changed: true });
        assert!(r.add(key(4), &entry(None, "mine.txt", 4)).is_ok());
        assert_eq!(r.view().len(), 3);
    }

    /// The list rule alone, from entries: what the simulation compares against.
    #[test]
    fn the_rule_alone_gives_the_same_list() {
        let r = Recents::new(1);
        let mut all = Vec::new();
        for i in 0..130u32 {
            let e = entry(None, &format!("f{}.txt", i % 110), i64::from(i));
            r.add(key(i), &e).unwrap();
            all.push((key(i), e));
        }
        assert_eq!(r.view(), view_of(all));
    }

    #[test]
    fn a_snapshot_is_the_whole_document() {
        let r = Recents::new(1);
        for i in 0..120u32 {
            r.add(key(i), &entry(None, &format!("f{i}.txt"), i64::from(i))).unwrap();
        }
        let copy = Recents::load(&r.snapshot().unwrap(), 2).unwrap();
        assert_eq!(copy.view(), r.view());
    }

    /// Remote updates: one Loro refuses, or over the limit, leaves the document as it was; anything else is taken,
    /// and an entry whose content or shape breaks the rules is never shown.
    #[test]
    fn hostile_updates_are_refused_or_hidden() {
        let r = Recents::new(1);
        r.add(key(1), &entry(None, "ok.txt", 1)).unwrap();
        let before = r.view();
        assert!(r.import(&[1, 2, 3, 4, 5, 6]).is_err());
        assert!(r.import(&[]).is_err());
        assert!(r.import(&vec![0; MAX_UPDATE_BYTES + 1]).is_err());
        for (k, bad) in [
            serde_json::to_string(&entry(None, "../escape.txt", 2)).unwrap(),
            serde_json::to_string(&RecentEntry { title: "x".repeat(MAX_TITLE_BYTES + 1), ..entry(None, "t.txt", 2) }).unwrap(),
            "not json".to_owned(),
        ]
        .into_iter()
        .enumerate()
        {
            let other = LoroDoc::new();
            let _ = other.set_peer_id(9 + k as u64);
            other.import(&r.snapshot().unwrap()).unwrap();
            let update = honest(&other, 77 + k as u32, bad);
            assert_eq!(r.import(&update).unwrap(), Imported::Applied { changed: false }, "taken, never shown");
            assert_eq!(r.view(), before);
        }
        // The wrong shape (not a string, or over 16 KiB): taken, hidden; the good entry beside it shows.
        for (i, value) in [LoroValue::I64(1), LoroValue::from("x".repeat(MAX_ENTRY_JSON + 1))].into_iter().enumerate() {
            let other = LoroDoc::new();
            let _ = other.set_peer_id(20 + i as u64);
            other.import(&r.snapshot().unwrap()).unwrap();
            let vv = other.oplog_vv();
            other.get_map(ENTRIES).insert(&hex(&key(90 + i as u32)), value).unwrap();
            other
                .get_map(ENTRIES)
                .insert(&hex(&key(92 + i as u32)), serde_json::to_string(&entry(None, &format!("fine{i}.txt"), 3)).unwrap())
                .unwrap();
            other.commit();
            assert_eq!(r.import(&other.export(ExportMode::updates(&vv)).unwrap()).unwrap(), Imported::Applied { changed: true });
            assert_eq!(r.view().len(), before.len() + 1 + i, "only the good entry shows");
        }
        // A document of a newer version.
        let newer = LoroDoc::new();
        newer.get_map(SCHEMA).insert("version", RECENTS_VERSION + 1).unwrap();
        newer.commit();
        let fresh = Recents::new(2);
        fresh.add(key(5), &entry(None, "mine.txt", 1)).unwrap();
        assert!(fresh.import(&newer.export(ExportMode::Snapshot).unwrap()).is_ok(), "taken, so every machine stays alike");
        let version = fresh.newer();
        // Which version wins is Loro's last-writer rule, the same everywhere; with it newer, adds stop here.
        if let Some(v) = version {
            assert_eq!(v, RECENTS_VERSION + 1);
            assert_eq!(fresh.add(key(6), &entry(None, "more.txt", 2)), Err(UpdateError::Newer(v)));
        }
        let other = Recents::new(3);
        other.import(&newer.export(ExportMode::Snapshot).unwrap()).unwrap();
        assert_eq!(other.newer(), Some(RECENTS_VERSION + 1), "alone, the newer version holds");
        assert!(matches!(other.add(key(7), &entry(None, "x.txt", 1)), Err(UpdateError::Newer(_))));
    }

    /// What a log lacks goes in causal order; a peer whose operations the log holds a middle of goes
    /// in two parts (one export of a peer's operations with a gap made Loro panic, found by the simulation), and the
    /// parts, imported in order into a copy of what the log held, make the whole document without waiting.
    #[test]
    fn missing_changes_go_in_causal_order_without_gaps() {
        let r = Recents::new(1);
        let other = Recents::new(2);
        let mut updates = Vec::new();
        for i in 0..6u32 {
            let (who, other_one) = if i % 2 == 0 { (&r, &other) } else { (&other, &r) };
            let u = who.add(key(i), &entry(None, &format!("f{i}.txt"), i64::from(i))).unwrap();
            other_one.import(&u).unwrap();
            updates.push(u);
        }
        // The log holds the third update only (a middle of peer 1's operations).
        let mut has = Spans::new();
        add_spans(&mut has, &spans_of(&updates[2]).unwrap());
        let copy = Recents::new(3);
        copy.import(&updates[2]).unwrap();
        let mut parts = 0;
        loop {
            let missing = r.missing(&has).unwrap();
            let Some(first) = missing.first() else { break };
            assert!(missing.windows(2).all(|w| w[0] != w[1]), "{first:?}");
            let (bytes, _) = r.export_prefix(&missing, 200, 1024 * 1024).unwrap();
            add_spans(&mut has, &spans_of(&bytes).unwrap());
            copy.import(&bytes).unwrap();
            parts += 1;
            assert!(parts < 50);
        }
        assert_eq!(copy.view(), r.view(), "every part applied once the ones before it were in");
        assert!(copy.covers(&r.doc.oplog_vv().iter().map(|(p, e)| (*p, *e)).collect::<Vec<_>>()));
    }

    /// An update is never refused for its shape: that would depend on what a machine has (m1 pruned the key the rogue
    /// overwrites, m2 didn't), so m1 would take it and m2 refuse it, and their lists would part for good.
    #[test]
    fn a_wrong_shape_doesnt_part_two_machines() {
        let a = Recents::new(1);
        let base = a.add(key(1), &entry(None, "k.txt", 1)).unwrap();
        let (m1, m2) = (Recents::new(2), Recents::new(3));
        m1.import(&base).unwrap();
        m2.import(&base).unwrap();
        // The rogue, from the base: overwrites key 1 with a wrong-shape value, and adds a good entry, in one commit.
        let rogue = LoroDoc::new();
        let _ = rogue.set_peer_id(9);
        rogue.import(&base).unwrap();
        let vv = rogue.oplog_vv();
        rogue.get_map(ENTRIES).insert(&hex(&key(1)), "x".repeat(MAX_ENTRY_JSON + 1)).unwrap();
        rogue.get_map(ENTRIES).insert(&hex(&key(50)), serde_json::to_string(&entry(None, "rogue-good.txt", 5)).unwrap()).unwrap();
        rogue.commit();
        let u = rogue.export(ExportMode::updates(&vv)).unwrap();
        // m1, honest, concurrently adds twice; the second add re-adds k.txt and prunes key 1 (a later Lamport time).
        let d1 = m1.add(key(2), &entry(None, "other.txt", 2)).unwrap();
        let d2 = m1.add(key(3), &entry(None, "k.txt", 3)).unwrap();
        m1.import(&u).unwrap();
        m2.import(&u).unwrap();
        // m1's next honest add depends on u.
        let d3 = m1.add(key(4), &entry(None, "after.txt", 4)).unwrap();
        for d in [&d1, &d2, &d3] {
            m2.import(d).unwrap();
        }
        let v1: Vec<String> = m1.view().into_iter().map(|e| e.path).collect();
        let v2: Vec<String> = m2.view().into_iter().map(|e| e.path).collect();
        assert_eq!(v1, v2, "the two machines show different lists");
    }
}
