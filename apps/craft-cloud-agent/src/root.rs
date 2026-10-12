//! The root marker, `<folder>/.craft-cloud/root.json` = `{ "space": …, "root": … }`.
//!
//! Written when a folder is set up for a space. Every pass checks it first: a folder whose marker is missing
//! (a drive that isn't mounted, a wiped or replaced folder, the wrong folder) or names another space stops the
//! pass before anything is read, so an empty folder is never taken for "delete everything".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use craft_cloud_proto::{NodeId, SpaceId};
use serde::{Deserialize, Serialize};

use crate::account::write_atomic;
use crate::disk::{AGENT_DIR, create_private_dir};

pub const MARKER_FILE: &str = "root.json";
/// A marker is a few dozen bytes; anything much bigger is not ours.
const MAX_MARKER_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootMarker {
    pub space: SpaceId,
    pub root: NodeId,
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("the synced folder {0} is missing or not a folder (a drive that isn't mounted?); nothing was synced")]
    FolderMissing(PathBuf),
    #[error(
        "{0} has no Craft Cloud marker (.craft-cloud/root.json): the folder was wiped or replaced, or it is not the synced folder. \
         Nothing was synced, so nothing was deleted on the server. To sync this folder as it is, run `logout`, then `login --adopt`."
    )]
    MarkerMissing(PathBuf),
    #[error("{0} is set up for another Craft Cloud space; nothing was synced")]
    OtherSpace(PathBuf),
    /// At login: the folder already has a marker for another space.
    #[error(
        "{} is already set up for another Craft Cloud space; nothing was set up. If that space no longer syncs here, \
         remove {} (only the agent's folder: your files stay), or choose another folder",
        .0.display(),
        .0.join(AGENT_DIR).display()
    )]
    SetUpForOtherSpace(PathBuf),
    #[error("{path}: the marker is damaged ({message}); nothing was synced")]
    Damaged { path: PathBuf, message: String },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

pub fn marker_path(folder: &Path) -> PathBuf {
    folder.join(AGENT_DIR).join(MARKER_FILE)
}

/// The marker in `folder`, if it has one.
pub fn read(folder: &Path) -> Result<Option<RootMarker>, RootError> {
    let path = marker_path(folder);
    let io_err = |source| RootError::Io { path: path.clone(), source };
    // `.craft-cloud/` must be a real folder: a link is never followed to a marker somewhere else.
    let agent = folder.join(AGENT_DIR);
    match fs::symlink_metadata(&agent) {
        Ok(md) if md.file_type().is_dir() => {}
        Ok(_) => return Err(RootError::Damaged { path: agent, message: "not a real folder (a link?)".into() }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RootError::Io { path: agent, source: e }),
    }
    let md = match fs::symlink_metadata(&path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(e)),
    };
    if !md.file_type().is_file() || md.len() > MAX_MARKER_BYTES {
        return Err(RootError::Damaged { path, message: "not a small regular file".into() });
    }
    let bytes = fs::read(&path).map_err(io_err)?;
    serde_json::from_slice(&bytes).map(Some).map_err(|e| RootError::Damaged { path, message: e.to_string() })
}

/// Writes the marker (creating `.craft-cloud/`).
pub fn write(folder: &Path, marker: &RootMarker) -> Result<(), RootError> {
    let dir = folder.join(AGENT_DIR);
    let io_err = |source| RootError::Io { path: dir.clone(), source };
    match fs::symlink_metadata(&dir) {
        Ok(md) if md.file_type().is_dir() => {}
        Ok(_) => return Err(RootError::Damaged { path: dir, message: "not a real folder (a link?); nothing was written through it".into() }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(folder).map_err(io_err)?;
            create_private_dir(&dir).map_err(io_err)?;
        }
        Err(e) => return Err(io_err(e)),
    }
    let path = marker_path(folder);
    let json = serde_json::to_vec_pretty(marker).map_err(|e| RootError::Damaged { path: path.clone(), message: e.to_string() })?;
    write_atomic(&path, &json, false).map_err(|source| RootError::Io { path, source })
}

/// Checks that `folder` is there and its marker names `expected`.
pub fn check(folder: &Path, expected: &RootMarker) -> Result<(), RootError> {
    if !folder.is_dir() {
        return Err(RootError::FolderMissing(folder.to_owned()));
    }
    match read(folder)? {
        None => Err(RootError::MarkerMissing(folder.to_owned())),
        Some(found) if found.space != expected.space || found.root != expected.root => Err(RootError::OtherSpace(folder.to_owned())),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_passes_only_with_its_own_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = RootMarker { space: SpaceId::generate(), root: NodeId::generate() };
        assert!(matches!(check(dir.path(), &marker), Err(RootError::MarkerMissing(_))));
        write(dir.path(), &marker).unwrap();
        check(dir.path(), &marker).unwrap();
        assert_eq!(read(dir.path()).unwrap(), Some(marker));
        let other = RootMarker { space: SpaceId::generate(), ..marker };
        assert!(matches!(check(dir.path(), &other), Err(RootError::OtherSpace(_))));
        assert!(matches!(check(&dir.path().join("gone"), &marker), Err(RootError::FolderMissing(_))));
        fs::write(marker_path(dir.path()), b"{").unwrap();
        assert!(matches!(check(dir.path(), &marker), Err(RootError::Damaged { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_agent_folder_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let marker = RootMarker { space: SpaceId::generate(), root: NodeId::generate() };
        // A real marker, but in a folder the link points to.
        write(elsewhere.path(), &marker).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join(AGENT_DIR), dir.path().join(AGENT_DIR)).unwrap();
        assert!(matches!(read(dir.path()), Err(RootError::Damaged { .. })));
        assert!(matches!(check(dir.path(), &marker), Err(RootError::Damaged { .. })));
        // Nothing is written through it.
        let other = RootMarker { space: SpaceId::generate(), ..marker };
        assert!(matches!(write(dir.path(), &other), Err(RootError::Damaged { .. })));
        assert_eq!(read(elsewhere.path()).unwrap(), Some(marker));
    }
}
