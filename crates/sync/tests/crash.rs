//! The agent saves the engine's state after every pass. A crash during a pass loses that pass's bookkeeping:
//! the engine restarts from the state saved after the pass before, with the folder and the server as the crashed
//! pass left them. These scenarios came out of the simulation's crash action.

mod common;

use std::sync::atomic::Ordering;

use common::{Server, machine, machine_with, path, restart, save_checkpoints, settle};
use craft_cloud_sync::{Fs, MemFs};

fn write(m: &mut common::Machine, p: &str, data: &[u8]) {
    m.fs_mut().write(&path(p), data, false, 0).unwrap();
}

/// The server replaced a file with a new one under the same name; a crashed pass removed the old one here and
/// downloaded the new one. The restarted engine still tracks the old one at that name, sees a changed file and
/// a newcomer: the newcomer's content is already there, so it is adopted, not copied.
#[tokio::test]
async fn a_newcomer_a_crashed_pass_already_downloaded_is_adopted() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "b.txt", b"old");
    settle(&mut [&mut a, &mut b], 5).await;
    a.fs_mut().remove_file(&path("b.txt")).unwrap();
    a.sync_once().await.unwrap();
    write(&mut a, "b.txt", b"new file, same name");
    a.sync_once().await.unwrap();

    let saved = b.state().clone();
    let crashed = b.sync_once().await.unwrap();
    assert_eq!(crashed.downloaded, vec![path("b.txt")]);
    let mut b = restart(&b, saved, |_| {});
    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(b.fs().read(&path("b.txt")).unwrap(), b"new file, same name");
    assert_eq!(b.fs().files().len(), 1);
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(a.fs().files(), b.fs().files());
}

/// B renamed a file here while A renamed it on the server, then B made a new file at the old name. B's pass takes
/// the new file for the tracked one (an atomic save looks the same; the renamed file is held open, so the scan
/// can't settle it), moves it where the server has it and uploads it as the next version, then crashes before it
/// saves the state. The restarted engine finds the renamed file by its ID: it must not move it over the server's
/// place and replace it with the version already there. That version is adopted; the renamed file stays and goes
/// up as a new file.
#[tokio::test]
async fn a_file_found_by_id_after_a_crash_isnt_replaced_by_the_version_already_in_place() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    write(&mut a, "notes", b"the user's file");
    settle(&mut [&mut a, &mut b], 5).await;

    a.fs_mut().rename(&path("notes"), &path("a.txt")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("notes"), &path("b.txt")).unwrap();
    write(&mut b, "notes", b"a new file");
    b.fs_mut().lock(&path("b.txt"));
    b.fs_mut().hold(&path("b.txt"));
    let saved = b.state().clone();
    let crashed = b.sync_once().await.unwrap();
    assert_eq!(crashed.moved_locally, vec![(path("notes"), path("a.txt"))], "{crashed:?}");
    let mut b = restart(&b, saved, |_| {});
    b.fs_mut().unlock_all();

    let report = b.sync_once().await.unwrap();
    assert!(report.conflict_copies.is_empty(), "{report:?}");
    assert_eq!(b.fs().read(&path("b.txt")).unwrap(), b"the user's file");
    assert_eq!(b.fs().read(&path("a.txt")).unwrap(), b"a new file");
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(a.fs().files(), b.fs().files());
    assert_eq!(a.fs().files().len(), 2);
}

/// A crashed pass moved a file where the server had moved it; later the server deleted it, and a newcomer that
/// clashes by case took the file's old name. The restarted engine finds the file by its ID and sends the move
/// again, which the server refuses (the node is gone). That must not repeat forever: the node is forgotten and
/// the file goes up as a new one.
#[tokio::test]
async fn a_refused_move_of_a_node_the_server_deleted_doesnt_repeat_forever() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("x")).unwrap();
    write(&mut a, "a.txt", b"moved, then deleted there");
    write(&mut a, "c.txt", b"renamed here");
    settle(&mut [&mut a, &mut b], 5).await;

    // A moves a.txt into x. B renames c.txt to A.TXT and syncs: its pass moves a.txt into x locally and pushes
    // its rename, then B crashes before it saves the state.
    a.fs_mut().rename(&path("a.txt"), &path("x/b.txt")).unwrap();
    a.sync_once().await.unwrap();
    b.fs_mut().rename(&path("c.txt"), &path("A.TXT")).unwrap();
    let saved = b.state().clone();
    b.sync_once().await.unwrap();
    assert!(b.fs().stat(&path("x/b.txt")).unwrap().is_some());
    let mut b = restart(&b, saved, |_| {});

    // A deletes the moved file; on B a new a.txt appears (a case twin of the tracked A.TXT).
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("x/b.txt")).unwrap();
    a.sync_once().await.unwrap();
    write(&mut b, "a.txt", b"newcomer");

    for _ in 0..3 {
        b.sync_once().await.unwrap();
    }
    let report = b.sync_once().await.unwrap();
    assert!(report.refused.is_empty(), "the refused move repeats: {report:?}");
    settle(&mut [&mut a, &mut b], 10).await;
    // Nothing was lost: the content B still had is a file on both machines.
    for m in [&a, &b] {
        assert!(m.fs().files().values().any(|d| d == b"moved, then deleted there"));
    }
}

/// Two machines with `y/a.txt` and padding (so the mass-delete guard doesn't hide a wrong delete).
async fn clip_setup() -> (Server, common::Machine, common::Machine) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    a.fs_mut().mkdir(&path("y")).unwrap();
    write(&mut a, "y/a.txt", b"clip");
    for i in 0..20 {
        write(&mut a, &format!("pad{i}.txt"), b"pad");
    }
    settle(&mut [&mut a, &mut b], 5).await;
    (server, a, b)
}

/// Seed 34673. The laptop's clip became a link, the desktop renamed it, the laptop renamed its folder and put the
/// clip back (a new file ID). The laptop's pass can't find the renamed clip, takes it for deleted here and deletes
/// it on the server; it dies right after that commit applies, before the answer. Restarted from the state saved
/// before the pass, the engine would find the put-back clip at the old place and remove it for the server's delete
/// (its own). The state saved at the checkpoint after the server's changes were applied already has the clip at
/// the server's place, where nothing is: the clip that is really there goes up again as a new file, and no
/// machine loses it.
#[tokio::test]
async fn a_delete_the_crashed_pass_sent_doesnt_remove_the_file_after_a_restart() {
    let (_server, mut a, mut b) = clip_setup().await;
    a.fs_mut().make_link(&path("y/a.txt")).unwrap();
    b.fs_mut().rename(&path("y/a.txt"), &path("y/c.psd")).unwrap();
    b.sync_once().await.unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().rename(&path("y"), &path("x")).unwrap();
    a.fs_mut().remove_file(&path("x/a.txt")).unwrap();
    write(&mut a, "x/a.txt", b"clip");

    let saved = save_checkpoints(&mut a);
    a.transport().faults.die_on_commit.store(1, Ordering::Relaxed);
    assert!(a.sync_once().await.is_err());
    let state = saved.lock().unwrap().clone();
    let mut a = restart(&a, state, |_| {});
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(a.fs().files(), b.fs().files());
    for m in [&a, &b] {
        assert!(m.fs().files().values().any(|d| d == b"clip"), "{:?}", m.fs().files().keys().collect::<Vec<_>>());
    }
}

/// Seed 112337's no-pass order, with a crash: the laptop's folder `y` was a link, is a real folder again holding a
/// link `a.txt`, and is renamed to `x` before any pass saw it. The pass holds the delete of `y/a.txt` behind
/// `x/a.txt` (a link new since the last finished push, with the node's name) and creates `x` on the server, then dies
/// right after that commit applies, before the answer. The hold was saved before the commit went, and the push never
/// finished, so the restarted engine still counts the link as new: the node behind it still isn't deleted.
#[tokio::test]
async fn a_held_delete_survives_a_crash_after_the_commit_that_tracks_the_links_folder() {
    let (_server, mut a, mut b) = clip_setup().await;
    a.fs_mut().make_link(&path("y")).unwrap();
    a.sync_once().await.unwrap();
    a.fs_mut().remove_file(&path("y")).unwrap();
    a.fs_mut().mkdir(&path("y")).unwrap();
    a.fs_mut().make_link(&path("y/a.txt")).unwrap();
    a.fs_mut().rename(&path("y"), &path("x")).unwrap();

    let saved = save_checkpoints(&mut a);
    a.transport().faults.die_on_commit.store(1, Ordering::Relaxed);
    assert!(a.sync_once().await.is_err());
    let state = saved.lock().unwrap().clone();
    let mut a = restart(&a, state, |_| {});
    for _ in 0..3 {
        let report = a.sync_once().await.unwrap();
        assert!(!report.removed_remotely.contains(&path("y/a.txt")), "{report:?}");
        assert_eq!(report.held_deletes, vec![(path("y/a.txt"), vec![path("x/a.txt")])], "{report:?}");
        b.sync_once().await.unwrap();
        assert_eq!(b.fs().read(&path("y/a.txt")).unwrap(), b"clip", "the desktop keeps it");
    }
}

/// Seed 59494. The desktop deletes A.TXT and dies right after that commit applies. The laptop had edited it, so its
/// edit wins and brings the node back at a new version. Meanwhile the desktop's user puts the old file back. The
/// restarted desktop must not take the brought-back node for the one it tracks (its delete went through): the node
/// arrives as new, and the file the user put back is kept (as a conflict copy), never replaced.
#[tokio::test]
async fn a_delete_the_crashed_pass_sent_stays_its_own_when_an_edit_brings_the_node_back() {
    let (_server, mut a, mut b) = clip_setup().await;
    write(&mut a, "A.TXT", b"the original");
    settle(&mut [&mut a, &mut b], 5).await;
    write(&mut a, "A.TXT", b"edited on the laptop");

    b.fs_mut().remove_file(&path("A.TXT")).unwrap();
    let saved = save_checkpoints(&mut b);
    b.transport().faults.die_on_commit.store(1, Ordering::Relaxed);
    assert!(b.sync_once().await.is_err());
    let state = saved.lock().unwrap().clone();
    let mut b = restart(&b, state, |_| {});
    a.sync_once().await.unwrap();
    write(&mut b, "A.TXT", b"the original");

    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(a.fs().files(), b.fs().files());
    for m in [&a, &b] {
        assert_eq!(m.fs().read(&path("A.TXT")).unwrap(), b"edited on the laptop");
        assert!(m.fs().files().values().any(|d| d == b"the original"), "{:?}", m.fs().files().keys().collect::<Vec<_>>());
    }
}

/// Seed 34283. The desktop deletes `notes` and renames its tracked conflict copy to `notes`. Its pass deletes both
/// nodes on the server (the file at `notes` is the copy, by its ID, so both look gone) and dies right after that
/// commit applies. The laptop edits `notes` meanwhile, so the edit brings that node back. The restarted desktop
/// moves its `notes` (the copy's content) aside for the laptop's edit; the copy's old place is free on disk, but
/// the copy's node still has it. The deletes are its own (written down before the commit went), so that node is
/// let go first, and a conflict name a tracked node has is never used anyway: the copy's content is never removed.
#[tokio::test]
async fn a_conflict_copy_never_lands_where_a_node_deleted_by_a_crashed_pass_was() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let mut a = machine(&server, &ana, "laptop").await;
    let mut b = machine(&server, &ana, "desktop").await;
    let copy = "notes (conflict, desktop, 2026-10-11 14-03)";
    write(&mut a, "notes", b"notes");
    write(&mut a, copy, b"the copy");
    for i in 0..20 {
        write(&mut a, &format!("pad{i}.txt"), b"pad");
    }
    settle(&mut [&mut a, &mut b], 5).await;

    b.fs_mut().remove_file(&path("notes")).unwrap();
    b.fs_mut().rename(&path(copy), &path("notes")).unwrap();
    let saved = save_checkpoints(&mut b);
    b.transport().faults.die_on_commit.store(1, Ordering::Relaxed);
    assert!(b.sync_once().await.is_err());
    let state = saved.lock().unwrap().clone();
    let mut b = restart(&b, state, |_| {});

    write(&mut a, "notes", b"notes, edited");
    a.sync_once().await.unwrap();
    settle(&mut [&mut a, &mut b], 5).await;
    assert_eq!(a.fs().files(), b.fs().files());
    for m in [&a, &b] {
        assert!(m.fs().files().values().any(|d| d == b"the copy"), "{:?}", m.fs().files().keys().collect::<Vec<_>>());
        assert_eq!(m.fs().read(&path("notes")).unwrap(), b"notes, edited");
    }
}

/// The desktop lands downloads in steps (Windows: the old file moved aside, the new one put in place, the old one
/// removed) and dies between two of them. The restarted engine puts things right before it looks at the folder:
/// the old file back if the new one isn't in place, the moved file removed if it is. Nothing is left over, nothing
/// is copied, and the edit arrives.
#[tokio::test]
async fn a_landing_cut_short_between_its_steps_is_put_right() {
    for steps in 0..4 {
        let server = Server::new().await;
        let ana = server.user("ana").await;
        let mut a = machine(&server, &ana, "laptop").await;
        let mut b = machine_with(&server, &ana, "desktop", MemFs::new().without_swap(), 50).await;
        write(&mut a, "notes", b"v1");
        settle(&mut [&mut a, &mut b], 5).await;
        write(&mut a, "notes", b"v2");
        a.sync_once().await.unwrap();

        let saved = save_checkpoints(&mut b);
        let dead = b.transport().faults.dead.clone();
        b.fs_mut().die_after(steps, dead);
        let _ = b.sync_once().await;
        let state = saved.lock().unwrap().clone();
        let mut b = restart(&b, state, |_| {});
        let names: Vec<String> = b.fs().files().into_keys().map(|p| p.to_string()).collect();
        assert!(steps != 1 || names.iter().any(|n| n.ends_with(".cc-aside")), "cut after moving aside: {names:?}");
        settle(&mut [&mut a, &mut b], 5).await;
        assert_eq!(a.fs().files(), b.fs().files(), "after {steps} steps");
        assert_eq!(b.fs().read(&path("notes")).unwrap(), b"v2");
        assert_eq!(b.fs().files().len(), 1, "after {steps} steps: {:?}", b.fs().files().keys().collect::<Vec<_>>());
    }
}

/// The desktop dies right after moving `dir/notes` aside for a download (a landing in steps), and the user renames
/// `dir` to `dir2` before the agent runs again. Recovery finds the moved file by its folder's ID and puts it back
/// under its own name there, so the server sees a folder move, never the file renamed to its `.cc-aside` name; the
/// edit then arrives at `dir2/notes`. With `dir2/notes` taken meanwhile (`clash`), the moved file gets a conflict
/// name instead, and the user's new file stays.
#[tokio::test]
async fn a_folder_renamed_after_a_crash_mid_landing_gets_its_file_back_under_its_name() {
    for clash in [false, true] {
        let server = Server::new().await;
        let ana = server.user("ana").await;
        let mut a = machine(&server, &ana, "laptop").await;
        let mut b = machine_with(&server, &ana, "desktop", MemFs::new().without_swap(), 50).await;
        b.fs_mut().mkdir(&path("dir")).unwrap();
        write(&mut b, "dir/notes", b"v1");
        settle(&mut [&mut a, &mut b], 5).await;
        write(&mut a, "dir/notes", b"v2");
        a.sync_once().await.unwrap();

        let saved = save_checkpoints(&mut b);
        let dead = b.transport().faults.dead.clone();
        b.fs_mut().die_after(1, dead);
        let _ = b.sync_once().await;
        let state = saved.lock().unwrap().clone();
        let mut b = restart(&b, state, |_| {});
        let names: Vec<String> = b.fs().files().into_keys().map(|p| p.to_string()).collect();
        assert!(names.len() == 1 && names[0].ends_with(".cc-aside"), "cut after moving aside: {names:?}");
        b.fs_mut().rename(&path("dir"), &path("dir2")).unwrap();
        if clash {
            write(&mut b, "dir2/notes", b"new here");
        }
        settle(&mut [&mut a, &mut b], 5).await;
        assert_eq!(a.fs().files(), b.fs().files(), "clash: {clash}");
        let names: Vec<String> = b.fs().files().into_keys().map(|p| p.to_string()).collect();
        assert!(names.iter().all(|n| !n.contains("cc-aside")), "clash: {clash}: {names:?}");
        if clash {
            assert_eq!(names.len(), 2, "{names:?}");
            assert_eq!(b.fs().read(&path("dir2/notes")).unwrap(), b"new here");
            assert!(names.iter().any(|n| n.starts_with("dir2/notes (conflict")), "{names:?}");
        } else {
            assert_eq!(names, ["dir2/notes"]);
            assert_eq!(b.fs().read(&path("dir2/notes")).unwrap(), b"v2");
        }
    }
}
