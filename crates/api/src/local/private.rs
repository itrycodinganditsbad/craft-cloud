//! Is this folder, or this file, safe to keep and read secrets in?
//!
//! The agent's data folder holds the device token and `local-api.json` (the local API's port and bearer token). A file
//! that is mode `0600` still can be replaced by anyone who may write to the folder it is in, and a folder that was
//! made by someone else, is a link, or sits in a world-writable place without the sticky bit can be swapped for another
//! one. Whoever can do that can point apps at a listener of their own. So both the agent (before it writes there) and
//! the apps (before they read `local-api.json` and send its token) ask these checks first.
//!
//! **Unix**: the folder must be a real folder (not a link), owned by the user running the program, and writable by no
//! one else; its parents must not be world-writable unless they have the sticky bit (`/tmp`). A discovery file must be
//! a regular file, owned by the user, with no group or other permissions.
//!
//! **Windows**: nothing here. Windows has no owner or mode in `std`, and asking for the ACL needs calls that this
//! workspace does not make (`unsafe` is forbidden, and no crate that wraps them is in the build). The agent instead
//! keeps its data folder inside the user's `%LOCALAPPDATA%`, which only that user and administrators can write. A data
//! folder that was made by another account inside it, or an ACL changed by hand, is not detected.

use std::path::Path;

/// Checks the folder `dir` itself and its parents (see the module's text). `Err` says what is wrong, in words for a
/// person, naming the path.
pub fn check_private_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        unix::check_dir(dir)?;
        unix::check_parents(dir)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Checks only the folders above `dir`, for a folder that is about to be made.
pub fn check_private_parents(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        unix::check_parents(dir)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Checks a file that holds a secret (`local-api.json`): a regular file, not a link, owned by the user, with no
/// permissions for group or others.
pub fn check_private_file(file: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        unix::check_file(file)
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(())
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::OnceLock;

    const STICKY: u32 = 0o1000;

    /// The user id this process runs as. `std` has no call for it and no crate here wraps one, so it is read from what the
    /// process owns: `/proc/self` where there is one, else a file it makes for a moment in the temporary folder (a new
    /// file belongs to the user making it). `None` when neither works: the owner is then not checked, and the modes are.
    fn current_uid() -> Option<u32> {
        static UID: OnceLock<Option<u32>> = OnceLock::new();
        *UID.get_or_init(|| {
            if let Ok(meta) = std::fs::metadata("/proc/self") {
                return Some(meta.uid());
            }
            probe_uid()
        })
    }

    fn probe_uid() -> Option<u32> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let name = format!(".craft-cloud-uid-{}-{nanos}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(name);
        let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).ok()?;
        let uid = file.metadata().ok().map(|meta| meta.uid());
        drop(file);
        let _ = std::fs::remove_file(&path);
        uid
    }

    pub(super) fn check_dir(dir: &Path) -> Result<(), String> {
        let shown = dir.display();
        // Not followed: a link in the place of the folder is what an attacker would put there.
        let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{shown}: {e}"))?;
        if meta.file_type().is_symlink() {
            return Err(format!("{shown} is a link, not a folder; the agent's data folder must be a real folder of your own"));
        }
        if !meta.is_dir() {
            return Err(format!("{shown} is not a folder"));
        }
        if let Some(uid) = current_uid()
            && meta.uid() != uid
        {
            return Err(format!("{shown} belongs to another user (id {}); the data folder must be yours alone", meta.uid()));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{shown} can be written by other users (mode {:03o}); run `chmod 700 {shown}` or pick another data folder",
                meta.mode() & 0o777
            ));
        }
        Ok(())
    }

    /// Every folder above `dir` up to the home folder (or the root): none may be world-writable without the sticky bit,
    /// since anyone could then rename the folder below it and put another in its place.
    pub(super) fn check_parents(dir: &Path) -> Result<(), String> {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from).filter(|home| home.is_absolute());
        let absolute = std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned());
        for parent in absolute.ancestors().skip(1) {
            if parent.as_os_str().is_empty() {
                continue;
            }
            let Ok(meta) = std::fs::metadata(parent) else { continue };
            if meta.mode() & 0o002 != 0 && meta.mode() & STICKY == 0 {
                return Err(format!(
                    "{} is writable by every user and has no sticky bit, so anyone could swap the folders below it; \
                     the agent's data folder must not be inside it",
                    parent.display()
                ));
            }
            if home.as_deref() == Some(parent) {
                break;
            }
        }
        Ok(())
    }

    pub(super) fn check_file(file: &Path) -> Result<(), String> {
        let shown = file.display();
        let meta = std::fs::symlink_metadata(file).map_err(|e| format!("{shown}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{shown} is not a regular file (a link?)"));
        }
        if let Some(uid) = current_uid()
            && meta.uid() != uid
        {
            return Err(format!("{shown} belongs to another user (id {})", meta.uid()));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!("{shown} can be read or written by other users (mode {:03o})", meta.mode() & 0o777));
        }
        Ok(())
    }
}

// Two attributes, not `cfg(all(test, unix))`: clippy's `allow-unwrap-in-tests` only recognises a plain `cfg(test)`.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// A folder of the test's own in the temporary folder, removed when dropped (the crate has no `tempfile`).
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!("craft-cloud-api-private-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            mode(&path, 0o700);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // A test may leave a folder unreadable; put it right so it can go.
            for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
                let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o700));
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn private_dir(parent: &Path, name: &str) -> std::path::PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir(&dir).unwrap();
        mode(&dir, 0o700);
        dir
    }

    #[test]
    fn an_own_folder_without_write_for_others_is_accepted() {
        let root = Scratch::new();
        let dir = private_dir(root.path(), "data");
        assert_eq!(check_private_dir(&dir), Ok(()));
        // Group read and execute are not a danger to the contents' integrity.
        mode(&dir, 0o750);
        assert_eq!(check_private_dir(&dir), Ok(()));
    }

    #[test]
    fn a_folder_others_can_write_is_refused() {
        let root = Scratch::new();
        let dir = private_dir(root.path(), "data");
        for bad in [0o770, 0o707, 0o777, 0o720] {
            mode(&dir, bad);
            let refused = check_private_dir(&dir).unwrap_err();
            assert!(refused.contains("can be written by other users"), "{bad:o}: {refused}");
        }
    }

    #[test]
    fn a_link_or_a_file_in_place_of_the_folder_is_refused() {
        let root = Scratch::new();
        let real = private_dir(root.path(), "real");
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(check_private_dir(&link).unwrap_err().contains("is a link"));
        let file = root.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(check_private_dir(&file).unwrap_err().contains("not a folder"));
        assert!(check_private_dir(&root.path().join("missing")).is_err());
    }

    #[test]
    fn a_parent_everyone_can_write_to_is_refused_unless_it_is_sticky() {
        let root = Scratch::new();
        let open = private_dir(root.path(), "open");
        let dir = private_dir(&open, "data");
        mode(&open, 0o777);
        assert!(check_private_dir(&dir).unwrap_err().contains("no sticky bit"));
        assert!(check_private_parents(&dir).unwrap_err().contains("no sticky bit"));
        mode(&open, 0o1777);
        assert_eq!(check_private_dir(&dir), Ok(()));
        // Group-writable parents are common (shared project folders) and are not refused.
        mode(&open, 0o775);
        assert_eq!(check_private_dir(&dir), Ok(()));
        mode(&open, 0o700);
    }

    #[test]
    fn a_discovery_file_must_be_the_users_alone() {
        let root = Scratch::new();
        let file = root.path().join("local-api.json");
        std::fs::write(&file, b"{}").unwrap();
        for good in [0o600, 0o400] {
            mode(&file, good);
            assert_eq!(check_private_file(&file), Ok(()), "{good:o}");
        }
        for bad in [0o640, 0o604, 0o644, 0o666] {
            mode(&file, bad);
            assert!(check_private_file(&file).unwrap_err().contains("by other users"), "{bad:o}");
        }
        let link = root.path().join("link.json");
        symlink(&file, &link).unwrap();
        assert!(check_private_file(&link).unwrap_err().contains("not a regular file"));
        assert!(check_private_file(&root.path().join("none")).is_err());
    }
}
