//! Files saved by Craft apps from live documents: two machines saving one live-linked file at once never make a
//! conflict copy (the version whose save includes the other's wins, else the newer upload), the versions carry their
//! records to the server, and a file edited outside Craft still gets its conflict copy.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::{Machine, Server, machine, path};
use craft_cloud_proto::{DocId, FileHash, NodeId, SaveRecord};
use craft_cloud_sync::{Fs, SaveRecords, Transport};

/// What each machine's apps saved: (node, content hash) to record.
#[derive(Default, Clone)]
struct Book(Arc<Mutex<HashMap<(NodeId, FileHash), SaveRecord>>>);

impl SaveRecords for Book {
    fn record(&self, node: NodeId, hash: &FileHash) -> Option<SaveRecord> {
        self.0.lock().unwrap().get(&(node, *hash)).cloned()
    }
}

impl Book {
    /// The app saved `data` to `p` from the document at `version`.
    fn save(&self, m: &mut Machine, p: &str, data: &[u8], doc: DocId, version: &[(u64, u32)]) {
        m.fs_mut().write(&path(p), data, false, 0).unwrap();
        let node = node(m, p);
        self.0.lock().unwrap().insert((node, FileHash::from_bytes(*blake3::hash(data).as_bytes())), SaveRecord { doc, version: version.to_vec() });
    }
}

fn node(m: &Machine, p: &str) -> NodeId {
    let want = path(p);
    m.state().base.keys().copied().find(|id| m.state().base_path(*id).as_ref() == Some(&want)).unwrap()
}

fn files(m: &Machine) -> Vec<(String, Vec<u8>)> {
    m.fs().files().into_iter().map(|(p, d)| (p.to_string(), d)).collect()
}

/// Two machines with save books, sharing `doc.kitdemo` (live-linked) at `v1`.
async fn two(server: &Server) -> (Machine, Machine, Book, Book, DocId) {
    let ana = server.user("ana").await;
    let (mut a, mut b) = (machine(server, &ana, "laptop").await, machine(server, &ana, "desktop").await);
    let (book_a, book_b) = (Book::default(), Book::default());
    a.set_save_records(Arc::new(book_a.clone()));
    b.set_save_records(Arc::new(book_b.clone()));
    a.fs_mut().write(&path("doc.kitdemo"), b"v1", false, 0).unwrap();
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    (a, b, book_a, book_b, DocId::generate())
}

async fn rounds(a: &mut Machine, b: &mut Machine) {
    for _ in 0..4 {
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
    }
}

/// The desktop's save includes everything the laptop's had (it was made later from the live document): the laptop
/// takes the desktop's version, and there is no conflict copy.
#[tokio::test]
async fn a_save_that_includes_the_other_wins_without_a_conflict_copy() {
    let server = Server::new().await;
    let (mut a, mut b, book_a, book_b, doc) = two(&server).await;
    book_a.save(&mut a, "doc.kitdemo", b"laptop's save", doc, &[(1, 5)]);
    book_b.save(&mut b, "doc.kitdemo", b"desktop's later save", doc, &[(1, 5), (2, 3)]);
    b.sync_once().await.unwrap();
    rounds(&mut a, &mut b).await;
    let want = vec![("doc.kitdemo".to_owned(), b"desktop's later save".to_vec())];
    assert_eq!(files(&a), want);
    assert_eq!(files(&b), want);
}

/// Neither save includes the other (both machines edited the live document offline, say): the newer upload wins
/// (it overwrites, no conflict copy; the live document has both sides' edits, and the next save writes them all).
#[tokio::test]
async fn concurrent_saves_take_the_newer_upload_without_a_conflict_copy() {
    let server = Server::new().await;
    let (mut a, mut b, book_a, book_b, doc) = two(&server).await;
    book_a.save(&mut a, "doc.kitdemo", b"laptop's save", doc, &[(1, 6)]);
    book_b.save(&mut b, "doc.kitdemo", b"desktop's save", doc, &[(1, 5), (2, 3)]);
    b.sync_once().await.unwrap();
    rounds(&mut a, &mut b).await;
    let want = vec![("doc.kitdemo".to_owned(), b"laptop's save".to_vec())];
    assert_eq!(files(&a), want, "the laptop's went up last");
    assert_eq!(files(&b), want);
    // The version on the server carries the laptop's record.
    let listing = a.transport().tree(None).await.unwrap();
    let file = listing.nodes.into_iter().find_map(|n| match n.kind {
        craft_cloud_proto::NodeKind::File(f) if !n.deleted => Some(f),
        _ => None,
    });
    assert_eq!(file.and_then(|f| f.save), Some(SaveRecord { doc, version: vec![(1, 6)] }));
}

/// Autosave on both machines, round after round: never a conflict copy, and both end on the same content.
#[tokio::test]
async fn autosaves_on_both_machines_never_make_a_conflict_copy() {
    let server = Server::new().await;
    let (mut a, mut b, book_a, book_b, doc) = two(&server).await;
    for round in 0..6u32 {
        // Each machine's live document has the other's earlier edits and its own new one.
        book_a.save(&mut a, "doc.kitdemo", format!("laptop {round}").as_bytes(), doc, &[(1, 10 + round), (2, 10 + round)]);
        book_b.save(&mut b, "doc.kitdemo", format!("desktop {round}").as_bytes(), doc, &[(1, 10 + round), (2, 11 + round)]);
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
    }
    rounds(&mut a, &mut b).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(files(&a).len(), 1, "no conflict copy: {:?}", files(&a));
}

/// One side edited outside Craft (no record): a conflict copy, as always.
#[tokio::test]
async fn a_file_edited_outside_craft_still_gets_a_conflict_copy() {
    let server = Server::new().await;
    let (mut a, mut b, book_a, _book_b, doc) = two(&server).await;
    book_a.save(&mut a, "doc.kitdemo", b"laptop's save", doc, &[(1, 5)]);
    b.fs_mut().write(&path("doc.kitdemo"), b"edited in a text editor", false, 0).unwrap();
    b.sync_once().await.unwrap();
    rounds(&mut a, &mut b).await;
    let all = files(&a);
    assert_eq!(all, files(&b));
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all.iter().any(|(p, _)| p.contains("(conflict")), "{all:?}");
    assert!(all.iter().any(|(_, d)| d == b"laptop's save") && all.iter().any(|(_, d)| d == b"edited in a text editor"));
}

/// Saves of two different documents (a copy of the file, say) are not one document's: a conflict copy.
#[tokio::test]
async fn saves_of_different_documents_are_a_conflict() {
    let server = Server::new().await;
    let (mut a, mut b, book_a, book_b, doc) = two(&server).await;
    book_a.save(&mut a, "doc.kitdemo", b"from one document", doc, &[(1, 5)]);
    book_b.save(&mut b, "doc.kitdemo", b"from another", DocId::generate(), &[(1, 5), (2, 1)]);
    b.sync_once().await.unwrap();
    rounds(&mut a, &mut b).await;
    assert_eq!(files(&a).len(), 2, "{:?}", files(&a));
}

/// The server's save includes the laptop's, so the laptop's file is to be replaced; but another program saves the
/// file while the engine reads it (at each of its reads in turn). That save is never replaced unseen: it ends as a
/// file on both machines (a conflict copy, having no record).
#[tokio::test]
async fn a_save_landing_while_the_server_wins_is_kept() {
    use common::{SaveDuring, SaveOn, machine_on};
    for n in 1..=5 {
        let server = Server::new().await;
        let ana = server.user("ana").await;
        let mut a = machine_on(&server, &ana, "laptop", SaveDuring::default(), 50, |_| {}).await;
        let mut b = machine(&server, &ana, "desktop").await;
        let (book_a, book_b) = (Book::default(), Book::default());
        a.set_save_records(Arc::new(book_a.clone()));
        b.set_save_records(Arc::new(book_b.clone()));
        a.fs().write(&path("doc.kitdemo"), b"v1");
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
        let doc = DocId::generate();
        // The laptop's save, and the desktop's later one that includes it.
        let laptop = b"laptop's save".to_vec();
        a.fs().write(&path("doc.kitdemo"), &laptop);
        let at = a.state().base.keys().copied().find(|id| a.state().base_path(*id).as_ref() == Some(&path("doc.kitdemo"))).unwrap();
        book_a.0.lock().unwrap().insert((at, FileHash::from_bytes(*blake3::hash(&laptop).as_bytes())), SaveRecord { doc, version: vec![(1, 5)] });
        book_b.save(&mut b, "doc.kitdemo", b"desktop's later save", doc, &[(1, 5), (2, 3)]);
        b.sync_once().await.unwrap();
        a.fs().arm(path("doc.kitdemo"), b"another program's save, meanwhile", SaveOn::Open(n));
        for _ in 0..5 {
            a.sync_once().await.unwrap();
            b.sync_once().await.unwrap();
        }
        let on_a: Vec<Vec<u8>> = a.fs().files().into_values().collect();
        let on_b: Vec<Vec<u8>> = b.fs().files().into_values().collect();
        if a.fs().fired() {
            for files in [&on_a, &on_b] {
                assert!(files.iter().any(|d| d == b"another program's save, meanwhile"), "read {n}: {files:?}");
            }
        }
        // (A save landing after the download replaced the file is an ordinary later edit: it goes up over the
        // desktop's version, which stays in the server's history.)
        assert_eq!(on_a.len(), on_b.len(), "read {n}");
    }
}
