//! Save records: what Craft apps tell the agent when they save a live document's file (the content's hash and the
//! document's version then, `DocsRequest::Saved`), kept in `<data folder>/live/<space>/saves.json` (atomically, the
//! newest [`MAX_SAVES`]) so a pass after a restart still has them. The sync engine reads them ([`SaveRecords`]): a
//! version it uploads with saved content carries the record, and two saved versions of one document never make a
//! conflict copy.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// A space's save records.
#[derive(Debug, Clone)]
pub struct SaveBook {
    path: PathBuf,
    space: SpaceId,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl SaveBook {
    pub fn new(data: &Path, space: SpaceId) -> Self {
        Self { path: data.join("live").join(space.to_string()).join("saves.json"), space }
    }

    fn entries(&self) -> Vec<Entry> {
        std::fs::read(&self.path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
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
        write_atomic(&self.path, &json, true).map_err(|e| e.to_string())
    }
}

impl SaveRecords for SaveBook {
    fn record(&self, node: NodeId, hash: &FileHash) -> Option<SaveRecord> {
        let doc = node_doc(self.space, node);
        let hash = hex(hash.as_bytes());
        self.entries().into_iter().find(|e| e.doc == doc && e.hash == hash).map(|e| SaveRecord { doc, version: e.version })
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
