//! A space where the account is a viewer: the engine downloads and applies the server's changes and never sends a
//! write. A local edit is kept (and reported), never overwritten and never lost; a role change in either direction is
//! followed cleanly, and a viewer's folder never reaches the server.

mod common;

use std::sync::atomic::Ordering;

use common::sim::Rng;
use common::{Machine, Server, path, settle, team_machine};
use craft_cloud_proto::SpaceId;
use craft_cloud_proto::api::SpaceRole;
use craft_cloud_sync::{Fs, READ_ONLY_EDITED, READ_ONLY_NEW, Report};

fn write(m: &mut Machine, p: &str, data: &[u8]) {
    m.fs_mut().write(&path(p), data, false, 0).unwrap();
}

fn read(m: &Machine, p: &str) -> Option<Vec<u8>> {
    m.fs().read(&path(p)).ok()
}

fn files(m: &Machine) -> Vec<(String, Vec<u8>)> {
    m.fs().files().into_iter().map(|(p, d)| (p.to_string(), d)).collect()
}

fn writes(m: &Machine) -> u64 {
    m.transport().faults.write_calls.load(Ordering::Relaxed)
}

fn kept(report: &Report, reason: &str) -> Vec<String> {
    report.skipped.iter().filter(|(_, why)| why == reason).map(|(p, _)| p.clone()).collect()
}

/// An editor (`ana`, the owner) and a viewer (`ben`) of one team space, each with a machine.
struct Team {
    server: Server,
    space: SpaceId,
    ben: craft_cloud_meta::UserRecord,
    ana: Machine,
    viewer: Machine,
}

async fn team() -> Team {
    let server = Server::new().await;
    let (ana, ben) = (server.user("ana").await, server.user("ben").await);
    let space = server.team_space("Studio", &ana).await;
    server.add_member(space, &ben, SpaceRole::Viewer).await;
    let ana_machine = team_machine(&server, &ana, space, "ana-pc").await;
    let viewer = team_machine(&server, &ben, space, "ben-pc").await;
    Team { server, space, ben, ana: ana_machine, viewer }
}

/// Ana puts a document and a folder with a file into the space; the viewer downloads them.
async fn seeded() -> Team {
    let mut t = team().await;
    write(&mut t.ana, "doc.txt", b"base");
    t.ana.fs_mut().mkdir(&path("Shots")).unwrap();
    write(&mut t.ana, "Shots/a.psd", b"layers");
    t.ana.sync_once().await.unwrap();
    let report = t.viewer.sync_once().await.unwrap();
    assert_eq!(report.downloaded.len(), 2, "{report:?}");
    assert!(!report.read_only, "nothing was written yet, so the engine has not met the refusal");
    assert_eq!(files(&t.ana), files(&t.viewer));
    t
}

#[tokio::test]
async fn a_viewers_edit_is_kept_and_never_uploaded() {
    let mut t = seeded().await;
    write(&mut t.viewer, "doc.txt", b"my edit");
    // The first push meets `read_only` once, learns it, and the same pass reports the edit as kept.
    let report = t.viewer.sync_once().await.unwrap();
    assert!(report.read_only && t.viewer.is_read_only(), "{report:?}");
    assert!(report.uploaded.is_empty() && report.removed_remotely.is_empty() && report.refused.is_empty(), "{report:?}");
    assert_eq!(kept(&report, READ_ONLY_EDITED), ["doc.txt"], "{report:?}");
    assert_eq!(read(&t.viewer, "doc.txt").unwrap(), b"my edit", "the edit is where it was");
    // The server's copy is unchanged: the owner's machine sees nothing new.
    let after = t.ana.sync_once().await.unwrap();
    assert!(!after.changed(), "{after:?}");
    assert_eq!(read(&t.ana, "doc.txt").unwrap(), b"base");

    // No upload loop: more passes make no write call at all, and say nothing new about the same edit.
    let calls = writes(&t.viewer);
    for _ in 0..5 {
        let again = t.viewer.sync_once().await.unwrap();
        assert!(again.read_only && !again.changed() && again.skipped.is_empty(), "{again:?}");
    }
    assert_eq!(writes(&t.viewer), calls, "a viewer's engine sends nothing once it knows its role");
    assert_eq!(read(&t.viewer, "doc.txt").unwrap(), b"my edit");
}

#[tokio::test]
async fn a_viewer_told_up_front_never_makes_a_write_call() {
    let mut t = seeded().await;
    t.viewer.set_read_only(true);
    write(&mut t.viewer, "doc.txt", b"my edit");
    write(&mut t.viewer, "new.txt", b"mine");
    let report = t.viewer.sync_once().await.unwrap();
    assert!(report.read_only);
    assert_eq!(writes(&t.viewer), 0);
    assert_eq!(kept(&report, READ_ONLY_EDITED), ["doc.txt"]);
    assert_eq!(kept(&report, READ_ONLY_NEW), ["new.txt"]);
}

#[tokio::test]
async fn new_local_files_stay_local_and_are_listed_when_the_set_changes() {
    let mut t = seeded().await;
    t.viewer.set_read_only(true);
    write(&mut t.viewer, "new.txt", b"mine");
    t.viewer.fs_mut().mkdir(&path("Mine")).unwrap();
    write(&mut t.viewer, "Mine/b.txt", b"also mine");
    let report = t.viewer.sync_once().await.unwrap();
    let mut listed = kept(&report, READ_ONLY_NEW);
    listed.sort();
    assert_eq!(listed, ["Mine/b.txt", "new.txt"]);
    assert!(report.uploaded.is_empty() && report.created_dirs.is_empty());
    // The same set is not listed again; a changed set is listed in full.
    assert!(t.viewer.sync_once().await.unwrap().skipped.is_empty());
    write(&mut t.viewer, "third.txt", b"3");
    let mut listed = kept(&t.viewer.sync_once().await.unwrap(), READ_ONLY_NEW);
    listed.sort();
    assert_eq!(listed, ["Mine/b.txt", "new.txt", "third.txt"]);
    // None of it is on the server.
    let after = t.ana.sync_once().await.unwrap();
    assert!(!after.changed(), "{after:?}");
    assert_eq!(files(&t.ana).len(), 2);
}

#[tokio::test]
async fn a_server_change_over_a_viewers_edit_keeps_both_and_uploads_nothing() {
    let mut t = seeded().await;
    t.viewer.set_read_only(true);
    write(&mut t.viewer, "doc.txt", b"viewer edit");
    t.viewer.sync_once().await.unwrap();
    write(&mut t.ana, "doc.txt", b"owner edit");
    t.ana.sync_once().await.unwrap();

    let report = t.viewer.sync_once().await.unwrap();
    assert_eq!(report.conflict_copies.len(), 1, "{report:?}");
    // The server's version lands under the original name; the edit is the conflict copy, kept and marked local-only.
    assert_eq!(read(&t.viewer, "doc.txt").unwrap(), b"owner edit");
    let copy = report.conflict_copies[0].to_string();
    assert!(copy.contains("(conflict"), "{copy}");
    assert_eq!(read(&t.viewer, &copy).unwrap(), b"viewer edit");
    assert_eq!(kept(&report, READ_ONLY_NEW), std::slice::from_ref(&copy));
    assert!(report.uploaded.is_empty());
    // It stays local: the owner never gets it, and the viewer stays quiet about it afterwards.
    let after = t.ana.sync_once().await.unwrap();
    assert!(!after.changed(), "{after:?}");
    assert!(t.ana.fs().files().keys().all(|p| !p.to_string().contains("conflict")));
    assert!(t.viewer.sync_once().await.unwrap().skipped.is_empty());
    assert_eq!(read(&t.viewer, &copy).unwrap(), b"viewer edit");
}

#[tokio::test]
async fn a_file_the_server_deleted_that_the_viewer_edited_is_kept() {
    let mut t = seeded().await;
    t.viewer.set_read_only(true);
    write(&mut t.viewer, "doc.txt", b"viewer edit");
    t.viewer.sync_once().await.unwrap();
    t.ana.fs_mut().remove_file(&path("doc.txt")).unwrap();
    t.ana.sync_once().await.unwrap();
    let report = t.viewer.sync_once().await.unwrap();
    assert_eq!(read(&t.viewer, "doc.txt").unwrap(), b"viewer edit", "the edit is never deleted: {report:?}");
    assert!(report.removed_locally.is_empty() && report.uploaded.is_empty());
    assert_eq!(writes(&t.viewer), 0);
}

#[tokio::test]
async fn a_local_delete_is_downloaded_again_with_a_notice() {
    let mut t = seeded().await;
    t.viewer.set_read_only(true);
    t.viewer.fs_mut().remove_file(&path("doc.txt")).unwrap();
    t.viewer.fs_mut().remove_file(&path("Shots/a.psd")).unwrap();
    t.viewer.fs_mut().remove_dir(&path("Shots")).unwrap();
    let report = t.viewer.sync_once().await.unwrap();
    let mut restored: Vec<String> = report.restored.iter().map(ToString::to_string).collect();
    restored.sort();
    assert_eq!(restored, ["Shots", "Shots/a.psd", "doc.txt"], "{report:?}");
    assert!(report.removed_remotely.is_empty() && report.pending_deletes.is_empty());
    assert_eq!(files(&t.ana), files(&t.viewer));
    assert_eq!(t.viewer.fs().dirs(), vec![path("Shots")]);
    assert_eq!(writes(&t.viewer), 0);
    // The server never saw a delete.
    let after = t.ana.sync_once().await.unwrap();
    assert!(!after.changed(), "{after:?}");
    assert!(!t.viewer.sync_once().await.unwrap().changed());
}

#[tokio::test]
async fn a_moved_file_comes_back_at_its_place_and_the_moved_one_stays_local() {
    let mut t = seeded().await;
    t.viewer.set_read_only(true);
    t.viewer.fs_mut().rename(&path("doc.txt"), &path("renamed.txt")).unwrap();
    let report = t.viewer.sync_once().await.unwrap();
    assert_eq!(report.restored, vec![path("doc.txt")], "{report:?}");
    assert_eq!(read(&t.viewer, "doc.txt").unwrap(), b"base");
    assert_eq!(read(&t.viewer, "renamed.txt").unwrap(), b"base");
    assert_eq!(kept(&report, READ_ONLY_NEW), ["renamed.txt"]);
    assert_eq!(writes(&t.viewer), 0);
}

#[tokio::test]
async fn a_role_raised_to_editor_uploads_the_local_files() {
    let mut t = seeded().await;
    write(&mut t.viewer, "doc.txt", b"viewer edit");
    write(&mut t.viewer, "new.txt", b"mine");
    let report = t.viewer.sync_once().await.unwrap();
    assert!(report.read_only);
    assert_eq!(writes(&t.viewer), 1, "one refused call, then none");
    t.viewer.sync_once().await.unwrap();

    t.server.set_role(t.space, &t.ben, SpaceRole::Editor).await;
    let report = t.viewer.sync_once().await.unwrap();
    assert!(!report.read_only && !t.viewer.is_read_only(), "{report:?}");
    let mut uploaded: Vec<String> = report.uploaded.iter().map(ToString::to_string).collect();
    uploaded.sort();
    assert_eq!(uploaded, ["doc.txt", "new.txt"], "the edit and the new file go up as ordinary changes: {report:?}");
    t.ana.sync_once().await.unwrap();
    assert_eq!(read(&t.ana, "doc.txt").unwrap(), b"viewer edit");
    assert_eq!(read(&t.ana, "new.txt").unwrap(), b"mine");
    settle(&mut [&mut t.ana, &mut t.viewer], 5).await;
    assert_eq!(files(&t.ana), files(&t.viewer));
}

#[tokio::test]
async fn a_role_lowered_to_viewer_stops_uploads_cleanly_and_keeps_every_edit() {
    let mut t = seeded().await;
    t.server.set_role(t.space, &t.ben, SpaceRole::Editor).await;
    write(&mut t.viewer, "doc.txt", b"edit while editor");
    let report = t.viewer.sync_once().await.unwrap();
    assert_eq!(report.uploaded, vec![path("doc.txt")]);
    assert!(!report.read_only);

    // Lowered between two passes: the next write is refused, the engine switches over in the same pass and nothing is lost.
    t.server.set_role(t.space, &t.ben, SpaceRole::Viewer).await;
    write(&mut t.viewer, "doc.txt", b"edit after the change");
    write(&mut t.viewer, "new.txt", b"mine");
    let report = t.viewer.sync_once().await.unwrap();
    assert!(report.read_only, "{report:?}");
    assert!(report.uploaded.is_empty() && report.refused.is_empty(), "{report:?}");
    assert_eq!(kept(&report, READ_ONLY_EDITED), ["doc.txt"]);
    assert_eq!(kept(&report, READ_ONLY_NEW), ["new.txt"]);
    assert_eq!(read(&t.viewer, "doc.txt").unwrap(), b"edit after the change");
    t.ana.sync_once().await.unwrap();
    assert_eq!(read(&t.ana, "doc.txt").unwrap(), b"edit while editor", "only what went up while the role allowed it");
    assert!(read(&t.ana, "new.txt").is_none());
    let calls = writes(&t.viewer);
    t.viewer.sync_once().await.unwrap();
    t.viewer.sync_once().await.unwrap();
    assert_eq!(writes(&t.viewer), calls);

    // And back: the same files go up.
    t.server.set_role(t.space, &t.ben, SpaceRole::Owner).await;
    let report = t.viewer.sync_once().await.unwrap();
    assert!(!report.read_only);
    t.ana.sync_once().await.unwrap();
    assert_eq!(read(&t.ana, "doc.txt").unwrap(), b"edit after the change");
    assert_eq!(read(&t.ana, "new.txt").unwrap(), b"mine");
}

/// Random edits, new files and deletes on a viewer's folder, and random edits on the owner's, over many seeds: the viewer
/// never makes a write call, nothing it wrote reaches the server, and nothing it wrote is lost from its disk.
#[tokio::test]
async fn simulated_viewers_never_send_a_write_and_never_lose_an_edit() {
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    for seed in 0..seeds {
        let mut rng = Rng::new(seed);
        let mut t = team().await;
        t.viewer.set_read_only(true);
        let names = ["a.txt", "b.txt", "x/c.txt", "x/y/d.txt"];
        // What the viewer wrote, by path, last.
        let mut mine: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
        for step in 0..40 {
            let name = names[rng.below(names.len())];
            let unique = format!("seed {seed} step {step} {}", rng.next()).into_bytes();
            let who = rng.below(4);
            let parent = name.rsplit_once('/').map(|(dir, _)| dir.to_owned());
            let m = if who == 0 { &mut t.ana } else { &mut t.viewer };
            if let Some(dir) = &parent {
                let mut at = String::new();
                for part in dir.split('/') {
                    at = if at.is_empty() { part.to_owned() } else { format!("{at}/{part}") };
                    let _ = m.fs_mut().mkdir(&path(&at));
                }
            }
            match rng.below(5) {
                0 | 1 => {
                    write(m, name, &unique);
                    if who != 0 {
                        mine.insert(name.to_owned(), unique);
                    }
                }
                2 => {
                    let _ = m.fs_mut().remove_file(&path(name));
                    if who != 0 {
                        mine.remove(name);
                    }
                }
                3 => {
                    t.ana.sync_once().await.unwrap();
                }
                _ => {
                    t.viewer.sync_once().await.unwrap();
                }
            }
        }
        for _ in 0..4 {
            t.ana.sync_once().await.unwrap();
            t.viewer.sync_once().await.unwrap();
        }
        assert_eq!(writes(&t.viewer), 0, "seed {seed}: the viewer made a write call");
        // Nothing the viewer wrote is on the server.
        let on_server: Vec<Vec<u8>> = t.ana.fs().files().into_values().collect();
        for (name, data) in &mine {
            let still_here = t.viewer.fs().files().values().any(|d| d == data);
            let reached_server = on_server.iter().any(|d| d == data);
            // Content the viewer wrote last at a path is on its disk (as the file, or as a conflict copy of it) unless the
            // owner's later change replaced the file after the viewer's pass did not see it: then it is a conflict copy.
            assert!(still_here, "seed {seed}: the viewer's content of {name} is gone");
            assert!(!reached_server, "seed {seed}: the viewer's content of {name} reached the server");
        }
    }
}
