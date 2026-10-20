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

/// Windows has no executable bit: an edit made there keeps the bit a Unix machine gave the file, and a new file
/// from there isn't executable.
#[tokio::test]
async fn an_edit_on_windows_keeps_the_exec_bit() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut unix = machine(&server, &ana, "laptop").await;
    let mut windows = machine_with(&server, &ana, "desktop", MemFs::new().without_exec_bit(), 50).await;
    unix.fs_mut().write(&path("run.sh"), b"echo one", true, 0).unwrap();
    settle(&mut [&mut unix, &mut windows], 5).await;
    write(&mut windows, "run.sh", b"echo two");
    write(&mut windows, "new.sh", b"echo new");
    settle(&mut [&mut windows, &mut unix], 5).await;
    assert_eq!(read(&unix, "run.sh"), b"echo two");
    let exec = |p: &str| matches!(unix.fs().stat(&path(p)).unwrap(), Some(craft_cloud_sync::FsKind::File(m)) if m.exec);
    assert!(exec("run.sh"), "the edit from Windows cleared the exec bit");
    assert!(!exec("new.sh"));
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
async fn ignore_patterns_ignore_case_as_the_os_does() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "real.txt", b"r");
    for junk in ["RENDER.TMP", "Clip.Part", "~$Report.docx", "THUMBS.DB", "Desktop.ini", ".~LOCK.notes.odt#"] {
        write(&mut a, junk, b"junk");
    }
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
    // The server speaking out of turn, not damaged data.
    assert!(matches!(pass, Ok(Err(craft_cloud_sync::SyncError::Transport(craft_cloud_sync::TransportError::Protocol(_))))), "{pass:?}");
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

/// A landing in steps fails after moving the old file aside, and putting it back fails too: the file's name is
/// empty until the next pass's recovery. That pass's scan counts the name as unknown, so no delete goes to the
/// server; after recovery the file is back, the server's edit lands, and nothing was deleted anywhere.
#[tokio::test]
async fn a_failed_put_back_never_sends_a_delete() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine_with(&server, &ana, "desktop", MemFs::new().without_swap(), 50).await;
    write(&mut a, "notes", b"v1");
    write(&mut a, "other", b"o");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut a, "notes", b"v2");
    a.sync_once().await.unwrap();

    b.fs_mut().failing_landings(true);
    let report = b.sync_once().await.unwrap();
    assert!(report.skipped.iter().any(|(p, _)| p == "notes"), "{report:?}");
    assert!(b.fs().stat(&path("notes")).unwrap().is_none(), "the name is empty until recovery");
    // The push sends nothing for it: no delete (not even one the server would refuse), no move, no upload.
    assert!(report.removed_remotely.is_empty() && report.refused.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    assert!(report.moved_remotely.is_empty() && report.uploaded.is_empty() && report.conflict_copies.is_empty(), "{report:?}");
    // Nothing was deleted or moved on the server.
    a.sync_once().await.unwrap();
    assert_eq!(files(&a), vec![("notes".to_owned(), b"v2".to_vec()), ("other".to_owned(), b"o".to_vec())]);

    b.fs_mut().failing_landings(false);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&b, "notes"), b"v2");
    assert_eq!(files(&a), files(&b));
    assert_eq!(files(&b).len(), 2, "{:?}", files(&b));
}

/// A file marked read-only here (Windows) is the user saying "don't change this": the server's delete never removes
/// it. The node is skipped with that reason and waits, nothing goes back up, and once the mark is cleared the delete
/// lands.
#[tokio::test]
async fn a_read_only_file_is_never_removed_and_the_delete_waits() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "final.doc", b"signed off");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().remove_file(&path("final.doc")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().set_read_only(&path("final.doc"), true);
    let report = b.sync_once().await.unwrap();
    let why = report.skipped.iter().find(|(p, _)| p == "final.doc").map(|(_, why)| why.clone()).unwrap_or_default();
    assert!(why.starts_with("read-only here, not removed"), "{report:?}");
    assert_eq!(read(&b, "final.doc"), b"signed off");
    // Still read-only: nothing comes back to the server, and the delete keeps waiting.
    for _ in 0..2 {
        let report = b.sync_once().await.unwrap();
        assert!(report.uploaded.is_empty() && report.removed_locally.is_empty(), "{report:?}");
        assert!(report.skipped.iter().any(|(p, _)| p == "final.doc"), "{report:?}");
    }
    a.sync_once().await.unwrap();
    assert!(files(&a).is_empty(), "{:?}", files(&a));
    b.fs_mut().set_read_only(&path("final.doc"), false);
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.removed_locally, vec![path("final.doc")]);
    settle(&mut [&mut a, &mut b], 5).await;
    assert!(files(&b).is_empty());
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

/// Seed 9807 of the network simulation: the folder `Footage/Day1` became a link and the user renamed `Footage` to
/// `Shots`, then made new `Footage/Day1` folders under the old names; meanwhile the other machine made the same rename
/// and moved `Day1` out to `Archive`. Path first, the new folders are the tracked ones here: the pull moves them to
/// the server's places (the renamed folder holding the link is moved aside for them), and `Day1`'s clip isn't in the
/// new `Day1`. The link carries the name `Day1` from where this machine had the clip before the pull, so the clip's
/// delete is held behind it, and once the link is a real folder again nothing is lost.
#[tokio::test]
async fn a_link_in_a_folder_renamed_on_both_sides_while_new_folders_took_its_names_holds_the_delete() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage/Day1")).unwrap();
    write(&mut a, "Footage/Day1/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage/Day1")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Footage"), &path("Shots")).unwrap();
    b.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Shots/Day1"), &path("Archive")).unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Shots")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage/Day1")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "the clip behind the link stays: {report:?}");
        assert_eq!(report.held_deletes.len(), 1, "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Archive/c0.mov"), b"clip", "the desktop keeps it");
    }
    // The drive is back: the link is a real folder again, and its clip goes up as a new file.
    let link = a.fs().files().keys().find(|p| p.to_string().ends_with("/c0.mov")).cloned();
    assert!(link.is_none(), "the clip is behind the link here: {:?}", a.fs().files());
    let at = a.fs().dirs().into_iter().find(|d| d.to_string().starts_with("Shots (conflict")).expect("the renamed folder, moved aside");
    let link = at.join(&craft_cloud_proto::Name::new("Day1").unwrap());
    a.fs_mut().remove_file(&link).unwrap();
    a.fs_mut().mkdir(&link).unwrap();
    write(&mut a, &format!("{link}/c0.mov"), b"clip");
    settle(&mut [&mut a, &mut b], 6).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, &format!("{link}/c0.mov")), b"clip");
}

/// The smallest shape of seed 9807: no rename on the other side above the folder. `Footage/Day1` became a link here;
/// the user renamed `Footage` to `Shots` and made new `Footage` and `Footage/Day1` folders, while the other machine
/// only renamed `Day1` to `Day2`. The pull moves the new `Day1` to `Day2` (path first, it is the tracked one), so the
/// link carries the name `Day1` from where this machine had the folder before the pull, and the clip's delete is held.
#[tokio::test]
async fn a_link_whose_folder_the_other_machine_renamed_while_a_new_folder_took_its_name_holds_the_delete() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage/Day1")).unwrap();
    write(&mut a, "Footage/Day1/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage/Day1")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Footage/Day1"), &path("Footage/Day2")).unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Shots")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage/Day1")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "the clip behind the link stays: {report:?}");
        assert!(!report.held_deletes.is_empty(), "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/Day2/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// Seed 9807's shape with the folder above deleted on the other side: it moved `Day1` out to `Archive` and deleted
/// `Footage`. The link here still carries the name `Day1` from where this machine had the folder, so the clip's
/// delete is held, on the server and on the other machine.
#[tokio::test]
async fn a_link_whose_folder_the_other_machine_moved_out_of_a_deleted_parent_holds_the_delete() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage/Day1")).unwrap();
    write(&mut a, "Footage/Day1/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage/Day1")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Footage/Day1"), &path("Archive")).unwrap();
    b.fs_mut().remove_dir(&path("Footage")).unwrap();
    b.sync_once().await.unwrap();
    b.approve_deletes();
    b.sync_once().await.unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Shots")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage/Day1")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "the clip behind the link stays: {report:?}");
        assert!(!report.held_deletes.is_empty(), "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Archive/c0.mov"), b"clip", "the desktop keeps it");
    }
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

/// Seed 112337: a folder that was a link is a real folder again (a new local ID), and the first pass after that fails
/// (offline). The user then makes a link inside it and moves the folder. The failed pass already recorded the
/// folder's new ID, so the move is sent as a move, and the node behind the inner link isn't deleted on the server.
#[tokio::test]
async fn a_folder_put_back_records_its_new_id_even_in_a_failed_pass() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.transport().faults.every.store(1, Ordering::Relaxed);
    assert!(a.sync_once().await.is_err(), "the server is offline");
    a.transport().faults.every.store(0, Ordering::Relaxed);
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    assert_eq!(report.moved_remotely, vec![(path("Footage"), path("Clips"))]);
    assert!(report.created_dirs.is_empty(), "{report:?}");
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
}

/// Seed 112337's other order (finding 6 of the second review): the same, with no pass at all between the folder put
/// back and the move. Nothing local tells the moved folder apart from a new one (it and its files have new local
/// IDs), so the move goes up as a delete plus a new folder. But the delete of the node behind the inner link is held:
/// a link that appeared since the last finished push carries its name. It waits, with a
/// reason, for as long as the link is there, and so does its folder; once the link is a real file again (here it goes
/// up as a new file), the held delete goes. Nothing is lost, and nothing behind the link is ever deleted.
#[tokio::test]
async fn a_moved_folder_put_back_with_no_pass_between_holds_the_delete_behind_its_link() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(
            !report.removed_remotely.contains(&path("Footage/c0.mov")) && !report.removed_remotely.contains(&path("Footage")),
            "the node behind the link and its folder stay: {report:?}"
        );
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    // The link is a real file again: it goes up as a new file, and the held delete goes.
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    write(&mut a, "Clips/c0.mov", b"clip");
    settle(&mut [&mut a, &mut b], 6).await;
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
    assert!(!b.fs().dirs().contains(&path("Footage")), "{:?}", b.fs().dirs());
    assert_eq!(files(&a), files(&b));
}

/// Seed 112337's no-pass order, up to its first pass: the delete of `Footage/c0.mov` is held behind `Clips/c0.mov`.
async fn held_setup() -> (Server, common::Machine, common::Machine) {
    let (server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Footage/c0.mov"), b"clip");
    (server, a, b)
}

/// Seed 804 (fix round 4): the folder put back is moved, with no pass in between, onto the place of a tracked folder
/// the user just deleted. The folder there now is that tracked folder to the engine (path first; it takes the new
/// local ID), but the link in it is new since the last finished push and carries the node's name: held.
#[tokio::test]
async fn a_folder_put_back_and_moved_onto_a_deleted_tracked_folder_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("Old")).unwrap();
    write(&mut a, "Old/o.txt", b"old");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().remove_file(&path("Old/o.txt")).unwrap();
    a.fs_mut().remove_dir(&path("Old")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Old")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Old/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// Seeds 13116, 19051 and 35808 (fix round 4): the folder put back is moved, and the user makes a new folder at its
/// old name, all before any pass. The old place isn't empty (the engine takes the new folder for the old one, path
/// first), but the link is new and carries the node's name, so the delete is held.
#[tokio::test]
async fn a_folder_put_back_and_moved_then_made_again_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    write(&mut a, "Footage/new.txt", b"new");
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(read(&b, "Footage/new.txt"), b"new");
}

/// The same, moved onto a name another machine just made a folder at: this machine's pull takes the moved folder for
/// that new server folder (it adopts the folder already there) before the push looks, so the link in it is held too.
#[tokio::test]
async fn a_folder_put_back_and_moved_onto_a_new_server_folder_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().mkdir(&path("Clips")).unwrap();
    write(&mut b, "Clips/b.txt", b"from the desktop");
    b.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(read(&a, "Clips/b.txt"), b"from the desktop");
}

/// The same, moved onto the name another machine's server move just took: the user here deleted the folder the
/// server moved, so this machine's pull takes the moved folder for it (a folder sits where the server moved it)
/// before the push looks, and the link in it is held too.
#[tokio::test]
async fn a_folder_put_back_and_moved_onto_a_server_moves_target_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("Other")).unwrap();
    write(&mut a, "Other/o.txt", b"other");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Other"), &path("Clips")).unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Other/o.txt")).unwrap();
    a.fs_mut().remove_dir(&path("Other")).unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// Seed 110570 (fix round 5): the folder put back is moved into another tracked folder, and that folder is then
/// renamed onto the put-back folder's old place, all before any pass. However the engine takes the folders at
/// `Footage` and `Footage/Sub`, the link there is new and carries the node's name, so the delete is held.
#[tokio::test]
async fn a_folder_put_back_and_moved_into_a_folder_renamed_onto_its_place_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("Other")).unwrap();
    write(&mut a, "Other/o.txt", b"other");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Other/Sub")).unwrap();
    a.fs_mut().rename(&path("Other"), &path("Footage")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Footage/Sub/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(read(&b, "Footage/o.txt"), b"other", "the renamed folder went up as a move");
}

/// Review 6, P1: seed 13116's gesture (the folder put back, a link made in it, the folder moved, a new folder made at
/// its old name) while an unrelated tracked folder can't be listed. The link is new since the last finished push and
/// carries the missing node's name, so the delete is held, whatever the folders around it look like.
#[tokio::test]
async fn a_folder_made_again_while_another_cant_be_listed_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("Locked")).unwrap();
    write(&mut a, "Locked/l.txt", b"locked");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().hold(&path("Locked"));
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    write(&mut a, "Footage/new.txt", b"new");
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// Review 6, P2: seed 804's gesture (the folder put back, a link made in it, the folder moved onto the place of a
/// tracked folder the user just deleted) while an unrelated tracked folder can't be listed: held.
#[tokio::test]
async fn a_folder_moved_onto_a_deleted_one_while_another_cant_be_listed_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("Old")).unwrap();
    write(&mut a, "Old/o.txt", b"old");
    a.fs_mut().mkdir(&path("Locked")).unwrap();
    write(&mut a, "Locked/l.txt", b"locked");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().hold(&path("Locked"));
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().remove_file(&path("Old/o.txt")).unwrap();
    a.fs_mut().remove_dir(&path("Old")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Old")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Old/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// Review 6, P4: another machine moves the node itself out of the folder that became a link here, and only then does
/// this machine's user put the folder back, make the node a link and move the folder. The pull lets the missing
/// node's base follow the server's move, but the hold judges it by where this machine last had it
/// (`Footage/c0.mov`): the new link `Clips/c0.mov` carries its name, so the delete is held and the desktop keeps
/// the file its user moved.
#[tokio::test]
async fn a_server_move_of_the_node_before_the_gesture_is_seen_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("c0.mov")).unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "c0.mov"), b"clip", "the desktop keeps what its user moved");
    }
}

/// The same when the other machine also renames the node: the server's name (`c0-desktop.mov`) isn't on the link, but
/// the name this machine last had it under is, and the hold judges it by that.
#[tokio::test]
async fn a_server_rename_of_the_node_before_the_gesture_is_seen_holds_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("c0-desktop.mov")).unwrap();
    b.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("c0-desktop.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("c0-desktop.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "c0-desktop.mov"), b"clip", "the desktop keeps what its user renamed");
    }
}

/// A hold whose node another machine renamed, and whose link's folder is then renamed too: the node's names no
/// longer include the link's (this machine's record of where it last had the node ends with a finished push), but
/// the hold follows its link by the link's own name to the path no finished push saw.
#[tokio::test]
async fn a_hold_follows_its_link_after_the_node_was_renamed_elsewhere() {
    let (_server, mut a, mut b) = held_setup().await;
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("Footage/c0-desktop.mov")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert_eq!(report.held_deletes, vec![(path("Footage/c0-desktop.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
    }
    b.sync_once().await.unwrap();
    b.fs_mut().rename(&path("Clips"), &path("Clips3")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0-desktop.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0-desktop.mov"), vec![path("Clips3/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0-desktop.mov"), b"clip", "the desktop keeps it");
    }
}

/// Review 6, finding 4: another machine renames a file onto a name where this machine, which deleted that file, has a
/// file of its own. Both are kept, as when the file is still here: this machine's file is moved aside as a conflict
/// copy (it goes up as a new file), and the renamed file lands at its name. Before, this machine's file went up as the
/// renamed file's next version, with no copy anywhere.
#[tokio::test]
async fn a_server_move_onto_a_file_of_this_machines_own_keeps_both() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().remove_file(&path("notes.txt")).unwrap();
    write(&mut a, "report.txt", b"mine, from the laptop");
    b.fs_mut().rename(&path("notes.txt"), &path("report.txt")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.conflict_copies.len(), 1, "{report:?}");
    assert!(report.removed_remotely.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 6).await;
    for m in [&a, &b] {
        assert_eq!(read(m, "report.txt"), b"notes");
        let copies = conflict_copies(m);
        assert_eq!(copies.len(), 1, "{:?}", files(m));
        assert_eq!(read(m, &copies[0]), b"mine, from the laptop");
    }
    assert_eq!(files(&a), files(&b));
}

/// A link that was already there at the last finished push never holds a delete. The user's `Footage` lives on
/// another drive behind a permanent link; a later real delete of `ProjectB/Footage/x.mov` (whose path has a folder
/// named `Footage`) goes on the first pass.
#[tokio::test]
async fn a_link_that_was_already_there_never_holds_a_later_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("ProjectB")).unwrap();
    a.fs_mut().mkdir(&path("ProjectB/Footage")).unwrap();
    write(&mut a, "ProjectB/Footage/x.mov", b"x");
    write(&mut a, "ProjectB/Footage/y.mov", b"y");
    a.fs_mut().make_link(&path("Footage")).unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().remove_file(&path("ProjectB/Footage/x.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert_eq!(report.removed_remotely, vec![path("ProjectB/Footage/x.mov")], "{report:?}");
    b.sync_once().await.unwrap();
    assert!(b.fs().read(&path("ProjectB/Footage/x.mov")).is_err(), "the user's delete reached the desktop");
    assert_eq!(read(&b, "ProjectB/Footage/y.mov"), b"y");
}

/// While a delete is held behind a link, the user makes an unrelated new folder at the old folder's name. The
/// folder is found at its path again, but the link still stands where the hold recorded it: the node behind it
/// isn't deleted. Once the user removes the link, the node is judged afresh: it is missing, so its delete goes.
#[tokio::test]
async fn a_new_folder_at_the_old_place_doesnt_release_a_held_delete() {
    let (_server, mut a, mut b) = held_setup().await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    write(&mut a, "Footage/other.txt", b"other");
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(read(&b, "Footage/other.txt"), b"other");
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
}

/// While a delete is held behind a link, another machine moves the held file out of its folder on the server. The
/// hold is by node, so it follows: the moved file isn't deleted on the server (the folder it left is).
#[tokio::test]
async fn a_server_move_of_the_held_node_doesnt_release_its_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("c0-moved.mov")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("c0-moved.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("c0-moved.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "c0-moved.mov"), b"clip", "the desktop keeps what its user moved");
    }
}

/// Seed 95497 (fix round 5): while a delete is held behind a link, another machine moves the held file onto a name
/// where this machine has a file of its own (one an app is still writing, so it hasn't gone up). Since fix round 6 a
/// server move onto a file of this machine's own moves that file aside and lands the node, so here the move waits
/// for the file to settle, and the hold stays where the node was; when the user then moves the file away, the move
/// is taken (the node is still missing here), and the node behind the link isn't deleted on the server.
#[tokio::test]
async fn a_held_node_moved_onto_a_file_of_this_machine_keeps_its_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    write(&mut a, "c0-moved.mov", b"mine, still being written");
    a.fs_mut().set_changing(&path("c0-moved.mov"), true);
    b.fs_mut().rename(&path("Footage/c0.mov"), &path("c0-moved.mov")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(still_changing(&report, "c0-moved.mov"), "{report:?}");
    assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
    assert_eq!(read(&a, "c0-moved.mov"), b"mine, still being written");
    a.fs_mut().rename(&path("c0-moved.mov"), &path("mine.mov")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("c0-moved.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("c0-moved.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "c0-moved.mov"), b"clip", "the desktop keeps what its user moved");
    }
}

/// The seed-95497 rule, reached through a folder: while a delete is held behind a link, another machine renames the
/// held file's folder onto a name where this machine has a folder of its own holding a file under the held file's
/// name. The pull takes that folder for the moved one (folders hold no content of their own), so the file is at the
/// held node's place; it isn't the node (another local ID, other content), so the hold stays, the file waits, and
/// when the user moves it away the node behind the link still isn't deleted on the server.
#[tokio::test]
async fn a_held_nodes_folder_moved_onto_a_folder_of_this_machine_keeps_its_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    a.fs_mut().mkdir(&path("F2")).unwrap();
    write(&mut a, "F2/c0.mov", b"mine");
    b.fs_mut().rename(&path("Footage"), &path("F2")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.held_deletes, vec![(path("F2/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
    assert!(!report.uploaded.contains(&path("F2/c0.mov")), "{report:?}");
    a.fs_mut().rename(&path("F2/c0.mov"), &path("mine.mov")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("F2/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("F2/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "F2/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(read(&b, "mine.mov"), b"mine");
}

/// While a delete is held behind a link, another machine renames the link's folder. This machine's pull moves the
/// folder (with the link) here: the link now stands at a path the last finished push saw no link at, under the same
/// name, so the hold follows it there and the node behind the link isn't deleted on the server.
#[tokio::test]
async fn a_server_rename_of_the_links_folder_keeps_the_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    b.fs_mut().rename(&path("Clips"), &path("Clips3")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips3/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(a.fs().stat(&path("Clips3/c0.mov")).unwrap(), Some(craft_cloud_sync::FsKind::Other));
}

/// While a delete is held behind a link, the user makes a new folder at the node's old place (a pass sees it), then
/// puts the link's folder back from another drive under a new name. Nothing stands where the hold saw its link, but
/// a link with its name stands at a path the last finished push saw no link at: the hold moves there, and the node
/// behind it isn't deleted on the server.
#[tokio::test]
async fn a_links_folder_put_back_under_a_new_name_keeps_the_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    write(&mut a, "Footage/new.txt", b"new");
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
    a.sync_once().await.unwrap();
    for f in ["Clips/c0.mov", "Clips/c1.mov", "Clips/c2.mov"] {
        a.fs_mut().remove_file(&path(f)).unwrap();
    }
    a.fs_mut().remove_dir(&path("Clips")).unwrap();
    a.fs_mut().mkdir(&path("Clips2")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Clips2/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Clips2/c0.mov")).unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips2/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    // The user removes the link: the node is judged afresh, and its delete goes.
    a.fs_mut().remove_file(&path("Clips2/c0.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
}

/// The same with a hold an older agent saved: its record loads as the link's path alone (the agent's
/// `a_held_delete_saved_by_an_older_agent_is_kept` covers the formats), and its state has no record of the links
/// the last finished push saw, so every link counts as new. The hold still finds the link in the renamed folder.
#[tokio::test]
async fn a_server_rename_of_the_links_folder_keeps_a_hold_an_older_agent_saved() {
    let (_server, a, mut b) = held_setup().await;
    let mut state = a.state().clone();
    for link in state.held_deletes.values_mut() {
        *link = craft_cloud_sync::HeldLink::at(link.first().unwrap().clone());
    }
    state.known_links.clear();
    let mut a = common::restart(&a, state, |_| {});
    b.fs_mut().rename(&path("Clips"), &path("Clips3")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips3/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// The same when this machine dies right after the pull renamed the link's folder, before any state that records
/// the rename is saved (here: the pass runs, and the machine restarts from the state saved before it). The restarted
/// engine finds the folder already at its new place, so it never renames anything: the hold finds its link by name at
/// the new path, which no finished push saw.
#[tokio::test]
async fn a_server_rename_of_the_links_folder_lost_in_a_crash_keeps_the_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    b.fs_mut().rename(&path("Clips"), &path("Clips3")).unwrap();
    b.sync_once().await.unwrap();
    let state = a.state().clone();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.moved_locally, vec![(path("Clips"), path("Clips3"))], "{report:?}");
    let mut a = common::restart(&a, state, |_| {});
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips3/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// The same when this machine's engine moves the link's folder aside: another machine deleted it and made a new
/// folder under its name, so the new folder's place is cleared here first (the old one holds the link, so it isn't
/// removed). The hold follows the folder.
#[tokio::test]
async fn a_links_folder_moved_aside_keeps_the_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    b.fs_mut().remove_file(&path("Clips/c1.mov")).unwrap();
    b.fs_mut().remove_file(&path("Clips/c2.mov")).unwrap();
    b.fs_mut().remove_dir(&path("Clips")).unwrap();
    b.sync_once().await.unwrap();
    b.fs_mut().mkdir(&path("Clips")).unwrap();
    write(&mut b, "Clips/new.txt", b"new");
    b.sync_once().await.unwrap();
    let mut aside = Vec::new();
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        aside.extend(report.conflict_copies.iter().cloned());
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
        let [(node, links)] = report.held_deletes.as_slice() else { panic!("one hold: {report:?}") };
        let [link] = links.as_slice() else { panic!("one link: {report:?}") };
        assert_eq!(node, &path("Footage/c0.mov"));
        assert_eq!(a.fs().stat(link).unwrap(), Some(craft_cloud_sync::FsKind::Other), "{report:?}");
        assert!(link.names()[0].as_str().starts_with("Clips (conflict"), "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(aside[0].names()[0].as_str().starts_with("Clips (conflict"), "{aside:?}");
}

/// A held node that turns up here again (another machine edited it, and it was downloaded into a folder made again
/// for it) is no longer held: the link has been shown not to be it. A later real delete of the file goes.
#[tokio::test]
async fn a_held_node_found_again_is_released() {
    let (_server, mut a, mut b) = held_setup().await;
    write(&mut b, "Footage/c0.mov", b"edited on desktop");
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(read(&a, "Footage/c0.mov"), b"edited on desktop");
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(a.state().held_deletes.is_empty(), "{:?}", a.state().held_deletes);
    write(&mut a, "Footage/c0.mov", b"edited on laptop");
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded, vec![path("Footage/c0.mov")], "{report:?}");
    a.fs_mut().remove_file(&path("Footage/c0.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
    b.sync_once().await.unwrap();
    assert!(b.fs().read(&path("Footage/c0.mov")).is_err(), "the user's delete reached the desktop");
    assert_eq!(a.fs().stat(&path("Clips/c0.mov")).unwrap(), Some(craft_cloud_sync::FsKind::Other), "the link is untouched");
}

/// Renaming the link itself (not a folder above it) counts as removing it: the node is judged afresh, and being
/// missing, it is deleted on the server (history keeps it), as for a tracked file that became a link at its place.
#[tokio::test]
async fn renaming_the_link_itself_releases_its_hold() {
    let (_server, mut a, mut b) = held_setup().await;
    a.fs_mut().rename(&path("Clips/c0.mov"), &path("Clips/c0-renamed.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
    b.sync_once().await.unwrap();
    assert!(b.fs().read(&path("Footage/c0.mov")).is_err());
}

/// A held node put back at its place by the user (its folder made again, the file copied back in) is found again
/// too: the hold goes, and nothing is deleted.
#[tokio::test]
async fn a_held_node_put_back_is_released() {
    let (_server, mut a, mut b) = held_setup().await;
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    write(&mut a, "Footage/c0.mov", b"clip");
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty() && report.removed_remotely.is_empty(), "{report:?}");
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Footage/c0.mov"), b"clip");
}

/// Review 7, finding 1: two new links carry the node's name in the same window (seed 112337's gesture, plus an
/// unrelated `A/c0.mov` moved to another drive). The hold keeps both: once a finished push has seen them, removing
/// the unrelated one leaves the hold on the real one, and the node isn't deleted while `Clips/c0.mov` stands. It ends
/// when none of them is left.
#[tokio::test]
async fn a_hold_keeps_every_candidate_link_while_any_stands() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("A")).unwrap();
    write(&mut a, "A/c0.mov", b"another clip");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Footage/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    // Same window: the user also moves A/c0.mov to the other drive, leaving a link.
    a.fs_mut().make_link(&path("A/c0.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    // The report names every link (review 8, finding 1): the user decides on all of them, not on the first.
    assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("A/c0.mov"), path("Clips/c0.mov")])], "{report:?}");
    b.sync_once().await.unwrap();
    // Later the user removes the unrelated link: the real one still stands.
    a.fs_mut().remove_file(&path("A/c0.mov")).unwrap();
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "deleted while Clips/c0.mov stands: {report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
    // The last candidate goes: the node is judged afresh and, being missing, deleted (history keeps it).
    a.fs_mut().remove_file(&path("Clips/c0.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
}

/// Review 7, finding 2: a wrong hold behind a new permanent link. In one window the user moves `ProjectA/Renders`
/// to another drive (a link that stays) and really deletes `ProjectB/Renders/old.exr`; the delete is held, because
/// "Renders" is a folder on its path. It stays held on every pass, also after approving everything the guard holds
/// back (that names no hold). An approval that names the held node lets its delete go; nothing else is deleted.
#[tokio::test]
async fn an_approval_naming_a_held_delete_lets_it_go() {
    let (_server, mut a, mut b) = linked_setup().await;
    for d in ["ProjectA", "ProjectA/Renders", "ProjectB", "ProjectB/Renders"] {
        a.fs_mut().mkdir(&path(d)).unwrap();
    }
    write(&mut a, "ProjectA/Renders/r.exr", b"r");
    write(&mut a, "ProjectB/Renders/old.exr", b"old");
    write(&mut a, "ProjectB/Renders/keep.exr", b"keep");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().make_link(&path("ProjectA/Renders")).unwrap();
    a.fs_mut().remove_file(&path("ProjectB/Renders/old.exr")).unwrap();
    let mut held = Vec::new();
    for _ in 0..3 {
        a.approve_deletes();
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty(), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("ProjectB/Renders/old.exr"), vec![path("ProjectA/Renders")])], "{report:?}");
        assert_eq!(report.held_nodes.len(), 1, "{report:?}");
        held = report.held_nodes.clone();
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "ProjectB/Renders/old.exr"), b"old");
    }
    // An approval naming another node releases nothing.
    a.approve_deletes_of([craft_cloud_proto::NodeId::generate()]);
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.held_nodes == held, "{report:?}");
    a.approve_deletes_of(held.clone());
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.removed_remotely, vec![path("ProjectB/Renders/old.exr")], "{report:?}");
    assert_eq!(report.removed_nodes, held, "{report:?}");
    assert!(report.held_deletes.is_empty() && a.state().held_deletes.is_empty(), "{report:?}");
    b.sync_once().await.unwrap();
    assert!(b.fs().read(&path("ProjectB/Renders/old.exr")).is_err(), "the user's delete reached the desktop");
    assert_eq!(read(&b, "ProjectB/Renders/keep.exr"), b"keep");
    assert_eq!(read(&b, "ProjectA/Renders/r.exr"), b"r", "the link's folder isn't deleted");
    assert_eq!(a.fs().stat(&path("ProjectA/Renders")).unwrap(), Some(craft_cloud_sync::FsKind::Other), "the link is untouched");
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.held_deletes.is_empty(), "{report:?}");
}

/// Known gap (review 7, finding 5), pinned: a link a finished push saw while the node was still behind another link
/// is known, so it holds nothing later. `Footage` is a link; the user copies the folder back as `Clips` while that
/// link stands and makes `Clips/c0.mov` a link; a pass runs (the tracked `Footage/*` is hidden, so nothing is
/// missing, and `Clips/c0.mov` becomes known). Removing the old `Footage` link then deletes `Footage/c0.mov` on the
/// server while `Clips/c0.mov` stands (history keeps it). If this starts holding the delete, the gap is closed:
/// update Known gaps.
#[tokio::test]
async fn known_gap_a_link_seen_while_the_node_was_behind_another_link_holds_nothing() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().make_link(&path("Footage")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().mkdir(&path("Clips")).unwrap();
    for i in 0..3 {
        write(&mut a, &format!("Clips/c{i}.mov"), b"clip");
    }
    a.fs_mut().make_link(&path("Clips/c0.mov")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty() && report.held_deletes.is_empty(), "{report:?}");
    a.fs_mut().remove_file(&path("Footage")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")), "{report:?}");
    b.sync_once().await.unwrap();
    assert!(b.fs().read(&path("Footage/c0.mov")).is_err(), "{:?}", files(&b));
}

/// Known gap (review 7, finding 3), pinned: for a node that isn't held, a file of this machine's own still being
/// written at the place another machine moved the node to doesn't make the delete wait. The laptop deletes
/// `notes.txt` and is still writing its own `report.txt`; the desktop renames `notes.txt` to `report.txt`. The
/// server move waits (the file is never moved aside while it changes), but the push still sends the laptop's delete:
/// the delete wins, as when the target is free. Nothing is lost (history keeps the desktop's version). Once the file
/// settles it goes up as a new file. Which of the delete and the move should win here is a follow-up (one rule for
/// both the occupied and the free target).
#[tokio::test]
async fn known_gap_a_server_move_onto_a_file_still_being_written_loses_to_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().remove_file(&path("notes.txt")).unwrap();
    write(&mut a, "report.txt", b"mine, still being written");
    a.fs_mut().set_changing(&path("report.txt"), true);
    b.fs_mut().rename(&path("notes.txt"), &path("report.txt")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    // The base stays where this machine had it (the move waited), so the delete is listed there.
    assert_eq!(report.removed_remotely, vec![path("notes.txt")], "{report:?}");
    assert!(still_changing(&report, "report.txt"), "{report:?}");
    a.fs_mut().set_changing(&path("report.txt"), false);
    settle(&mut [&mut a, &mut b], 6).await;
    for m in [&a, &b] {
        assert_eq!(read(m, "report.txt"), b"mine, still being written");
        assert!(conflict_copies(m).is_empty(), "{:?}", files(m));
        assert!(m.fs().read(&path("notes.txt")).is_err());
    }
}

/// The same with a link of this machine's own at the place (one with another name, so it holds nothing): the server
/// move waits, and the push still sends the delete.
#[tokio::test]
async fn known_gap_a_server_move_onto_a_link_loses_to_the_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().remove_file(&path("notes.txt")).unwrap();
    a.fs_mut().make_link(&path("report.txt")).unwrap();
    b.fs_mut().rename(&path("notes.txt"), &path("report.txt")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.removed_remotely, vec![path("notes.txt")], "{report:?}");
    b.sync_once().await.unwrap();
    assert!(b.fs().read(&path("report.txt")).is_err() && b.fs().read(&path("notes.txt")).is_err(), "{:?}", files(&b));
    assert_eq!(a.fs().stat(&path("report.txt")).unwrap(), Some(craft_cloud_sync::FsKind::Other), "the link is untouched");
    assert!(conflict_copies(&a).is_empty(), "{:?}", files(&a));
}

/// Known gap (review 7, finding 4), pinned: a server rename of a folder this machine deleted, onto a folder of its
/// own, makes that folder the moved one, and a file of its own in it under a tracked file's name goes up as that
/// file's next version (no conflict copy; history keeps the desktop's clip). The laptop deletes `Footage` and makes
/// its own `F2/c0.mov`; the desktop renames `Footage` to `F2`.
#[tokio::test]
async fn known_gap_a_server_folder_rename_onto_an_own_folder_takes_its_file_as_the_next_version() {
    let (_server, mut a, mut b) = linked_setup().await;
    for i in 0..3 {
        a.fs_mut().remove_file(&path(&format!("Footage/c{i}.mov"))).unwrap();
    }
    a.fs_mut().remove_dir(&path("Footage")).unwrap();
    a.fs_mut().mkdir(&path("F2")).unwrap();
    write(&mut a, "F2/c0.mov", b"mine");
    b.fs_mut().rename(&path("Footage"), &path("F2")).unwrap();
    b.sync_once().await.unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.uploaded.contains(&path("F2/c0.mov")) && report.conflict_copies.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 6).await;
    for m in [&a, &b] {
        assert_eq!(read(m, "F2/c0.mov"), b"mine");
        assert!(conflict_copies(m).is_empty(), "{:?}", files(m));
    }
}

/// Finding 1's other road: the hold sits on the real link, the user renames its folder, and in the same window an
/// unrelated link with the same name appears at a smaller path. The hold follows to both, so removing the unrelated
/// one later doesn't end it.
#[tokio::test]
async fn a_hold_follows_to_every_link_with_its_name() {
    let (_server, mut a, mut b) = held_setup().await;
    a.fs_mut().mkdir(&path("B")).unwrap();
    a.fs_mut().make_link(&path("B/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Clips"), &path("Clips2")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("B/c0.mov"), path("Clips2/c0.mov")])], "{report:?}");
    b.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("B/c0.mov")).unwrap();
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("Footage/c0.mov")), "deleted while Clips2/c0.mov stands: {report:?}");
        assert_eq!(report.held_deletes, vec![(path("Footage/c0.mov"), vec![path("Clips2/c0.mov")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(read(&b, "Footage/c0.mov"), b"clip", "the desktop keeps it");
    }
}

/// A real delete of a folder while a link with the same name as a file in it sits in another tracked folder: the
/// link was there at the last finished push, so it can't be the folder's file moved with it, and the delete goes.
#[tokio::test]
async fn a_same_named_link_in_a_tracked_folder_doesnt_hold_a_delete() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().mkdir(&path("Other")).unwrap();
    write(&mut a, "Other/keep.txt", b"keep");
    a.fs_mut().make_link(&path("Other/c0.mov")).unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    for i in 0..3 {
        a.fs_mut().remove_file(&path(&format!("Footage/c{i}.mov"))).unwrap();
    }
    a.fs_mut().remove_dir(&path("Footage")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.held_deletes.is_empty(), "{report:?}");
    assert!(report.removed_remotely.contains(&path("Footage/c0.mov")) && report.removed_remotely.contains(&path("Footage")), "{report:?}");
    b.sync_once().await.unwrap();
    assert!(!files(&b).iter().any(|(p, _)| p.starts_with("Footage")), "{:?}", files(&b));
}

/// A new folder ID the start of a pass can't take (here a renamed folder holding a link keeps its ID, and a new
/// folder took its old name) costs one look, not a second full scan on every pass until it is resolved.
#[tokio::test]
async fn a_declined_folder_id_doesnt_cost_a_scan_every_pass() {
    let (_server, mut a, mut b) = linked_setup().await;
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    a.fs_mut().make_link(&path("Clips/c0.mov")).unwrap();
    a.fs_mut().mkdir(&path("Footage")).unwrap();
    for _ in 0..3 {
        a.sync_once().await.unwrap();
    }
    b.sync_once().await.unwrap();
    let per_pass = |m: &common::Machine| m.fs().root_lists();
    let (a0, b0) = (per_pass(&a), per_pass(&b));
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty(), "{report:?}");
    b.sync_once().await.unwrap();
    assert_eq!(per_pass(&a) - a0, per_pass(&b) - b0, "a pass here lists the folder as often as on a machine with nothing to learn");
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

/// A clip replaced by a link, next to an untracked file whose name differs only by case, then the folder renamed
/// (seed 45982). In the renamed folder neither name is tracked, so the case twin wins the name and the link is
/// skipped as a clash, but it is still a link: the clip behind it is never deleted on the server.
#[tokio::test]
async fn a_link_skipped_as_a_case_clash_still_hides_its_tracked_file() {
    let (_server, mut a, mut b) = linked_setup().await;
    write(&mut a, "Footage/C0.MOV", b"an untracked twin");
    a.fs_mut().make_link(&path("Footage/c0.mov")).unwrap();
    a.fs_mut().rename(&path("Footage"), &path("Clips")).unwrap();
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty(), "{report:?}");
    }
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "Clips/c0.mov"), b"clip");
}

/// The laptop gives a.txt b.txt's content and renames it to c.txt (or replaces it with a new c.txt holding that
/// content); the desktop deletes a.txt and renames b.txt to c.txt. The file at c.txt holds the server's content,
/// but it is the tracked b.txt (its file ID says so), so the server's node doesn't adopt it: two nodes never share
/// one file, and b.txt isn't deleted on the server.
async fn a_tracked_file_is_never_adopted_by_another_node(new_node: bool) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "a.txt", b"one");
    write(&mut a, "b.txt", b"two");
    settle(&mut [&mut a, &mut b], 5).await;
    if new_node {
        a.fs_mut().remove_file(&path("a.txt")).unwrap();
        write(&mut a, "c.txt", b"two");
    } else {
        write(&mut a, "a.txt", b"two");
        a.sync_once().await.unwrap();
        a.fs_mut().rename(&path("a.txt"), &path("c.txt")).unwrap();
    }
    a.sync_once().await.unwrap();
    b.fs_mut().remove_file(&path("a.txt")).unwrap();
    b.fs_mut().rename(&path("b.txt"), &path("c.txt")).unwrap();
    let report = b.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(files(&b).iter().filter(|(_, d)| d == b"two").count(), 2, "{:?}", files(&b));
}

#[tokio::test]
async fn a_tracked_file_is_never_adopted_by_a_moved_node() {
    a_tracked_file_is_never_adopted_by_another_node(false).await;
}

#[tokio::test]
async fn a_tracked_file_is_never_adopted_by_a_new_node() {
    a_tracked_file_is_never_adopted_by_another_node(true).await;
}

/// The laptop saves notes again with the same bytes, which goes up as a new version with the same manifest. The
/// desktop has notes open and locked: the new version is recorded without touching the file (nothing to download,
/// nothing to compare, no conflict copy), and a real edit later lands as usual. A rename that comes with such a
/// version is still applied.
#[tokio::test]
async fn a_new_version_with_the_same_content_is_no_change() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "notes", b"same");
    write(&mut a, "other", b"other");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut a, "notes", b"same");
    write(&mut a, "other", b"other");
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded.len(), 2, "{report:?}");
    a.fs_mut().rename(&path("other"), &path("renamed")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().lock(&path("notes"));
    let report = b.sync_once().await.unwrap();
    assert!(report.downloaded.is_empty() && report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(report.moved_locally, vec![(path("other"), path("renamed"))]);
    assert!(!b.sync_once().await.unwrap().changed());
    b.fs_mut().unlock_all();
    write(&mut a, "notes", b"edited");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&b, "notes"), b"edited");
    assert!(conflict_copies(&b).is_empty());
}

/// Rename blocked (Windows): an app holds the desktop's notes open, so the laptop's edit can't replace it. The
/// download is kept and the node skipped as in use; later passes land the kept file once it is let go, without
/// downloading it again. A kept download whose version is no longer current is dropped. Both ways of landing: one
/// exchange, and in steps.
#[tokio::test]
async fn a_download_onto_a_file_in_use_is_kept_and_lands_later() {
    for swap in [true, false] {
        let server = Server::new().await;
        let ana = server.user("ana").await;
        let mut a = machine(&server, &ana, "laptop").await;
        let fs = if swap { MemFs::new() } else { MemFs::new().without_swap() };
        let mut b = machine_with(&server, &ana, "desktop", fs, 50).await;
        write(&mut a, "notes", b"v1");
        settle(&mut [&mut a, &mut b], 5).await;

        b.fs_mut().hold(&path("notes"));
        write(&mut a, "notes", b"v2");
        a.sync_once().await.unwrap();
        let takes = b.fs().kept_takes();
        for pass in 0..2 {
            let report = b.sync_once().await.unwrap();
            // The first pass downloads and keeps; the next one asks whether the file can be replaced before it takes the
            // kept file back, and skips without moving or hashing it.
            assert_eq!(b.fs().kept_takes(), takes + 1, "pass {pass}");
            assert!(report.downloaded.is_empty() && report.conflict_copies.is_empty(), "{report:?}");
            assert!(
                report.skipped.iter().any(|(p, why)| p == "notes" && why.contains("in use") && why.ends_with("tried again next pass")),
                "{report:?}"
            );
            assert_eq!(read(&b, "notes"), b"v1");
            assert_eq!(b.fs().kept().unwrap().len(), 1);
        }
        let fetched = b.transport().faults.chunk_downloads.load(Ordering::Relaxed);
        b.fs_mut().release(&path("notes"));
        let report = b.sync_once().await.unwrap();
        assert_eq!(report.downloaded, vec![path("notes")], "{report:?}");
        assert_eq!(read(&b, "notes"), b"v2");
        assert_eq!(b.transport().faults.chunk_downloads.load(Ordering::Relaxed), fetched, "the kept download landed");
        assert!(b.fs().kept().unwrap().is_empty());

        // Kept, then overtaken by a newer version: that one is downloaded, the kept one dropped.
        b.fs_mut().hold(&path("notes"));
        write(&mut a, "notes", b"v3");
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
        write(&mut a, "notes", b"v4");
        a.sync_once().await.unwrap();
        b.fs_mut().release(&path("notes"));
        let report = b.sync_once().await.unwrap();
        assert_eq!(report.downloaded, vec![path("notes")], "{report:?}");
        assert_eq!(read(&b, "notes"), b"v4");
        assert!(b.fs().kept().unwrap().is_empty());
        settle(&mut [&mut a, &mut b], 5).await;
        assert_eq!(files(&a), files(&b));
        assert!(conflict_copies(&b).is_empty());
    }
}

/// The user renames a tracked folder and makes a new one under the old name (seed 10402). Path first: the new
/// folder is the tracked one, the renamed one goes up as new. The tracked folder takes the new folder's ID even
/// though its old ID is still in the folder (on the renamed one), so a later rename of it is sent as a move, not
/// as a delete and a new folder.
#[tokio::test]
async fn a_folder_made_again_under_a_tracked_name_takes_its_new_id() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "pad.txt", b"pad");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().rename(&path("x"), &path("old")).unwrap();
    a.fs_mut().mkdir(&path("x")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.created_dirs, vec![path("old")], "{report:?}");
    a.fs_mut().rename(&path("x"), &path("renamed")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.moved_remotely, vec![(path("x"), path("renamed"))], "{report:?}");
    assert!(report.removed_remotely.is_empty(), "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(b.fs().dirs(), vec![path("old"), path("renamed")]);
}

fn still_changing(report: &craft_cloud_sync::Report, p: &str) -> bool {
    report.skipped.iter().any(|(at, why)| at == p && why.starts_with(craft_cloud_sync::STILL_CHANGING))
}

/// A new file and an edit an app is still writing wait ("still changing"): nothing goes up, nothing is deleted on
/// the server, and once they settle they sync like any other change.
#[tokio::test]
async fn files_still_being_written_wait_until_they_settle() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "edit.txt", b"one");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut a, "edit.txt", b"two, half written");
    write(&mut a, "render.mov", b"frame 1");
    a.fs_mut().set_changing(&path("edit.txt"), true);
    a.fs_mut().set_changing(&path("render.mov"), true);
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(report.uploaded.is_empty() && report.removed_remotely.is_empty(), "{report:?}");
        assert!(still_changing(&report, "edit.txt") && still_changing(&report, "render.mov"), "{report:?}");
    }
    b.sync_once().await.unwrap();
    assert_eq!(files(&b), vec![("edit.txt".to_owned(), b"one".to_vec())]);
    a.fs_mut().set_changing(&path("edit.txt"), false);
    a.fs_mut().set_changing(&path("render.mov"), false);
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded.len(), 2, "{report:?}");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
}

/// A file edited here that an app is still writing is neither moved aside as a conflict copy nor replaced when the
/// server has a new version: it waits, and the conflict is handled as usual once it settles.
#[tokio::test]
async fn a_server_edit_waits_for_a_file_still_being_written() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "doc.txt", b"base");
    write(&mut a, "new.txt", b"mine, half written");
    a.fs_mut().set_changing(&path("new.txt"), true);
    settle(&mut [&mut b], 1).await;
    a.sync_once().await.unwrap();
    b.sync_once().await.unwrap();
    write(&mut b, "doc.txt", b"from desktop");
    write(&mut b, "new.txt", b"theirs");
    b.sync_once().await.unwrap();
    write(&mut a, "doc.txt", b"from laptop, half written");
    a.fs_mut().set_changing(&path("doc.txt"), true);
    let report = a.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty() && report.downloaded.is_empty(), "{report:?}");
    assert!(still_changing(&report, "doc.txt") && still_changing(&report, "new.txt"), "{report:?}");
    assert_eq!(read(&a, "doc.txt"), b"from laptop, half written");
    assert_eq!(read(&a, "new.txt"), b"mine, half written");
    a.fs_mut().set_changing(&path("doc.txt"), false);
    a.fs_mut().set_changing(&path("new.txt"), false);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&a, "doc.txt"), b"from desktop");
    assert_eq!(conflict_copies(&a).len(), 2, "{:?}", files(&a));
}

/// A newcomer from the server whose place holds a file an app is still writing, as a case twin or where its folder
/// must go, waits before anything is made or moved aside (seed 44450: the newcomer was skipped only after its folders
/// were made and its case twin moved aside). Once the file settles, the usual conflict handling runs.
#[tokio::test]
async fn a_newcomer_waits_before_moving_aside_a_file_still_being_written() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "A.TXT", b"mine, half written");
    write(&mut a, "d", b"a file named like a folder, half written");
    a.fs_mut().set_changing(&path("A.TXT"), true);
    a.fs_mut().set_changing(&path("d"), true);
    write(&mut b, "a.txt", b"theirs");
    b.fs_mut().mkdir(&path("d")).unwrap();
    write(&mut b, "d/x.txt", b"inside");
    b.sync_once().await.unwrap();
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(report.conflict_copies.is_empty() && report.created_dirs.is_empty() && report.downloaded.is_empty(), "{report:?}");
        assert!(still_changing(&report, "A.TXT") && still_changing(&report, "d"), "{report:?}");
        assert_eq!(read(&a, "A.TXT"), b"mine, half written");
        assert_eq!(read(&a, "d"), b"a file named like a folder, half written");
    }
    a.fs_mut().set_changing(&path("A.TXT"), false);
    a.fs_mut().set_changing(&path("d"), false);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    assert_eq!(read(&a, "d/x.txt"), b"inside");
}

/// A server move (a rename on the desktop) onto a name whose case twin here is untracked and still being written
/// waits, before its first step: the twin is neither moved aside nor touched (the check in `settle_occupants`). Once
/// it settles, the move applies, and the twin is kept as a conflict copy.
#[tokio::test]
async fn a_server_move_waits_for_a_case_twin_still_being_written() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "a.txt", b"shared");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut a, "B.TXT", b"mine, half written");
    a.fs_mut().set_changing(&path("B.TXT"), true);
    b.fs_mut().rename(&path("a.txt"), &path("b.txt")).unwrap();
    b.sync_once().await.unwrap();
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(report.conflict_copies.is_empty() && report.moved_locally.is_empty(), "{report:?}");
        assert!(still_changing(&report, "B.TXT"), "{report:?}");
        assert_eq!(read(&a, "B.TXT"), b"mine, half written");
        assert_eq!(read(&a, "a.txt"), b"shared");
    }
    a.fs_mut().set_changing(&path("B.TXT"), false);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(read(&a, "b.txt"), b"shared");
    assert!(files(&a).iter().any(|(p, d)| p.contains("(conflict") && d == b"mine, half written"), "{:?}", files(&a));
    assert_eq!(files(&a), files(&b));
}

/// A file renamed and edited here that an app is still writing, found by its local ID when the server has a new
/// version of it, waits: it is neither moved aside as a conflict copy nor replaced. Once it settles, both versions
/// are kept as usual.
#[tokio::test]
async fn a_moved_file_still_being_written_waits_for_a_server_edit() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("d")).unwrap();
    write(&mut a, "d/doc.txt", b"base");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut b, "d/doc.txt", b"from desktop");
    b.sync_once().await.unwrap();
    // The folder is found by its local ID, and the file through it.
    a.fs_mut().rename(&path("d"), &path("e")).unwrap();
    write(&mut a, "e/doc.txt", b"from laptop, half written");
    a.fs_mut().set_changing(&path("e/doc.txt"), true);
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        assert!(report.conflict_copies.is_empty() && report.downloaded.is_empty(), "{report:?}");
        assert!(still_changing(&report, "e/doc.txt"), "{report:?}");
        assert_eq!(read(&a, "e/doc.txt"), b"from laptop, half written");
    }
    a.fs_mut().set_changing(&path("e/doc.txt"), false);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(files(&a), files(&b));
    let contents: Vec<Vec<u8>> = files(&a).into_iter().map(|(_, d)| d).collect();
    assert!(contents.contains(&b"from desktop".to_vec()) && contents.contains(&b"from laptop, half written".to_vec()), "{:?}", files(&a));
}

/// A delete the server refuses (the file was edited and moved there, and that change waits here behind a link) keeps
/// the folder the file is tracked in: the folder's delete is dropped with it, so no node is left tracked under a
/// folder the server no longer has (seed 44450).
#[tokio::test]
async fn a_refused_delete_keeps_its_folder() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    a.fs_mut().mkdir(&path("f")).unwrap();
    write(&mut a, "x/keep.txt", b"keep");
    write(&mut a, "f/c.psd", b"layers");
    for i in 0..20 {
        write(&mut a, &format!("pad{i}.txt"), b"pad");
    }
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut b, "f/c.psd", b"layers, edited");
    b.sync_once().await.unwrap();
    b.fs_mut().rename(&path("f/c.psd"), &path("x/c.psd")).unwrap();
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.moved_remotely.len(), 1, "{report:?}");
    a.fs_mut().make_link(&path("x")).unwrap();
    a.fs_mut().remove_file(&path("f/c.psd")).unwrap();
    a.fs_mut().remove_dir(&path("f")).unwrap();
    let report = a.sync_once().await.unwrap();
    assert!(report.removed_remotely.is_empty(), "{report:?}");
    let state = a.state();
    for (id, entry) in &state.base {
        if let Some(parent) = entry.parent {
            assert!(state.base.contains_key(&parent), "{id} ({:?}) is tracked under a folder that isn't: {report:?}", entry.name);
        }
    }
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "x/c.psd"), b"layers, edited");
}

// ---- per-file problems never stop the rest (review H2, M4, M1) ----

#[tokio::test]
async fn a_file_over_the_size_limit_is_skipped_and_everything_else_syncs() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    // A small limit stands in for the protocol's 256 GiB.
    let mut a = common::machine_config(&server, &ana, "laptop", MemFs::new(), 50, |c| c.max_file_bytes = 10).await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "capture.mov", b"twenty bytes of film");
    write(&mut a, "notes.txt", b"small");
    for _ in 0..2 {
        let report = a.sync_once().await.unwrap();
        let why = report.skipped.iter().find(|(p, _)| p == "capture.mov").map(|(_, why)| why.as_str());
        assert!(why.is_some_and(|w| w.starts_with("too big to sync: 20 bytes")), "{report:?}");
        assert!(!report.uploaded.contains(&path("capture.mov")), "{report:?}");
    }
    b.sync_once().await.unwrap();
    assert_eq!(files(&b), vec![("notes.txt".to_owned(), b"small".to_vec())]);
    // Edited down to size, it goes up like any other file.
    write(&mut a, "capture.mov", b"short");
    assert!(a.sync_once().await.unwrap().uploaded.contains(&path("capture.mov")));
}

#[tokio::test]
async fn no_free_conflict_name_skips_only_that_file() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "doc.txt", b"v1");
    settle(&mut [&mut a, &mut b], 5).await;
    // Every conflict-copy name this minute is taken on the desktop.
    let name = craft_cloud_proto::Name::new("doc.txt").unwrap();
    for counter in 1..=1000 {
        let copy = craft_cloud_proto::conflict_name(&name, false, "desktop", common::STAMP, counter).unwrap();
        write(&mut b, copy.as_str(), b"old copy");
    }
    write(&mut a, "doc.txt", b"v2 from the laptop");
    a.sync_once().await.unwrap();
    write(&mut b, "doc.txt", b"v2 from the desktop");
    write(&mut b, "other.txt", b"other");
    let report = b.sync_once().await.unwrap();
    let why = report.skipped.iter().find(|(p, _)| p == "doc.txt").map(|(_, why)| why.as_str());
    assert!(why.is_some_and(|w| w.contains("no free conflict-copy name")), "{:?}", report.skipped.last());
    assert!(report.uploaded.contains(&path("other.txt")), "everything else keeps syncing");
    assert_eq!(read(&b, "doc.txt"), b"v2 from the desktop", "the edit stays put");
}

#[tokio::test]
async fn a_file_too_big_for_the_accounts_storage_is_skipped_and_the_rest_syncs() {
    let mut server = Server::new().await;
    server.meta = server.meta.clone().with_stored_bytes_cap(Some(400));
    let ana = server.user("ana").await;
    // 400 bytes of room count as room here (one chunk of 4 MiB is the floor otherwise).
    let mut a = common::machine_config(&server, &ana, "laptop", MemFs::new(), 50, |c| c.quota_floor = 100).await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "big.bin", &[7u8; 1000]);
    write(&mut a, "big2.bin", &[8u8; 1000]);
    write(&mut a, "small.txt", b"small");
    a.fs_mut().mkdir(&path("Folder")).unwrap();
    let report = a.sync_once().await.unwrap();
    for big in ["big.bin", "big2.bin"] {
        let why = report.skipped.iter().find(|(p, _)| p == big).map(|(_, why)| why.as_str());
        assert!(why.is_some_and(|w| w.starts_with("too big for the account's storage on the server (0 of 400 bytes used)")), "{report:?}");
    }
    assert_eq!(a.fs().reads_of(&path("big2.bin")), 0, "a file bigger than the room the first refusal showed is never read");
    assert_eq!(report.uploaded, vec![path("small.txt")]);
    b.sync_once().await.unwrap();
    assert_eq!(files(&b), vec![("small.txt".to_owned(), b"small".to_vec())]);
    assert_eq!(b.fs().dirs(), vec![path("Folder")], "everything else syncs");
}

#[tokio::test]
async fn an_edit_that_fits_by_dedup_is_uploaded_after_a_refused_edit() {
    let free = Server::new().await;
    let ana = free.user("ana").await;
    let mut a = machine(&free, &ana, "laptop").await;
    write(&mut a, "a-refused.bin", &[1u8; 1000]);
    write(&mut a, "b-synced.bin", &[2u8; 1000]);
    a.sync_once().await.unwrap();
    // 400 bytes of room are left.
    let capped = free.meta.clone().with_stored_bytes_cap(Some(free.meta.stored_bytes(ana.space).await.unwrap() + 400));
    let mut a = common::on_meta(&a, capped, |c| c.quota_floor = 100);
    // The first edit needs more room than there is and is refused. The second file is bigger than the room, but
    // its new content is a copy of what the server stores already: only a manifest goes up.
    write(&mut a, "a-refused.bin", &[3u8; 1000]);
    write(&mut a, "b-synced.bin", &[1u8; 1000]);
    let reads = a.fs().reads_of(&path("b-synced.bin"));
    let report = a.sync_once().await.unwrap();
    assert!(a.fs().reads_of(&path("b-synced.bin")) > reads, "the edit is read: it can fit against its own base");
    assert_eq!(report.uploaded, vec![path("b-synced.bin")], "{report:?}");
    let why = report.skipped.iter().find(|(p, _)| p == "a-refused.bin").map(|(_, why)| why.as_str());
    assert!(why.is_some_and(|w| w.starts_with("too big for the account's storage")), "{report:?}");
}

#[tokio::test]
async fn an_account_with_no_room_left_stops_the_pass() {
    // What one small file stores, measured on a server without a cap.
    let free = Server::new().await;
    let ana = free.user("ana").await;
    let mut a = machine(&free, &ana, "laptop").await;
    write(&mut a, "small.txt", b"small");
    a.sync_once().await.unwrap();
    let one_file = free.meta.stored_bytes(ana.space).await.unwrap();

    // A cap of exactly that: the next file finds no room at all.
    let mut server = Server::new().await;
    server.meta = server.meta.clone().with_stored_bytes_cap(Some(one_file));
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "small.txt", b"small");
    a.sync_once().await.unwrap();
    write(&mut a, "more.txt", b"more");
    let full = a.sync_once().await;
    assert!(
        matches!(&full, Err(craft_cloud_sync::SyncError::Transport(craft_cloud_sync::TransportError::QuotaExceeded { used, limit })) if used == limit),
        "{full:?}"
    );
}

#[tokio::test]
async fn a_nearly_full_account_counts_as_full() {
    // 400 bytes of room left: less than one chunk, so the account counts as full and the pass stops, instead of
    // every new file being read and refused on every pass.
    let mut server = Server::new().await;
    server.meta = server.meta.clone().with_stored_bytes_cap(Some(400));
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "big.bin", &[7u8; 1000]);
    let full = a.sync_once().await;
    assert!(
        matches!(&full, Err(craft_cloud_sync::SyncError::Transport(craft_cloud_sync::TransportError::QuotaExceeded { used: 0, limit: 400 }))),
        "{full:?}"
    );
}

#[tokio::test]
async fn chunks_the_server_lost_are_sent_again_once() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "photo.raw", b"raw sensor data");
    // The one chunk is answered as stored and lost: the manifest is refused for it.
    a.transport().faults.lose_chunks.store(1, Ordering::Relaxed);
    let report = a.sync_once().await.unwrap();
    assert_eq!(report.uploaded, vec![path("photo.raw")]);
    assert_eq!(a.transport().faults.chunk_uploads.load(Ordering::Relaxed), 2, "exactly the missing chunk went again, once");
    b.sync_once().await.unwrap();
    assert_eq!(read(&b, "photo.raw"), b"raw sensor data");
}

#[tokio::test]
async fn chunks_lost_twice_fail_the_pass_for_a_later_try() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    write(&mut a, "photo.raw", b"raw sensor data");
    a.transport().faults.lose_chunks.store(2, Ordering::Relaxed);
    let failed = a.sync_once().await;
    assert!(matches!(failed, Err(craft_cloud_sync::SyncError::Transport(craft_cloud_sync::TransportError::MissingChunks(_)))), "{failed:?}");
    assert_eq!(a.transport().faults.chunk_uploads.load(Ordering::Relaxed), 2, "one retry, no more");
    // The next pass (the run loop's backoff) gets it up.
    assert_eq!(a.sync_once().await.unwrap().uploaded, vec![path("photo.raw")]);
}

// ---- what a pass costs (review M2) ----

/// A second machine's first sync of 5,000 files in 10 folders, then a pass with nothing to do. Counted in what grew
/// with the square of the folder's size before (the base's path index built from scratch per node, a folder listed
/// per file for the twin checks), not in time, which depends on the machine: each folder is listed a few times a
/// pass, and the index is built once.
#[tokio::test]
async fn a_pass_costs_about_the_size_of_the_folder_not_its_square() {
    use craft_cloud_proto::{ChunkId, ChunkRef, ChunkerParamsId, FileHash, Manifest, Name, NewVersion, NodeId, Op, VersionId};
    use craft_cloud_store::BlobKey;
    const FOLDERS: usize = 10;
    const PER_FOLDER: usize = 500;
    let server = Server::new().await;
    let ana = server.user("ana").await;
    // The library goes straight into the server (one content for every file), which is quicker than an upload.
    let data = b"x";
    let chunk = ChunkId::from_bytes(*blake3::hash(data).as_bytes());
    let manifest =
        Manifest::new(ChunkerParamsId::new("fastcdc-v1").unwrap(), 1, FileHash::from_bytes(*chunk.as_bytes()), vec![ChunkRef { id: chunk, len: 1 }])
            .unwrap();
    server.store.put(BlobKey::chunk(&chunk), data).await.unwrap();
    server.meta.add_chunk(ana.space, &chunk, 2, 0).await.unwrap();
    server.store.put(BlobKey::manifest(&manifest.id()), &manifest.encode()).await.unwrap();
    server.meta.add_manifest(ana.space, &manifest, 100, 0).await.unwrap();
    let device = server.meta.create_device(ana.id, "seed", &[7; 32], 0).await.unwrap();
    let root = server.meta.space_info(ana.space).await.unwrap().root;
    for f in 0..FOLDERS {
        let folder = NodeId::generate();
        let mut ops = vec![Op::Mkdir { node: folder, parent: root, name: Name::new(&format!("f{f}")).unwrap() }];
        for i in 0..PER_FOLDER {
            let version = NewVersion { id: VersionId::generate(), manifest: manifest.id(), mtime_ms: 1, exec: false, save: None };
            ops.push(Op::Create { node: NodeId::generate(), parent: folder, name: Name::new(&format!("{i}.txt")).unwrap(), version });
            if ops.len() == 1000 {
                server.meta.commit(ana.space, device, &std::mem::take(&mut ops), 0).await.unwrap();
            }
        }
        if !ops.is_empty() {
            server.meta.commit(ana.space, device, &ops, 0).await.unwrap();
        }
    }

    let mut b = machine_with(&server, &ana, "desktop", MemFs::new(), 5000).await;
    let report = b.sync_once().await.unwrap();
    assert_eq!(report.downloaded.len(), FOLDERS * PER_FOLDER);
    let (lists, builds) = (b.fs().lists(), b.state().base.path_builds());
    // Twin checks list each folder once (the root as well), and the push's scan lists them again.
    assert!(lists <= 3 * (FOLDERS as u64 + 1), "{lists} listings for {FOLDERS} folders");
    assert!(builds <= 2, "the path index was built {builds} times");

    let report = b.sync_once().await.unwrap();
    assert!(!report.changed(), "{report:?}");
    let lists = b.fs().lists() - lists;
    assert!(lists <= 2 * (FOLDERS as u64 + 1), "{lists} listings for {FOLDERS} folders");
    assert!(b.state().base.path_builds() <= builds, "a pass with nothing to do builds no index");
}

/// Emptying a live document (`live reset-doc`) is a generation of the live service only: the file sync never sees it.
/// A new space epoch here would make a machine that was offline while another edited and deleted files win with its
/// stale copies.
#[tokio::test]
async fn a_live_reset_doc_leaves_the_file_sync_alone() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "a.txt", b"v1");
    write(&mut a, "gone.txt", b"delete me");
    settle(&mut [&mut a, &mut b], 5).await;
    // B goes offline; A edits and deletes, and syncs.
    write(&mut a, "a.txt", b"v2 from the laptop");
    a.fs_mut().remove_file(&path("gone.txt")).unwrap();
    a.sync_once().await.unwrap();
    // The admin runs `live reset-doc` on the account's recents of an app.
    let app = craft_cloud_proto::live::AppName::new("wordcraft").unwrap();
    let doc = craft_cloud_proto::live::recents_doc(ana.space, &app);
    server.meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, 0).await.unwrap();
    let epoch = server.meta.space_info(ana.space).await.unwrap().epoch;
    server.meta.reset_live_doc(ana.space, doc).await.unwrap();
    assert_eq!(server.meta.space_info(ana.space).await.unwrap().epoch, epoch, "the file sync's epoch is untouched");
    settle(&mut [&mut b, &mut a], 5).await;
    assert_eq!(read(&a, "a.txt"), b"v2 from the laptop", "A's acked edit was overwritten by B's stale copy");
    assert!(!files(&a).iter().any(|(p, _)| p == "gone.txt"), "A's delete was undone");
    assert_eq!(read(&b, "a.txt"), b"v2 from the laptop");
}
