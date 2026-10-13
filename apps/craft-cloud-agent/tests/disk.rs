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

/// The exchange where the system has one (Linux, macOS), else the steps; then the steps, forced.
#[test]
fn writes_land_only_on_what_was_checked() {
    let (_dir, mut fs) = disk();
    c::writes_land_only_on_what_was_checked(&mut fs);
    let (dir, fs) = disk();
    let mut fs = fs.without_swap();
    c::writes_land_only_on_what_was_checked(&mut fs);
    assert_eq!(temps(&fs), 0, "no landing record or temporary file is left: {:?}", std::fs::read_dir(fs.tmp_dir()).unwrap().collect::<Vec<_>>());
    drop(dir);
}

#[test]
fn kept_writes_land_later() {
    let (_dir, mut fs) = disk();
    c::kept_writes_land_later(&mut fs);
    assert_eq!(temps(&fs), 0);
}

/// A landing in steps whose put-back fails (the download can't move into place after the file there was moved
/// aside, and moving that one back hits a disk error too) keeps its landing record, and the next pass's recovery
/// puts the file back. Runs on every platform, so Windows CI covers the steps it always uses.
#[test]
fn a_failed_put_back_is_finished_by_recovery() {
    use craft_cloud_sync::LandError;
    let (dir, fs) = disk();
    let mut fs = fs.without_swap().failing_landings().failing_put_backs();
    let old = fs.write(&rel("a.txt"), b"the user's", false, T0).unwrap();
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"download").unwrap();
    let Err(LandError::Io(_)) = fs.finish_write(&rel("a.txt"), w, false, T0, Some(old)) else { panic!("landed") };
    let names = |p: &std::path::Path| -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        n.sort();
        n
    };
    let left = names(dir.path());
    assert!(!left.contains(&"a.txt".to_owned()) && left.iter().any(|n| n.ends_with(".cc-aside")), "{left:?}");
    assert!(names(&fs.tmp_dir()).iter().any(|n| n.ends_with(".landing")), "the record stays");
    // Until recovery: the moved file isn't listed, and the empty target is unsettled (never taken for deleted).
    assert_eq!(fs.list(&rel("")).unwrap().into_iter().map(|e| e.name).collect::<Vec<_>>(), [".craft-cloud"]);
    assert_eq!(fs.unsettled(), [rel("a.txt")]);
    let mut fs = DiskFs::new(dir.path());
    fs.recover().unwrap();
    assert!(fs.unsettled().is_empty());
    assert_eq!(names(dir.path()), [".craft-cloud", "a.txt"]);
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"the user's");
    assert_eq!(temps(&fs), 0);
}

/// A landing in steps whose last step fails (the moved file can't be removed) keeps its record, and until recovery
/// finishes it the moved file isn't listed, so the scan never uploads the old content as a new file.
#[test]
fn a_moved_file_whose_removal_failed_isnt_listed_until_recovery() {
    let (dir, fs) = disk();
    let mut fs = fs.without_swap().failing_aside_removals();
    let old = fs.write(&rel("a.txt"), b"old", false, T0).unwrap();
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"new").unwrap();
    fs.finish_write(&rel("a.txt"), w, false, T0, Some(old)).unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3, "the moved file is still on disk");
    let listed = |fs: &DiskFs| -> Vec<String> {
        let mut n: Vec<String> = fs.list(&rel("")).unwrap().into_iter().map(|e| e.name).collect();
        n.sort();
        n
    };
    assert_eq!(listed(&fs), [".craft-cloud", "a.txt"]);
    fs.recover().unwrap();
    assert_eq!(listed(&fs), [".craft-cloud", "a.txt"]);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "recovery removed it");
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"new");
    assert_eq!(temps(&fs), 0);
}

/// A crash right after the exchange (Linux, macOS) leaves what was at the target in the temporary folder. Cleaning
/// up interrupted writes never removes it; recovery removes it when it is the file the engine checked (the download
/// stays), and when it is anything else (an app saved between the check and the exchange) exchanges it back, so the
/// save is at the user's name again and the download goes. Nothing is lost either way, and no copy appears.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn a_crash_right_after_the_exchange_never_loses_what_came_out() {
    let names = |p: &std::path::Path| -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(p).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        n.sort();
        n
    };
    for app_saved in [false, true] {
        let (dir, fs) = disk();
        let mut fs = fs.stopping_after_exchange();
        let old = fs.write(&rel("a.txt"), b"the user's", false, T0).unwrap();
        // What the engine checked: the file there, or (an app saved meanwhile) an older one.
        let checked = if app_saved { craft_cloud_sync::FileMeta { mtime_ms: old.mtime_ms - 1_000, ..old } } else { old };
        let mut w = fs.begin_write(&rel("a.txt")).unwrap();
        w.write_all(b"download").unwrap();
        assert!(fs.finish_write(&rel("a.txt"), w, false, T0, Some(checked)).is_err(), "stops after the exchange");
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"download");

        // The next run: interrupted writes are cleaned up, then recovery.
        let mut fs = DiskFs::new(dir.path());
        assert_eq!(fs.clean_tmp().unwrap(), 0, "the file that came out isn't an interrupted write");
        fs.recover().unwrap();
        assert_eq!(temps(&fs), 0, "app saved: {app_saved}");
        let want: &[u8] = if app_saved { b"the user's" } else { b"download" };
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), want, "app saved: {app_saved}");
        assert_eq!(names(dir.path()), [".craft-cloud", "a.txt"], "app saved: {app_saved}");
    }
}

/// Two saves around a landing by exchange (Linux, macOS): one between the engine's check and the exchange, one
/// between the look at what came out and the exchange back. The newest save ends up at the user's name, the older
/// one next to it, and the download doesn't land.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn a_second_save_during_the_exchange_back_stays_at_the_name() {
    use craft_cloud_sync::LandError;
    let (dir, fs) = disk();
    let mut fs = fs.saving_before_exchange_back();
    let old = fs.write(&rel("a.txt"), b"the user's", false, T0).unwrap();
    let checked = craft_cloud_sync::FileMeta { mtime_ms: old.mtime_ms - 1_000, ..old };
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"download").unwrap();
    let Err(LandError::Changed(_)) = fs.finish_write(&rel("a.txt"), w, false, T0, Some(checked)) else { panic!("landed") };
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"a newer save");
    let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    assert_eq!(left.len(), 3, "{left:?}");
    let aside = left.iter().find(|n| n.ends_with(".cc-aside")).expect("the older save is next to it");
    assert_eq!(std::fs::read(dir.path().join(aside)).unwrap(), b"the user's");
    assert_eq!(temps(&fs), 0);
}

/// The same two saves, but the older one can't be moved next to the target: it stays in the temporary folder with
/// its swap record (the error says where), and the next pass's recovery moves it next to the target.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn a_save_that_cant_be_moved_aside_keeps_its_record_for_recovery() {
    use craft_cloud_sync::LandError;
    let (dir, fs) = disk();
    let mut fs = fs.saving_before_exchange_back().failing_rescues();
    let old = fs.write(&rel("a.txt"), b"the user's", false, T0).unwrap();
    let checked = craft_cloud_sync::FileMeta { mtime_ms: old.mtime_ms - 1_000, ..old };
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"download").unwrap();
    let Err(LandError::Io(e)) = fs.finish_write(&rel("a.txt"), w, false, T0, Some(checked)) else { panic!("not an error") };
    assert!(e.to_string().contains(".craft-cloud/tmp") && e.to_string().contains(".swap"), "{e}");
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"a newer save");
    assert_eq!(temps(&fs), 2, "the swap file and its record stay");
    let mut fs = DiskFs::new(dir.path());
    fs.recover().unwrap();
    assert_eq!(temps(&fs), 0);
    let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    left.sort();
    assert_eq!(left.len(), 3, "{left:?}");
    let aside = left.iter().find(|n| n.ends_with(".cc-aside")).expect("the older save is next to it");
    assert_eq!(std::fs::read(dir.path().join(aside)).unwrap(), b"the user's");
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"a newer save");
}

/// A look at what the exchange back brought out that fails decides nothing: the node is skipped with an error, the
/// swap record stays, and the next pass's recovery finds the download there and removes it.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn a_failed_look_after_the_exchange_back_keeps_the_record() {
    use craft_cloud_sync::LandError;
    let (dir, fs) = disk();
    let mut fs = fs.failing_looks_after_exchange_back();
    let old = fs.write(&rel("a.txt"), b"the user's", false, T0).unwrap();
    let checked = craft_cloud_sync::FileMeta { mtime_ms: old.mtime_ms - 1_000, ..old };
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"download").unwrap();
    let Err(LandError::Io(e)) = fs.finish_write(&rel("a.txt"), w, false, T0, Some(checked)) else { panic!("not an error") };
    assert!(e.to_string().contains("can't look"), "{e}");
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"the user's");
    assert_eq!(temps(&fs), 2, "the swap file and its record stay");
    let mut fs = DiskFs::new(dir.path());
    fs.recover().unwrap();
    assert_eq!(temps(&fs), 0);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "only the file and .craft-cloud");
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"the user's");
}

/// Windows: an app holds the file open without letting it be deleted or renamed (no `FILE_SHARE_DELETE`). The
/// download can't replace it: it comes back as in use by another program, is kept, and lands once the app lets go.
#[cfg(windows)]
#[test]
fn a_file_an_app_holds_open_is_in_use_and_the_kept_download_lands_later() {
    use std::os::windows::fs::OpenOptionsExt;

    use craft_cloud_sync::LandError;
    let (dir, mut fs) = disk();
    let old = fs.write(&rel("open.doc"), b"old", false, T0).unwrap();
    // FILE_SHARE_READ | FILE_SHARE_WRITE: no FILE_SHARE_DELETE.
    let held = std::fs::OpenOptions::new().read(true).write(true).share_mode(0x1 | 0x2).open(dir.path().join("open.doc")).unwrap();
    let mut w = fs.begin_write(&rel("open.doc")).unwrap();
    w.write_all(b"the server's version").unwrap();
    let Err(LandError::InUse(w, why)) = fs.finish_write(&rel("open.doc"), w, false, T0, Some(old)) else {
        panic!("replaced a file an app holds open")
    };
    assert!(why.to_string().contains("in use by another program"), "{why}");
    let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names.len(), 2, "nothing moved aside is left: {names:?}");
    fs.keep(w, "n.v.m").unwrap();
    drop(held);
    let hash = craft_cloud_proto::FileHash::from_bytes(*blake3::hash(b"the server's version").as_bytes());
    let w = fs.take_kept("n.v.m", &hash).unwrap().unwrap();
    fs.finish_write(&rel("open.doc"), w, false, T0, Some(old)).unwrap();
    assert_eq!(std::fs::read(dir.path().join("open.doc")).unwrap(), b"the server's version");
    assert_eq!(temps(&fs), 0);
}

/// Windows: a read-only file is the user saying "don't change this". A remote edit never replaces it (nor clears the
/// attribute): it comes back as "read-only here, not replaced" with nothing moved aside, is kept, and lands once the
/// user clears the attribute.
#[cfg(windows)]
#[test]
fn a_read_only_file_is_never_replaced_and_the_kept_download_lands_later() {
    use craft_cloud_sync::LandError;
    let (dir, mut fs) = disk();
    let old = fs.write(&rel("final.doc"), b"signed off", false, T0).unwrap();
    let path = dir.path().join("final.doc");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms.clone()).unwrap();
    let mut w = fs.begin_write(&rel("final.doc")).unwrap();
    w.write_all(b"the server's version").unwrap();
    let Err(LandError::InUse(w, why)) = fs.finish_write(&rel("final.doc"), w, false, T0, Some(old)) else { panic!("replaced a read-only file") };
    assert!(why.to_string().contains("read-only here, not replaced"), "{why}");
    assert_eq!(std::fs::read(&path).unwrap(), b"signed off");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly(), "the attribute stays");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "nothing moved aside (the file and .craft-cloud)");
    fs.keep(w, "n.v.m").unwrap();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
    let hash = craft_cloud_proto::FileHash::from_bytes(*blake3::hash(b"the server's version").as_bytes());
    let w = fs.take_kept("n.v.m", &hash).unwrap().unwrap();
    fs.finish_write(&rel("final.doc"), w, false, T0, Some(old)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"the server's version");
    assert_eq!(temps(&fs), 0);
}

/// Windows: a read-only file is never removed either. The removal fails with the error the engine turns into a
/// skip ("read-only here, not removed"), the file and its attribute stay, and once the attribute is cleared it goes.
#[cfg(windows)]
#[test]
fn a_read_only_file_is_never_removed() {
    let (dir, mut fs) = disk();
    fs.write(&rel("final.doc"), b"signed off", false, T0).unwrap();
    let path = dir.path().join("final.doc");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms.clone()).unwrap();
    let e = fs.remove_file(&rel("final.doc")).unwrap_err();
    assert!(craft_cloud_sync::is_read_only(&e), "{e}");
    assert_eq!(e.to_string(), "read-only here, not removed");
    assert_eq!(std::fs::read(&path).unwrap(), b"signed off");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly(), "the attribute stays");
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
    fs.remove_file(&rel("final.doc")).unwrap();
    assert!(!path.exists());
}

/// A kept download that changed on disk since it was kept is hashed again when taken back, and dropped.
#[test]
fn a_kept_download_that_changed_on_disk_is_dropped() {
    let (_dir, mut fs) = disk();
    let mut w = fs.begin_write(&rel("a")).unwrap();
    w.write_all(b"the server's version").unwrap();
    fs.keep(w, "n.v.m").unwrap();
    std::fs::write(fs.tmp_dir().join("n.v.m.kept"), b"something else").unwrap();
    let hash = craft_cloud_proto::FileHash::from_bytes(*blake3::hash(b"the server's version").as_bytes());
    assert!(fs.take_kept("n.v.m", &hash).unwrap().is_none());
    assert!(fs.kept().unwrap().is_empty());
    assert_eq!(temps(&fs), 0, "dropped, not left behind");
}

/// Kept downloads and landing records live in the temporary folder, but cleaning it before a pass removes only the
/// leftovers of interrupted writes.
#[test]
fn cleaning_the_temporary_folder_keeps_kept_downloads_and_landing_records() {
    let (_dir, mut fs) = disk();
    let mut w = fs.begin_write(&rel("a")).unwrap();
    w.write_all(b"kept").unwrap();
    fs.keep(w, "n.v.m").unwrap();
    std::fs::write(fs.tmp_dir().join("0123.landing"), b"{}").unwrap();
    std::mem::forget(fs.begin_write(&rel("b")).unwrap());
    assert_eq!(fs.clean_tmp().unwrap(), 1);
    assert_eq!(fs.kept().unwrap(), ["n.v.m"]);
    assert!(fs.tmp_dir().join("0123.landing").exists());
    // Keys never leave the temporary folder.
    for key in ["", "../x", "a/b", ".hidden", "a b"] {
        let w = fs.begin_write(&rel("c")).unwrap();
        assert!(fs.keep(w, key).is_err(), "{key:?}");
        assert!(fs.take_kept(key, &craft_cloud_proto::FileHash::from_bytes([0; 32])).is_err(), "{key:?}");
    }
}

/// A file that may not be replaced (on Windows: one an app holds open; here: a folder without write permission)
/// is in use: the download comes back to be kept, and lands once it can, with nothing left over.
#[cfg(unix)]
#[test]
fn a_file_that_cant_be_replaced_is_in_use() {
    use std::os::unix::fs::PermissionsExt;

    use craft_cloud_sync::LandError;
    for swap in [true, false] {
        let (dir, fs) = disk();
        let mut fs = if swap { fs } else { fs.without_swap() };
        fs.mkdir(&rel("locked")).unwrap();
        let old = fs.write(&rel("locked/a.txt"), b"old", false, T0).unwrap();
        let folder = dir.path().join("locked");
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(folder.join("probe"), b"").is_ok() {
            // Permissions don't hold for this user (root): nothing to show here.
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let mut w = fs.begin_write(&rel("locked/a.txt")).unwrap();
        w.write_all(b"new").unwrap();
        let Err(LandError::InUse(w, _)) = fs.finish_write(&rel("locked/a.txt"), w, false, T0, Some(old)) else {
            panic!("replaced a file that may not be replaced")
        };
        assert_eq!(std::fs::read(folder.join("a.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1, "nothing moved aside is left");
        fs.keep(w, "k").unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let w = fs.take_kept("k", &craft_cloud_proto::FileHash::from_bytes(*blake3::hash(b"new").as_bytes())).unwrap().unwrap();
        fs.finish_write(&rel("locked/a.txt"), w, false, T0, Some(old)).unwrap();
        assert_eq!(std::fs::read(folder.join("a.txt")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
        assert_eq!(temps(&fs), 0);
    }
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
    let old = fs.write(&rel("a.txt"), b"old", false, T0).unwrap();
    let mut w = fs.begin_write(&rel("a.txt")).unwrap();
    w.write_all(b"new content").unwrap();
    assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"old");
    assert_eq!(temps(&fs), 1, "the new content waits in a temporary file");
    let meta = fs.finish_write(&rel("a.txt"), w, false, T0 + 1_000, Some(old)).unwrap();
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
    assert!(fs.finish_write(&rel("a"), w, false, T0, None).is_err());
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

#[cfg(unix)]
#[test]
fn the_agent_folders_a_download_lands_in_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut fs = DiskFs::new(dir.path());
    let mut writer = fs.begin_write(&rel("a")).unwrap();
    writer.write_all(b"partial").unwrap();
    for path in [dir.path().join(".craft-cloud"), fs.tmp_dir()] {
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700, "{}", path.display());
    }
}

#[cfg(unix)]
#[test]
fn a_linked_agent_folder_does_not_count_as_the_marker() {
    let (real, _) = disk();
    let dir = tempfile::tempdir().unwrap();
    // The link leads to a folder with a real marker in it.
    std::os::unix::fs::symlink(real.path().join(".craft-cloud"), dir.path().join(".craft-cloud")).unwrap();
    let mut fs = DiskFs::new(dir.path());
    assert!(fs.list(&rel("")).is_err());
    assert!(fs.write(&rel("a"), b"a", false, T0).is_err(), "nothing is written through the link");
    assert_eq!(std::fs::read_dir(real.path().join(".craft-cloud")).unwrap().count(), 1);
}
