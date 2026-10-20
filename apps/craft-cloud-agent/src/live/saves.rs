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
use crate::cli::now_ms;

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

/// Each book's saves possibly on their way ([`Pending`]), by book file.
static PENDING: Mutex<BTreeMap<PathBuf, Pending>> = Mutex::new(BTreeMap::new());
/// How long content is waited for in all, at most, from the first time it was: no announcement renews it.
pub const MAX_PENDING_WAIT: Duration = Duration::from_secs(10 * 60);
/// How long after an app session had a document open a change to its file still counts as a Craft save on its way
/// (the session may be reconnecting, or the agent restarting), and an app connection's announcements still hold after
/// it went.
pub const SESSION_GRACE: Duration = Duration::from_secs(60);
/// Most announcements one app connection has held at once (the kit keeps 16 records); past it, the oldest go.
pub const MAX_ANNOUNCED_PER_CONN: usize = 64;

/// Whether an app session has a document open on this machine, or since when none has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Open,
    ClosedAt(Instant),
}

/// Saves a Craft app may be making whose records haven't come: the sync waits for them rather than take the content
/// for an edit outside Craft ([`SaveRecords::pending`]).
///
/// Content of a document's file counts as such a save while an app session has the document open on this machine and
/// for [`SESSION_GRACE`] after (reconnecting, or the agent restarting: the agent keeps when sessions had documents open,
/// [`Pending::sessions`]), or while a connection's announcement of it holds (at most [`MAX_ANNOUNCED_PER_CONN`] per
/// connection, only of documents it has open, for [`SESSION_GRACE`] after it went). Content waits from the first time
/// it was waited for: an announced save at most [`MAX_PENDING_WAIT`], one only a session makes likely (never announced:
/// saved while the app had no agent) at most [`SESSION_GRACE`]; never again after, and announcing renews nothing. Past
/// that the content counts as made outside Craft, as without an app.
#[derive(Debug, Default)]
pub struct Pending {
    by_conn: HashMap<u64, std::collections::VecDeque<(DocId, String)>>,
    /// Announcements of connections that went, and until when they hold.
    gone: HashMap<(DocId, String), Instant>,
    sessions: HashMap<DocId, Seen>,
    /// The first time each content was waited for.
    first: HashMap<(DocId, String), Instant>,
    /// The grace, when not [`SESSION_GRACE`] (tests).
    grace: Option<Duration>,
}

impl Pending {
    /// Another grace than [`SESSION_GRACE`] (tests: the run loop's `session_grace`).
    pub fn set_grace(&mut self, grace: Duration) {
        self.grace = Some(grace);
    }

    fn grace(&self) -> Duration {
        self.grace.unwrap_or(SESSION_GRACE)
    }

    /// Connection `conn` announced a save of `doc` with content `hash`; refused (false) when it hasn't `doc` open.
    pub fn announce(&mut self, conn: u64, doc: DocId, hash: &str, open: bool) -> bool {
        if !open {
            return false;
        }
        let mine = self.by_conn.entry(conn).or_default();
        if !mine.iter().any(|(d, h)| *d == doc && h == hash) {
            mine.push_back((doc, hash.to_owned()));
            while mine.len() > MAX_ANNOUNCED_PER_CONN {
                mine.pop_front();
            }
        }
        true
    }

    /// Connection `conn` closed `doc`: its announcements of it no longer hold.
    pub fn closed(&mut self, conn: u64, doc: DocId) {
        if let Some(mine) = self.by_conn.get_mut(&conn) {
            mine.retain(|(d, _)| *d != doc);
        }
    }

    /// Connection `conn` went without closing: its announcements hold [`SESSION_GRACE`] more.
    pub fn gone(&mut self, conn: u64, now: Instant) {
        let grace = self.grace();
        for key in self.by_conn.remove(&conn).unwrap_or_default() {
            self.gone.insert(key, now + grace);
        }
    }

    /// Whether an app session has `doc` open on this machine now.
    pub fn session(&mut self, doc: DocId, open: bool, now: Instant) {
        let seen = if open { Seen::Open } else { Seen::ClosedAt(now) };
        self.sessions.insert(doc, seen);
    }

    /// The record of `doc`'s content `hash` came: nothing more to wait for.
    pub fn recorded(&mut self, doc: DocId, hash: &str) {
        let key = (doc, hash.to_owned());
        for mine in self.by_conn.values_mut() {
            mine.retain(|k| *k != key);
        }
        self.gone.remove(&key);
        self.first.remove(&key);
    }

    /// Whether content `hash` of `doc`'s file is (possibly) a Craft save on its way, to be waited for now.
    pub fn is_pending(&mut self, doc: DocId, hash: &str, now: Instant) -> bool {
        let (key, grace) = ((doc, hash.to_owned()), self.grace());
        let session = match self.sessions.get(&doc) {
            Some(Seen::Open) => true,
            Some(Seen::ClosedAt(at)) => now.saturating_duration_since(*at) < grace,
            None => false,
        };
        let announced = self.by_conn.values().any(|mine| mine.contains(&key)) || self.gone.get(&key).is_some_and(|until| now < *until);
        if !session && !announced {
            return false;
        }
        // (Old firsts are forgotten after a day; one that waited its time stays done until then.)
        self.first.retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(24 * 3600));
        let first = *self.first.entry(key).or_insert(now);
        now.saturating_duration_since(first) < if announced { MAX_PENDING_WAIT } else { grace }
    }

    /// When sessions last had documents open, as kept on disk: by document, `None` while one has it open, else when it
    /// closed (ms since the epoch).
    pub fn sessions(&self, now: Instant, now_ms: i64) -> BTreeMap<DocId, Option<i64>> {
        self.sessions
            .iter()
            .filter_map(|(doc, seen)| match seen {
                Seen::Open => Some((*doc, None)),
                Seen::ClosedAt(at) => {
                    let ago = now.saturating_duration_since(*at);
                    (ago < self.grace()).then(|| (*doc, Some(now_ms - i64::try_from(ago.as_millis()).unwrap_or(i64::MAX))))
                }
            })
            .collect()
    }

    /// What [`Pending::sessions`] kept, read back at the agent's start: one open then went at the restart.
    pub fn restore(&mut self, kept: &BTreeMap<DocId, Option<i64>>, now: Instant, now_ms: i64) {
        for (doc, closed) in kept {
            let ago = closed.map_or(Duration::ZERO, |ms| Duration::from_millis(u64::try_from(now_ms.saturating_sub(ms)).unwrap_or(u64::MAX)));
            if ago < self.grace() {
                self.sessions.insert(*doc, Seen::ClosedAt(now.checked_sub(ago).unwrap_or(now)));
            }
        }
    }
}

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

    /// This book's saves possibly on their way ([`Pending`]), shared by every book of its file in the process.
    pub fn with_pending<T>(&self, f: impl FnOnce(&mut Pending) -> T) -> T {
        let mut all = PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(all.entry(self.path.clone()).or_default())
    }

    /// Where [`Pending::sessions`] is kept: next to the book.
    fn sessions_path(&self) -> PathBuf {
        self.path.with_file_name("sessions.json")
    }

    /// Writes when sessions last had documents open (atomically), so a restarted agent still waits for their saves.
    pub fn keep_sessions(&self) -> Result<(), String> {
        let kept = self.with_pending(|p| p.sessions(Instant::now(), now_ms()));
        let json = serde_json::to_vec(&kept).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        write_atomic(&self.sessions_path(), &json, true).map_err(|e| e.to_string())
    }

    /// Reads back what [`SaveBook::keep_sessions`] wrote (at the agent's start).
    pub fn restore_sessions(&self) {
        let kept: BTreeMap<DocId, Option<i64>> =
            std::fs::read(self.sessions_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        self.with_pending(|p| p.restore(&kept, Instant::now(), now_ms()));
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
        self.with_pending(|p| p.recorded(doc, hash));
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
        let doc = node_doc(self.space, node);
        let hash = hex(hash.as_bytes());
        self.find(doc, &hash).is_none() && self.with_pending(|p| p.is_pending(doc, &hash, Instant::now()))
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

    /// The bounds on saves possibly on their way: a session's document waits while open and a grace after; an
    /// announcement holds only for a document its connection has open, at most 64 per connection (the oldest go),
    /// a grace after the connection went, and no longer once closed; each content waits 10 minutes in all, from its
    /// first wait, and announcing again doesn't renew it; a record ends the wait; what is kept on disk comes back.
    #[test]
    fn pending_saves_are_bounded() {
        let t0 = Instant::now();
        let (doc, other) = (DocId::generate(), DocId::generate());
        let mut p = Pending::default();
        assert!(!p.is_pending(doc, "h", t0), "nothing known");
        // Sessions.
        p.session(doc, true, t0);
        assert!(p.is_pending(doc, "h", t0), "while open");
        assert!(p.is_pending(doc, "o", t0));
        assert!(!p.is_pending(doc, "o", t0 + SESSION_GRACE), "never announced: a grace at most, even while open");
        p.session(doc, false, t0);
        assert!(p.is_pending(doc, "h2", t0 + SESSION_GRACE - Duration::from_secs(1)), "a grace after");
        assert!(!p.is_pending(doc, "h3", t0 + SESSION_GRACE), "not past it");
        // Announcements: only of an open document.
        assert!(!p.announce(1, other, "a", false));
        assert!(!p.is_pending(other, "a", t0));
        assert!(p.announce(1, other, "a", true));
        assert!(p.is_pending(other, "a", t0));
        // At most 64 per connection: the oldest go.
        for i in 0..MAX_ANNOUNCED_PER_CONN {
            assert!(p.announce(1, other, &format!("n{i}"), true));
        }
        assert!(!p.is_pending(other, "a", t0), "the oldest went");
        assert!(p.is_pending(other, "n63", t0));
        // Closed: they no longer hold. Gone: a grace more.
        p.closed(1, other);
        assert!(!p.is_pending(other, "n63", t0));
        assert!(p.announce(2, other, "g", true));
        p.gone(2, t0);
        assert!(p.is_pending(other, "g", t0 + SESSION_GRACE - Duration::from_secs(1)));
        assert!(!p.is_pending(other, "g", t0 + SESSION_GRACE));
        // Ten minutes in all, never renewed.
        assert!(p.announce(3, other, "long", true));
        assert!(p.is_pending(other, "long", t0));
        assert!(p.is_pending(other, "long", t0 + MAX_PENDING_WAIT - Duration::from_secs(1)));
        assert!(!p.is_pending(other, "long", t0 + MAX_PENDING_WAIT));
        assert!(p.announce(4, other, "long", true));
        assert!(!p.is_pending(other, "long", t0 + MAX_PENDING_WAIT + Duration::from_secs(5)), "announcing again renews nothing");
        // A record ends it.
        assert!(p.announce(3, other, "r", true));
        p.recorded(other, "r");
        p.closed(3, other);
        assert!(!p.is_pending(other, "r", t0));
        // Kept on disk and read back: a document open at the restart waits a grace from it.
        let mut q = Pending::default();
        q.session(doc, true, t0);
        let kept = q.sessions(t0, 1_000_000);
        let mut back = Pending::default();
        back.restore(&kept, t0, 1_000_000);
        assert!(back.is_pending(doc, "x", t0 + Duration::from_secs(1)));
        assert!(!back.is_pending(doc, "y", t0 + SESSION_GRACE));
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
