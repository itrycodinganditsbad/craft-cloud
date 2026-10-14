//! The folder watcher for the `run` loop: the OS's change notifications (`notify`: inotify on Linux, FSEvents on
//! macOS, `ReadDirectoryChangesW` on Windows) for the whole synced folder, turned into "something changed" and
//! "events were lost" for the scheduler. What it sees only decides *when* a pass runs; every pass scans the whole
//! folder, so a missed or extra event never loses or invents a change.
//!
//! - Events under the agent's own `.craft-cloud/` folder (temporary files, records, the marker) are ignored, so
//!   downloads in progress don't keep the loop busy.
//! - Reads are ignored (the engine's own hashing opens files); a file closed after writing counts as a change.
//! - An error from the watcher, or an event that says events were dropped (a full queue), asks for a pass at once.

use std::path::{Path, PathBuf};

use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedSender;

use craft_cloud_proto::Name;
use craft_cloud_sync::RelPath;

use crate::disk::AGENT_DIR;

/// What the watcher reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Something in the folder changed: at these paths (none: anywhere). `closed`: a writer closed the file after
    /// writing it (Linux's `IN_CLOSE_WRITE`), so it may be done.
    Changed { paths: Vec<PathBuf>, closed: bool },
    /// The watcher failed or dropped events: only a full pass can tell what changed.
    Lost(String),
}

/// A running watcher; dropping it stops watching.
pub struct FolderWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Both spellings of `folder`: the one given, and the one the OS resolves (FSEvents reports resolved paths, e.g.
/// /private/var for /var on macOS).
pub fn roots(folder: &Path) -> Vec<PathBuf> {
    let mut roots = vec![folder.to_owned()];
    if let Ok(real) = std::fs::canonicalize(folder)
        && real != folder
    {
        roots.push(real);
    }
    roots
}

/// The path of an event's `path` inside the folder (`roots`, see [`roots`]), if it is in it and every name in it is
/// one the engine syncs.
pub fn relative(roots: &[PathBuf], path: &Path) -> Option<RelPath> {
    let inside = roots.iter().find_map(|root| path.strip_prefix(root).ok())?;
    let names = inside.components().map(|c| Name::new(c.as_os_str().to_str()?).ok()).collect::<Option<Vec<Name>>>()?;
    (!names.is_empty()).then(|| RelPath::from_names(names))
}

/// Starts watching `folder` (and everything below it), sending what happens to `events`.
///
/// On Linux, `notify` adds a watch for a folder created inside the watched tree after its create event; a folder
/// replaced by a link in that moment is followed (its `metadata()` and the walk follow the link). That only adds
/// wake-ups for changes outside the folder: every pass scans the folder itself and never acts on a link.
pub fn watch(folder: &Path, events: UnboundedSender<WatchEvent>) -> Result<FolderWatcher, notify::Error> {
    let own: Vec<PathBuf> = roots(folder).iter().map(|root| root.join(AGENT_DIR)).collect();
    // Links inside the folder are never followed (the engine doesn't either): nothing outside it is watched.
    let config = notify::Config::default().with_follow_symlinks(false);
    let mut watcher = notify::RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            let event = match result {
                Ok(event) if event.need_rescan() => WatchEvent::Lost("the watcher dropped events".into()),
                Ok(event) if !counts(&event, &own) => return,
                Ok(event) => {
                    let closed = matches!(event.kind, EventKind::Access(AccessKind::Close(AccessMode::Write)));
                    WatchEvent::Changed { paths: event.paths, closed }
                }
                Err(e) => WatchEvent::Lost(e.to_string()),
            };
            // The loop has ended when nobody listens any more.
            let _ = events.send(event);
        },
        config,
    )?;
    watcher.watch(folder, RecursiveMode::Recursive)?;
    Ok(FolderWatcher { _watcher: watcher })
}

/// Whether an event is a change in the synced folder: not a read, and not only inside the agent's own folder.
fn counts(event: &Event, own: &[PathBuf]) -> bool {
    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
        EventKind::Access(_) => return false,
        _ => {}
    }
    event.paths.is_empty() || event.paths.iter().any(|p| !own.iter().any(|o| p.starts_with(o)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind};

    fn event(kind: EventKind, paths: &[&str]) -> Event {
        let mut event = Event::new(kind);
        for p in paths {
            event = event.add_path(PathBuf::from(p));
        }
        event
    }

    #[test]
    fn the_agents_own_folder_and_reads_dont_count() {
        let own = vec![PathBuf::from("/f/.craft-cloud")];
        let create = EventKind::Create(CreateKind::File);
        assert!(counts(&event(create, &["/f/a.txt"]), &own));
        assert!(!counts(&event(create, &["/f/.craft-cloud/tmp/x.part"]), &own));
        assert!(!counts(&event(create, &["/f/.craft-cloud"]), &own));
        assert!(counts(&event(EventKind::Modify(ModifyKind::Any), &["/f/.craft-cloud/tmp/x.part", "/f/b.txt"]), &own), "a rename out of it");
        assert!(counts(&event(create, &["/f/.craft-cloudy"]), &own), "a name that only starts the same");
        assert!(!counts(&event(EventKind::Access(AccessKind::Open(AccessMode::Read)), &["/f/a.txt"]), &own));
        assert!(!counts(&event(EventKind::Access(AccessKind::Close(AccessMode::Read)), &["/f/a.txt"]), &own));
        assert!(counts(&event(EventKind::Access(AccessKind::Close(AccessMode::Write)), &["/f/a.txt"]), &own));
        assert!(counts(&event(EventKind::Any, &[]), &own), "an event without paths may be anything");
    }

    #[test]
    fn event_paths_map_into_the_folder() {
        let roots = vec![PathBuf::from("/var/f"), PathBuf::from("/private/var/f")];
        assert_eq!(relative(&roots, Path::new("/var/f/a/b.txt")), Some(craft_cloud_sync::rel("a/b.txt")));
        assert_eq!(relative(&roots, Path::new("/private/var/f/a.txt")), Some(craft_cloud_sync::rel("a.txt")));
        assert_eq!(relative(&roots, Path::new("/var/f")), None, "the folder itself");
        assert_eq!(relative(&roots, Path::new("/var/g/a.txt")), None, "outside it");
    }
}
