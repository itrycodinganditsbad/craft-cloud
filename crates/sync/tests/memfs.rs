//! The in-memory filesystem behaves like a careful real disk: the engine's tests are only as good as it is. The
//! behaviours live in `craft_cloud_sync::conformance` so the agent's real disk runs the same ones.

use craft_cloud_sync::MemFs;
use craft_cloud_sync::conformance as c;

#[test]
fn writes_are_atomic_and_give_a_new_identity() {
    c::writes_are_atomic_and_give_a_new_identity(&mut MemFs::new(), true);
}

#[test]
fn ranges_stop_at_the_end_of_the_file() {
    c::ranges_stop_at_the_end_of_the_file(&mut MemFs::new());
}

#[test]
fn folders_need_parents_and_only_empty_ones_go() {
    c::folders_need_parents_and_only_empty_ones_go(&mut MemFs::new());
}

#[test]
fn renaming_a_folder_moves_everything_in_it_and_keeps_ids() {
    c::renaming_a_folder_moves_everything_in_it_and_keeps_ids(&mut MemFs::new());
}

#[test]
fn renames_never_replace() {
    c::renames_never_replace(&mut MemFs::new());
}

#[test]
fn ids_survive_renames_and_change_on_replace() {
    c::ids_survive_renames_and_change_on_replace(&mut MemFs::new());
}

#[test]
fn links_list_as_other_and_are_never_read() {
    use craft_cloud_sync::{Fs, FsKind, rel};
    let mut fs = MemFs::new();
    fs.mkdir(&rel("d")).unwrap();
    fs.write(&rel("d/f"), b"f", false, 0).unwrap();
    fs.make_link(&rel("d")).unwrap();
    assert_eq!(fs.stat(&rel("d")).unwrap(), Some(FsKind::Other));
    assert_eq!(fs.stat(&rel("d/f")).unwrap(), None, "a link replaces what was there");
    assert_eq!(fs.list(&rel("")).unwrap()[0].kind, FsKind::Other);
    assert!(fs.read(&rel("d")).is_err());
    assert!(fs.list(&rel("d")).is_err());
    fs.remove_file(&rel("d")).unwrap();
    assert!(fs.files().is_empty() && fs.dirs().is_empty());
}

#[test]
fn a_machine_dies_after_its_last_allowed_change() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use craft_cloud_sync::{Fs, rel};
    let mut fs = MemFs::new();
    let dead = Arc::new(AtomicBool::new(false));
    fs.die_after(2, dead.clone());
    fs.mkdir(&rel("a")).unwrap();
    fs.write(&rel("a/f"), b"f", false, 0).unwrap();
    assert!(!dead.load(Ordering::Relaxed));
    assert!(fs.mkdir(&rel("b")).is_err(), "the third change never happens");
    assert!(dead.load(Ordering::Relaxed));
    assert!(fs.list(&rel("")).is_err() && fs.read(&rel("a/f")).is_err(), "nothing works once it died");
    fs.revive();
    assert_eq!(fs.read(&rel("a/f")).unwrap(), b"f");
    assert_eq!(fs.dirs(), vec![rel("a")]);
}

#[test]
fn locked_files_cant_be_read_but_still_list() {
    c::locked_files_cant_be_read_but_still_list(&mut MemFs::new());
}

#[test]
fn held_paths_cant_be_removed_renamed_replaced_or_listed() {
    c::held_paths_cant_be_removed_renamed_replaced_or_listed(&mut MemFs::new());
}
