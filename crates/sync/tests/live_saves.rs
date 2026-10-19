//! Files saved by Craft apps from live documents: two machines saving one live-linked file at once never make a
//! conflict copy (the version whose save includes the other's wins, else the newer upload), the versions carry their
//! records to the server, and a file edited outside Craft still gets its conflict copy.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::{Machine, Server, machine, path};
use craft_cloud_proto::{DocId, FileHash, NodeId, SaveRecord};
use craft_cloud_sync::{Fs, SaveRecords, Transport};

/// What each machine's apps saved: (node, content hash) to record. Here a record's frontiers stand for a version
/// vector, `(peer, end)`, and one includes another as vectors do (the agent asks its copy of the document; the tests
/// need no Loro). With `unknown` set, nothing can be told.
#[derive(Default, Clone)]
struct Book(Arc<Mutex<HashMap<(NodeId, FileHash), SaveRecord>>>, Arc<std::sync::atomic::AtomicBool>);

impl SaveRecords for Book {
    fn record(&self, node: NodeId, hash: &FileHash) -> Option<SaveRecord> {
        self.0.lock().unwrap().get(&(node, *hash)).cloned()
    }

    fn includes(&self, a: &SaveRecord, b: &SaveRecord) -> Option<bool> {
        if self.1.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        Some(b.frontiers.iter().all(|(peer, end)| a.frontiers.iter().any(|(p, e)| p == peer && e >= end)))
    }
}

impl Book {
    /// The app saved `data` to `p` from the document at `version`.
    fn save(&self, m: &mut Machine, p: &str, data: &[u8], doc: DocId, version: &[(u64, u32)]) {
        m.fs_mut().write(&path(p), data, false, 0).unwrap();
        let node = node(m, p);
        self.0.lock().unwrap().insert((node, FileHash::from_bytes(*blake3::hash(data).as_bytes())), SaveRecord { doc, frontiers: version.to_vec() });
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
    // The file's live document, as the agent names it.
    let doc = craft_cloud_proto::live::node_doc(a.state().space.unwrap(), node(&a, "doc.kitdemo"));
    (a, b, book_a, book_b, doc)
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
    assert_eq!(file.and_then(|f| f.save), Some(SaveRecord { doc, frontiers: vec![(1, 6)] }));
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
        book_a.0.lock().unwrap().insert((at, FileHash::from_bytes(*blake3::hash(&laptop).as_bytes())), SaveRecord { doc, frontiers: vec![(1, 5)] });
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

/// The server's file version of `doc.kitdemo`, as `m` lists it.
async fn server_file(m: &Machine) -> craft_cloud_proto::FileState {
    let listing = m.transport().tree(None).await.unwrap();
    listing
        .nodes
        .into_iter()
        .find_map(|n| match n.kind {
            craft_cloud_proto::NodeKind::File(f) if !n.deleted => Some(f),
            _ => None,
        })
        .unwrap()
}

/// A record that reaches the agent after its file went up (the app saved while the agent was away): the next pass adds
/// it to that version on the server, once; the other machine learns it from the change feed; a version that has a
/// record keeps it; and the call again changes nothing.
#[tokio::test]
async fn a_record_that_comes_late_is_added_to_its_version() {
    let server = Server::new().await;
    let (mut a, mut b, book_a, _book_b, doc) = two(&server).await;
    let saved = b"saved while the agent was away".to_vec();
    a.fs_mut().write(&path("doc.kitdemo"), &saved, false, 0).unwrap();
    a.sync_once().await.unwrap();
    let up = server_file(&a).await;
    assert_eq!(up.save, None, "it went up without its record");
    // The record comes.
    let record = SaveRecord { doc, frontiers: vec![(1, 7)] };
    book_a.0.lock().unwrap().insert((node(&a, "doc.kitdemo"), FileHash::from_bytes(*blake3::hash(&saved).as_bytes())), record.clone());
    a.sync_once().await.unwrap();
    let late = server_file(&a).await;
    assert_eq!((late.version, late.save.clone()), (up.version, Some(record.clone())), "added to the same version");
    b.sync_once().await.unwrap();
    let on_b = b.state().remote.values().find_map(|n| n.file.clone()).unwrap();
    assert_eq!(on_b.save, Some(record.clone()), "the other machine learns it");
    // Appended only, and the same again changes nothing.
    let other = SaveRecord { doc, frontiers: vec![(9, 9)] };
    let seq = a.transport().space().await.unwrap().seq;
    a.transport().attach_save(node(&a, "doc.kitdemo"), up.version, &other).await.unwrap();
    a.transport().attach_save(node(&a, "doc.kitdemo"), up.version, &record).await.unwrap();
    assert_eq!(server_file(&a).await.save, Some(record));
    assert_eq!(a.transport().space().await.unwrap().seq, seq, "nothing changed");
    assert!(a.transport().attach_save(NodeId::generate(), up.version, &other).await.is_err(), "no such version there");
}

/// When the agent can't tell which of two Craft saves is newer, neither is taken: no conflict copy, nothing goes up
/// over the other, and both files stay as they are; once it can tell, the newer wins as always. Past the wait, it is
/// a conflict copy, as any two edits are (never a guessed winner).
#[tokio::test]
async fn saves_whose_order_is_unknown_wait_then_are_a_conflict() {
    use std::sync::atomic::Ordering;
    let server = Server::new().await;
    let (mut a, mut b, book_a, book_b, doc) = two(&server).await;
    book_a.1.store(true, Ordering::Relaxed);
    book_b.1.store(true, Ordering::Relaxed);
    book_a.save(&mut a, "doc.kitdemo", b"laptop's save", doc, &[(1, 5)]);
    book_b.save(&mut b, "doc.kitdemo", b"desktop's later save", doc, &[(1, 5), (2, 3)]);
    b.sync_once().await.unwrap();
    rounds(&mut a, &mut b).await;
    assert_eq!(files(&a), vec![("doc.kitdemo".to_owned(), b"laptop's save".to_vec())], "waits: kept as it is");
    assert_eq!(files(&b), vec![("doc.kitdemo".to_owned(), b"desktop's later save".to_vec())]);
    assert_eq!(server_file(&a).await.save.map(|s| s.frontiers), Some(vec![(1, 5), (2, 3)]), "nothing went up over it");
    // Known now: the desktop's includes the laptop's.
    book_a.1.store(false, Ordering::Relaxed);
    rounds(&mut a, &mut b).await;
    let want = vec![("doc.kitdemo".to_owned(), b"desktop's later save".to_vec())];
    assert_eq!((files(&a), files(&b)), (want.clone(), want));
    // Never known, on a machine that waits no longer: a conflict copy, as always.
    let ana = server.meta.user_by_name("ana").await.unwrap().unwrap();
    let mut c = common::machine_config(&server, &ana, "tablet", craft_cloud_sync::MemFs::new(), 50, |c| c.save_order_wait_ms = 0).await;
    let book_c = Book::default();
    c.set_save_records(Arc::new(book_c.clone()));
    c.sync_once().await.unwrap();
    book_c.1.store(true, Ordering::Relaxed);
    book_c.save(&mut c, "doc.kitdemo", b"tablet's save", doc, &[(3, 1)]);
    book_b.save(&mut b, "doc.kitdemo", b"desktop's next save", doc, &[(1, 5), (2, 4)]);
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        c.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
    }
    assert_eq!(files(&c).len(), 2, "a conflict copy: {:?}", files(&c));
}
