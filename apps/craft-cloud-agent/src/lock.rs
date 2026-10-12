//! One agent at a time: a command that changes anything (`login`, `sync`, `logout`) holds an exclusive lock on
//! `<data dir>/agent.lock` and, once the folder exists, on `<folder>/.craft-cloud/lock` for as long as it runs.
//! Two agents on one data folder would overwrite each other's state; two on one synced folder would race for
//! the same files (and one would clean the other's temporary files away).
//!
//! The locks are the OS's advisory file locks (`flock` on Unix, `LockFileEx` on Windows), released when the
//! process ends, even by a crash, so a stale lock never blocks the next run. A filesystem that doesn't support
//! locking (some network shares; NFS without `lockd` answers `ENOLCK` or `EOPNOTSUPP`) can't be protected this way: the command goes ahead with a printed warning.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

pub const DATA_LOCK: &str = "agent.lock";
pub const FOLDER_LOCK: &str = "lock";

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    _file: Option<File>,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another craft-cloud-agent is running for this account or folder ({0} is locked); wait for it to finish")]
    Busy(PathBuf),
    #[error("{0} is not a regular file (a link?); remove it and run the command again")]
    NotAFile(PathBuf),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

/// Takes the lock at `path` (creating the file). Returns the lock and, when the filesystem can't lock, a warning
/// to show (the lock is then not held).
///
/// A link (or anything but a regular file) at `path` is refused, never opened or followed: the lock file is never
/// created or truncated somewhere else. (The check and the open are separate calls, like the disk's link checks.)
pub fn acquire(path: &Path) -> Result<(Lock, Option<String>), LockError> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if !md.file_type().is_file() => return Err(LockError::NotAFile(path.to_owned())),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(LockError::Io { path: path.to_owned(), source }),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|source| LockError::Io { path: path.to_owned(), source })?;
    match file.try_lock() {
        Ok(()) => Ok((Lock { _file: Some(file) }, None)),
        Err(TryLockError::WouldBlock) => Err(LockError::Busy(path.to_owned())),
        Err(TryLockError::Error(e)) if cannot_lock(&e) => Ok((
            Lock { _file: None },
            Some(format!(
                "warning: {} can't be locked on this filesystem; make sure no other craft-cloud-agent runs at the same time",
                path.display()
            )),
        )),
        Err(TryLockError::Error(source)) => Err(LockError::Io { path: path.to_owned(), source }),
    }
}

/// Whether `e` says this filesystem can't lock at all (as opposed to the lock being held): `Unsupported`, and on
/// Unix the raw `ENOLCK` and `EOPNOTSUPP`/`ENOTSUP` that NFS without a lock daemon (`lockd`) and some FUSE or
/// network mounts answer with.
fn cannot_lock(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    if let Some(code) = e.raw_os_error() {
        use rustix::io::Errno;
        let found = Errno::from_raw_os_error(code);
        return [Errno::NOLCK, Errno::OPNOTSUPP, Errno::NOTSUP].contains(&found);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystems_that_cannot_lock_are_told_from_a_lock_that_is_held() {
        assert!(cannot_lock(&io::Error::from(io::ErrorKind::Unsupported)));
        assert!(!cannot_lock(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!cannot_lock(&io::Error::other("x")));
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            use rustix::io::Errno;
            for errno in [Errno::NOLCK, Errno::OPNOTSUPP, Errno::NOTSUP] {
                assert!(cannot_lock(&io::Error::from_raw_os_error(errno.raw_os_error())), "{errno}");
            }
            // EACCES/EAGAIN-style answers are real failures or a held lock, not "can't lock here".
            for errno in [Errno::ACCESS, Errno::AGAIN, Errno::BADF, Errno::IO] {
                assert!(!cannot_lock(&io::Error::from_raw_os_error(errno.raw_os_error())), "{errno}");
            }
        }
    }

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DATA_LOCK);
        let (first, warning) = acquire(&path).unwrap();
        assert!(warning.is_none());
        assert!(matches!(acquire(&path), Err(LockError::Busy(_))));
        drop(first);
        acquire(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_locks_place_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"keep").unwrap();
        let path = dir.path().join(DATA_LOCK);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(acquire(&path), Err(LockError::NotAFile(_))));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(acquire(&path), Err(LockError::NotAFile(_))));
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }
}
