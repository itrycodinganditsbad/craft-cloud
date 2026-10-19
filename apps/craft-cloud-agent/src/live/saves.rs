//! Save records: what Craft apps tell the agent when they save a live document's file (the content's hash and the
//! document's version then, `DocsRequest::Saved`), kept in `<data folder>/live/<space>/saves.json` (atomically, the
//! newest [`MAX_SAVES`]) so a pass after a restart still has them. The sync engine reads them ([`SaveRecords`]): a
//! version it uploads with saved content carries the record, and two saved versions of one document never make a
//! conflict copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use craft_cloud_proto::live::node_doc;
use craft_cloud_proto::ops::MAX_SAVE_PEERS;
use craft_cloud_proto::{DocId, FileHash, NodeId, SaveRecord, SpaceId};
use craft_cloud_sync::SaveRecords;
use serde::{Deserialize, Serialize};

use crate::account::write_atomic;

/// How many saves the book keeps (newest first).
pub const MAX_SAVES: usize = 512;

/// One save.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    doc: DocId,
    /// The content's BLAKE3 hash, hex.
    hash: String,
    version: Vec<(u64, u32)>,
    at_ms: i64,
}

/// Adds from several app connections at once take turns.
static WRITING: Mutex<()> = Mutex::new(());

/// The book as last read: the file's modification time and length then, and its entries by document and content.
type Read = (Option<(std::time::SystemTime, u64)>, Arc<HashMap<(DocId, String), Entry>>);

/// A space's save records.
#[derive(Debug, Clone)]
pub struct SaveBook {
    path: PathBuf,
    space: SpaceId,
    /// Read again only when the file changed (the sync engine asks for every file of every pass).
    cache: Arc<Mutex<Option<Read>>>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl SaveBook {
    pub fn new(data: &Path, space: SpaceId) -> Self {
        Self { path: data.join("live").join(space.to_string()).join("saves.json"), space, cache: Arc::new(Mutex::new(None)) }
    }

    fn entries(&self) -> Vec<Entry> {
        std::fs::read(&self.path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// The entries by document and content, from the cache unless the file changed.
    fn by_key(&self) -> Arc<HashMap<(DocId, String), Entry>> {
        let stamp = std::fs::metadata(&self.path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((seen, map)) = cache.as_ref()
            && *seen == stamp
            && stamp.is_some()
        {
            return map.clone();
        }
        let mut map = HashMap::new();
        // Newest first in the file: the first of a key is kept.
        for e in self.entries() {
            map.entry((e.doc, e.hash.clone())).or_insert(e);
        }
        let map = Arc::new(map);
        *cache = Some((stamp, map.clone()));
        map
    }

    /// The record of `doc`'s save with content `hash` (hex), if an app on this machine saved exactly that.
    pub fn find(&self, doc: DocId, hash: &str) -> Option<SaveRecord> {
        self.by_key().get(&(doc, hash.to_owned())).map(|e| SaveRecord { doc, version: e.version.clone() })
    }

    /// Notes a save of `doc` with content `hash` (64 hex digits) at `version`. Refused (`Err` with why) when the
    /// hash or the version doesn't look like one.
    pub fn add(&self, doc: DocId, hash: &str, version: Vec<(u64, u32)>, at_ms: i64) -> Result<(), String> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err("a save's hash is 64 lowercase hex digits".into());
        }
        if version.len() > MAX_SAVE_PEERS {
            return Err(format!("a save's version has at most {MAX_SAVE_PEERS} entries"));
        }
        let mut version = version;
        version.sort_unstable();
        version.dedup_by_key(|(peer, _)| *peer);
        let _turn = WRITING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = self.entries();
        entries.retain(|e| !(e.doc == doc && e.hash == hash));
        entries.insert(0, Entry { doc, hash: hash.to_owned(), version, at_ms });
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

impl SaveRecords for SaveBook {
    fn record(&self, node: NodeId, hash: &FileHash) -> Option<SaveRecord> {
        self.find(node_doc(self.space, node), &hex(hash.as_bytes()))
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
        assert_eq!(book.record(node, &hash), Some(SaveRecord { doc, version: vec![(1, 9), (2, 1)] }), "sorted");
        assert_eq!(book.record(NodeId::generate(), &hash), None, "another file's document");
        assert!(book.add(doc, "nope", vec![], 1).is_err());
        assert!(book.add(doc, &hex(&[1; 32]), vec![(0, 0); MAX_SAVE_PEERS + 1], 1).is_err());
        for i in 0..MAX_SAVES + 5 {
            book.add(doc, &hex(&[(i % 250) as u8; 32]), vec![(1, i as u32)], i as i64).unwrap();
        }
        assert!(book.entries().len() <= MAX_SAVES);
        let reopened = SaveBook::new(temp.path(), space);
        assert!(reopened.record(node, &FileHash::from_bytes([3; 32])).is_some(), "kept on disk");
    }
}
