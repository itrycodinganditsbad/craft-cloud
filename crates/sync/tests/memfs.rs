//! The in-memory filesystem behaves like a careful real disk: the engine's tests are only as good as it is.

use std::io::{ErrorKind, Write};

use craft_cloud_sync::{Fs, FsKind, MemFs, rel};

#[test]
fn writes_are_atomic_and_give_a_new_identity() {
    let mut fs = MemFs::new();
    let first = fs.write(&rel("a.txt"), b"one", false, 0).unwrap();
    // Nothing is visible until the write finishes; an abandoned write leaves the old content.
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"half").unwrap();
    assert_eq!(fs.read(&rel("a.txt")).unwrap(), b"one");
    drop(w);
    assert_eq!(fs.read(&rel("a.txt")).unwrap(), b"one");
    let second = fs.write(&rel("a.txt"), b"two", true, 0).unwrap();
    assert_ne!(first.id, second.id, "a replace is a new file, like temp + rename");
    assert!(second.mtime_ms > first.mtime_ms);
    assert!(second.exec);
    assert_eq!(fs.stat(&rel("a.txt")).unwrap(), Some(FsKind::File(second)));
}

#[test]
fn ranges_stop_at_the_end_of_the_file() {
    let mut fs = MemFs::new();
    fs.write(&rel("f"), b"0123456789", false, 0).unwrap();
    assert_eq!(fs.read_at(&rel("f"), 2, 3).unwrap(), b"234");
    assert_eq!(fs.read_at(&rel("f"), 8, 10).unwrap(), b"89");
    assert_eq!(fs.read_at(&rel("f"), 20, 5).unwrap(), b"");
}

#[test]
fn folders_need_parents_and_only_empty_ones_go() {
    let mut fs = MemFs::new();
    assert!(fs.write(&rel("x/a.txt"), b"a", false, 0).is_err());
    fs.mkdir(&rel("x")).unwrap();
    assert_eq!(fs.mkdir(&rel("x")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    fs.write(&rel("x/a.txt"), b"a", false, 0).unwrap();
    assert!(fs.remove_dir(&rel("x")).is_err());
    assert!(fs.remove_file(&rel("x")).is_err());
    fs.remove_file(&rel("x/a.txt")).unwrap();
    fs.remove_dir(&rel("x")).unwrap();
    assert!(fs.remove_dir(&rel("")).is_err(), "the root never goes");
}

#[test]
fn renaming_a_folder_moves_everything_in_it_and_keeps_ids() {
    let mut fs = MemFs::new();
    fs.mkdir(&rel("x")).unwrap();
    fs.mkdir(&rel("x/y")).unwrap();
    let meta = fs.write(&rel("x/y/f"), b"f", false, 0).unwrap();
    let Some(FsKind::Dir { id }) = fs.stat(&rel("x")).unwrap() else { panic!("x is a folder") };
    fs.rename(&rel("x"), &rel("z")).unwrap();
    assert_eq!(fs.stat(&rel("z")).unwrap(), Some(FsKind::Dir { id }));
    assert_eq!(fs.stat(&rel("z/y/f")).unwrap(), Some(FsKind::File(meta)));
    assert_eq!(fs.stat(&rel("x")).unwrap(), None);
    // Never onto something, never into itself.
    fs.write(&rel("g"), b"g", false, 0).unwrap();
    assert_eq!(fs.rename(&rel("g"), &rel("z")).unwrap_err().kind(), ErrorKind::AlreadyExists);
    assert!(fs.rename(&rel("z"), &rel("z/y/inner")).is_err());
}

#[test]
fn locked_files_cant_be_read_but_still_list() {
    let mut fs = MemFs::new();
    fs.write(&rel("busy.mov"), b"frames", false, 0).unwrap();
    fs.lock(&rel("busy.mov"));
    assert_eq!(fs.read(&rel("busy.mov")).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert!(fs.read_at(&rel("busy.mov"), 0, 1).is_err());
    assert!(matches!(fs.stat(&rel("busy.mov")).unwrap(), Some(FsKind::File(_))));
    assert_eq!(fs.list(&rel("")).unwrap().len(), 1);
    fs.unlock(&rel("busy.mov"));
    assert_eq!(fs.read(&rel("busy.mov")).unwrap(), b"frames");
}
