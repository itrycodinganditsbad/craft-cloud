//! The real disk keeps the promises `MemFs` makes (the shared conformance behaviours), plus what only a real
//! disk can show: temporary files, links, modes.

use std::io::Write;

use craft_cloud_agent::disk::DiskFs;
use craft_cloud_sync::conformance::{self as c, T0};
use craft_cloud_sync::{Fs, FsKind, rel};

/// A folder set up as the agent sets one up: with its root marker.
fn disk() -> (tempfile::TempDir, DiskFs) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".craft-cloud")).unwrap();
    std::fs::write(dir.path().join(".craft-cloud/root.json"), b"{}").unwrap();
    let fs = DiskFs::new(dir.path());
    (dir, fs)
}

/// The temporary files in the agent's folder.
fn temps(fs: &DiskFs) -> usize {
    std::fs::read_dir(fs.tmp_dir()).map(|d| d.count()).unwrap_or(0)
}

#[test]
fn writes_are_atomic_and_give_a_new_identity() {
    let (_dir, mut fs) = disk();
    c::writes_are_atomic_and_give_a_new_identity(&mut fs, cfg!(unix));
}

#[test]
fn ranges_stop_at_the_end_of_the_file() {
    let (_dir, mut fs) = disk();
    c::ranges_stop_at_the_end_of_the_file(&mut fs);
}

#[test]
fn folders_need_parents_and_only_empty_ones_go() {
    let (_dir, mut fs) = disk();
    c::folders_need_parents_and_only_empty_ones_go(&mut fs);
}

#[test]
fn renaming_a_folder_moves_everything_in_it_and_keeps_ids() {
    let (_dir, mut fs) = disk();
    c::renaming_a_folder_moves_everything_in_it_and_keeps_ids(&mut fs);
}

#[test]
fn renames_never_replace() {
    let (_dir, mut fs) = disk();
    c::renames_never_replace(&mut fs);
}

#[test]
fn ids_survive_renames_and_change_on_replace() {
    let (_dir, mut fs) = disk();
    c::ids_survive_renames_and_change_on_replace(&mut fs);
}

#[test]
fn a_replace_keeps_the_old_content_until_it_finishes() {
    let (dir, mut fs) = disk();
    fs.write(&rel("a.txt"), b"old", false, T0).unwrap();
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"new content").unwrap();
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"old");
    assert_eq!(temps(&fs), 1, "the new content waits in a temporary file");
    let meta = fs.finish_write(&rel("a.txt"), w, false, T0 + 1_000).unwrap();
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"new content");
    assert_eq!(meta.mtime_ms, T0 + 1_000);
    assert_eq!(meta.size, 11);
    assert_eq!(temps(&fs), 0);
}

#[test]
fn a_dropped_writer_leaves_no_temporary_file() {
    let (_dir, mut fs) = disk();
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"half").unwrap();
    assert_eq!(temps(&fs), 1);
    drop(w);
    assert_eq!(temps(&fs), 0);
    assert_eq!(fs.stat(&rel("a.txt")).unwrap(), None);
}

#[test]
fn a_failed_finish_leaves_no_temporary_file() {
    let (_dir, mut fs) = disk();
    let mut w = fs.begin_write(&rel("a")).unwrap();
    w.write_all(b"data").unwrap();
    // A folder took the name while the content was written.
    fs.mkdir(&rel("a")).unwrap();
    assert!(fs.finish_write(&rel("a"), w, false, T0).is_err());
    assert_eq!(temps(&fs), 0);
    assert!(matches!(fs.stat(&rel("a")).unwrap(), Some(FsKind::Dir { .. })));
}

#[test]
fn leftover_temporary_files_are_cleaned() {
    let (_dir, mut fs) = disk();
    let w = fs.begin_write(&rel("a")).unwrap();
    std::mem::forget(w); // as if the agent crashed mid-write
    assert_eq!(temps(&fs), 1);
    assert_eq!(fs.clean_tmp().unwrap(), 1);
    assert_eq!(temps(&fs), 0);
}

#[test]
fn the_agent_folder_is_listed_but_never_holds_a_synced_name() {
    let (_dir, mut fs) = disk();
    fs.write(&rel("a"), b"a", false, T0).unwrap();
    let names: Vec<String> = fs.list(&rel("")).unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&".craft-cloud".to_owned()), "the engine ignores it by name: {names:?}");
    assert!(names.contains(&"a".to_owned()));
}

#[test]
fn modification_times_and_the_exec_bit_land() {
    let (dir, mut fs) = disk();
    let meta = fs.write(&rel("run.sh"), b"#!/bin/sh\n", true, T0).unwrap();
    assert_eq!(meta.mtime_ms, T0);
    let Some(FsKind::File(stat)) = fs.stat(&rel("run.sh")).unwrap() else { panic!("a file") };
    assert_eq!(stat, meta);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o100, 0o100, "{mode:o}");
        assert!(meta.exec);
        let plain = fs.write(&rel("run.sh"), b"text", false, T0).unwrap();
        assert!(!plain.exec);
        assert_eq!(std::fs::metadata(dir.path().join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        assert!(!meta.exec, "Windows has no exec bit");
    }
}

#[cfg(unix)]
#[test]
fn a_replace_keeps_the_files_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut fs) = disk();
    let mode = |name: &str| std::fs::metadata(dir.path().join(name)).unwrap().permissions().mode() & 0o777;
    fs.write(&rel("secret.key"), b"k1", false, T0).unwrap();
    std::fs::set_permissions(dir.path().join("secret.key"), std::fs::Permissions::from_mode(0o600)).unwrap();
    fs.write(&rel("secret.key"), b"k2, a remote edit", false, T0).unwrap();
    assert_eq!(mode("secret.key"), 0o600);
    fs.write(&rel("secret.key"), b"k3", true, T0).unwrap();
    assert_eq!(mode("secret.key"), 0o700, "the exec bit follows read access");
}

#[test]
fn now_is_the_system_clock() {
    let (_dir, fs) = disk();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!((fs.now_ms() - now).abs() < 60_000);
}

#[cfg(unix)]
#[test]
fn links_are_other_and_never_followed() {
    let (dir, mut fs) = disk();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("file-link")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("dir-link")).unwrap();
    let listed = fs.list(&rel("")).unwrap();
    for name in ["file-link", "dir-link"] {
        let entry = listed.iter().find(|e| e.name == name).unwrap();
        assert_eq!(entry.kind, FsKind::Other, "{name}");
        assert_eq!(fs.stat(&rel(name)).unwrap(), Some(FsKind::Other));
    }
    assert!(fs.read(&rel("file-link")).is_err());
    assert!(fs.list(&rel("dir-link")).is_err());
    assert_eq!(fs.stat(&rel("dir-link/secret")).unwrap(), None);
    assert!(fs.read(&rel("dir-link/secret")).is_err());
    assert!(fs.write(&rel("dir-link/new"), b"x", false, T0).is_err());
    assert!(fs.mkdir(&rel("dir-link/new")).is_err());
    assert!(fs.remove_file(&rel("dir-link/secret")).is_err());
    assert!(fs.remove_dir(&rel("dir-link")).is_err());
    assert!(!outside.path().join("new").exists());
    assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"outside");
}

#[cfg(unix)]
#[test]
fn nothing_is_written_or_deleted_through_a_linked_temporary_folder() {
    let (dir, mut fs) = disk();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(elsewhere.path().join("keep.part"), b"not ours").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".craft-cloud/tmp")).unwrap();
    assert!(fs.clean_tmp().is_err());
    assert!(fs.write(&rel("a"), b"a", false, T0).is_err());
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 1);
    assert!(elsewhere.path().join("keep.part").exists());
}

#[cfg(not(target_vendor = "apple"))]
#[test]
fn names_that_need_normalising_are_other() {
    let (dir, fs) = disk();
    std::fs::write(dir.path().join("cafe\u{301}.txt"), b"x").unwrap();
    let listed = fs.list(&rel("")).unwrap();
    assert_eq!(listed.iter().find(|e| e.name == "cafe\u{301}.txt").unwrap().kind, FsKind::Other);
}

#[cfg(target_os = "linux")]
#[test]
fn names_that_arent_utf8_are_other() {
    use std::os::unix::ffi::OsStrExt;
    let (dir, fs) = disk();
    std::fs::write(dir.path().join(std::ffi::OsStr::from_bytes(b"bad\xff")), b"x").unwrap();
    let listed = fs.list(&rel("")).unwrap();
    let bad: Vec<_> = listed.iter().filter(|e| e.name != ".craft-cloud").collect();
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].kind, FsKind::Other);
}

#[test]
fn the_root_lists_only_while_its_marker_is_there() {
    let (dir, mut fs) = disk();
    fs.write(&rel("a"), b"a", false, T0).unwrap();
    fs.list(&rel("")).unwrap();
    // The folder is emptied (or the drive unmounted) in the middle of a pass: listing fails, it never reads as
    // empty.
    std::fs::remove_dir_all(dir.path().join(".craft-cloud")).unwrap();
    assert!(fs.list(&rel("")).is_err());
    std::fs::create_dir(dir.path().join(".craft-cloud")).unwrap();
    std::fs::create_dir(dir.path().join(".craft-cloud/root.json")).unwrap();
    assert!(fs.list(&rel("")).is_err(), "a folder in the marker's place doesn't count");
}

#[cfg(unix)]
#[test]
fn a_root_that_is_a_link_is_a_folder() {
    let (real, _) = disk();
    let holder = tempfile::tempdir().unwrap();
    let link = holder.path().join("synced");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    let mut fs = DiskFs::new(&link);
    assert!(matches!(fs.stat(&rel("")).unwrap(), Some(FsKind::Dir { .. })));
    fs.write(&rel("a"), b"a", false, T0).unwrap();
    fs.write(&rel("B"), b"b", false, T0).unwrap();
    let listed = fs.list(&rel("")).unwrap();
    assert!(matches!(listed.iter().find(|e| e.name == "a").unwrap().kind, FsKind::File(_)));
    // Top-level names have a real parent for the engine's twin checks (`case_twin`, `linked_twin`).
    assert!(matches!(fs.stat(&rel("a").parent().unwrap()).unwrap(), Some(FsKind::Dir { .. })));
    // Links inside the root stay links.
    std::os::unix::fs::symlink(real.path(), real.path().join("inner")).unwrap();
    assert_eq!(fs.stat(&rel("inner")).unwrap(), Some(FsKind::Other));
    // A missing root is nothing, not an error.
    assert_eq!(DiskFs::new(holder.path().join("gone")).stat(&rel("")).unwrap(), None);
}
