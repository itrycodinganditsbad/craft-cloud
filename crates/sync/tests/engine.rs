//! The engine's behaviour, scenario by scenario, on machines syncing through an in-process server.

mod common;

use std::sync::atomic::Ordering;

use common::{SaveDuring, SaveOn, Server, machine, machine_on, machine_with, path, settle};
use craft_cloud_sync::{Fs, MemFs, Transport};

const CONFLICT_STAMP: &str = "(conflict, desktop, 2026-10-11 14-03)";

fn conflict_copies(m: &common::Machine) -> Vec<String> {
    files(m).into_iter().map(|(p, _)| p).filter(|p| p.contains("(conflict")).collect()
}

fn write(m: &mut common::Machine, p: &str, data: &[u8]) {
    m.fs_mut().write(&path(p), data, false, 0).unwrap();
}

fn read(m: &common::Machine, p: &str) -> Vec<u8> {
    m.fs().read(&path(p)).unwrap()
}

fn files(m: &common::Machine) -> Vec<(String, Vec<u8>)> {
    m.fs().files().into_iter().map(|(p, d)| (p.to_string(), d)).collect()
}

#[tokio::test]
async fn a_folder_reaches_a_second_machine() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Shots")).unwrap();
    write(&mut a, "Shots/010.psd", b"layered image");
    write(&mut a, "notes.txt", b"hello");
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded.len(), 2);
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.downloaded.len(), 2);
    assert_eq!(files(&a), files(&b));
    assert_eq!(b.fs().dirs(), vec![path("Shots")]);
    // Nothing more to do on either side.
    assert!(!a.sync_once().await.unwrap().changed());
    assert!(!b.sync_once().await.unwrap().changed());
}

#[tokio::test]
async fn edits_renames_moves_and_deletes_propagate() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "x/f.txt", b"one");
    write(&mut a, "g.txt", b"gone soon");
    settle(&mut [&mut a, &mut b], 5).await;

    write(&mut a, "x/f.txt", b"two");
    a.fs_mut().rename(&path("x"), &path("y")).unwrap();
    a.fs_mut().remove_file(&path("g.txt")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.moved_remotely.len(), 1, "the folder move is one operation: {report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b), vec![("y/f.txt".to_owned(), b"two".to_vec())]);

    // A file move keeps its identity (and so its history) instead of becoming delete + create.
    b.fs_mut().mkdir(&path("z")).unwrap();
    b.fs_mut().rename(&path("y/f.txt"), &path("z/f.txt")).unwrap();
    let report = b.sync_once().await.unwrap();
    assert_eq!((report.moved_remotely.len(), report.uploaded.len()), (1, 0));
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), vec![("z/f.txt".to_owned(), b"two".to_vec())]);
    assert_eq!(a.fs().dirs(), vec![path("y"), path("z")]);
}

#[tokio::test]
async fn both_edited_keeps_both_copies() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "doc.txt", b"base");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut a, "doc.txt", b"from laptop");
    write(&mut b, "doc.txt", b"from desktop");
    a.sync_once().await.unwrap();
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.conflict_copies, vec![path("doc (conflict, desktop, 2026-10-11 14-03).txt")]);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&a, "doc.txt"), b"from laptop");
    assert_eq!(read(&a, "doc (conflict, desktop, 2026-10-11 14-03).txt"), b"from desktop");
}

#[tokio::test]
async fn an_edit_wins_over_a_delete() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("d")).unwrap();
    write(&mut a, "d/f.txt", b"v1");
    settle(&mut [&mut a, &mut b], 5).await;
    // One machine deletes the file and its folder; the other edits the file.
    a.fs_mut().remove_file(&path("d/f.txt")).unwrap();
    a.fs_mut().remove_dir(&path("d")).unwrap();
    a.sync_once().await.unwrap();
    write(&mut b, "d/f.txt", b"v2 edited offline");
    settle(&mut [&mut b, &mut a], 5).await;
    assert_eq!(files(&a), vec![("d/f.txt".to_owned(), b"v2 edited offline".to_vec())]);
    assert_eq!(files(&a), files(&b));
}

#[tokio::test]
async fn mass_deletes_wait_for_approval() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    for i in 0..20 {
        write(&mut a, &format!("f{i}.txt"), format!("file {i}").as_bytes());
    }
    settle(&mut [&mut a, &mut b], 5).await;
    // Everything vanishes on one machine (an unmounted drive, a wrong rm -rf).
    for i in 0..20 {
        a.fs_mut().remove_file(&path(&format!("f{i}.txt"))).unwrap();
    }
    let report = a.sync_once().await.unwrap();
    assert_eq!((report.pending_deletes.len(), report.removed_remotely.len()), (20, 0));
    b.sync_once().await.unwrap();
    assert_eq!(b.fs().files().len(), 20);
    // A few deletes go through without asking.
    write(&mut a, "keep.txt", b"k");
    a.approve_deletes();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.removed_remotely.len(), 20);
    // The desktop asks too before it removes them.
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(b.fs().files().len(), 21);
    b.approve_deletes();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b), vec![("keep.txt".to_owned(), b"k".to_vec())]);
}

#[tokio::test]
async fn mass_deletes_from_the_server_wait_for_approval_here_too() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    for i in 0..30 {
        write(&mut a, &format!("f{i}.txt"), format!("file {i}").as_bytes());
    }
    settle(&mut [&mut a, &mut b], 5).await;
    for i in 0..30 {
        a.fs_mut().remove_file(&path(&format!("f{i}.txt"))).unwrap();
    }
    a.approve_deletes();
    assert_eq!(a.sync_once().await.unwrap().removed_remotely.len(), 30);
    // The desktop holds the removals back and asks; nothing goes back up meanwhile.
    for _ in 0..2 {
        let report = b.sync_once().await.unwrap();
        assert!(report.removed_locally.is_empty() && report.uploaded.is_empty(), "{report:?}");
        assert_eq!(report.pending_local_deletes.len(), 30);
        assert!(report.pending_deletes.is_empty());
    }
    assert_eq!(b.fs().files().len(), 30);
    b.approve_deletes();
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.removed_locally.len(), 30);
    settle(&mut [&mut a, &mut b], 5).await;
    assert!(files(&b).is_empty() && files(&a).is_empty());
}

#[tokio::test]
async fn a_first_sync_cut_short_starts_over_as_a_first_sync() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "doc.txt", b"from the laptop");
    a.sync_once().await.unwrap();
    write(&mut b, "doc.txt", b"from the desktop");
    // The first call (the space) goes through, the listing fails.
    b.transport().faults.every.store(2, Ordering::Relaxed);
    assert!(b.sync_once().await.is_err());
    b.transport().faults.every.store(0, Ordering::Relaxed);
    assert!(b.state().space.is_none());
    b.sync_once().await.unwrap();
    assert!(!b.state().reconcile_pending);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&b, "doc.txt"), b"from the laptop");
    assert_eq!(conflict_copies(&b).len(), 1);
    assert_eq!(files(&a), files(&b));
}

#[tokio::test]
async fn joining_with_existing_files_never_overwrites_them() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "same.txt", b"identical");
    write(&mut a, "differs.txt", b"laptop version");
    a.sync_once().await.unwrap();
    // The second machine already has files at the same paths.
    let mut fs = MemFs::new();
    fs.write(&path("same.txt"), b"identical", false, 0).unwrap();
    fs.write(&path("differs.txt"), b"desktop version", false, 0).unwrap();
    let mut b = machine_with(&server, &ana, "desktop", fs, 50).await;
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.downloaded, vec![path("differs.txt")]);
    assert_eq!(report.conflict_copies, vec![path("differs (conflict, desktop, 2026-10-11 14-03).txt")]);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, "differs (conflict, desktop, 2026-10-11 14-03).txt"), b"desktop version");
}

#[tokio::test]
async fn after_a_server_restore_local_files_are_kept_and_reuploaded() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "a.txt", b"v1");
    a.sync_once().await.unwrap();
    write(&mut a, "a.txt", b"v2 newer than the backup");
    write(&mut a, "b.txt", b"only here");
    // The admin restores and starts a new epoch (here: same data, new epoch).
    server.meta.reset_epoch(ana.space).await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_locally.is_empty() && report.downloaded.is_empty(), "{report:?}");
    assert_eq!(read(&a, "a.txt"), b"v2 newer than the backup");
    let mut b = machine(&server, &ana, "desktop").await;
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b), vec![("a.txt".to_owned(), b"v2 newer than the backup".to_vec()), ("b.txt".to_owned(), b"only here".to_vec())]);
}

#[tokio::test]
async fn ignored_and_unsyncable_names_stay_local() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "real.txt", b"r");
    write(&mut a, ".DS_Store", b"junk");
    write(&mut a, "draft.tmp", b"junk");
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded, vec![path("real.txt")]);
    b.sync_once().await.unwrap();
    assert_eq!(files(&b), vec![("real.txt".to_owned(), b"r".to_vec())]);
}

#[tokio::test]
async fn a_failed_pass_loses_nothing_and_the_next_one_finishes() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    for i in 0..6 {
        write(&mut a, &format!("f{i}.txt"), format!("content {i}").as_bytes());
    }
    a.transport().faults.every.store(3, Ordering::Relaxed);
    let mut errors = 0;
    for _ in 0..30 {
        match a.sync_once().await {
            Ok(r) if !r.changed() => break,
            Ok(_) => {}
            Err(_) => errors += 1,
        }
    }
    assert!(errors > 0, "the fault injection should have hit");
    a.transport().faults.every.store(0, Ordering::Relaxed);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(b.fs().files().len(), 6);
}

#[tokio::test]
async fn listings_and_feeds_page() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine_with(&server, &ana, "laptop", MemFs::new(), 3).await;
    let mut b = machine_with(&server, &ana, "desktop", MemFs::new(), 3).await;
    for i in 0..10 {
        write(&mut a, &format!("f{i}.txt"), format!("c{i}").as_bytes());
    }
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    for i in 0..10 {
        write(&mut b, &format!("f{i}.txt"), format!("d{i}").as_bytes());
    }
    settle(&mut [&mut b, &mut a], 5).await;
    assert_eq!(files(&a), files(&b));
}

#[tokio::test]
async fn an_edit_in_a_deleted_folder_survives_when_its_old_name_is_taken() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("d")).unwrap();
    write(&mut a, "d/f.txt", b"v1");
    settle(&mut [&mut a, &mut b], 5).await;
    // The laptop renames the folder, deletes it, and makes a new folder with its last name.
    a.fs_mut().rename(&path("d"), &path("e")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("e/f.txt")).unwrap();
    a.fs_mut().remove_dir(&path("e")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().mkdir(&path("e")).unwrap();
    write(&mut a, "e/other.txt", b"o");
    a.sync_once().await.unwrap();
    // Meanwhile the desktop edited the file offline.
    write(&mut b, "d/f.txt", b"v2 edited offline");
    settle(&mut [&mut b, &mut a], 6).await;
    assert_eq!(files(&a), files(&b));
    assert!(files(&a).iter().any(|(_, d)| d == b"v2 edited offline"), "{:?}", files(&a));
}

#[tokio::test]
async fn a_file_that_cant_be_read_is_skipped_and_never_overwritten() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "shared.txt", b"v1");
    settle(&mut [&mut a, &mut b], 5).await;
    // Another program holds two files locked on the laptop: a new one and the edited shared one.
    write(&mut a, "render.mov", b"half written");
    write(&mut a, "shared.txt", b"laptop edit");
    a.fs_mut().lock(&path("render.mov"));
    a.fs_mut().lock(&path("shared.txt"));
    write(&mut a, "other.txt", b"fine");
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded, vec![path("other.txt")]);
    let mut skipped: Vec<&str> = report.skipped.iter().map(|(p, _)| p.as_str()).collect();
    skipped.sort_unstable();
    assert_eq!(skipped, vec!["render.mov", "shared.txt"]);
    // The desktop edits the shared file meanwhile. Both changed, so the laptop's copy (still unreadable)
    // becomes a conflict copy and the desktop's edit takes the name.
    write(&mut b, "shared.txt", b"desktop edit");
    b.sync_once().await.unwrap();
    a.sync_once().await.unwrap();
    let here = a.fs().files();
    assert_eq!(here.get(&path("shared.txt")).map(Vec::as_slice), Some(&b"desktop edit"[..]));
    assert_eq!(here.get(&path("shared (conflict, laptop, 2026-10-11 14-03).txt")).map(Vec::as_slice), Some(&b"laptop edit"[..]));
    // Once the files are free, everything syncs and both edits survive.
    a.fs_mut().unlock(&path("render.mov"));
    a.fs_mut().unlock(&path("shared.txt"));
    settle(&mut [&mut a, &mut b], 6).await;
    assert_eq!(files(&a), files(&b));
    let all = files(&a);
    for want in [&b"laptop edit"[..], b"desktop edit", b"half written", b"fine"] {
        assert!(all.iter().any(|(_, d)| d == want), "{all:?}");
    }
}

/// The desktop's app saves `doc.txt` at the moment `on` while the laptop's new version comes in. The save is
/// never overwritten: both edits end up on both machines, the desktop's as a conflict copy.
async fn a_save_while_a_download_comes_in_is_kept(on: SaveOn) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine_on(&server, &ana, "desktop", SaveDuring::default(), 50, |_| {}).await;
    write(&mut a, "doc.txt", b"v1");
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    write(&mut a, "doc.txt", b"v2 from laptop");
    a.sync_once().await.unwrap();
    b.fs().arm(path("doc.txt"), b"desktop save", on);
    b.sync_once().await.unwrap();
    assert!(b.fs().fired(), "{on:?}: the save should have happened during the pass");
    assert!(b.fs().files().values().any(|d| d == b"desktop save"), "{on:?}: {:?}", b.fs().files());
    for _ in 0..4 {
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
    }
    let here: Vec<(String, Vec<u8>)> = b.fs().files().into_iter().map(|(p, d)| (p.to_string(), d)).collect();
    assert_eq!(here, files(&a), "{on:?}");
    let want = vec![(format!("doc {CONFLICT_STAMP}.txt"), b"desktop save".to_vec()), ("doc.txt".to_owned(), b"v2 from laptop".to_vec())];
    assert_eq!(here, want, "{on:?}");
}

#[tokio::test]
async fn a_save_during_a_download_is_kept() {
    a_save_while_a_download_comes_in_is_kept(SaveOn::BeginWrite).await;
}

#[tokio::test]
async fn a_save_while_the_file_is_verified_is_kept() {
    // The engine reads the file up to three times before it decides it may replace it (the quick check's
    // hash, then the full verification); a save landing during any of those reads must survive.
    for n in 1..=3 {
        a_save_while_a_download_comes_in_is_kept(SaveOn::Open(n)).await;
    }
}

#[tokio::test]
async fn a_reconcile_cut_short_still_keeps_newer_local_files() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "a.txt", b"v1");
    a.sync_once().await.unwrap();
    write(&mut a, "a.txt", b"v2 newer than the backup");
    server.meta.reset_epoch(ana.space).await.unwrap();
    // The reconcile pass fails part way through applying the server's tree.
    a.transport().faults.every.store(3, Ordering::Relaxed);
    assert!(a.sync_once().await.is_err());
    a.transport().faults.every.store(0, Ordering::Relaxed);
    a.sync_once().await.unwrap();
    assert_eq!(files(&a), vec![("a.txt".to_owned(), b"v2 newer than the backup".to_vec())]);
    let mut b = machine(&server, &ana, "desktop").await;
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b), vec![("a.txt".to_owned(), b"v2 newer than the backup".to_vec())]);
}

#[tokio::test]
async fn os_metadata_in_a_remotely_deleted_folder_goes_with_it() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "x/f.txt", b"one");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut b, "x/.DS_Store", b"finder junk");
    a.fs_mut().remove_file(&path("x/f.txt")).unwrap();
    a.fs_mut().remove_dir(&path("x")).unwrap();
    a.sync_once().await.unwrap();
    settle(&mut [&mut b, &mut a], 5).await;
    assert!(b.fs().dirs().is_empty() && files(&b).is_empty(), "{:?}", b.fs().files());
    assert!(a.fs().dirs().is_empty());
}

#[tokio::test]
async fn an_office_lock_file_keeps_a_remotely_deleted_folder_local() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "x/f.txt", b"one");
    settle(&mut [&mut a, &mut b], 5).await;
    // Word holds a document open on the desktop; its lock file sits in the folder.
    write(&mut b, "x/~$doc.docx", b"owner");
    a.fs_mut().remove_file(&path("x/f.txt")).unwrap();
    a.fs_mut().remove_dir(&path("x")).unwrap();
    a.sync_once().await.unwrap();
    settle(&mut [&mut b, &mut a], 5).await;
    assert_eq!(files(&b), vec![("x/~$doc.docx".to_owned(), b"owner".to_vec())]);
    // The server doesn't get the folder back.
    assert!(a.fs().dirs().is_empty(), "{:?}", a.fs().dirs());
    let mut c = machine(&server, &ana, "studio").await;
    c.sync_once().await.unwrap();
    assert!(c.fs().dirs().is_empty(), "{:?}", c.fs().dirs());
}

#[tokio::test]
async fn a_lost_commit_reply_makes_no_conflict_copy() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "doc.txt", b"v1");
    a.sync_once().await.unwrap();
    write(&mut a, "doc.txt", b"v2");
    a.transport().faults.lose_commit_reply.store(true, Ordering::Relaxed);
    assert!(a.sync_once().await.is_err());
    let report = a.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty() && report.uploaded.is_empty(), "{report:?}");
    assert_eq!(files(&a), vec![("doc.txt".to_owned(), b"v2".to_vec())]);
    let mut b = machine(&server, &ana, "desktop").await;
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b), vec![("doc.txt".to_owned(), b"v2".to_vec())]);
    assert!(conflict_copies(&a).is_empty());
}

#[tokio::test]
async fn a_refusal_naming_no_operation_fails_the_pass() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "doc.txt", b"v1");
    a.transport().faults.bad_conflict_index.store(true, Ordering::Relaxed);
    let pass = tokio::time::timeout(std::time::Duration::from_secs(10), a.sync_once()).await;
    assert!(matches!(pass, Ok(Err(_))), "{pass:?}");
    a.transport().faults.bad_conflict_index.store(false, Ordering::Relaxed);
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded, vec![path("doc.txt")]);
}

#[tokio::test]
async fn a_tracked_folder_deleted_on_the_server_leaves_no_empty_conflict_copy() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut b, "x/Thumbs.db", b"explorer junk");
    // The laptop replaces the folder with a file of the same name.
    a.fs_mut().remove_dir(&path("x")).unwrap();
    write(&mut a, "x", b"now a file");
    settle(&mut [&mut a], 5).await;
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert!(b.fs().dirs().is_empty(), "{:?}", b.fs().dirs());
    assert_eq!(files(&b), vec![("x".to_owned(), b"now a file".to_vec())]);
}

#[tokio::test]
async fn a_file_that_cant_be_removed_doesnt_stop_the_rest() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "x.txt", b"x");
    settle(&mut [&mut a, &mut b], 5).await;
    // The laptop deletes x.txt while an app on the desktop holds it open (Windows can't remove it).
    a.fs_mut().remove_file(&path("x.txt")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().hold(&path("x.txt"));
    write(&mut b, "new.txt", b"new work");
    let report = b.sync_once().await.unwrap();
    assert!(report.skipped.iter().any(|(p, _)| p == "x.txt"), "{report:?}");
    assert_eq!(report.uploaded, vec![path("new.txt")]);
    a.sync_once().await.unwrap();
    assert_eq!(files(&a), vec![("new.txt".to_owned(), b"new work".to_vec())]);
    // Still held: nothing comes back to the server, and the next pass tries again.
    let report = b.sync_once().await.unwrap();
    assert!(!report.changed(), "{report:?}");
    b.fs_mut().release(&path("x.txt"));
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.removed_locally, vec![path("x.txt")]);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
}

#[tokio::test]
async fn a_rename_here_and_an_edit_there_meet_in_one_file() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "doc.txt", b"v1");
    settle(&mut [&mut a, &mut b], 5).await;
    b.fs_mut().rename(&path("doc.txt"), &path("renamed.txt")).unwrap();
    write(&mut a, "doc.txt", b"v2 laptop");
    a.sync_once().await.unwrap();
    settle(&mut [&mut b, &mut a], 8).await;
    for m in [&a, &b] {
        assert_eq!(files(m), vec![("renamed.txt".to_owned(), b"v2 laptop".to_vec())]);
    }
    assert_eq!(b.state().remote.len(), 2, "the root and one file: {:?}", b.state().remote);
}

#[tokio::test]
async fn a_folder_moved_here_and_an_edit_inside_it_there_meet() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Shots")).unwrap();
    for i in 0..5 {
        write(&mut a, &format!("Shots/s{i}.psd"), format!("shot {i}").as_bytes());
    }
    settle(&mut [&mut a, &mut b], 5).await;
    // The desktop files the folder away; the laptop edits one shot meanwhile.
    b.fs_mut().mkdir(&path("Archive")).unwrap();
    b.fs_mut().rename(&path("Shots"), &path("Archive/Shots")).unwrap();
    write(&mut a, "Shots/s0.psd", b"shot 0 edited");
    a.sync_once().await.unwrap();
    let report = b.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    settle(&mut [&mut b, &mut a], 8).await;
    let mut expected: Vec<(String, Vec<u8>)> = (1..5).map(|i| (format!("Archive/Shots/s{i}.psd"), format!("shot {i}").into_bytes())).collect();
    expected.insert(0, ("Archive/Shots/s0.psd".to_owned(), b"shot 0 edited".to_vec()));
    for m in [&a, &b] {
        assert_eq!(files(m), expected);
        assert_eq!(m.fs().dirs(), vec![path("Archive"), path("Archive/Shots")]);
    }
    // The root, two folders and five shots: nothing was duplicated.
    assert_eq!(b.state().remote.len(), 8);
}

#[tokio::test]
async fn a_download_cut_short_after_a_move_makes_no_conflict_copy() {
    // The pass fails at its k-th server call, somewhere after the move was applied.
    for k in 1..8u64 {
        let server = Server::new().await;
        let ana = server.user("ana").await;
        let mut a = machine(&server, &ana, "laptop").await;
        let mut b = machine(&server, &ana, "desktop").await;
        write(&mut a, "doc.txt", b"v1");
        settle(&mut [&mut a, &mut b], 5).await;
        write(&mut a, "doc.txt", b"v2 laptop");
        a.sync_once().await.unwrap();
        a.fs_mut().rename(&path("doc.txt"), &path("renamed.txt")).unwrap();
        a.sync_once().await.unwrap();
        let faults = b.transport().faults.clone();
        faults.every.store(20, Ordering::Relaxed);
        while b.transport().space().await.is_ok() {}
        for _ in 0..(20 - k) {
            b.transport().space().await.unwrap();
        }
        let _ = b.sync_once().await;
        faults.every.store(0, Ordering::Relaxed);
        settle(&mut [&mut b, &mut a], 8).await;
        assert!(conflict_copies(&b).is_empty(), "k={k}: {:?}", files(&b));
        assert_eq!(files(&b), vec![("renamed.txt".to_owned(), b"v2 laptop".to_vec())], "k={k}");
    }
}

#[tokio::test]
async fn files_moved_deeper_than_the_tree_allows_are_never_deleted_on_the_server() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("keep")).unwrap();
    a.fs_mut().mkdir(&path("keep/inner")).unwrap();
    write(&mut a, "keep/inner/f.txt", b"deep");
    settle(&mut [&mut a, &mut b], 5).await;
    // The desktop files `keep` at depth 256, the deepest allowed, so `keep/inner` ends up too deep.
    let mut deep = String::new();
    for _ in 0..craft_cloud_proto::limits::MAX_TREE_DEPTH - 1 {
        deep.push_str("d/");
        b.fs_mut().mkdir(&path(&deep)).unwrap();
    }
    b.fs_mut().rename(&path("keep"), &path(&format!("{deep}keep"))).unwrap();
    let report = b.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    a.sync_once().await.unwrap();
    assert_eq!(a.fs().files().into_values().collect::<Vec<_>>(), vec![b"deep".to_vec()]);
}

#[tokio::test]
async fn a_folder_that_cant_be_listed_is_never_deleted_on_the_server() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("private")).unwrap();
    a.fs_mut().mkdir(&path("private/deep")).unwrap();
    write(&mut a, "private/p.txt", b"p");
    write(&mut a, "private/deep/q.txt", b"q");
    settle(&mut [&mut a, &mut b], 5).await;
    b.fs_mut().hold(&path("private"));
    b.fs_mut().mkdir(&path("secret")).unwrap();
    b.fs_mut().hold(&path("secret"));
    write(&mut b, "new.txt", b"new work");
    for _ in 0..3 {
        let report = b.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
        assert!(report.skipped.iter().any(|(p, _)| p == "private"), "{report:?}");
    }
    settle(&mut [&mut a], 5).await;
    assert_eq!(files(&a).len(), 3, "{:?}", files(&a));
    assert!(a.fs().files().contains_key(&path("new.txt")));
    // The folder that can't be listed isn't created on the server until it can be.
    assert_eq!(a.fs().dirs(), vec![path("private"), path("private/deep")]);
    b.fs_mut().unlock_all();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(a.fs().dirs(), b.fs().dirs());
}

#[tokio::test]
async fn a_file_renamed_to_an_ignored_name_doesnt_take_the_remote_edit() {
    // Word renames the document to a `~WRL….tmp` name mid-save. That file is out of sync's sight, so it
    // must not count as the tracked file moved: the remote edit lands at the old name and stays on the server.
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "doc.txt", b"v1");
    settle(&mut [&mut a, &mut b], 5).await;
    b.fs_mut().rename(&path("doc.txt"), &path("doc.tmp")).unwrap();
    write(&mut a, "doc.txt", b"v2 laptop");
    settle(&mut [&mut a, &mut b], 8).await;
    assert_eq!(read(&a, "doc.txt"), b"v2 laptop");
    assert_eq!(read(&b, "doc.txt"), b"v2 laptop");
    assert_eq!(read(&b, "doc.tmp"), b"v1");
}

#[tokio::test]
async fn a_file_moved_aside_for_a_newcomer_is_not_sent_as_a_user_move() {
    // The server moved a.txt into x and a new a.txt took its old name. Here x is held open, so the move can't
    // happen; the newcomer still needs the name, so the tracked file is moved aside. That rename is the engine's,
    // not the user's: it must not go up as a move (the server's node would leave x for a conflict name).
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "a.txt", b"moved into x");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().rename(&path("a.txt"), &path("x/a.txt")).unwrap();
    a.sync_once().await.unwrap();
    write(&mut a, "a.txt", b"a newcomer");
    a.sync_once().await.unwrap();
    b.fs_mut().hold(&path("x"));
    let report = b.sync_once().await.unwrap();
    assert!(report.moved_remotely.is_empty(), "{report:?}");
    b.fs_mut().unlock_all();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, "x/a.txt"), b"moved into x");
    assert_eq!(read(&b, "a.txt"), b"a newcomer");
    assert!(conflict_copies(&b).is_empty(), "{:?}", files(&b));
}

#[tokio::test]
async fn an_approval_releases_only_the_deletes_it_names() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    for i in 0..30 {
        write(&mut a, &format!("f{i:02}.txt"), format!("file {i}").as_bytes());
    }
    settle(&mut [&mut a, &mut b], 5).await;
    for i in 0..12 {
        a.fs_mut().remove_file(&path(&format!("f{i:02}.txt"))).unwrap();
    }
    let shown = a.sync_once().await.unwrap();
    assert_eq!((shown.pending_deletes.len(), shown.pending_nodes.len()), (12, 12));
    // More vanish before the user approves what they saw: only the 12 shown go.
    for i in 12..24 {
        a.fs_mut().remove_file(&path(&format!("f{i:02}.txt"))).unwrap();
    }
    a.approve_deletes_of(shown.pending_nodes.clone());
    let report = a.sync_once().await.unwrap();
    assert_eq!((report.removed_remotely.len(), report.pending_deletes.len()), (12, 12), "{report:?}");
    assert!(report.pending_nodes.iter().all(|id| !shown.pending_nodes.contains(id)));

    // The same on the receiving side: the desktop shows 12 to remove, the laptop then lets 12 more go.
    let shown = b.sync_once().await.unwrap();
    assert_eq!(shown.pending_local_deletes.len(), 12, "{shown:?}");
    a.approve_deletes();
    a.sync_once().await.unwrap();
    b.approve_deletes_of(shown.pending_nodes.clone());
    let report = b.sync_once().await.unwrap();
    assert_eq!((report.removed_locally.len(), report.pending_local_deletes.len()), (12, 12), "{report:?}");
    assert_eq!(b.fs().files().len(), 18);
}

#[tokio::test]
async fn a_tracked_folder_or_file_that_became_a_link_is_never_deleted() {
    // Footage moved to another drive with a link left in its place; notes.txt replaced by a link too. Neither is
    // gone: nothing is deleted on the server or on the other machine.
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    write(&mut a, "notes.txt", b"notes");
    write(&mut a, "other.txt", b"other");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.fs_mut().make_link(&path("notes.txt")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    for name in ["Footage", "notes.txt"] {
        assert!(report.skipped.iter().any(|(p, why)| p == name && why == craft_cloud_sync::OTHER_REASON), "{report:?}");
    }
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b).len(), 5, "{:?}", files(&b));
    // A case-changed spelling counts too (names are compared ignoring case).
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().make_link(&path("FOOTAGE")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty(), "{report:?}");
}

/// Two machines with a folder of three clips, a notes file and padding (so the mass-delete guard doesn't hide a
/// wrong delete).
async fn linked_setup() -> (Server, common::Machine, common::Machine) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    write(&mut a, "notes.txt", b"notes");
    for i in 0..20 {
        write(&mut a, &format!("pad{i}.txt"), b"pad");
    }
    settle(&mut [&mut a, &mut b], 5).await;
    (server, a, b)
}

/// The link is still there, untouched, and no conflict copy was made of it.
fn link_untouched(m: &common::Machine, p: &str) {
    assert_eq!(m.fs().stat(&path(p)).unwrap(), Some(craft_cloud_sync::FsKind::Other), "{p} is still the link");
    assert!(conflict_copies(m).is_empty(), "{:?}", files(m));
    assert!(m.fs().dirs().is_empty(), "no real folder was made in the link's place: {:?}", m.fs().dirs());
}

#[tokio::test]
async fn a_remote_edit_inside_a_folder_that_became_a_link_waits() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    write(&mut b, "Footage/c0.mov", b"clip v2");
    write(&mut b, "Footage/new.mov", b"a new clip");
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
        assert!(report.skipped.iter().any(|(p, _)| p == "Footage"), "{report:?}");
    }
    link_untouched(&a, "Footage");
    b.sync_once().await.unwrap();
    assert_eq!(files(&b).iter().filter(|(p, _)| p.starts_with("Footage/")).count(), 4, "{:?}", files(&b));
}

#[tokio::test]
async fn a_remote_edit_of_a_file_that_became_a_link_waits() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("notes.txt")).unwrap();
    write(&mut b, "notes.txt", b"notes v2");
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.downloaded.is_empty() && report.moved_locally.is_empty(), "{report:?}");
    assert_eq!(a.fs().stat(&path("notes.txt")).unwrap(), Some(craft_cloud_sync::FsKind::Other));
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "notes.txt"), b"notes v2");
    // Once it is a real file again, the server's version is checked against it like any other.
    a.fs_mut().remove_file(&path("notes.txt")).unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&a, "notes.txt"), b"notes v2");
}

#[tokio::test]
async fn a_remote_rename_of_a_folder_that_became_a_link_deletes_nothing() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    b.fs_mut().rename(&path("Footage"), &path("Footage2")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    }
    link_untouched(&a, "Footage");
    b.sync_once().await.unwrap();
    assert_eq!(files(&b).iter().filter(|(p, _)| p.starts_with("Footage2/")).count(), 3, "{:?}", files(&b));
}

#[tokio::test]
async fn a_newcomer_whose_name_is_a_case_twin_of_a_linked_folder_waits() {
    // The server renamed Footage to Archive and made a new FOOTAGE; here Footage is a link. The newcomer would
    // need the link moved aside as a case twin: it waits instead, and nothing of Archive is deleted.
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    b.fs_mut().rename(&path("Footage"), &path("Archive")).unwrap();
    b.sync_once().await.unwrap();
    b.fs_mut().mkdir(&path("FOOTAGE")).unwrap();
    write(&mut b, "FOOTAGE/new.txt", b"new");
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    }
    link_untouched(&a, "Footage");
    b.sync_once().await.unwrap();
    assert_eq!(files(&b).iter().filter(|(p, _)| p.starts_with("Archive/")).count(), 3, "{:?}", files(&b));
}

#[tokio::test]
async fn a_rename_onto_a_case_twin_of_a_linked_file_waits() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("notes.txt")).unwrap();
    b.fs_mut().rename(&path("notes.txt"), &path("old-notes.txt")).unwrap();
    b.sync_once().await.unwrap();
    b.fs_mut().rename(&path("pad1.txt"), &path("NOTES.txt")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "{report:?}");
    }
    assert_eq!(a.fs().stat(&path("notes.txt")).unwrap(), Some(craft_cloud_sync::FsKind::Other));
    assert!(conflict_copies(&a).is_empty(), "{:?}", files(&a));
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "old-notes.txt"), b"notes");
    assert_eq!(read(&b, "NOTES.txt"), b"pad");
}

#[tokio::test]
async fn a_folder_the_server_needs_is_never_made_over_a_linked_case_twin() {
    // `x` was deleted here and `X` is now a link; the server adds a file in `x`. Bringing `x` back would need the
    // link moved aside as a case twin: the file waits instead.
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "x/f.txt", b"f");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut b, "x/new.txt", b"new");
    b.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("x/f.txt")).unwrap();
    a.fs_mut().remove_dir(&path("x")).unwrap();
    a.fs_mut().make_link(&path("X")).unwrap();
    a.sync_once().await.unwrap();
    assert_eq!(a.fs().stat(&path("X")).unwrap(), Some(craft_cloud_sync::FsKind::Other), "the link is untouched");
    assert!(conflict_copies(&a).is_empty(), "{:?}", files(&a));
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "x/new.txt"), b"new");
}

#[tokio::test]
async fn a_case_only_rename_next_to_a_linked_case_twin_waits_in_one_piece() {
    // The server renames notes.txt to Notes.txt; here an untracked link NOTES.TXT sits next to it. The rename
    // would have to move the link aside, so it waits, and the file is never left at a temporary name.
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("NOTES.TXT")).unwrap();
    b.fs_mut().rename(&path("notes.txt"), &path("Notes.txt")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.moved_remotely.is_empty() && report.removed_remotely.is_empty(), "{report:?}");
    }
    assert_eq!(a.fs().stat(&path("NOTES.TXT")).unwrap(), Some(craft_cloud_sync::FsKind::Other));
    assert_eq!(read(&a, "notes.txt"), b"notes");
    assert!(conflict_copies(&a).is_empty(), "{:?}", files(&a));
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Notes.txt"), b"notes");
    // Once the link is gone, the rename lands.
    a.fs_mut().remove_file(&path("NOTES.TXT")).unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&a, "Notes.txt"), b"notes");
    assert_eq!(files(&a), files(&b));
}

#[tokio::test]
async fn a_file_that_only_got_a_new_time_is_removed_by_a_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    // Same content, new time: not an edit.
    a.fs_mut().write(&path("notes.txt"), b"notes", false, 999_999).unwrap();
    b.fs_mut().remove_file(&path("notes.txt")).unwrap();
    b.sync_once().await.unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    assert!(!files(&a).iter().any(|(p, _)| p == "notes.txt"), "{:?}", files(&a));
    assert!(!files(&b).iter().any(|(p, _)| p == "notes.txt"), "{:?}", files(&b));
    // Different content is an edit, and the edit wins.
    write(&mut a, "pad3.txt", b"pad, edited");
    b.fs_mut().remove_file(&path("pad3.txt")).unwrap();
    b.sync_once().await.unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&b, "pad3.txt"), b"pad, edited");
}

#[tokio::test]
async fn a_delete_under_a_linked_folder_lands_once_the_folder_is_real_again() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    b.fs_mut().remove_file(&path("Footage/c0.mov")).unwrap();
    b.sync_once().await.unwrap();
    a.sync_once().await.unwrap();
    // The real folder comes back with its old contents (the drive is mounted again, say).
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    settle(&mut [&mut a, &mut b], 5).await;
    for m in [&a, &b] {
        let names: Vec<String> = files(m).into_iter().map(|(p, _)| p).filter(|p| p.starts_with("Footage/")).collect();
        assert_eq!(names, ["Footage/c1.mov", "Footage/c2.mov"], "the deleted clip doesn't come back");
    }
}

#[tokio::test]
async fn a_remote_rename_of_a_file_that_became_a_link_deletes_nothing() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("notes.txt")).unwrap();
    b.fs_mut().rename(&path("notes.txt"), &path("notes2.txt")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "{report:?}");
    }
    assert_eq!(a.fs().stat(&path("notes.txt")).unwrap(), Some(craft_cloud_sync::FsKind::Other));
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "notes2.txt"), b"notes");
}
