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

/// Saving a file again with the same bytes gives it a new time and file ID, but it isn't an edit: a remote edit
/// simply replaces it, with no conflict copy (edits are decided by content, as deletes are).
#[tokio::test]
async fn a_file_saved_again_unchanged_takes_a_remote_edit_without_a_conflict() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "notes.txt", b"base");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut b, "notes.txt", b"base");
    write(&mut a, "notes.txt", b"edited on the laptop");
    a.sync_once().await.unwrap();
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(report.downloaded, vec![path("notes.txt")]);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&b), vec![("notes.txt".to_owned(), b"edited on the laptop".to_vec())]);
    assert_eq!(files(&a), files(&b));
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
async fn approvals_given_before_a_pass_add_up() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    for i in 0..30 {
        write(&mut a, &format!("f{i:02}.txt"), format!("file {i}").as_bytes());
    }
    a.sync_once().await.unwrap();
    for i in 0..12 {
        a.fs_mut().remove_file(&path(&format!("f{i:02}.txt"))).unwrap();
    }
    let shown = a.sync_once().await.unwrap();
    assert_eq!(shown.pending_nodes.len(), 12);
    let (first, second) = shown.pending_nodes.split_at(5);
    a.approve_deletes_of(first.to_vec());
    a.approve_deletes_of(second.to_vec());
    let report = a.sync_once().await.unwrap();
    assert_eq!((report.removed_remotely.len(), report.pending_deletes.len()), (12, 0), "{report:?}");
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

/// Both machines rename a file and save the same new content into it (the same edit made twice; the save makes
/// a new file, so the desktop can't match it to the old one by file ID). The desktop finds the server's version
/// already where the server has it: it is adopted, with no conflict copy.
#[tokio::test]
async fn the_same_rename_and_edit_on_both_sides_makes_no_conflict_copy() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "a.txt", b"draft");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().rename(&path("a.txt"), &path("final.txt")).unwrap();
    a.sync_once().await.unwrap();
    write(&mut a, "final.txt", b"final text");
    a.sync_once().await.unwrap();
    b.fs_mut().remove_file(&path("a.txt")).unwrap();
    write(&mut b, "final.txt", b"final text");
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty() && report.downloaded.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    for m in [&a, &b] {
        assert_eq!(files(m), vec![("final.txt".to_owned(), b"final text".to_vec())]);
    }
}

/// The laptop deletes `a.txt`, renames `b.txt` to `a.txt` and edits it. The desktop deleted `b.txt` and saved the
/// same final text over its `a.txt`. The server's edit wins over the local delete, and finds its content already
/// in the old `a.txt` (a node the server deleted): that file is adopted, not kept as a copy.
#[tokio::test]
async fn an_edit_that_wins_adopts_a_deleted_occupant_holding_its_content() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "a.txt", b"old a");
    write(&mut a, "b.txt", b"old b");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().remove_file(&path("a.txt")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().rename(&path("b.txt"), &path("a.txt")).unwrap();
    a.sync_once().await.unwrap();
    write(&mut a, "a.txt", b"final");
    a.sync_once().await.unwrap();
    b.fs_mut().remove_file(&path("b.txt")).unwrap();
    write(&mut b, "a.txt", b"final");
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty() && report.downloaded.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    for m in [&a, &b] {
        assert_eq!(files(m), vec![("a.txt".to_owned(), b"final".to_vec())]);
    }
}

/// A clip replaced by a link, then its folder renamed: the link moved with the folder, so the clip is still not
/// gone. The folder's move goes up, the clip's node stays, and once the clip is real again it syncs where it is.
#[tokio::test]
async fn a_file_that_became_a_link_in_a_folder_the_user_renamed_is_never_deleted() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    assert_eq!(report.moved_remotely, vec![(path("Footage"), path("Clips"))]);
    // The other machine renames, and deletes nothing.
    let report = b.sync_once().await.unwrap();
    assert!(report.removed_locally.is_empty(), "{report:?}");
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
    // The clip comes back, edited meanwhile on the other drive.
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    write(&mut a, "Clips/c0.mov", b"clip, graded");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip, graded");
    assert_eq!(files(&a), files(&b));
}

/// The same, but a new folder then takes the renamed folder's old name. The folder found by its file ID still
/// tells where the link went, so the clip isn't deleted while it is a link.
#[tokio::test]
async fn a_file_that_became_a_link_in_a_folder_moved_away_is_never_deleted() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    }
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Footage/c0.mov"), b"clip");
    // Once the clip is back it goes up where it is, and nothing is lost.
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    write(&mut a, "Clips/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
}

/// A clip replaced by a link, its folder renamed here, and the clip renamed on the other machine: the server's
/// rename can't find the clip by its file ID (it is a link), but it is in the renamed folder, so it isn't taken
/// for deleted. The rename waits until the clip is real again.
#[tokio::test]
async fn a_server_rename_of_a_file_that_became_a_link_in_a_renamed_folder_waits() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("Footage/c9.mov")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    }
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Clips/c9.mov"), b"clip");
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    write(&mut a, "Clips/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&a, "Clips/c9.mov"), b"clip");
}

/// As above, but the other machine also moves the clip out and deletes the folder. The deleted folder stays
/// tracked here while the clip in it waits, so the clip still isn't taken for deleted, and nothing is sent for the
/// folder the server no longer has.
#[tokio::test]
async fn a_link_in_a_renamed_folder_the_server_deleted_keeps_its_node() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("c0.mov")).unwrap();
    for i in 1..3 {
        b.fs_mut().remove_file(&path(&format!("Footage/c{i}.mov"))).unwrap();
    }
    b.fs_mut().remove_dir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.refused.is_empty(), "{report:?}");
    }
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "c0.mov"), b"clip");
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    // Back with a new file ID, where the user's folder is: it goes up from there.
    write(&mut a, "Clips/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
}

/// The other machine replaces the folder with a new one of the same name and moves the clip into it. The old
/// folder here holds the clip as a link, so it isn't moved aside for the new one: the new folder waits until the
/// clip is real again, and nothing is deleted meanwhile.
#[tokio::test]
async fn a_folder_holding_a_tracked_link_isnt_moved_aside_for_a_newcomer() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    b.fs_mut().mkdir(&path("Tmp")).unwrap();
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("Tmp/c0.mov")).unwrap();
    for i in 1..3 {
        b.fs_mut().remove_file(&path(&format!("Footage/c{i}.mov"))).unwrap();
    }
    b.fs_mut().remove_dir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Tmp"), &path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.conflict_copies.is_empty(), "{report:?}");
        assert_eq!(a.fs().stat(&path("Footage/c0.mov")).unwrap(), Some(craft_cloud_sync::FsKind::Other));
    }
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Footage/c0.mov"), b"clip");
    // The clip comes back (a new file to this disk); nothing is lost.
    a.fs_mut().remove_file(&path("Footage/c0.mov")).unwrap();
    write(&mut a, "Footage/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert!(files(&a).iter().any(|(p, d)| p.contains("c0") && d == b"clip"), "{:?}", files(&a));
}

/// The laptop renames a folder; the desktop deletes it and makes a new empty folder of the same name. The laptop
/// makes the new folder for the server's change: it isn't the deleted one, so it isn't removed, and the new
/// folder stays on the server.
#[tokio::test]
async fn a_new_folder_at_a_deleted_folders_name_is_not_taken_for_it() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for i in 0..3 {
        b.fs_mut().remove_file(&path(&format!("Footage/c{i}.mov"))).unwrap();
    }
    b.fs_mut().remove_dir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    b.fs_mut().mkdir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(!report.removed_remotely.contains(&path("Footage")), "{report:?}");
    settle(&mut [&mut a, &mut b], 6).await;
    for m in [&a, &b] {
        assert!(m.fs().dirs().contains(&path("Footage")), "{:?}", m.fs().dirs());
    }
    assert_eq!(files(&a), files(&b));
}

/// The same with a clip in the renamed folder that turned into a link, which the desktop moved out: neither the
/// new folder nor the clip is deleted on the server while the link stays.
#[tokio::test]
async fn a_new_folder_at_a_kept_deleted_folders_name_deletes_nothing() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("c0.mov")).unwrap();
    for i in 1..3 {
        b.fs_mut().remove_file(&path(&format!("Footage/c{i}.mov"))).unwrap();
    }
    b.fs_mut().remove_dir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().mkdir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..4 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "{report:?}");
        b.sync_once().await.unwrap();
    }
    assert!(b.fs().dirs().contains(&path("Footage")), "{:?}", b.fs().dirs());
    assert_eq!(read(&b, "c0.mov"), b"clip");
}

/// A folder that came back from another drive (a link replaced with the real folder again) has a new file ID.
/// A later rename of it is still a move, so a link inside it isn't taken for deleted either.
#[tokio::test]
async fn a_folder_put_back_from_another_drive_is_still_moved_as_one() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    assert_eq!(report.moved_remotely, vec![(path("Footage"), path("Clips"))]);
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
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

/// The laptop deletes `name` and creates a new file at `newcomer`; the desktop only touched its copy of `name`
/// (the same bytes written again: a new time and file ID). Deleting is decided by content, so the old file goes
/// when the newcomer takes its name (or a case twin of it), as it would for a plain delete: no conflict copy.
async fn a_touched_file_the_server_deleted_makes_way(name: &str, newcomer: &str, content: &[u8]) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, name, b"old");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().remove_file(&path(name)).unwrap();
    a.sync_once().await.unwrap();
    write(&mut a, newcomer, content);
    a.sync_once().await.unwrap();
    write(&mut b, name, b"old");
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(report.removed_locally, vec![path(name)]);
    settle(&mut [&mut a, &mut b], 5).await;
    for m in [&a, &b] {
        assert_eq!(files(m), vec![(newcomer.to_owned(), content.to_vec())]);
    }
}

#[tokio::test]
async fn a_touched_file_the_server_deleted_makes_way_for_a_newcomer() {
    a_touched_file_the_server_deleted_makes_way("a.txt", "a.txt", b"brand new").await;
}

#[tokio::test]
async fn a_touched_file_the_server_deleted_makes_way_for_a_case_twin() {
    a_touched_file_the_server_deleted_makes_way("a.txt", "A.TXT", b"brand new").await;
    // The same content under the new name (copied, then the original deleted): still no copy of the old file.
    a_touched_file_the_server_deleted_makes_way("a.txt", "A.TXT", b"old").await;
}

/// The laptop deletes `notes` and renames another file to that name. On the desktop the old `notes` couldn't be
/// removed at first (an app held it open), so it is still there, unchanged, when the server's rename lands on
/// its name: it goes, like any unchanged file the server deleted, instead of coming back as a conflict copy.
#[tokio::test]
async fn a_server_rename_onto_a_deleted_unchanged_file_removes_it() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "notes", b"old notes");
    write(&mut a, "b.txt", b"other");
    settle(&mut [&mut a, &mut b], 5).await;
    b.fs_mut().hold(&path("notes"));
    a.fs_mut().remove_file(&path("notes")).unwrap();
    a.sync_once().await.unwrap();
    let report = b.sync_once().await.unwrap();
    assert!(report.removed_locally.is_empty(), "{report:?}");
    a.fs_mut().rename(&path("b.txt"), &path("notes")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().unlock_all();
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(report.removed_locally, vec![path("notes")]);
    assert_eq!(report.moved_locally, vec![(path("b.txt"), path("notes"))]);
    settle(&mut [&mut a, &mut b], 5).await;
    for m in [&a, &b] {
        assert_eq!(files(m), vec![("notes".to_owned(), b"other".to_vec())]);
    }
}

/// The same with a case twin: the server renames a file to `NOTES` while the desktop still has the deleted,
/// unchanged `notes`.
#[tokio::test]
async fn a_server_rename_onto_a_case_twin_of_a_deleted_unchanged_file_removes_it() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "notes", b"old notes");
    write(&mut a, "b.txt", b"other");
    settle(&mut [&mut a, &mut b], 5).await;
    b.fs_mut().hold(&path("notes"));
    a.fs_mut().remove_file(&path("notes")).unwrap();
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().rename(&path("b.txt"), &path("NOTES")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().unlock_all();
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(report.removed_locally, vec![path("notes")]);
    settle(&mut [&mut a, &mut b], 5).await;
    for m in [&a, &b] {
        assert_eq!(files(m), vec![("NOTES".to_owned(), b"other".to_vec())]);
    }
}

/// The server renames a file onto the name of a folder it deleted, which the desktop couldn't remove yet (it
/// couldn't be listed). The empty folder goes; one with new files in it is moved aside, never removed.
#[tokio::test]
async fn a_server_rename_onto_a_deleted_folder_removes_it_only_if_bare() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("old")).unwrap();
    a.fs_mut().mkdir(&path("kept")).unwrap();
    write(&mut a, "b.txt", b"other");
    write(&mut a, "c.txt", b"more");
    settle(&mut [&mut a, &mut b], 5).await;
    b.fs_mut().hold(&path("old"));
    b.fs_mut().hold(&path("kept"));
    a.fs_mut().remove_dir(&path("old")).unwrap();
    a.fs_mut().remove_dir(&path("kept")).unwrap();
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().rename(&path("b.txt"), &path("old")).unwrap();
    a.fs_mut().rename(&path("c.txt"), &path("kept")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().unlock_all();
    write(&mut b, "kept/new.txt", b"made here");
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.removed_locally, vec![path("old")], "{report:?}");
    assert_eq!(report.conflict_copies.len(), 1, "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, "old"), b"other");
    assert_eq!(read(&b, "kept"), b"more");
    assert!(files(&b).iter().any(|(p, d)| p.ends_with("/new.txt") && d == b"made here"), "{:?}", files(&b));
}
