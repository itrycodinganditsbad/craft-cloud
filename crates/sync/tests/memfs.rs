//! The in-memory filesystem behaves like a careful real disk: the engine's tests are only as good as it is. The
//! behaviours live in `craft_cloud_sync::conformance` so the agent's real disk runs the same ones.

use craft_cloud_sync::MemFs;
use craft_cloud_sync::conformance as c;

#[test]
fn writes_are_atomic_and_give_a_new_identity() {
    c::writes_are_atomic_and_give_a_new_identity(&mut MemFs::new(), true);
    c::writes_are_atomic_and_give_a_new_identity(&mut MemFs::new().without_exec_bit(), false);
}

#[test]
fn writes_land_only_on_what_was_checked() {
    c::writes_land_only_on_what_was_checked(&mut MemFs::new());
    c::writes_land_only_on_what_was_checked(&mut MemFs::new().without_swap());
}

#[test]
fn kept_writes_land_later() {
    c::kept_writes_land_later(&mut MemFs::new());
}

/// A held file ([`MemFs::hold`]) can't be replaced: the write comes back as in use and lands once it is let go.
#[test]
fn a_held_file_is_in_use_for_a_landing() {
    use std::io::Write;

    use craft_cloud_sync::{Fs, LandError, rel};
    for mut fs in [MemFs::new(), MemFs::new().without_swap()] {
        let meta = fs.write(&rel("open.doc"), b"old", false, 0).unwrap();
        fs.hold(&rel("open.doc"));
        let mut w = fs.begin_write(&rel("open.doc")).unwrap();
        w.write_all(b"new").unwrap();
        let Err(LandError::InUse(w, _)) = fs.finish_write(&rel("open.doc"), w, false, 0, Some(meta)) else { panic!("replaced a held file") };
        assert_eq!(fs.files().len(), 1);
        assert_eq!(fs.read(&rel("open.doc")).unwrap(), b"old");
        fs.release(&rel("open.doc"));
        fs.finish_write(&rel("open.doc"), w, false, 0, Some(meta)).unwrap();
        assert_eq!(fs.read(&rel("open.doc")).unwrap(), b"new");
    }
}

/// A landing in steps that a dying machine cuts short leaves the old file or the new one, never neither: the next
/// pass's [`Fs::recover`] puts the moved file back, or removes it once the new one is in place. If something took
/// the free name meanwhile, the moved file stays as it is, next to it.
#[test]
fn a_landing_in_steps_cut_short_is_put_right() {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use craft_cloud_sync::{Fs, rel};
    for n in 0..5 {
        let mut fs = MemFs::new().without_swap();
        let meta = fs.write(&rel("a.txt"), b"old", false, 0).unwrap();
        fs.die_after(n, Arc::new(AtomicBool::new(false)));
        let mut w = fs.begin_write(&rel("a.txt")).unwrap();
        w.write_all(b"new").unwrap();
        let landed = fs.finish_write(&rel("a.txt"), w, false, 0, Some(meta)).is_ok();
        fs.revive();
        fs.recover().unwrap();
        let files = fs.files();
        assert_eq!(files.len(), 1, "after {n} steps: {files:?}");
        let expected: &[u8] = if n >= 2 { b"new" } else { b"old" };
        assert_eq!(files.get(&rel("a.txt")).map(Vec::as_slice), Some(expected), "after {n} steps");
        assert_eq!(landed, n >= 3);
    }
    // The old file moved aside, then the machine died, then a user saved at the free name.
    let mut fs = MemFs::new().without_swap();
    let meta = fs.write(&rel("a.txt"), b"old", false, 0).unwrap();
    fs.die_after(1, Arc::new(AtomicBool::new(false)));
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"new").unwrap();
    assert!(fs.finish_write(&rel("a.txt"), w, false, 0, Some(meta)).is_err());
    fs.revive();
    fs.write(&rel("a.txt"), b"the user's", false, 0).unwrap();
    fs.recover().unwrap();
    let mut contents: Vec<Vec<u8>> = fs.files().into_values().collect();
    contents.sort();
    assert_eq!(contents, [b"old".to_vec(), b"the user's".to_vec()]);
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
