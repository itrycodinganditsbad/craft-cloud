//! Chunks claimed from another space instead of uploaded: an engine told it may claim from the account's personal space takes the
//! chunks that space has without sending their bytes, uploads the rest, and falls back to uploading everything when the server
//! gives it no such source. What it claimed is really in the space: another member downloads it.

mod common;

use std::sync::atomic::Ordering;

use common::{Machine, Server, claiming_machine, machine, path, team_machine};
use craft_cloud_proto::api::SpaceRole;
use craft_cloud_sync::Fs;

fn write(m: &mut Machine, p: &str, data: &[u8]) {
    m.fs_mut().write(&path(p), data, false, 0).unwrap();
}

fn uploads(m: &Machine) -> u64 {
    m.transport().faults.chunk_uploads.load(Ordering::Relaxed)
}

fn claims(m: &Machine) -> u64 {
    m.transport().faults.claims.load(Ordering::Relaxed)
}

#[tokio::test]
async fn chunks_the_personal_space_has_are_claimed_and_only_the_rest_is_uploaded() {
    let server = Server::new().await;
    let (ana, ben) = (server.user("ana").await, server.user("ben").await);
    let team = server.team_space("Studio", &ana).await;
    server.add_member(team, &ben, SpaceRole::Editor).await;

    // Ana's personal space has the brief; her team machine has the brief and a new file the personal space lacks.
    let mut personal = machine(&server, &ana, "ana-personal").await;
    write(&mut personal, "brief.txt", b"the brief, already in the personal space");
    personal.sync_once().await.unwrap();
    let mut mover = claiming_machine(&server, &ana, team, ana.space, "ana-team").await;
    write(&mut mover, "brief.txt", b"the brief, already in the personal space");
    write(&mut mover, "new.txt", b"only the team space gets this one");
    let report = mover.sync_once().await.unwrap();
    assert_eq!(report.uploaded.len(), 2, "both files are in the team space after the pass: {report:?}");
    assert_eq!(uploads(&mover), 1, "only the chunk no space had was uploaded");
    assert!(claims(&mover) >= 1);

    // What was claimed is really in the team space: another member downloads both files.
    let mut member = team_machine(&server, &ben, team, "ben-pc").await;
    let seen = member.sync_once().await.unwrap();
    assert_eq!(seen.downloaded.len(), 2, "{seen:?}");
    assert_eq!(member.fs().read(&path("brief.txt")).unwrap(), b"the brief, already in the personal space");
    assert_eq!(member.fs().read(&path("new.txt")).unwrap(), b"only the team space gets this one");
}

#[tokio::test]
async fn a_source_the_account_may_not_read_is_no_error_and_the_chunks_are_uploaded() {
    let server = Server::new().await;
    let (ana, ben) = (server.user("ana").await, server.user("ben").await);
    let team = server.team_space("Studio", &ana).await;
    server.add_member(team, &ben, SpaceRole::Editor).await;
    let mut personal = machine(&server, &ana, "ana-personal").await;
    write(&mut personal, "secret.txt", b"in ana's personal space only");
    personal.sync_once().await.unwrap();

    // Ben names ana's personal space as the source: the server answers as for a space that is not there, nothing is claimed,
    // and the file goes up by itself. The same bytes in ben's folder are his own, so uploading them leaks nothing.
    let mut thief = claiming_machine(&server, &ben, team, ana.space, "ben-team").await;
    write(&mut thief, "secret.txt", b"in ana's personal space only");
    let report = thief.sync_once().await.unwrap();
    assert_eq!(report.uploaded.len(), 1, "{report:?}");
    assert_eq!(uploads(&thief), 1, "uploaded, not claimed");
    assert!(claims(&thief) >= 1, "it did ask");
    // And the team space has it, from the upload.
    let mut other = team_machine(&server, &ana, team, "ana-pc").await;
    other.sync_once().await.unwrap();
    assert_eq!(other.fs().read(&path("secret.txt")).unwrap(), b"in ana's personal space only");
}

#[tokio::test]
async fn an_engine_not_told_to_claim_uploads_everything() {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    let team = server.team_space("Studio", &ana).await;
    let mut personal = machine(&server, &ana, "ana-personal").await;
    write(&mut personal, "brief.txt", b"the brief");
    personal.sync_once().await.unwrap();
    let mut plain = team_machine(&server, &ana, team, "ana-team").await;
    write(&mut plain, "brief.txt", b"the brief");
    plain.sync_once().await.unwrap();
    assert_eq!((uploads(&plain), claims(&plain)), (1, 0));
}
