//! Behaviours every [`Fs`] must have, as test functions that run against any implementation: [`MemFs`] in this
//! crate's tests, the agent's real disk in its own. The engine's tests are only as good as `MemFs` is close to a
//! careful real disk, and the agent is only as safe as its disk keeps the same promises.
//!
//! Test support only (the `test-support` feature): the functions panic on a broken promise.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{ErrorKind, Write};

use crate::fs::{Fs, FsKind, MemFs, rel};

/// A modification time far from both the Unix epoch and now, so every filesystem can store it.
pub const T0: i64 = 1_700_000_000_000;

/// Writes land atomically: nothing shows until `finish_write`, an abandoned write leaves the old content, and a
/// replace is a new file (a new ID, as temp + rename gives). `exec_bit` says whether the filesystem keeps the
/// executable bit (not on Windows).
pub fn writes_are_atomic_and_give_a_new_identity<F: Fs>(fs: &mut F, exec_bit: bool) {
    let first = fs.write(&rel("a.txt"), b"one", false, T0).unwrap();
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"half").unwrap();
    assert_eq!(fs.read(&rel("a.txt")).unwrap(), b"one");
    drop(w);
    assert_eq!(fs.read(&rel("a.txt")).unwrap(), b"one");
    let second = fs.write(&rel("a.txt"), b"two", true, T0 + 5_000).unwrap();
    assert_ne!(first.id, second.id, "a replace is a new file, like temp + rename");
    assert!(second.mtime_ms > first.mtime_ms);
    assert_eq!(second.exec, exec_bit);
    assert_eq!(second.size, 3);
    assert_eq!(fs.stat(&rel("a.txt")).unwrap(), Some(FsKind::File(second)));
    assert_eq!(fs.read(&rel("a.txt")).unwrap(), b"two");
}

/// `read_at` returns fewer bytes only at the end of the file, and nothing past it.
pub fn ranges_stop_at_the_end_of_the_file<F: Fs>(fs: &mut F) {
    fs.write(&rel("f"), b"0123456789", false, T0).unwrap();
    assert_eq!(fs.read_at(&rel("f"), 2, 3).unwrap(), b"234");
    assert_eq!(fs.read_at(&rel("f"), 8, 10).unwrap(), b"89");
    assert_eq!(fs.read_at(&rel("f"), 20, 5).unwrap(), b"");
}

/// Files need their folder; `mkdir` never replaces; only empty folders are removed; the root never is.
pub fn folders_need_parents_and_only_empty_ones_go<F: Fs>(fs: &mut F) {
    assert!(fs.write(&rel("x/a.txt"), b"a", false, T0).is_err());
    fs.mkdir(&rel("x")).unwrap();
    assert_eq!(fs.mkdir(&rel("x")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    fs.write(&rel("x/a.txt"), b"a", false, T0).unwrap();
    assert!(fs.remove_dir(&rel("x")).is_err());
    assert!(fs.remove_file(&rel("x")).is_err());
    fs.remove_file(&rel("x/a.txt")).unwrap();
    fs.remove_dir(&rel("x")).unwrap();
    assert_eq!(fs.stat(&rel("x")).unwrap(), None);
    assert!(fs.remove_dir(&rel("")).is_err(), "the root never goes");
}

/// Renaming a folder carries everything in it along with its IDs; a rename never lands on something and never
/// moves a folder into itself.
pub fn renaming_a_folder_moves_everything_in_it_and_keeps_ids<F: Fs>(fs: &mut F) {
    fs.mkdir(&rel("x")).unwrap();
    fs.mkdir(&rel("x/y")).unwrap();
    let meta = fs.write(&rel("x/y/f"), b"f", false, T0).unwrap();
    let Some(FsKind::Dir { id }) = fs.stat(&rel("x")).unwrap() else { panic!("x is a folder") };
    fs.rename(&rel("x"), &rel("z")).unwrap();
    assert_eq!(fs.stat(&rel("z")).unwrap(), Some(FsKind::Dir { id }));
    assert_eq!(fs.stat(&rel("z/y/f")).unwrap(), Some(FsKind::File(meta)));
    assert_eq!(fs.stat(&rel("x")).unwrap(), None);
    // Never onto something, never into itself.
    fs.write(&rel("g"), b"g", false, T0).unwrap();
    assert_eq!(fs.rename(&rel("g"), &rel("z")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert!(fs.rename(&rel("z"), &rel("z/y/inner")).is_err());
    assert_eq!(fs.read(&rel("g")).unwrap(), b"g");
}

/// A rename onto an existing file or folder fails and leaves both alone.
pub fn renames_never_replace<F: Fs>(fs: &mut F) {
    fs.write(&rel("a"), b"a", false, T0).unwrap();
    fs.write(&rel("b"), b"b", false, T0).unwrap();
    fs.mkdir(&rel("d")).unwrap();
    fs.mkdir(&rel("e")).unwrap();
    assert_eq!(fs.rename(&rel("a"), &rel("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs.rename(&rel("a"), &rel("d")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs.rename(&rel("d"), &rel("e")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs.rename(&rel("d"), &rel("b")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs.read(&rel("a")).unwrap(), b"a");
    assert_eq!(fs.read(&rel("b")).unwrap(), b"b");
    assert!(matches!(fs.stat(&rel("d")).unwrap(), Some(FsKind::Dir { .. })));
    assert!(matches!(fs.stat(&rel("e")).unwrap(), Some(FsKind::Dir { .. })));
}

/// A file keeps its ID through a rename and gets a new one when it is replaced.
pub fn ids_survive_renames_and_change_on_replace<F: Fs>(fs: &mut F) {
    let first = fs.write(&rel("a"), b"a", false, T0).unwrap();
    assert!(first.id.is_some());
    fs.rename(&rel("a"), &rel("b")).unwrap();
    let Some(FsKind::File(moved)) = fs.stat(&rel("b")).unwrap() else { panic!("b is a file") };
    assert_eq!(moved.id, first.id);
    let replaced = fs.write(&rel("b"), b"b", false, T0).unwrap();
    assert_ne!(replaced.id, first.id);
}

/// Locks are a [`MemFs`] feature (a real disk has them from other programs): a locked file lists and stats,
/// but can't be read.
pub fn locked_files_cant_be_read_but_still_list(fs: &mut MemFs) {
    fs.write(&rel("busy.mov"), b"frames", false, 0).unwrap();
    fs.lock(&rel("busy.mov"));
    assert_eq!(fs.read(&rel("busy.mov")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert!(fs.read_at(&rel("busy.mov"), 0, 1).is_err());
    assert!(matches!(fs.stat(&rel("busy.mov")).unwrap(), Some(FsKind::File(_))));
    assert_eq!(fs.list(&rel("")).unwrap().len(), 1);
    fs.unlock(&rel("busy.mov"));
    assert_eq!(fs.read(&rel("busy.mov")).unwrap(), b"frames");
}

/// Holds are a [`MemFs`] feature: a held path can't be removed, renamed, replaced or listed, but still reads.
pub fn held_paths_cant_be_removed_renamed_replaced_or_listed(fs: &mut MemFs) {
    fs.mkdir(&rel("d")).unwrap();
    fs.write(&rel("open.doc"), b"text", false, 0).unwrap();
    fs.hold(&rel("open.doc"));
    fs.hold(&rel("d"));
    assert_eq!(fs.remove_file(&rel("open.doc")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs.rename(&rel("open.doc"), &rel("moved.doc")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs.write(&rel("open.doc"), b"new", false, 0).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs.list(&rel("d")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs.remove_dir(&rel("d")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs.read(&rel("open.doc")).unwrap(), b"text");
    fs.release(&rel("open.doc"));
    fs.remove_file(&rel("open.doc")).unwrap();
    fs.unlock_all();
    fs.remove_dir(&rel("d")).unwrap();
}
