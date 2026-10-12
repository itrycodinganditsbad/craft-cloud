//! The agent saves the engine's state after every pass. A crash during a pass loses that pass's bookkeeping:
//! the engine restarts from the state saved after the pass before, with the folder and the server as the crashed
//! pass left them. These scenarios came out of the simulation's crash action.

mod common;

use common::{Server, machine, path, restart, settle};
use craft_cloud_sync::Fs;

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
