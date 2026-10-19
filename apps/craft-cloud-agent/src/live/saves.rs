//! Save records: what Craft apps tell the agent when they save a live document's file (the content's hash and the
//! document's frontiers then, `DocsRequest::Saved`), kept in `<data folder>/live/<space>/saves.json` (atomically, the
//! newest [`SAVES_PER_DOC`] of each document, [`MAX_SAVES`] in all) so a pass after a restart still has them. The sync
//! engine reads them ([`SaveRecords`]): a version it uploads with saved content carries the record, and two saved
//! versions of one document never make a conflict copy. Which of two records is newer only the live hub can say, from
//! its copy of the document ([`Orders`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use craft_cloud_proto::live::node_doc;
use craft_cloud_proto::ops::MAX_SAVE_HEADS;
use craft_cloud_proto::{DocId, FileHash, NodeId, SaveRecord, SpaceId};
use craft_cloud_sync::SaveRecords;
use serde::{Deserialize, Serialize};

use crate::account::write_atomic;

/// How many saves the book keeps of one document (newest first)…
pub const SAVES_PER_DOC: usize = 32;
/// …and in all.
pub const MAX_SAVES: usize = 4096;
/// How often the book looks whether its file changed (the engine asks for every file of every pass).
const LOOK_EVERY: Duration = Duration::from_millis(500);
/// How long the sync waits for the live hub to say which of two saves is newer.
const ORDER_WAIT: Duration = Duration::from_secs(2);

/// One save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    doc: DocId,
    /// The content's BLAKE3 hash, hex.
    hash: String,
    frontiers: Vec<(u64, u32)>,
    at_ms: i64,
}

/// Adds from several app connections at once take turns.
static WRITING: Mutex<()> = Mutex::new(());

/// Saves announced (`saving`) whose records haven't come yet, by book file, then by document and content: since when.
type Announced = HashMap<(DocId, String), Instant>;
static ANNOUNCED: Mutex<BTreeMap<PathBuf, Announced>> = Mutex::new(BTreeMap::new());
/// How long an announced save is waited for: as long as the sync waits to learn which of two saves is newer.
pub const ANNOUNCED_FOR: Duration = Duration::from_secs(10 * 60);

/// The books' caches, by file (shared by every [`SaveBook`] of one file in the process).
type Cache = Arc<Mutex<Option<Read>>>;
static CACHES: Mutex<BTreeMap<PathBuf, Cache>> = Mutex::new(BTreeMap::new());

/// The book as last read: when it was looked at, the file's modification time and length then, and its entries by
/// document and content.
type Read = (Instant, Option<(std::time::SystemTime, u64)>, Arc<HashMap<(DocId, String), Entry>>);

/// A space's save records.
#[derive(Debug, Clone)]
pub struct SaveBook {
    path: PathBuf,
    space: SpaceId,
    /// Read again after an add here, or when the file changed (looked at no more often than [`LOOK_EVERY`]).
    cache: Cache,
}

/// A question for the live hub: whether `doc` as frontiers `a` name it holds every operation of frontiers `b`. The
/// answer is `None` when its copy lacks the operations (or the document isn't here).
#[derive(Debug)]
pub struct Order {
    pub doc: DocId,
    pub a: Vec<(u64, u32)>,
    pub b: Vec<(u64, u32)>,
    pub reply: std::sync::mpsc::Sender<Option<bool>>,
}

/// Where each running hub takes [`Order`] questions, by its save book's file (one per data folder and space).
static ORDERS: Mutex<BTreeMap<PathBuf, tokio::sync::mpsc::UnboundedSender<Order>>> = Mutex::new(BTreeMap::new());

/// The live hub's end of [`Order`] questions: registered while it lives.
pub struct Orders {
    key: PathBuf,
}

impl Orders {
    /// Takes the questions about the save book of `data` and `space` from now on (until dropped).
    pub fn serve(data: &Path, space: SpaceId) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Order>) {
        let key = SaveBook::new(data, space).path;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        ORDERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.clone(), tx);
        (Self { key }, rx)
    }
}

impl Drop for Orders {
    fn drop(&mut self) {
        ORDERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.key);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl SaveBook {
    pub fn new(data: &Path, space: SpaceId) -> Self {
        let path = data.join("live").join(space.to_string()).join("saves.json");
        // One cache for every book of this file in the process, so an add is seen at once by the sync engine's.
        let cache = CACHES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(path.clone()).or_default().clone();
        Self { path, space, cache }
    }

    /// The entries on disk; one that doesn't read (an earlier build's form, a damaged one) is left out, the rest kept.
    fn entries(&self) -> Vec<Entry> {
        let all: Vec<serde_json::Value> = std::fs::read(&self.path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        all.into_iter().filter_map(|e| serde_json::from_value(e).ok()).collect()
    }

    /// The entries by document and content, from the cache unless the file changed.
    fn by_key(&self) -> Arc<HashMap<(DocId, String), Entry>> {
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, _, map)) = cache.as_ref()
            && at.elapsed() < LOOK_EVERY
        {
            return map.clone();
        }
        let stamp = std::fs::metadata(&self.path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        if let Some((at, seen, map)) = cache.as_mut()
            && *seen == stamp
            && stamp.is_some()
        {
            *at = Instant::now();
            return map.clone();
        }
        let mut map = HashMap::new();
        // Newest first in the file: the first of a key is kept.
        for e in self.entries() {
            map.entry((e.doc, e.hash.clone())).or_insert(e);
        }
        let map = Arc::new(map);
        *cache = Some((Instant::now(), stamp, map.clone()));
        map
    }

    /// An app announced it is writing `doc`'s file with content `hash` (hex), its record to follow (kept in memory: an
    /// app announces its saves again when it reconnects).
    pub fn announce(&self, doc: DocId, hash: &str) {
        if self.find(doc, hash).is_some() {
            return;
        }
        let mut all = ANNOUNCED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let mine = all.entry(self.path.clone()).or_default();
        mine.retain(|_, since| now.duration_since(*since) < ANNOUNCED_FOR);
        mine.entry((doc, hash.to_owned())).or_insert(now);
    }

    /// The announced save of `doc` with content `hash` won't have its record (its app closed the document first).
    pub fn forget(&self, doc: DocId, hash: &str) {
        if let Some(mine) = ANNOUNCED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&self.path) {
            mine.remove(&(doc, hash.to_owned()));
        }
    }

    /// Whether a save of `doc` with content `hash` (hex) was announced and its record hasn't come yet (for at most
    /// [`ANNOUNCED_FOR`]).
    pub fn announced(&self, doc: DocId, hash: &str) -> bool {
        ANNOUNCED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.path)
            .and_then(|mine| mine.get(&(doc, hash.to_owned())))
            .is_some_and(|since| since.elapsed() < ANNOUNCED_FOR)
    }

    /// The record of `doc`'s save with content `hash` (hex), if an app on this machine saved exactly that.
    pub fn find(&self, doc: DocId, hash: &str) -> Option<SaveRecord> {
        self.by_key().get(&(doc, hash.to_owned())).map(|e| SaveRecord { doc, frontiers: e.frontiers.clone() })
    }

    /// Whether `hash` and `frontiers` look like a save's: 64 lowercase hex digits, and at most [`MAX_SAVE_HEADS`]
    /// heads (`Err` says why).
    pub fn check(hash: &str, frontiers: &[(u64, u32)]) -> Result<(), String> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err("a save's hash is 64 lowercase hex digits".into());
        }
        if frontiers.len() > MAX_SAVE_HEADS {
            return Err(format!("a save's frontiers have at most {MAX_SAVE_HEADS} heads"));
        }
        Ok(())
    }

    /// Notes a save of `doc` with content `hash` (64 hex digits) at `frontiers`. Refused (`Err` with why) when the
    /// hash or the frontiers don't look like a save's ([`SaveBook::check`]).
    pub fn add(&self, doc: DocId, hash: &str, frontiers: Vec<(u64, u32)>, at_ms: i64) -> Result<(), String> {
        Self::check(hash, &frontiers)?;
        // Its record came: no longer on its way.
        self.forget(doc, hash);
        let mut frontiers = frontiers;
        frontiers.sort_unstable();
        frontiers.dedup();
        let _turn = WRITING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = self.entries();
        entries.retain(|e| !(e.doc == doc && e.hash == hash));
        entries.insert(0, Entry { doc, hash: hash.to_owned(), frontiers, at_ms });
        // The newest of each document, and of all.
        let mut per_doc: HashMap<DocId, usize> = HashMap::new();
        entries.retain(|e| {
            let n = per_doc.entry(e.doc).or_default();
            *n += 1;
            *n <= SAVES_PER_DOC
        });
        entries.truncate(MAX_SAVES);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec(&entries).map_err(|e| e.to_string())?;
        let written = write_atomic(&self.path, &json, true).map_err(|e| e.to_string());
        // (A write within the file clock's resolution could keep its stamp.)
        *self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        written
    }
}

/// Whether the content `hash` (hex) an app read from `doc`'s file was edited outside Craft, decided once, at the open:
/// - an app on this machine saved exactly that ([`SaveBook::find`], `saved_here`): no;
/// - it is the content the sync agreed on (`base`: version and hash) and that version is the server's: no if the
///   version carries a record of this document (an app on another machine saved it), yes if none;
/// - it is the agreed content but the server has a newer version: the sync brings that one; can't tell (none);
/// - other content (an edit here the sync hasn't taken, with no record here): yes.
///
/// No window of recent saves: every Craft save of the file is either this machine's record or its version's.
pub fn changed_outside(
    doc: DocId,
    hash: &str,
    saved_here: bool,
    base: Option<(craft_cloud_proto::VersionId, FileHash)>,
    remote: Option<&craft_cloud_proto::FileState>,
) -> Option<bool> {
    if saved_here {
        return Some(false);
    }
    let (version, agreed) = base?;
    if hex(agreed.as_bytes()) != hash {
        return Some(true);
    }
    let remote = remote?;
    if remote.version != version {
        return None;
    }
    Some(!remote.save.as_ref().is_some_and(|s| s.doc == doc))
}

impl SaveRecords for SaveBook {
    fn record(&self, node: NodeId, hash: &FileHash) -> Option<SaveRecord> {
        self.find(node_doc(self.space, node), &hex(hash.as_bytes()))
    }

    fn pending(&self, node: NodeId, hash: &FileHash) -> bool {
        self.announced(node_doc(self.space, node), &hex(hash.as_bytes()))
    }

    /// Asked of the live hub, which holds the document's operations; `None` when no hub runs, it doesn't answer in
    /// time, or its copy can't tell.
    fn includes(&self, a: &SaveRecord, b: &SaveRecord) -> Option<bool> {
        if a.doc != b.doc {
            return Some(false);
        }
        let hub = ORDERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&self.path).cloned()?;
        let (reply, answer) = std::sync::mpsc::channel();
        hub.send(Order { doc: a.doc, a: a.frontiers.clone(), b: b.frontiers.clone(), reply }).ok()?;
        answer.recv_timeout(ORDER_WAIT).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_are_kept_by_document_and_content() {
        let temp = tempfile::tempdir().unwrap();
        let space = SpaceId::generate();
        let book = SaveBook::new(temp.path(), space);
        let node = NodeId::generate();
        let doc = node_doc(space, node);
        let hash = FileHash::from_bytes([7; 32]);
        assert_eq!(book.record(node, &hash), None);
        book.add(doc, &hex(&[7; 32]), vec![(2, 1), (1, 9)], 1).unwrap();
        assert_eq!(book.record(node, &hash), Some(SaveRecord { doc, frontiers: vec![(1, 9), (2, 1)] }), "sorted");
        assert_eq!(book.record(NodeId::generate(), &hash), None, "another file's document");
        assert!(book.add(doc, "nope", vec![], 1).is_err());
        assert!(book.add(doc, &hex(&[1; 32]), vec![(0, 0); MAX_SAVE_HEADS + 1], 1).is_err());
        // Many saves of one document keep its newest; another document's saves stay.
        let other = node_doc(space, NodeId::generate());
        book.add(other, &hex(&[9; 32]), vec![(5, 5)], 1).unwrap();
        for i in 0..SAVES_PER_DOC + 5 {
            book.add(doc, &hex(&[(i % 250) as u8; 32]), vec![(1, i as u32)], i as i64).unwrap();
        }
        assert_eq!(book.entries().iter().filter(|e| e.doc == doc).count(), SAVES_PER_DOC);
        assert!(book.find(other, &hex(&[9; 32])).is_some(), "a busy document never pushes another's out");
        let reopened = SaveBook::new(temp.path(), space);
        assert!(reopened.record(node, &FileHash::from_bytes([20; 32])).is_some(), "kept on disk");
        assert!(reopened.record(node, &FileHash::from_bytes([0; 32])).is_none(), "the oldest went");
        // An entry in an earlier build's form is left out; the others still read.
        let mut on_disk: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&book.path).unwrap()).unwrap();
        on_disk.insert(0, serde_json::json!({ "doc": doc, "hash": hex(&[200; 32]), "version": [[1, 5]], "at_ms": 1 }));
        std::fs::write(&book.path, serde_json::to_vec(&on_disk).unwrap()).unwrap();
        let reread = SaveBook { path: book.path.clone(), space, cache: Arc::new(Mutex::new(None)) };
        assert!(reread.record(node, &FileHash::from_bytes([20; 32])).is_some(), "the others read");
        assert!(reread.record(node, &FileHash::from_bytes([200; 32])).is_none());
        // No hub: which is newer can't be told.
        let (a, b) = (SaveRecord { doc, frontiers: vec![(1, 2)] }, SaveRecord { doc, frontiers: vec![(1, 1)] });
        assert_eq!(book.includes(&a, &b), None);
    }

    /// Whether a file was edited outside Craft: by this machine's records, or the agreed version's record, whatever
    /// number of saves came before.
    #[test]
    fn a_file_is_changed_outside_unless_a_craft_save_has_its_content() {
        use craft_cloud_proto::{DeviceId, Epoch, FileState, ManifestId, VersionId};
        let doc = DocId::generate();
        let (v, h) = (VersionId::generate(), FileHash::from_bytes([5; 32]));
        let file = |version: VersionId, save: Option<SaveRecord>| FileState {
            version,
            manifest: ManifestId::from_bytes([0; 32]),
            size: 1,
            mtime_ms: 0,
            exec: false,
            device: DeviceId::generate(),
            epoch: Epoch::generate(),
            save,
        };
        let ours = Some(SaveRecord { doc, frontiers: vec![(1, 2)] });
        let theirs = Some(SaveRecord { doc: DocId::generate(), frontiers: vec![(1, 2)] });
        let agreed = hex(h.as_bytes());
        assert_eq!(changed_outside(doc, "ab", true, None, None), Some(false), "saved here");
        assert_eq!(changed_outside(doc, &agreed, false, Some((v, h)), Some(&file(v, ours.clone()))), Some(false), "saved elsewhere");
        assert_eq!(changed_outside(doc, &agreed, false, Some((v, h)), Some(&file(v, None))), Some(true), "a version without a record");
        assert_eq!(changed_outside(doc, &agreed, false, Some((v, h)), Some(&file(v, theirs))), Some(true), "another document's record");
        assert_eq!(changed_outside(doc, &agreed, false, Some((v, h)), Some(&file(VersionId::generate(), ours))), None, "a newer version comes");
        assert_eq!(changed_outside(doc, "other", false, Some((v, h)), None), Some(true), "edited here");
        assert_eq!(changed_outside(doc, &agreed, false, None, None), None, "not synced");
    }
}
