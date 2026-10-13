//! The real filesystem: [`DiskFs`] gives the sync engine a folder on this computer.
//!
//! - **Links are never followed.** Symlinks, junctions and other special files are [`FsKind::Other`]. Every
//!   operation checks that the folders above its path are real folders, so a folder the user replaced with a
//!   link is not walked into. (The check and the operation are separate system calls, so a link swapped in
//!   between them is still followed; closing that needs `openat`-style calls and comes with the watcher.)
//! - **The root itself may be a link.** A synced folder that is a symlink or junction (the user's choice) is
//!   followed and is a folder; links *inside* it are still [`FsKind::Other`].
//! - **Writes are atomic, and land only on what was checked.** New content goes to a temporary file in
//!   `<root>/.craft-cloud/tmp/` (same volume, so renames are atomic), is flushed to disk, gets the replaced file's
//!   permissions (Unix), its modification time and executable bit, and then lands at the target only if the target
//!   holds what the engine checked ([`Fs::finish_write`]): nothing (a no-replace rename), or exactly the file it
//!   verified. On Linux and macOS the two files are exchanged in one step (`renameat2(RENAME_EXCHANGE)`,
//!   `renameatx_np(RENAME_SWAP)`) and the one that came out is compared with what was expected; anything else is
//!   exchanged back. The download first moves to a `.swap` name `clean_tmp` never removes, with a record next to it,
//!   so a crash right after the exchange can't let the file that came out (perhaps an app's save) be cleaned up:
//!   [`Fs::recover`] removes it only if it is the download or the checked file; a save while the download is still
//!   at the target is exchanged back (the save keeps the user's name, the download goes), and anything else moves
//!   next to the target as `.cc-aside`. On Windows, and on filesystems without an exchange, the landing takes steps: the file there is
//!   moved to `<name>.<random>.cc-aside` next to it (never replacing anything), checked, the download renamed into
//!   the free name, and the moved file removed, with a record in the temporary folder written first. A crash between
//!   the steps leaves both files on disk, never neither; the next pass's [`Fs::recover`] puts the moved file back if
//!   nothing took its place, removes it if the landing was complete but for that, and otherwise leaves it as a file
//!   the scan sees and syncs (it may be someone's save, so nothing removes `.cc-aside` files blindly). If the user
//!   moved the folder before recovery ran, the moved file is found there by the folder's ID (recorded too) and handled
//!   the same way under its own name, with a conflict name instead of `.cc-aside` when that name is taken. While a
//!   landing's record is pending (a step failed and recovery hasn't finished it), its moved file is left out of
//!   listings, so the scan never uploads it. Any error, or a writer dropped without landing, removes the temporary
//!   file.
//! - **Downloads that can't land are kept.** A target another program holds open (Windows) or that may not be
//!   replaced is [`LandError::InUse`]; the engine keeps the download in the temporary folder (`<key>.kept`, see
//!   [`Fs::keep`]) and lands it on a later pass without downloading again; while a quick look says the target still
//!   can't be replaced ([`Fs::replace_blocked`]), the kept file isn't touched at all. [`DiskFs::clean_tmp`] removes only
//!   interrupted writes (`.part`), never kept downloads, swap files or landing records.
//! - **In-flight downloads are private (Unix).** `.craft-cloud/` and `tmp/` are created with mode 0700 when the
//!   agent makes them, so other users can't read a partly downloaded file. (Folders that already exist keep their
//!   mode.) Windows folders get the default ACL of the synced folder.
//! - **Renames never replace.** On Linux and macOS the OS's no-replace rename (`renameat2(RENAME_NOREPLACE)`,
//!   `renameatx_np(RENAME_EXCL)`) does it atomically. On Windows a file is renamed as a new hard link (which fails
//!   if the name is taken, atomically on NTFS) and the removal of the old name, after checking the file may be
//!   deleted. An app that opens it without delete-sharing right between the check and the removal leaves the file
//!   with both names; a landing then keeps its record and the next pass's recovery removes the extra name. Folders,
//!   read-only files, files on filesystems without hard links (FAT/exFAT), and filesystems that refuse the flag
//!   elsewhere fall back to a check right before a plain rename: a file another program creates at the target in
//!   that instant is replaced. The window is a few microseconds and needs a program racing the agent for a name the
//!   agent just checked.
//! - **Hashing never blocks an app (Windows).** Files are opened for reading with every share flag, so an app
//!   can still save, rename or delete them while the agent reads.
//! - **File IDs** come from the `file-id` crate (inode and device on Unix, NTFS file ID and volume serial on
//!   Windows), folded to a `u64` with BLAKE3; see [`fold_file_id`].
//! - **Names** that aren't valid UTF-8, and (outside macOS, whose filesystems ignore normalisation) names not
//!   in NFC, are listed as [`FsKind::Other`]: the engine works with NFC names, and such a file couldn't be
//!   found again by its NFC name. They are skipped with a reason, never synced or deleted.
//! - **Known limits.** A volume mounted *inside* the synced folder: renames and downloads across it fail with
//!   "crosses devices" (EXDEV; temporary files live on the root's volume), so those entries are skipped with that
//!   reason every pass and nothing is lost. FAT/exFAT have no stable file IDs (Linux derives them from the
//!   entry's position, so a rename changes them): a move there is seen as a delete and a new file (the content
//!   goes up again; history keeps the old node), and files are hashed more often. A landing in steps compares the
//!   file with its ID right before moving it aside, and after a plain rename (no hard link) only its size, time and
//!   executable bit; recovery does the same. Windows read-only files are
//!   never replaced (the user's "don't change this"): a remote edit to one is skipped with "read-only here, not
//!   replaced", the download is kept like one for a file in use, and it lands once the attribute is cleared.
//! - **The root lists only with its marker.** Listing the root fails unless `.craft-cloud/root.json` is there as
//!   a regular file, so a folder emptied or unmounted in the middle of a pass fails the pass instead of looking
//!   like everything was deleted.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use craft_cloud_proto::{FileHash, Name};
use craft_cloud_sync::{FileMeta, Fs, FsEntry, FsKind, LandError, RelPath};
use file_id::FileId;
use serde::{Deserialize, Serialize};

/// The agent's own folder inside the sync root. The engine never syncs it.
pub const AGENT_DIR: &str = ".craft-cloud";
/// Temporary files, inside [`AGENT_DIR`].
const TMP_DIR: &str = "tmp";
/// Extension of temporary files (also ignored by the engine).
const TMP_EXT: &str = "part";
/// Extension of a kept download in [`TMP_DIR`]: a write that couldn't land, kept for a later pass ([`Fs::keep`]).
const KEPT_EXT: &str = "kept";
/// Extension of a landing record in [`TMP_DIR`]: a landing in steps under way ([`Fs::recover`]).
const LANDING_EXT: &str = "landing";
/// Extension of a download about to be exchanged with the file at its target, in [`TMP_DIR`]. After the exchange
/// it holds what was at the target, which may be an app's save, so [`DiskFs::clean_tmp`] never removes it; a
/// record next to it ([`SWAP_RECORD_EXT`]) lets [`Fs::recover`] decide.
const SWAP_EXT: &str = "swap";
/// Extension of the record that goes with a [`SWAP_EXT`] file (same stem): the target and both files' stats.
const SWAP_RECORD_EXT: &str = "swapping";
/// Appended to a landing or swap record recovery can't read (`<record>.bad`): set aside so passes go on, never
/// cleaned up, and listed in every pass's report and by `status` until the user removes it ([`DiskFs::needs_you`]).
const BAD_EXT: &str = "bad";
/// Extension of a file an interrupted landing by exchange took out of the folder and recovery couldn't put back
/// (its folder is gone, or its record can't be read): never cleaned up, listed until the user moves it.
const RESCUED_EXT: &str = "rescued";
/// The last part of the name a file gets while a download lands over it in steps: `<name>.<random>.cc-aside`,
/// next to it.
const ASIDE_SUFFIX: &str = "cc-aside";
/// Longest key a kept download may have (it is part of a file name).
const MAX_KEY_LEN: usize = 200;
/// Largest buffer `read_at` allocates up front; longer reads grow as data arrives.
const MAX_READ_PREALLOC: usize = 4 * 1024 * 1024;

/// A folder on disk, as the engine sees it.
#[derive(Debug, Clone)]
pub struct DiskFs {
    root: PathBuf,
    /// Whether a download may land with the system's exchange of two files (see [`DiskFs::without_swap`]).
    swap: bool,
    /// Files moved aside by a landing in steps whose record is still pending (full paths), with the landing's target:
    /// listings leave the moved files out, so the scan never uploads them (recovery puts them back or removes them),
    /// and the targets are [`Fs::unsettled`], so a target left empty is never taken for deleted. Set when a landing
    /// leaves its record, and by [`Fs::recover`] from the records it couldn't finish.
    pending_asides: BTreeMap<PathBuf, RelPath>,
    /// This device's name, for the conflict-copy names recovery gives (see [`DiskFs::with_device_name`]).
    device_name: String,
    /// What the last [`Fs::recover`] found that needs the user ([`DiskFs::needs_you`]), for [`Fs::notices`].
    notices: Vec<(String, String)>,
    /// Faults and detours for tests of paths a real disk rarely takes.
    hooks: Hooks,
}

/// Test hooks (see the `#[doc(hidden)]` builders on [`DiskFs`]). All off outside tests.
#[derive(Debug, Clone, Copy, Default)]
struct Hooks {
    /// Putting a moved file back fails ([`DiskFs::failing_put_backs`]).
    fail_put_backs: bool,
    /// Moving the download into place during a landing in steps fails ([`DiskFs::failing_landings`]).
    fail_landings: bool,
    /// Removing the moved file at the end of a landing in steps fails ([`DiskFs::failing_aside_removals`]).
    fail_aside_removals: bool,
    /// Landings in steps move files with a plain rename ([`DiskFs::plain_moves`]).
    plain_moves: bool,
    /// A landing by exchange stops right after the exchange, as a crash would ([`DiskFs::stopping_after_exchange`]).
    stop_after_exchange: bool,
    /// An app saves over the target again right before the exchange back ([`DiskFs::saving_before_exchange_back`]).
    save_before_exchange_back: bool,
    /// The look at what an exchange back brought out fails ([`DiskFs::failing_looks_after_exchange_back`]).
    fail_looks: bool,
    /// Moving a save the exchange took out next to the target fails ([`DiskFs::failing_rescues`]).
    fail_rescues: bool,
}

/// What a landing record in [`TMP_DIR`] holds: the target, the name its file was moved to, and what both files
/// look like (see [`Fs::recover`]).
#[derive(Debug, Serialize, Deserialize)]
struct LandingRecord {
    /// The target, `/`-separated below the root.
    target: String,
    /// The moved file's name, in the target's folder.
    aside: String,
    /// The file that was at the target.
    expected: RecordedMeta,
    /// The file that lands there.
    incoming: RecordedMeta,
    /// Whether both moves went through a hard link, so both files kept their IDs and recovery compares them too
    /// (see [`same_file_after_move`]). Written down after the download took the name; until then (and in records
    /// from before it existed) recovery compares size, time and executable bit only.
    #[serde(default)]
    ids_kept: bool,
    /// The ID of the folder both files are in, so recovery finds the moved file after the user moved that folder
    /// (see [`DiskFs::apply_landing`]). `None` in records from before it existed.
    #[serde(default)]
    folder_id: Option<u64>,
}

/// What a swap record in [`TMP_DIR`] holds: an exchange of a download with the file at `target` is under way, and
/// the swap file next to the record is the download (before the exchange, or after one back) or what was at the
/// target (after it). See [`DiskFs::land_by_exchange`].
#[derive(Debug, Serialize, Deserialize)]
struct SwapRecord {
    /// The target, `/`-separated below the root.
    target: String,
    /// The file the engine checked at the target.
    expected: RecordedMeta,
    /// The download.
    incoming: RecordedMeta,
}

impl LandingRecord {
    /// How the files of this landing moved, as far as recovery can count on.
    fn moved(&self) -> Moved {
        if self.ids_kept { Moved::Link } else { Moved::Plain }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct RecordedMeta {
    size: u64,
    mtime_ms: i64,
    exec: bool,
    id: Option<u64>,
}

impl From<FileMeta> for RecordedMeta {
    fn from(m: FileMeta) -> Self {
        Self { size: m.size, mtime_ms: m.mtime_ms, exec: m.exec, id: m.id }
    }
}

impl From<RecordedMeta> for FileMeta {
    fn from(m: RecordedMeta) -> Self {
        Self { size: m.size, mtime_ms: m.mtime_ms, exec: m.exec, id: m.id }
    }
}

/// A file being written: a temporary file in `<root>/.craft-cloud/tmp/`. Dropping it before
/// [`Fs::finish_write`] lands it removes the temporary file.
#[derive(Debug)]
pub struct DiskWriter {
    file: Option<File>,
    temp: PathBuf,
    /// Whether dropping this removes `temp` (until the rename landed it).
    armed: bool,
}

impl Write for DiskWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.as_mut().ok_or_else(|| io::Error::other("the write already finished"))?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }
}

impl Drop for DiskWriter {
    fn drop(&mut self) {
        // Close first: Windows can't remove an open file.
        drop(self.file.take());
        if self.armed {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

fn not_found(path: &RelPath) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{path}: not found"))
}

/// Folds a file ID into the engine's `u64`: the first 8 bytes (little-endian) of the BLAKE3 hash of a tagged
/// encoding of every field. Collisions are as unlikely as for any 64-bit hash; two files that collide would
/// only make the engine hash content it could otherwise have skipped, or look for a moved file in the wrong
/// place and then check it by content.
pub fn fold_file_id(id: FileId) -> u64 {
    let mut hasher = blake3::Hasher::new();
    match id {
        FileId::Inode { device_id, inode_number } => {
            hasher.update(&[1]);
            hasher.update(&device_id.to_le_bytes());
            hasher.update(&inode_number.to_le_bytes());
        }
        FileId::LowRes { volume_serial_number, file_index } => {
            hasher.update(&[2]);
            hasher.update(&volume_serial_number.to_le_bytes());
            hasher.update(&file_index.to_le_bytes());
        }
        FileId::HighRes { volume_serial_number, file_id } => {
            hasher.update(&[3]);
            hasher.update(&volume_serial_number.to_le_bytes());
            hasher.update(&file_id.to_le_bytes());
        }
    }
    let hash = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(hash.as_bytes().get(..8).unwrap_or(&[0; 8]));
    u64::from_le_bytes(first)
}

/// The ID of the file or folder at `path`, whose metadata (not following links) is `md`.
#[cfg(unix)]
fn local_id(md: &Metadata, _path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(fold_file_id(FileId::new_inode(md.dev(), md.ino())))
}

/// The ID of the file or folder at `path`. Windows metadata doesn't carry the file ID on stable Rust, so the
/// `file-id` crate opens the entry (no data access, every share flag: it never blocks an app).
#[cfg(windows)]
fn local_id(_md: &Metadata, path: &Path) -> Option<u64> {
    file_id::get_file_id(path).ok().map(fold_file_id)
}

#[cfg(not(any(unix, windows)))]
fn local_id(_md: &Metadata, path: &Path) -> Option<u64> {
    file_id::get_file_id(path).ok().map(fold_file_id)
}

#[cfg(unix)]
fn is_exec(md: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_exec(_md: &Metadata) -> bool {
    false
}

/// Milliseconds since the Unix epoch (negative before it).
fn millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    }
}

fn system_time(ms: i64) -> io::Result<SystemTime> {
    let time = if ms >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_millis(ms.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_millis(ms.unsigned_abs()))
    };
    time.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("modification time {ms} ms is out of range")))
}

fn file_meta(md: &Metadata, path: &Path) -> FileMeta {
    FileMeta { size: md.len(), mtime_ms: md.modified().map_or(0, millis), exec: is_exec(md), id: local_id(md, path) }
}

/// What an entry is, from its metadata (which must not follow links).
fn kind_of(md: &Metadata, path: &Path) -> FsKind {
    let ft = md.file_type();
    if ft.is_symlink() {
        FsKind::Other
    } else if ft.is_dir() {
        FsKind::Dir { id: local_id(md, path) }
    } else if ft.is_file() {
        FsKind::File(file_meta(md, path))
    } else {
        FsKind::Other
    }
}

/// Opens a file for reading without ever blocking another program's save, rename or delete (Windows).
fn open_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
        options.share_mode(0x1 | 0x2 | 0x4);
    }
    options.open(path)
}

/// Whether `raw` names the entry the engine will look for: an NFC name (or any name on macOS, whose
/// filesystems find a file by any normalisation of its name). Names the engine rejects are its business.
fn name_round_trips(raw: &str) -> bool {
    cfg!(target_vendor = "apple") || Name::new(raw).map_or(true, |name| name.as_str() == raw)
}

/// Whether `a` and `b` are one entry under two spellings (a case-only rename on a case-insensitive filesystem).
///
/// Only a respelling counts: the same folder, names that differ but are equal ignoring case, the same file ID, and
/// a listing of the folder that shows only one of the two spellings. Two hard links to one file (`a` and `b`, or
/// even `b` and `B` on a case-sensitive disk) share an ID but are two entries, and renaming one onto the other
/// must fail, not do nothing.
fn same_entry(a: &Path, b: &Path) -> bool {
    let lower = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    let (Some(name_a), Some(name_b)) = (a.file_name(), b.file_name()) else { return false };
    if a.parent() != b.parent() || name_a == name_b || lower(a) != lower(b) {
        return false;
    }
    let same_id = match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(ma), Ok(mb)) => {
            let (ia, ib) = (local_id(&ma, a), local_id(&mb, b));
            ia.is_some() && ia == ib
        }
        _ => false,
    };
    if !same_id {
        return false;
    }
    // One entry shows up under one spelling only.
    let Some(parent) = a.parent() else { return false };
    let Ok(entries) = fs::read_dir(parent) else { return false };
    let mut shown = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == name_a || name == name_b {
            shown += 1;
        }
    }
    shown == 1
}

/// Renames when nothing is at `to`: a check right before a plain rename. Something created at `to` between the
/// two is replaced (see the module docs).
fn check_then_rename(from: &Path, to: &Path) -> io::Result<()> {
    match fs::symlink_metadata(to) {
        Ok(_) if same_entry(from, to) => fs::rename(from, to),
        Ok(_) => Err(io::Error::new(io::ErrorKind::AlreadyExists, "the target already exists")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::rename(from, to),
        Err(e) => Err(e),
    }
}

/// Renames without ever replacing: the OS's atomic no-replace rename, or [`check_then_rename`] on filesystems
/// that refuse the flag.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    move_no_replace(from, to).map(|_| ())
}

/// How a file was moved, which decides what a look at it afterwards can compare (see [`same_file_after_move`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moved {
    /// Through a new hard link and the removal of the old name (Windows, NTFS): the file keeps its ID.
    Link,
    /// A rename. FAT and exFAT have no file IDs of their own: Windows and Linux derive one from the directory entry,
    /// so a renamed file may come out with another ID.
    Plain,
}

/// Whether `found`, the file at a path a file was just moved to, is `expected`, the file that was moved: size, time and
/// executable bit always, and the file ID only when the move kept it ([`Moved::Link`]). The ID was compared right
/// before the move, so a plain rename can't let another file with the same size and time slip through unless it
/// replaced the moved one in between.
fn same_file_after_move(found: FileMeta, expected: FileMeta, moved: Moved) -> bool {
    match moved {
        Moved::Link => found == expected,
        Moved::Plain => found.size == expected.size && found.mtime_ms == expected.mtime_ms && found.exec == expected.exec,
    }
}

/// [`rename_no_replace`], saying how the entry moved.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn move_no_replace(from: &Path, to: &Path) -> io::Result<Moved> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(Moved::Plain),
        // The filesystem or kernel doesn't support the flag (EINVAL on Linux, ENOTSUP on macOS).
        Err(e) if [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP].contains(&e) => check_then_rename(from, to).map(|()| Moved::Plain),
        // A case-only rename on a case-insensitive filesystem: the "existing" target is the source itself.
        Err(e) if e == Errno::EXIST && same_entry(from, to) => fs::rename(from, to).map(|()| Moved::Plain),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn move_no_replace(from: &Path, to: &Path) -> io::Result<Moved> {
    if fs::symlink_metadata(from)?.file_type().is_file() {
        return link_rename(from, to);
    }
    check_then_rename(from, to).map(|()| Moved::Plain)
}

/// Renames a file without ever replacing: a new hard link at `to` (refused, atomically, if anything is there),
/// then the old name removed. The file keeps its ID and times. If the old name can't be removed (an app holds the
/// file open without letting it be deleted), the new link goes again and the error is returned. If an app opens the
/// file that way between the delete-access check below and the removal, neither name can go and the file keeps both;
/// a landing then keeps its record, and the next pass's recovery removes the extra name once it can. Filesystems without
/// hard links (FAT, exFAT) fall back to [`check_then_rename`], and so does a case-only respelling of one entry.
#[cfg_attr(any(target_os = "linux", target_os = "android", target_vendor = "apple"), allow(dead_code))]
fn link_rename(from: &Path, to: &Path) -> io::Result<Moved> {
    // A read-only file is renamed the old way: removing its old name would clear the read-only attribute, which the
    // new name shares (Rust removes a read-only file on Windows by clearing it). Landings never replace one at all
    // (see `Fs::finish_write` below).
    if fs::symlink_metadata(from)?.permissions().readonly() {
        return check_then_rename(from, to).map(|()| Moved::Plain);
    }
    // Windows: a file an app holds open without letting it be deleted can still get a new hard link, but then
    // neither name can be removed again. Asking for delete access first fails the same way the removal would, before
    // anything is done. The handle is open only for that instant, with every share flag, so it only gets in the way
    // of a program opening the file in that same instant without sharing it.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // DELETE; FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
        drop(OpenOptions::new().access_mode(0x0001_0000).share_mode(0x1 | 0x2 | 0x4).open(from)?);
    }
    match fs::hard_link(from, to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && same_entry(from, to) => return fs::rename(from, to).map(|()| Moved::Plain),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
        // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED (Windows): no hard links on this filesystem.
        Err(e) if e.kind() == io::ErrorKind::Unsupported || (cfg!(windows) && matches!(e.raw_os_error(), Some(1 | 50))) => {
            return check_then_rename(from, to).map(|()| Moved::Plain);
        }
        Err(e) => return Err(e),
    }
    if let Err(e) = fs::remove_file(from) {
        let _ = fs::remove_file(to);
        return Err(e);
    }
    Ok(Moved::Link)
}

/// Exchanges the entries at `a` and `b` in one step (`renameat2(RENAME_EXCHANGE)` on Linux, `renameatx_np` with
/// `RENAME_SWAP` on macOS; rustix maps its `EXCHANGE` flag to each). `Ok(false)` when the system or the filesystem
/// can't (the caller lands in steps instead).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn exchange(a: &Path, b: &Path) -> io::Result<bool> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;
    match renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(true),
        Err(e) if [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP].contains(&e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Whether this system can exchange two files in one step (some filesystems still can't: [`exchange`] says so).
const HAS_EXCHANGE: bool = cfg!(any(target_os = "linux", target_os = "android", target_vendor = "apple"));

/// Windows has no exchange of two files without `unsafe` calls: downloads land in steps there.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn exchange(_a: &Path, _b: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Whether a rename failed because the file is in use or may not be replaced: a sharing or lock violation on
/// Windows (an app holds the file open), or permission denied anywhere.
fn in_use(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    e.kind() == io::ErrorKind::PermissionDenied || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
}

/// What [`in_use`] means, in words: on Windows a sharing or lock violation is "in use by another program"; permission
/// denied (a folder or file that may not be written, on Unix) is "can't be replaced here (no permission)".
fn in_use_reason(e: &io::Error) -> io::Error {
    let why = if cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)) {
        "in use by another program"
    } else {
        "can't be replaced here (no permission)"
    };
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}

/// `n` random bytes as hex.
fn random_hex(n: usize) -> io::Result<String> {
    let mut random = vec![0u8; n];
    getrandom::fill(&mut random).map_err(|e| io::Error::other(format!("the system random source failed: {e}")))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

/// The name a file at `name` is moved to while a download lands over it in steps: `<name>.<random>.cc-aside`, with
/// `name` shortened (at a character boundary) so the whole stays within 255 bytes.
fn aside_name(name: &str) -> io::Result<String> {
    let suffix = format!(".{}.{ASIDE_SUFFIX}", random_hex(6)?);
    let mut keep = name.len().min(255usize.saturating_sub(suffix.len()));
    while !name.is_char_boundary(keep) {
        keep -= 1;
    }
    Ok(format!("{}{suffix}", name.get(..keep).unwrap_or_default()))
}

/// Saves what the writer's temporary path holds when it is someone's file, not the download: moved next to the
/// target `full` under a `.cc-aside` name (the scan syncs it): `Ok` with that path. If that fails (or `fail`, a test
/// hook), the file stays where it is, a swap file whose record the caller keeps, so the next pass's recovery moves it
/// next to the target: `Err` with its path. The writer is left pointing nowhere, so it is neither removed, kept nor
/// landed.
fn rescue(writer: &mut DiskWriter, full: &Path, parent: &Path, fail: bool) -> Result<PathBuf, PathBuf> {
    writer.armed = false;
    let temp = std::mem::take(&mut writer.temp);
    match full.file_name().and_then(|n| n.to_str()).map(aside_name) {
        Some(Ok(name)) if !fail && rename_no_replace(&temp, &parent.join(&name)).is_ok() => Ok(parent.join(name)),
        _ => Err(temp),
    }
}

/// What is at `full` (not following links), as the engine sees it.
fn kind_at(full: &Path) -> io::Result<Option<FsKind>> {
    match fs::symlink_metadata(full) {
        Ok(md) => Ok(Some(kind_of(&md, full))),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Makes a rename in `dir` durable (Unix; Windows has no directory flush). Best effort: the data itself was
/// flushed before the rename.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

/// Gives the new content the permissions of the file it replaces (`replaced`, its metadata), so a file the user
/// made private (0600) stays private; a new file keeps the usual `0666 & !umask`. Then sets the executable bit
/// the way `chmod +x` does: execute wherever read is allowed.
#[cfg(unix)]
fn set_mode(file: &File, replaced: Option<&Metadata>, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = match replaced {
        Some(old) if old.file_type().is_file() => old.permissions().mode() & 0o777,
        _ => file.metadata()?.permissions().mode() & 0o777,
    };
    let mode = if exec { mode | ((mode & 0o444) >> 2) } else { mode & !0o111 };
    file.set_permissions(fs::Permissions::from_mode(mode))
}

/// Windows: the new file gets the folder's default ACL, like any file an app saves with temp + rename. There is
/// no read-only attribute to carry over: a read-only file is never replaced at all (`finish_write` refuses it
/// before the first step, and the download is kept until the attribute is cleared).
#[cfg(not(unix))]
fn set_mode(_file: &File, _replaced: Option<&Metadata>, _exec: bool) -> io::Result<()> {
    Ok(())
}

/// Makes the folder `dir`, owner-only (0700) on Unix, so a download in flight can't be read by other users. A folder
/// that already exists keeps its mode.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::DirBuilder::new().create(dir)
    }
}

/// A new temporary file. Unix gives it the usual `0666 & !umask` mode, like any file an app creates.
fn create_temp(dir: &Path) -> io::Result<(File, PathBuf)> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o666);
    }
    loop {
        let mut random = [0u8; 12];
        getrandom::fill(&mut random).map_err(|e| io::Error::other(format!("the system random source failed: {e}")))?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{name}.{TMP_EXT}"));
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

impl DiskFs {
    /// The folder at `root`. Nothing is checked or created until the engine asks.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            swap: true,
            pending_asides: BTreeMap::new(),
            device_name: "this computer".into(),
            notices: Vec::new(),
            hooks: Hooks::default(),
        }
    }

    /// Names conflict copies that recovery makes after this device (as the engine names its own).
    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = name.into();
        self
    }

    /// Makes removing the moved file at the end of a landing in steps fail, as a disk error would: for tests that
    /// the record then stays and the moved file isn't listed.
    #[doc(hidden)]
    pub fn failing_aside_removals(mut self) -> Self {
        self.hooks.fail_aside_removals = true;
        self
    }

    /// Moves files during a landing in steps with a plain rename, as on FAT and exFAT (no hard links), even where a
    /// hard link would do: for tests of that path on any system.
    #[doc(hidden)]
    pub fn plain_moves(mut self) -> Self {
        self.hooks.plain_moves = true;
        self
    }

    /// Makes moving the download into place during a landing in steps fail, as a disk error would: for tests of what
    /// happens after the file there was moved aside.
    #[doc(hidden)]
    pub fn failing_landings(mut self) -> Self {
        self.hooks.fail_landings = true;
        self
    }

    /// Makes a landing by exchange stop right after the exchange and leave everything as it is, as a crash there
    /// would: for tests of [`Fs::recover`].
    #[doc(hidden)]
    pub fn stopping_after_exchange(mut self) -> Self {
        self.hooks.stop_after_exchange = true;
        self
    }

    /// Plays an app that saves over the target (writes "a newer save" and renames it into place) right before a
    /// landing by exchange exchanges back: for tests of a second save in that instant.
    #[doc(hidden)]
    pub fn saving_before_exchange_back(mut self) -> Self {
        self.hooks.save_before_exchange_back = true;
        self
    }

    /// Makes the look at what a landing by exchange's exchange back brought out fail, as a disk error would: for tests
    /// that the swap record then stays for [`Fs::recover`].
    #[doc(hidden)]
    pub fn failing_looks_after_exchange_back(mut self) -> Self {
        self.hooks.fail_looks = true;
        self
    }

    /// Makes moving a save that a landing by exchange took out of the target next to it fail, as a disk error would:
    /// for tests that the swap record then stays for [`Fs::recover`].
    #[doc(hidden)]
    pub fn failing_rescues(mut self) -> Self {
        self.hooks.fail_rescues = true;
        self
    }

    /// Moves `from` to `to` for a landing in steps, never replacing anything (see [`DiskFs::plain_moves`]).
    fn move_for_landing(&self, from: &Path, to: &Path) -> io::Result<Moved> {
        if self.hooks.plain_moves { check_then_rename(from, to).map(|()| Moved::Plain) } else { move_no_replace(from, to) }
    }

    /// Lands downloads in steps, the way Windows and filesystems without an exchange do, even where the system can
    /// exchange two files: for tests of that path.
    pub fn without_swap(mut self) -> Self {
        self.swap = false;
        self
    }

    /// Makes every put-back of a file moved aside during a landing in steps fail, as a disk error would: for tests
    /// that the landing record then stays for [`Fs::recover`].
    #[doc(hidden)]
    pub fn failing_put_backs(mut self) -> Self {
        self.hooks.fail_put_backs = true;
        self
    }

    /// Moves the file moved aside back to `full` and says whether the landing record can go: when it is back, or when
    /// something took its place (the moved file then stays as a file the scan sees; recovery would leave it too).
    /// Otherwise the record stays, and the next pass's [`Fs::recover`] puts it back.
    fn put_back(&self, aside: &Path, full: &Path) -> bool {
        let moved = if self.hooks.fail_put_backs { Err(io::Error::other("put-backs fail (test)")) } else { rename_no_replace(aside, full) };
        if moved.is_ok() {
            return true;
        }
        match (kind_at(aside).ok().flatten(), kind_at(full).ok().flatten()) {
            // One file under both names (a rename through a hard link whose old name couldn't be removed): the record
            // stays, and recovery removes the extra name.
            (Some(FsKind::File(a)), Some(FsKind::File(t))) if a.id.is_some() && a.id == t.id => false,
            (_, there) => there.is_some(),
        }
    }

    /// Where the write kept under `key` lives. Keys are the engine's (node, version and manifest IDs): anything
    /// else that could leave the folder or clash with a name the folder uses is refused.
    fn kept_path(&self, key: &str) -> io::Result<PathBuf> {
        let fine = !key.is_empty() && key.len() <= MAX_KEY_LEN && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        if !fine || key.starts_with('.') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a key for a kept download"));
        }
        Ok(self.tmp_dir().join(format!("{key}.{KEPT_EXT}")))
    }

    /// Lands `temp` (the prepared download, `incoming`) at `full`, where the file `expected` is, in steps: the file
    /// there is compared with `expected` (ID included), moves to a name next to it (never replacing anything), is
    /// checked again (the ID only if the move went through a hard link, which keeps it: FAT and exFAT give a renamed
    /// file a new one), the download moves into the free name, and the moved file is removed. A record in the temporary folder, written first, lets
    /// [`Fs::recover`] finish or undo a landing a crash cut short. A failure after the first step puts the moved
    /// file back where nothing took its place.
    fn land_in_steps(
        &mut self,
        path: &RelPath,
        full: &Path,
        mut writer: DiskWriter,
        expected: FileMeta,
        incoming: FileMeta,
    ) -> Result<FileMeta, LandError<DiskWriter>> {
        let (Some(parent), Some(name)) = (full.parent(), full.file_name().and_then(|n| n.to_str())) else {
            return Err(LandError::Io(io::Error::other(format!("{path}: not a file name"))));
        };
        // 0. The file there is the one the engine checked, ID included: after a plain rename (FAT, exFAT) the moved
        // file's ID may differ, so this is the last look that can tell an app's save with the same size and time.
        if kind_at(full)? != Some(FsKind::File(expected)) {
            return Err(LandError::Changed(writer));
        }
        let aside_name = aside_name(name)?;
        let aside = parent.join(&aside_name);
        let tmp = self.real_tmp(true)?.ok_or_else(|| io::Error::other("no temporary folder"))?;
        let record_path = tmp.join(format!("{}.{LANDING_EXT}", random_hex(12)?));
        let mut record = LandingRecord {
            target: path.to_string(),
            aside: aside_name,
            expected: expected.into(),
            incoming: incoming.into(),
            ids_kept: false,
            folder_id: self.folder_id(parent),
        };
        let json = serde_json::to_vec(&record).map_err(|e| io::Error::other(e.to_string()))?;
        crate::account::write_atomic(&record_path, &json, true)?;
        let forget = |record_path: &Path| {
            let _ = fs::remove_file(record_path);
        };
        // 1. The file there moves aside. If that failed halfway (a rename through a hard link that couldn't remove
        // the old name), the file has both names and the record stays, so recovery removes the extra one.
        let moved_by = match self.move_for_landing(full, &aside) {
            Ok(moved_by) => moved_by,
            Err(e) => {
                if kind_at(&aside).ok().flatten().is_none() {
                    forget(&record_path);
                } else {
                    self.pending_asides.insert(aside, path.clone());
                }
                return Err(match e.kind() {
                    io::ErrorKind::NotFound => LandError::Changed(writer),
                    _ if in_use(&e) => LandError::InUse(writer, in_use_reason(&e)),
                    _ => LandError::Io(e),
                });
            }
        };
        // 2. It is the file the engine checked, or it goes back. The ID counts only if the move kept it.
        let moved = kind_at(&aside).ok().flatten();
        if !matches!(moved, Some(FsKind::File(found)) if same_file_after_move(found, expected, moved_by)) {
            // If something took the free name meanwhile, the moved file stays next to it (the scan sees it).
            if self.put_back(&aside, full) {
                forget(&record_path);
            } else {
                self.pending_asides.insert(aside, path.clone());
            }
            return Err(LandError::Changed(writer));
        }
        // 3. The download takes the free name.
        let landing =
            if self.hooks.fail_landings { Err(io::Error::other("landings fail (test)")) } else { self.move_for_landing(&writer.temp, full) };
        let landed_by = match landing {
            Ok(landed_by) => landed_by,
            Err(e) => {
                if self.put_back(&aside, full) {
                    forget(&record_path);
                } else {
                    self.pending_asides.insert(aside, path.clone());
                }
                return Err(match e.kind() {
                    io::ErrorKind::AlreadyExists => LandError::Changed(writer),
                    _ if in_use(&e) => LandError::InUse(writer, in_use_reason(&e)),
                    _ => LandError::Io(e),
                });
            }
        };
        writer.armed = false;
        sync_dir(parent);
        // Both files kept their IDs (hard links): written down so recovery compares the IDs too. If this fails or a
        // crash comes first, recovery compares without them, as for a plain rename.
        if moved_by == Moved::Link && landed_by == Moved::Link {
            record.ids_kept = true;
            if let Ok(json) = serde_json::to_vec(&record) {
                let _ = crate::account::write_atomic(&record_path, &json, true);
            }
        }
        // 4. The moved file goes. If it can't, the record stays and the next pass's recovery tries again; until then
        // listings leave the moved file out, so the scan doesn't upload the old content as a new file.
        let removal = if self.hooks.fail_aside_removals { Err(io::Error::other("removals fail (test)")) } else { fs::remove_file(&aside) };
        if removal.is_ok() {
            forget(&record_path);
        } else {
            self.pending_asides.insert(aside, path.clone());
        }
        Ok(incoming)
    }

    /// Lands the download at `full`, where the file `expected` is, with one exchange (Linux, macOS): the two are
    /// exchanged, the file that came out is compared with `expected`, and anything else is exchanged back.
    ///
    /// After the exchange the download's temporary path holds what was at the target, which may be an app's save. So
    /// before it, the download moves to a `.swap` name [`DiskFs::clean_tmp`] never removes, with a record of the
    /// target and both stats written first. A crash at any point leaves that file for [`Fs::recover`], which removes
    /// it only if it is the download or the file the engine checked (or that file is back at the target). A save it
    /// holds goes back to the target by an exchange while the download is still there (the download then goes);
    /// otherwise it moves next to the target as a `.cc-aside` file the scan sees.
    fn land_by_exchange(
        &mut self,
        path: &RelPath,
        full: &Path,
        parent: &Path,
        mut writer: DiskWriter,
        expected: FileMeta,
        incoming: FileMeta,
    ) -> Result<FileMeta, LandError<DiskWriter>> {
        let tmp = self.real_tmp(true)?.ok_or_else(|| io::Error::other("no temporary folder"))?;
        let stem = random_hex(12)?;
        let record_path = tmp.join(format!("{stem}.{SWAP_RECORD_EXT}"));
        let swap = tmp.join(format!("{stem}.{SWAP_EXT}"));
        let record = SwapRecord { target: path.to_string(), expected: expected.into(), incoming: incoming.into() };
        let json = serde_json::to_vec(&record).map_err(|e| io::Error::other(e.to_string()))?;
        crate::account::write_atomic(&record_path, &json, true)?;
        let forget = || {
            let _ = fs::remove_file(&record_path);
        };
        // A rename in the temporary folder keeps the download's ID and time: the record still describes it.
        if let Err(e) = fs::rename(&writer.temp, &swap) {
            forget();
            return Err(LandError::Io(e));
        }
        writer.temp = swap;
        match exchange(&writer.temp, full) {
            Ok(true) => {}
            Ok(false) => {
                self.unswap(&mut writer, &record_path);
                return self.land_in_steps(path, full, writer, expected, incoming);
            }
            Err(e) => {
                self.unswap(&mut writer, &record_path);
                return Err(match e.kind() {
                    // Nothing at the target any more.
                    io::ErrorKind::NotFound => LandError::Changed(writer),
                    _ if in_use(&e) => LandError::InUse(writer, in_use_reason(&e)),
                    _ => LandError::Io(e),
                });
            }
        }
        if self.hooks.stop_after_exchange {
            writer.armed = false;
            return Err(LandError::Io(io::Error::other("stopped after the exchange (test)")));
        }
        // The swap file now holds what was at the target.
        if kind_at(&writer.temp).ok().flatten() == Some(FsKind::File(expected)) {
            // The file the engine checked: it goes, then the record. If it can't go, the record stays, and recovery
            // removes it (it is still the checked file).
            writer.armed = false;
            if fs::remove_file(&writer.temp).is_ok() {
                forget();
            }
            sync_dir(parent);
            return Ok(incoming);
        }
        // Something else (an app saved in between): exchanged back, and the download comes back.
        if self.hooks.save_before_exchange_back {
            let newer = parent.join(".cc-test-save");
            fs::write(&newer, b"a newer save")?;
            fs::rename(&newer, full)?;
        }
        let stranded = |swap: &Path| {
            let name = swap.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("it is in {AGENT_DIR}/{TMP_DIR} as {name}, and the next pass moves it next to the file")
        };
        let back = exchange(&writer.temp, full);
        // What the swap file holds after the exchange back. A look that fails decides nothing: the node is skipped, and
        // the record stays, so the next pass's recovery removes the swap file if it is the download, or moves it next
        // to the target if it is a save.
        let came_back = match back {
            Ok(true) if self.hooks.fail_looks => Err(io::Error::other("looks fail (test)")),
            Ok(true) => kind_at(&writer.temp),
            _ => Ok(None),
        };
        let came_back = match came_back {
            Ok(kind) => kind,
            Err(e) => {
                writer.armed = false;
                let name = writer.temp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return Err(LandError::Io(io::Error::new(
                    e.kind(),
                    format!(
                        "{path}: can't look at what an exchange back brought out ({e}); it stays in {AGENT_DIR}/{TMP_DIR} as {name} until the next pass puts it right"
                    ),
                )));
            }
        };
        match back {
            // The swap file holds the download again.
            Ok(true) if came_back == Some(FsKind::File(incoming)) => {
                self.unswap(&mut writer, &record_path);
                Err(LandError::Changed(writer))
            }
            // An app saved over the target again between the look and the exchange back. Its save replaced the
            // download, so the swap file now holds that newest save and the target the older one. The newest save
            // belongs at the user's name (it is what the app and the user see as current), so the two are exchanged
            // once more; the older save, back in the swap file, goes next to the target as `.cc-aside`, where the scan
            // uploads it. No save is lost, and the download is gone (it is downloaded again). If that exchange fails,
            // the newest save is the one that goes next to the target: still nothing lost.
            // The record goes only once the older save is next to the target; if it couldn't be moved there, it stays
            // as the swap file with its record, and the next pass's recovery moves it.
            Ok(true) => {
                let _ = exchange(&writer.temp, full);
                match rescue(&mut writer, full, parent, self.hooks.fail_rescues) {
                    Ok(_) => {
                        forget();
                        Err(LandError::Changed(writer))
                    }
                    Err(swap) => Err(LandError::Io(io::Error::other(format!(
                        "{path}: an app saved it twice while the server's version landed; the older save couldn't be moved next to it: {}",
                        stranded(&swap)
                    )))),
                }
            }
            // Can't happen short of the disk failing between two calls: the other file is never removed. As above, the
            // record goes only once the save is next to the target.
            Ok(false) | Err(_) => {
                let place = match rescue(&mut writer, full, parent, self.hooks.fail_rescues) {
                    Ok(aside) => {
                        forget();
                        let name = aside.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        format!("it is next to it as {name}")
                    }
                    Err(swap) => stranded(&swap),
                };
                Err(LandError::Io(io::Error::other(format!(
                    "{path}: the download landed over a file saved meanwhile and couldn't be undone; {place}"
                ))))
            }
        }
    }

    /// Moves the download in a swap file back to a temporary name (kept, landed or cleaned up like any write), then
    /// removes its swap record. If the move fails, the record stays and recovery removes the swap file, which is the
    /// download.
    fn unswap(&self, writer: &mut DiskWriter, record_path: &Path) {
        let Ok(stem) = random_hex(12) else { return };
        let temp = self.tmp_dir().join(format!("{stem}.{TMP_EXT}"));
        if fs::rename(&writer.temp, &temp).is_ok() {
            writer.temp = temp;
            let _ = fs::remove_file(record_path);
        }
    }

    /// Sets aside a record recovery can't read (or that names no file in the folder), so passes go on: renamed to
    /// `<record>.bad`, never cleaned up, and listed by [`DiskFs::needs_you`] until the user removes it. A swap record's
    /// swap file (which may hold an app's save) becomes `<stem>.rescued`, listed the same way.
    fn set_aside(record_path: &Path) -> io::Result<()> {
        let name = record_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        fs::rename(record_path, record_path.with_file_name(format!("{name}.{BAD_EXT}")))?;
        let swap = record_path.with_extension(SWAP_EXT);
        if record_path.extension().is_some_and(|ext| ext == SWAP_RECORD_EXT) && Self::lstat(&swap)?.is_some() {
            fs::rename(&swap, swap.with_extension(RESCUED_EXT))?;
        }
        Ok(())
    }

    /// The record at `record_path`, or `None` when it can't be read or isn't one.
    fn read_record<T: serde::de::DeserializeOwned>(record_path: &Path) -> Option<T> {
        serde_json::from_slice(&fs::read(record_path).ok()?).ok()
    }

    /// Whether `target` (from a record) names a file in the folder.
    fn names_a_file(target: &str) -> bool {
        let rel = craft_cloud_sync::rel(target);
        rel.to_string() == target && !rel.is_root()
    }

    /// Puts right the exchange a swap record describes (see [`DiskFs::land_by_exchange`]). The record goes once that
    /// is done. A record that can't be read is set aside ([`DiskFs::set_aside`]). Errors name the record's target,
    /// or the record in `.craft-cloud/tmp` when it can't be set aside.
    fn recover_swap(&self, record_path: &Path) -> io::Result<()> {
        let shown = format!("{AGENT_DIR}/{TMP_DIR}/{}", record_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
        let named = |what: &str, e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"));
        let record = match Self::read_record::<SwapRecord>(record_path) {
            Some(record) if Self::names_a_file(&record.target) => record,
            _ => return Self::set_aside(record_path).map_err(|e| named(&shown, e)),
        };
        self.apply_swap(&record, &record_path.with_extension(SWAP_EXT)).map_err(|e| named(&record.target, e))?;
        fs::remove_file(record_path).map_err(|e| named(&shown, e))
    }

    /// See [`DiskFs::recover_swap`].
    fn apply_swap(&self, record: &SwapRecord, swap: &Path) -> io::Result<()> {
        let Some(found) = kind_at(swap)? else { return Ok(()) };
        let target = craft_cloud_sync::rel(&record.target);
        if target.to_string() != record.target || target.is_root() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a swap record names no file in the folder"));
        }
        let full = self.checked(&target)?;
        let at_target = match &full {
            Some(full) => kind_at(full)?,
            None => None,
        };
        let (expected, incoming) = (FsKind::File(record.expected.into()), FsKind::File(record.incoming.into()));
        // The download (the exchange never happened, or was undone), the file the engine checked (the landing was
        // done but for removing it), or the checked file is back at the target (so this one is ours): it goes.
        if found == incoming || found == expected || at_target == Some(expected) {
            return fs::remove_file(swap);
        }
        // Anything else is what the exchange took out of the target: an app's save. While the download is still at the
        // target, the two are exchanged back, so the save is at the user's name again (as if the download never came)
        // and the swap file holds the download, which goes. If an app saved over the target once more in that instant,
        // the swap file holds that newest save instead: exchanged once more, so the newest save stays at the name and
        // the older one goes next to it, as below.
        if let Some(full) = &full
            && at_target == Some(incoming)
            && matches!(exchange(swap, full), Ok(true))
        {
            if kind_at(swap)? == Some(incoming) {
                return fs::remove_file(swap);
            }
            let _ = exchange(swap, full);
        }
        // Otherwise (the target holds something else now, or the system can't exchange) the save goes next to the
        // target, where the scan sees it; if the target's folder is gone, it stays in the temporary folder under a name
        // nothing cleans up, and the error says where.
        if let Some(full) = full
            && let (Some(parent), Some(name)) = (full.parent(), full.file_name().and_then(|n| n.to_str()))
            && self.is_folder(&target.parent().unwrap_or_default())?
        {
            return rename_no_replace(swap, &parent.join(aside_name(name)?));
        }
        let rescued = swap.with_extension(RESCUED_EXT);
        fs::rename(swap, &rescued)?;
        let name = rescued.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Err(io::Error::other(format!(
            "its folder is gone; the file an interrupted download took out of it is in {AGENT_DIR}/{TMP_DIR} as {name}, which nothing cleans up: move it back yourself"
        )))
    }

    /// Finishes or undoes the landing a record describes (see [`Fs::recover`]). The record goes once that is done,
    /// or once there is nothing left to do. A record that can't be read is set aside ([`DiskFs::set_aside`]).
    /// Errors name the record's target, or the record in `.craft-cloud/tmp` when it can't be set aside.
    fn recover_landing(&self, record_path: &Path) -> io::Result<()> {
        let shown = format!("{AGENT_DIR}/{TMP_DIR}/{}", record_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
        let named = |what: &str, e: io::Error| io::Error::new(e.kind(), format!("{what}: {e}"));
        let record = match Self::read_record::<LandingRecord>(record_path) {
            Some(record) if Self::names_a_file(&record.target) && Name::new(&record.aside).is_ok() => record,
            _ => return Self::set_aside(record_path).map_err(|e| named(&shown, e)),
        };
        self.apply_landing(&record).map_err(|e| named(&record.target, e))?;
        fs::remove_file(record_path).map_err(|e| named(&shown, e))
    }

    /// The full path of the file moved aside that the landing record at `record_path` names, and the landing's
    /// target, if the record can be read.
    fn aside_of(&self, record_path: &Path) -> Option<(PathBuf, RelPath)> {
        let record: LandingRecord = serde_json::from_slice(&fs::read(record_path).ok()?).ok()?;
        let target = craft_cloud_sync::rel(&record.target);
        if target.to_string() != record.target || target.is_root() {
            return None;
        }
        let aside = target.parent()?.join(&Name::new(&record.aside).ok()?);
        Some((self.full(&aside), target))
    }

    /// Puts right what the landing `record` describes; see [`DiskFs::recover_landing`].
    fn apply_landing(&self, record: &LandingRecord) -> io::Result<()> {
        let target = craft_cloud_sync::rel(&record.target);
        if target.to_string() != record.target || target.is_root() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a landing record names no file in the folder"));
        }
        let aside_name = Name::new(&record.aside).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let target_name = target.file_name().map(|n| n.as_str().to_owned()).unwrap_or_default();
        let aside = target.parent().unwrap_or_default().join(&aside_name);
        // Where the moved file is: at its recorded place, or, when the user moved its folder before this ran, in the
        // folder that has the recorded folder ID (or, failing that, next to the one file with its random name that
        // still looks like the moved file). A folder above that is gone or a link, and no moved folder found: nothing
        // of this landing can be reached any more.
        let at_place = match (self.checked(&target)?, self.checked(&aside)?) {
            (Some(full), Some(aside_full)) if kind_at(&aside_full)?.is_some() => Some((full, aside_full)),
            _ => None,
        };
        let (full, aside_full, moved_with_folder) = match at_place {
            Some((full, aside_full)) => (full, aside_full, false),
            None => match self.moved_folder(record)? {
                Some(dir) => (dir.join(&target_name), dir.join(aside_name.as_str()), true),
                None => return Ok(()),
            },
        };
        match (kind_at(&aside_full)?, kind_at(&full)?) {
            // Moved aside, and nothing took its place: it goes back.
            (Some(FsKind::File(_)), None) => rename_no_replace(&aside_full, &full)?,
            // One file under both names (a rename through a hard link that couldn't remove the old name): the extra
            // name goes; nothing is lost.
            (Some(FsKind::File(moved)), Some(FsKind::File(there))) if moved.id.is_some() && moved.id == there.id => {
                fs::remove_file(&aside_full)?;
            }
            // The landing was done but for removing the moved file. The IDs count only if both moves kept them
            // (hard links); a plain rename on FAT or exFAT gives both files new ones.
            (Some(FsKind::File(moved)), Some(FsKind::File(there)))
                if same_file_after_move(moved, record.expected.into(), record.moved())
                    && same_file_after_move(there, record.incoming.into(), record.moved()) =>
            {
                fs::remove_file(&aside_full)?;
            }
            // In a folder the user moved, with its name taken by something else: a conflict name next to it (never
            // replacing anything), so the scan sees a copy, not the tracked file renamed to its `.cc-aside` name.
            (Some(FsKind::File(_)), Some(_)) if moved_with_folder => self.to_conflict_name(&aside_full, &full)?,
            // Gone already, or something else is going on: the moved file stays, and the scan sees it.
            _ => {}
        }
        Ok(())
    }

    /// The folder a landing's moved file is in now, when the user moved the landing's folder (see
    /// [`DiskFs::apply_landing`]): the one with the record's folder ID that holds a file with the moved file's name,
    /// else the first that holds one which still looks like the moved file (size, time, executable bit). Walks the
    /// folder (never through links, never into `.craft-cloud`); only recovery does this, and only for a moved file
    /// that isn't where its record says.
    fn moved_folder(&self, record: &LandingRecord) -> io::Result<Option<PathBuf>> {
        let mut by_look = None;
        let mut queue = vec![(self.root.clone(), 0usize)];
        while let Some((dir, depth)) = queue.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                if dir == self.root && entry.file_name() == AGENT_DIR {
                    continue;
                }
                let path = entry.path();
                let Ok(md) = fs::symlink_metadata(&path) else { continue };
                if md.file_type().is_dir() && depth < craft_cloud_proto::limits::MAX_TREE_DEPTH {
                    queue.push((path, depth + 1));
                } else if md.file_type().is_file() && entry.file_name().to_str() == Some(record.aside.as_str()) {
                    if record.folder_id.is_some() && self.folder_id(&dir) == record.folder_id {
                        return Ok(Some(dir));
                    }
                    if by_look.is_none() && same_file_after_move(file_meta(&md, &path), record.expected.into(), Moved::Plain) {
                        by_look = Some(dir.clone());
                    }
                }
            }
        }
        Ok(by_look)
    }

    /// The ID of the folder at `dir` (the root may be a link: followed).
    fn folder_id(&self, dir: &Path) -> Option<u64> {
        let md = if dir == self.root { fs::metadata(dir) } else { fs::symlink_metadata(dir) }.ok()?;
        md.file_type().is_dir().then(|| local_id(&md, dir)).flatten()
    }

    /// Moves `from` to a free conflict-copy name next to `like` (the name it would have had), never replacing.
    fn to_conflict_name(&self, from: &Path, like: &Path) -> io::Result<()> {
        let (Some(parent), Some(name)) = (like.parent(), like.file_name().and_then(|n| n.to_str()).and_then(|n| Name::new(n).ok())) else {
            return Err(io::Error::other("no conflict name for a file without a name"));
        };
        let stamp = crate::cli::conflict_stamp();
        for counter in 1..=1000 {
            let copy =
                craft_cloud_proto::conflict_name(&name, false, &self.device_name, stamp, counter).map_err(|e| io::Error::other(e.to_string()))?;
            match rename_no_replace(from, &parent.join(copy.as_str())) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                other => return other,
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free conflict name"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The root marker; the root can't be listed without it (see [`Fs::list`] below).
    fn marker(&self) -> PathBuf {
        self.root.join(AGENT_DIR).join(crate::root::MARKER_FILE)
    }

    /// Where temporary files go.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join(AGENT_DIR).join(TMP_DIR)
    }

    /// What recovery left in the temporary folder for the user, as (file, what to do) pairs: records it couldn't
    /// read (`<record>.bad`) and files an interrupted download took out of the folder that it couldn't put back
    /// (`.rescued`). Nothing removes them; they are listed in every pass's report and by `status` until the user does.
    pub fn needs_you(&self) -> io::Result<Vec<(String, String)>> {
        let Some(tmp) = self.real_tmp(false)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for entry in fs::read_dir(tmp)? {
            let path = entry?.path();
            let shown = format!("{AGENT_DIR}/{TMP_DIR}/{}", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
            match path.extension() {
                Some(ext) if ext == BAD_EXT => {
                    // The target, if that much of the record is readable.
                    let target = fs::read(&path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .and_then(|v| v.get("target")?.as_str().map(str::to_owned))
                        .filter(|t| !t.is_empty());
                    let check = match target {
                        Some(target) => format!("check the files next to {target}"),
                        None => "check the folder for files whose names end in .cc-aside".to_owned(),
                    };
                    out.push((
                        shown.clone(),
                        format!("a record of an interrupted download can't be read and was set aside; {check}, then remove {shown}"),
                    ));
                }
                Some(ext) if ext == RESCUED_EXT => out.push((
                    shown,
                    "a file an interrupted download took out of the folder (it may be a save) and couldn't put back: move it back yourself"
                        .to_owned(),
                )),
                _ => {}
            }
        }
        out.sort();
        Ok(out)
    }

    /// Removes temporary files a crashed pass left behind (`.part`: interrupted writes). Kept downloads, landing and
    /// swap records, and swap files (which may hold an app's save) are [`Fs::recover`]'s. Call it before a pass, while
    /// no other pass runs on this folder. Returns how many it removed.
    pub fn clean_tmp(&self) -> io::Result<usize> {
        let Some(tmp) = self.real_tmp(false)? else { return Ok(0) };
        let entries = fs::read_dir(tmp)?;
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let is_temp = entry.path().extension().is_some_and(|ext| ext == TMP_EXT);
            if is_temp && entry.file_type().is_ok_and(|t| t.is_file()) {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The temporary folder, after checking that it and `.craft-cloud/` are real folders (never links: nothing is
    /// ever written or deleted through one). `create` makes missing ones; otherwise `None` when one is missing.
    fn real_tmp(&self, create: bool) -> io::Result<Option<PathBuf>> {
        let agent = self.root.join(AGENT_DIR);
        for dir in [agent.clone(), agent.join(TMP_DIR)] {
            match Self::lstat(&dir)? {
                Some(md) if md.file_type().is_dir() => {}
                Some(_) => return Err(io::Error::other(format!("{} is not a real folder (a link?); nothing is written through it", dir.display()))),
                None if create => create_private_dir(&dir)?,
                None => return Ok(None),
            }
        }
        Ok(Some(self.tmp_dir()))
    }

    fn full(&self, path: &RelPath) -> PathBuf {
        let mut full = self.root.clone();
        for name in path.names() {
            full.push(name.as_str());
        }
        full
    }

    /// The full path of `path`, after checking that every folder above it (below the root) is a real folder, not
    /// a link. `Ok(None)` when one isn't.
    fn checked(&self, path: &RelPath) -> io::Result<Option<PathBuf>> {
        let mut full = self.root.clone();
        let names = path.names();
        let above = names.len().saturating_sub(1);
        for name in names.get(..above).unwrap_or_default() {
            full.push(name.as_str());
            match fs::symlink_metadata(&full) {
                Ok(md) if md.file_type().is_dir() => {}
                Ok(_) => return Ok(None),
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        if let Some(leaf) = names.last() {
            full.push(leaf.as_str());
        }
        Ok(Some(full))
    }

    /// Like [`DiskFs::checked`], with a missing or linked folder above `path` as an error.
    fn require(&self, path: &RelPath) -> io::Result<PathBuf> {
        self.checked(path)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path}: a folder above it is missing or a link")))
    }

    /// The entry at `full` without following links; `None` when nothing is there.
    fn lstat(full: &Path) -> io::Result<Option<Metadata>> {
        match fs::symlink_metadata(full) {
            Ok(md) => Ok(Some(md)),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether `path` is a real folder: the root counts when it is a folder or a link to one (the user's choice).
    fn is_folder(&self, path: &RelPath) -> io::Result<bool> {
        if path.is_root() {
            return match fs::metadata(&self.root) {
                Ok(md) => Ok(md.is_dir()),
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(false),
                Err(e) => Err(e),
            };
        }
        Ok(Self::lstat(&self.full(path))?.is_some_and(|md| md.file_type().is_dir()))
    }

    /// The full path of a regular file at `path` (not a link, folder or device).
    fn regular_file(&self, path: &RelPath) -> io::Result<PathBuf> {
        let full = self.require(path)?;
        match Self::lstat(&full)? {
            Some(md) if md.file_type().is_file() => Ok(full),
            Some(_) => Err(io::Error::other(format!("{path}: not a regular file"))),
            None => Err(not_found(path)),
        }
    }

    /// Checks that `path` can take a file: its folder exists and nothing but a file is there.
    fn file_target(&self, path: &RelPath) -> io::Result<PathBuf> {
        if path.is_root() {
            return Err(io::Error::other("the root is a folder"));
        }
        let full = self.require(path)?;
        let parent = path.parent().unwrap_or_default();
        if !self.is_folder(&parent)? {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{parent}: no such folder")));
        }
        if Self::lstat(&full)?.is_some_and(|md| md.file_type().is_dir()) {
            return Err(io::Error::other(format!("{path}: is a directory")));
        }
        Ok(full)
    }
}

impl Fs for DiskFs {
    type Writer = DiskWriter;

    fn now_ms(&self) -> i64 {
        millis(SystemTime::now())
    }

    /// Only Unix-like systems have one; elsewhere (Windows) every file reads as not executable.
    fn has_exec_bit(&self) -> bool {
        cfg!(unix)
    }

    fn list(&self, dir: &RelPath) -> io::Result<Vec<FsEntry>> {
        let full = self.require(dir)?;
        if !dir.is_root() && !Self::lstat(&full)?.is_some_and(|md| md.file_type().is_dir()) {
            return Err(io::Error::other(format!("{dir}: not a folder")));
        }
        // The root is listed only while its marker is there: a folder that is emptied or unmounted during a pass
        // fails the pass instead of reading as "everything was deleted".
        // `.craft-cloud/` itself must be a real folder: a link there is never followed to a marker somewhere else.
        let agent_ok = Self::lstat(&self.root.join(AGENT_DIR))?.is_some_and(|md| md.file_type().is_dir());
        if dir.is_root() && !(agent_ok && Self::lstat(&self.marker())?.is_some_and(|md| md.file_type().is_file())) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the folder's marker (.craft-cloud/root.json) is gone: the folder was emptied, replaced or unmounted",
            ));
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&full)? {
            let entry = entry?;
            let path = entry.path();
            // A file a landing in steps moved aside, whose landing isn't finished: recovery's, never synced.
            if self.pending_asides.contains_key(&path) {
                continue;
            }
            let (name, readable) = match entry.file_name().into_string() {
                Ok(name) => {
                    let ok = name_round_trips(&name);
                    (name, ok)
                }
                Err(raw) => (raw.to_string_lossy().into_owned(), false),
            };
            // An entry that vanished since the listing is simply not there.
            let md = match fs::symlink_metadata(&path) {
                Ok(md) => md,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let kind = if readable { kind_of(&md, &path) } else { FsKind::Other };
            out.push(FsEntry { name, kind });
        }
        Ok(out)
    }

    fn stat(&self, path: &RelPath) -> io::Result<Option<FsKind>> {
        if path.is_root() {
            // The root is the user's choice: a link or junction there is followed, and it is a folder.
            return match fs::metadata(&self.root) {
                Ok(md) if md.is_dir() => Ok(Some(FsKind::Dir { id: local_id(&md, &self.root) })),
                Ok(_) => Ok(Some(FsKind::Other)),
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
                Err(e) => Err(e),
            };
        }
        let Some(full) = self.checked(path)? else { return Ok(None) };
        Ok(Self::lstat(&full)?.map(|md| kind_of(&md, &full)))
    }

    fn open(&self, path: &RelPath) -> io::Result<Box<dyn Read + Send + '_>> {
        Ok(Box::new(open_read(&self.regular_file(path)?)?))
    }

    fn read_at(&self, path: &RelPath, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut file = open_read(&self.regular_file(path)?)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut out = Vec::with_capacity(len.min(MAX_READ_PREALLOC));
        file.take(len as u64).read_to_end(&mut out)?;
        Ok(out)
    }

    fn begin_write(&mut self, path: &RelPath) -> io::Result<DiskWriter> {
        self.file_target(path)?;
        let tmp = self.real_tmp(true)?.ok_or_else(|| io::Error::other("no temporary folder"))?;
        let (file, temp) = create_temp(&tmp)?;
        Ok(DiskWriter { file: Some(file), temp, armed: true })
    }

    /// Lands the download only on what the engine checked:
    ///
    /// - **Nothing expected**: the OS's no-replace rename (see the module docs).
    /// - **A file expected, Linux and macOS**: the download and the file there are exchanged in one step; the file
    ///   that came out is compared with what was expected. The expected one is removed; anything else (an app saved
    ///   in between) is exchanged back, and the download comes back as [`LandError::Changed`]. A crash in between is
    ///   put right by [`Fs::recover`] (see [`DiskFs::land_by_exchange`]).
    /// - **Windows, and filesystems without an exchange**: in steps (see [`DiskFs::land_in_steps`]); a crash between
    ///   them leaves both files on disk, never neither, and the next pass's [`Fs::recover`] puts them right.
    ///
    /// A file another program holds open (Windows), or that may not be replaced, is [`LandError::InUse`].
    ///
    /// Known limit: once the file that came out of the exchange (or was moved aside) matched, the download is in
    /// place and its stat is returned; an app that renames a save over the target in the instant before the old file
    /// is removed isn't seen here. Nothing is lost: the save is at the target, and the next pass sees a file that
    /// differs from what was recorded (an edit) and handles it as one.
    fn finish_write(
        &mut self,
        path: &RelPath,
        mut writer: DiskWriter,
        exec: bool,
        mtime_ms: i64,
        expected: Option<FileMeta>,
    ) -> Result<FileMeta, LandError<DiskWriter>> {
        let full = self.file_target(path)?;
        // Windows: a read-only file is the user saying "don't change this". It is never replaced: the download is kept,
        // like one for a file in use, and lands once the attribute is cleared. Checked before the first step.
        if cfg!(windows) && expected.is_some() && Self::lstat(&full)?.is_some_and(|md| md.file_type().is_file() && md.permissions().readonly()) {
            return Err(LandError::InUse(writer, io::Error::new(io::ErrorKind::PermissionDenied, "read-only here, not replaced")));
        }
        // A write kept from an earlier pass is closed: it is opened again to set its time and mode.
        let file = match writer.file.take() {
            Some(file) => file,
            None => OpenOptions::new().write(true).open(&writer.temp)?,
        };
        set_mode(&file, Self::lstat(&full)?.as_ref(), exec)?;
        file.set_modified(system_time(mtime_ms)?)?;
        file.sync_all()?;
        let md = file.metadata()?;
        // The ID is read before the rename (Windows reads it by path); a rename on one volume keeps it.
        let incoming = file_meta(&md, &writer.temp);
        drop(file);
        let parent = full.parent().map(Path::to_owned).unwrap_or_else(|| self.root.clone());
        let Some(expected) = expected else {
            return match rename_no_replace(&writer.temp, &full) {
                Ok(()) => {
                    writer.armed = false;
                    sync_dir(&parent);
                    Ok(incoming)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(LandError::Changed(writer)),
                Err(e) if in_use(&e) => Err(LandError::InUse(writer, in_use_reason(&e))),
                Err(e) => Err(LandError::Io(e)),
            };
        };
        if !self.swap || !HAS_EXCHANGE {
            return self.land_in_steps(path, &full, writer, expected, incoming);
        }
        self.land_by_exchange(path, &full, &parent, writer, expected, incoming)
    }

    fn keep(&mut self, mut writer: DiskWriter, key: &str) -> io::Result<()> {
        let kept = self.kept_path(key)?;
        drop(writer.file.take());
        // Replaces an older copy kept under the same key (the same version).
        fs::rename(&writer.temp, &kept)?;
        writer.armed = false;
        Ok(())
    }

    fn take_kept(&mut self, key: &str, hash: &FileHash) -> io::Result<Option<DiskWriter>> {
        let kept = self.kept_path(key)?;
        match Self::lstat(&kept)? {
            Some(md) if md.file_type().is_file() => {
                // Back to a temporary name: no longer kept, and cleaned up like any write if the agent stops now.
                let temp = self.tmp_dir().join(format!("{}.{TMP_EXT}", random_hex(12)?));
                fs::rename(&kept, &temp)?;
                let writer = DiskWriter { file: None, temp, armed: true };
                // Hashed again: a kept file that changed on disk is dropped (with the writer), never landed.
                let mut hasher = blake3::Hasher::new();
                io::copy(&mut open_read(&writer.temp)?, &mut hasher)?;
                Ok((hasher.finalize().as_bytes() == hash.as_bytes()).then_some(writer))
            }
            _ => Ok(None),
        }
    }

    /// Windows: a read-only file, or one an app holds open without letting it be deleted (asking for delete access,
    /// with every share flag, fails the way moving it would). Linux and macOS: a folder this user may not write.
    /// Anything the look can't tell is left to the landing.
    fn replace_blocked(&self, path: &RelPath) -> Option<String> {
        let full = self.checked(path).ok()??;
        let md = Self::lstat(&full).ok()??;
        if !md.file_type().is_file() {
            return None;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            if md.permissions().readonly() {
                return Some("read-only here, not replaced".into());
            }
            // DELETE; FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
            if let Err(e) = OpenOptions::new().access_mode(0x0001_0000).share_mode(0x1 | 0x2 | 0x4).open(&full)
                && matches!(e.raw_os_error(), Some(32 | 33))
            {
                return Some("in use by another program".into());
            }
        }
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            use rustix::fs::{Access, access};
            let parent = full.parent()?;
            if access(parent, Access::WRITE_OK | Access::EXEC_OK).is_err_and(|e| e == rustix::io::Errno::ACCESS) {
                return Some("can't be replaced here (no permission)".into());
            }
        }
        None
    }

    fn kept(&self) -> io::Result<Vec<String>> {
        let Some(tmp) = self.real_tmp(false)? else { return Ok(Vec::new()) };
        let mut keys = Vec::new();
        for entry in fs::read_dir(tmp)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == KEPT_EXT)
                && let Some(key) = path.file_stem().and_then(|s| s.to_str())
            {
                keys.push(key.to_owned());
            }
        }
        Ok(keys)
    }

    fn discard_kept(&mut self, key: &str) -> io::Result<()> {
        match fs::remove_file(self.kept_path(key)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn unsettled(&self) -> Vec<RelPath> {
        self.pending_asides.values().cloned().collect()
    }

    /// Goes through the landing and swap records in the temporary folder (see [`DiskFs::land_in_steps`] and
    /// [`DiskFs::land_by_exchange`]). A record that can't be read is set aside as `<record>.bad` and listed in
    /// [`Fs::notices`] (see [`DiskFs::needs_you`]); one that can't be acted on stays for the next pass, and the first
    /// error is returned once all were tried.
    fn recover(&mut self) -> io::Result<()> {
        self.pending_asides.clear();
        self.notices.clear();
        let Some(tmp) = self.real_tmp(false)? else { return Ok(()) };
        let mut first_error = None;
        let paths = fs::read_dir(tmp)?.map(|entry| entry.map(|e| e.path())).collect::<io::Result<Vec<_>>>()?;
        for path in paths {
            let result = match path.extension() {
                Some(ext) if ext == LANDING_EXT => self.recover_landing(&path).inspect_err(|_| {
                    // Still pending: its moved file stays out of listings, and its target is unsettled.
                    if let Some((aside, target)) = self.aside_of(&path) {
                        self.pending_asides.insert(aside, target);
                    }
                }),
                Some(ext) if ext == SWAP_RECORD_EXT => self.recover_swap(&path),
                _ => Ok(()),
            };
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        }
        self.notices = self.needs_you().unwrap_or_default();
        first_error.map_or(Ok(()), Err)
    }

    fn notices(&self) -> Vec<(String, String)> {
        self.notices.clone()
    }

    fn mkdir(&mut self, path: &RelPath) -> io::Result<()> {
        if path.is_root() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "the root already exists"));
        }
        fs::create_dir(self.require(path)?)
    }

    fn remove_file(&mut self, path: &RelPath) -> io::Result<()> {
        let full = self.require(path)?;
        match Self::lstat(&full)? {
            Some(md) if md.file_type().is_dir() => Err(io::Error::other(format!("{path}: is a directory"))),
            // Windows: a read-only file is the user saying "don't change this", and is never removed (Rust's removal
            // would clear the attribute and go ahead). On Unix the folder's write permission decides, as usual.
            Some(md) if cfg!(windows) && md.file_type().is_file() && md.permissions().readonly() => Err(craft_cloud_sync::read_only_error()),
            Some(_) => fs::remove_file(&full),
            None => Err(not_found(path)),
        }
    }

    fn remove_dir(&mut self, path: &RelPath) -> io::Result<()> {
        if path.is_root() {
            return Err(io::Error::other("the root is never removed"));
        }
        let full = self.require(path)?;
        match Self::lstat(&full)? {
            Some(md) if md.file_type().is_dir() => fs::remove_dir(&full),
            Some(_) => Err(io::Error::other(format!("{path}: not a folder"))),
            None => Err(not_found(path)),
        }
    }

    fn rename(&mut self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        if from.is_root() || to.is_root() {
            return Err(io::Error::other("the root can't be renamed"));
        }
        if to.starts_with(from) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "can't move a folder into itself"));
        }
        let source = self.require(from)?;
        let target = self.require(to)?;
        if Self::lstat(&source)?.is_none() {
            return Err(not_found(from));
        }
        let parent = to.parent().unwrap_or_default();
        if !self.is_folder(&parent)? {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{parent}: no such folder")));
        }
        rename_no_replace(&source, &target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_round_trip_through_system_time() {
        for ms in [0, 1, 1_700_000_000_123, -5_000] {
            assert_eq!(millis(system_time(ms).unwrap()), ms);
        }
    }

    #[test]
    fn folded_ids_differ_by_kind_and_value() {
        let a = fold_file_id(FileId::new_inode(1, 2));
        assert_eq!(a, fold_file_id(FileId::new_inode(1, 2)));
        assert_ne!(a, fold_file_id(FileId::new_inode(2, 1)));
        assert_ne!(a, fold_file_id(FileId::new_low_res(1, 2)));
        assert_ne!(fold_file_id(FileId::new_low_res(1, 2)), fold_file_id(FileId::new_high_res(1, 2)));
    }

    #[test]
    fn the_checked_rename_fallback_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, d) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("d"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        fs::create_dir(&d).unwrap();
        assert_eq!(check_then_rename(&a, &b).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(check_then_rename(&a, &d).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"b");
        // The same entry under another spelling (a case-only rename on a case-insensitive disk) is no clash.
        assert!(!same_entry(&a, &a), "the same spelling is not a respelling");
        assert!(same_entry(&a, &dir.path().join("A")) == dir.path().join("A").exists(), "a respelling, where the disk folds case");
        assert!(!same_entry(&a, &b));
        // Hard links share an ID but are two entries.
        fs::hard_link(&b, dir.path().join("b2")).unwrap();
        assert!(!same_entry(&b, &dir.path().join("b2")));
        assert_eq!(check_then_rename(&b, &dir.path().join("b2")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        // `b`/`B` hard links: two entries wherever the disk tells them apart.
        let upper = dir.path().join("B");
        if !upper.exists() {
            fs::hard_link(&b, &upper).unwrap();
            assert!(!same_entry(&b, &upper), "two listed spellings are two entries");
            assert_eq!(check_then_rename(&b, &upper).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(fs::read(&b).unwrap(), b"b");
        }
        check_then_rename(&a, &dir.path().join("c")).unwrap();
        assert_eq!(fs::read(dir.path().join("c")).unwrap(), b"a");
    }

    /// Linux and macOS exchange two files in one step; that is what downloads land with there.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn the_system_exchanges_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert!(exchange(&a, &b).unwrap(), "no exchange on this filesystem");
        assert_eq!((fs::read(&a).unwrap(), fs::read(&b).unwrap()), (b"b".to_vec(), b"a".to_vec()));
        assert_eq!(exchange(&a, &dir.path().join("gone")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    /// The rename Windows uses for files: a new hard link, then the old name goes. It never replaces, and the file
    /// keeps its ID.
    #[test]
    fn a_rename_through_a_hard_link_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("c"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert_eq!(link_rename(&a, &b).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!((fs::read(&a).unwrap(), fs::read(&b).unwrap()), (b"a".to_vec(), b"b".to_vec()));
        let id = |p: &Path| local_id(&fs::symlink_metadata(p).unwrap(), p);
        let before = id(&a);
        assert_eq!(link_rename(&a, &c).unwrap(), Moved::Link);
        assert!(!a.exists());
        assert_eq!(fs::read(&c).unwrap(), b"a");
        assert_eq!(id(&c), before);
    }

    #[test]
    fn aside_names_stay_within_255_bytes() {
        let short = aside_name("notes.txt").unwrap();
        assert!(short.starts_with("notes.txt.") && short.ends_with(".cc-aside"), "{short}");
        let long = aside_name(&"é".repeat(127)).unwrap();
        assert!(long.len() <= 255 && long.ends_with(".cc-aside"), "{}", long.len());
    }

    /// A folder as the agent sets it up, a target file, and the file moved aside from it.
    fn landing_setup() -> (tempfile::TempDir, DiskFs, FileMeta) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(AGENT_DIR)).unwrap();
        fs::write(dir.path().join(AGENT_DIR).join(crate::root::MARKER_FILE), b"{}").unwrap();
        let mut disk = DiskFs::new(dir.path());
        let old = disk.write(&craft_cloud_sync::rel("a.txt"), b"old", false, 1_700_000_000_000).unwrap();
        fs::rename(dir.path().join("a.txt"), dir.path().join("a.txt.1.cc-aside")).unwrap();
        (dir, disk, old)
    }

    fn record(disk: &DiskFs, expected: FileMeta, incoming: FileMeta) {
        record_with(disk, expected, incoming, false);
    }

    fn record_with(disk: &DiskFs, expected: FileMeta, incoming: FileMeta, ids_kept: bool) {
        let record = LandingRecord {
            target: "a.txt".into(),
            aside: "a.txt.1.cc-aside".into(),
            expected: expected.into(),
            incoming: incoming.into(),
            ids_kept,
            folder_id: None,
        };
        fs::write(disk.tmp_dir().join("r.landing"), serde_json::to_vec(&record).unwrap()).unwrap();
    }

    /// A crash right after `dir/a.txt` moved aside, then the user renamed `dir` to `dir2` (and, with `clash`, saved a
    /// new `dir2/a.txt`): recovery finds the moved file by its folder's ID (or, in a record without one, by its name
    /// and look) and puts it back under its own name there, or under a conflict name when that is taken. Never left
    /// as `.cc-aside`, which the scan would take for the tracked file's new name.
    #[test]
    fn a_file_moved_aside_in_a_folder_the_user_renamed_goes_back_under_its_name() {
        for (clash, with_id) in [(false, true), (true, true), (false, false)] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join(AGENT_DIR)).unwrap();
            fs::write(dir.path().join(AGENT_DIR).join(crate::root::MARKER_FILE), b"{}").unwrap();
            let mut disk = DiskFs::new(dir.path()).with_device_name("desk");
            disk.mkdir(&craft_cloud_sync::rel("dir")).unwrap();
            let old = disk.write(&craft_cloud_sync::rel("dir/a.txt"), b"old", false, 1_700_000_000_000).unwrap();
            let incoming = FileMeta { size: 3, mtime_ms: 1, exec: false, id: Some(1) };
            fs::rename(dir.path().join("dir/a.txt"), dir.path().join("dir/a.txt.1.cc-aside")).unwrap();
            let record = LandingRecord {
                target: "dir/a.txt".into(),
                aside: "a.txt.1.cc-aside".into(),
                expected: old.into(),
                incoming: incoming.into(),
                ids_kept: false,
                folder_id: if with_id { disk.folder_id(&dir.path().join("dir")) } else { None },
            };
            assert!(!with_id || record.folder_id.is_some());
            fs::write(disk.tmp_dir().join("r.landing"), serde_json::to_vec(&record).unwrap()).unwrap();
            fs::rename(dir.path().join("dir"), dir.path().join("dir2")).unwrap();
            if clash {
                fs::write(dir.path().join("dir2/a.txt"), b"new here").unwrap();
            }
            disk.recover().unwrap();
            let left = names(&dir.path().join("dir2"));
            assert!(!disk.tmp_dir().join("r.landing").exists());
            if clash {
                assert_eq!(fs::read(dir.path().join("dir2/a.txt")).unwrap(), b"new here");
                assert_eq!(left.len(), 2, "{left:?}");
                let copy = left.iter().find(|n| n.starts_with("a (conflict, desk, ")).expect("a conflict copy");
                assert_eq!(fs::read(dir.path().join("dir2").join(copy)).unwrap(), b"old");
            } else {
                assert_eq!(left, ["a.txt"], "with an ID: {with_id}");
                assert_eq!(fs::read(dir.path().join("dir2/a.txt")).unwrap(), b"old");
            }
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n != AGENT_DIR).collect();
        names.sort();
        names
    }

    #[test]
    fn a_landing_cut_short_after_moving_aside_is_undone() {
        let (dir, mut disk, old) = landing_setup();
        record(&disk, old, old);
        disk.recover().unwrap();
        assert_eq!(names(dir.path()), ["a.txt"]);
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"old");
        assert!(!disk.tmp_dir().join("r.landing").exists());
    }

    #[test]
    fn a_landing_cut_short_before_the_removal_is_finished() {
        let (dir, mut disk, old) = landing_setup();
        let new = disk.write(&craft_cloud_sync::rel("a.txt"), b"new", false, 1_700_000_000_000).unwrap();
        record(&disk, old, new);
        disk.recover().unwrap();
        assert_eq!(names(dir.path()), ["a.txt"]);
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"new");
    }

    /// After a plain rename (FAT, exFAT) a file may come out with another ID; after a move through a hard link it
    /// never does. The look after the move compares the ID only in the second case.
    #[test]
    fn the_look_after_a_move_compares_the_id_only_if_the_move_kept_it() {
        let expected = FileMeta { size: 3, mtime_ms: 1_700_000_000_000, exec: false, id: Some(7) };
        let renumbered = FileMeta { id: Some(8), ..expected };
        assert!(same_file_after_move(expected, expected, Moved::Link));
        assert!(same_file_after_move(expected, expected, Moved::Plain));
        assert!(!same_file_after_move(renumbered, expected, Moved::Link), "a hard link keeps the ID: another one is another file");
        assert!(same_file_after_move(renumbered, expected, Moved::Plain), "a plain rename may renumber the file");
        for other in [FileMeta { size: 4, ..expected }, FileMeta { mtime_ms: 1, ..expected }, FileMeta { exec: true, ..expected }] {
            assert!(!same_file_after_move(other, expected, Moved::Plain), "{other:?}");
            assert!(!same_file_after_move(other, expected, Moved::Link), "{other:?}");
        }
    }

    /// The forced plain rename lands like the system's own way, and still never lands on a file that changed.
    #[test]
    fn a_landing_in_steps_with_plain_renames_lands_only_on_what_was_checked() {
        let (dir, disk, old) = landing_setup();
        fs::rename(dir.path().join("a.txt.1.cc-aside"), dir.path().join("a.txt")).unwrap();
        let mut disk = disk.without_swap().plain_moves();
        let path = craft_cloud_sync::rel("a.txt");
        // An app's save with the same size and time but another ID: refused before anything moves.
        let other_id = FileMeta { id: old.id.map(|id| id ^ 1), ..old };
        let mut w = disk.begin_write(&path).unwrap();
        w.write_all(b"new").unwrap();
        let Err(LandError::Changed(w)) = disk.finish_write(&path, w, false, 1_700_000_000_000, Some(other_id)) else { panic!("landed") };
        drop(w);
        assert_eq!(names(dir.path()), ["a.txt"]);
        let mut w = disk.begin_write(&path).unwrap();
        w.write_all(b"new").unwrap();
        disk.finish_write(&path, w, false, 1_700_000_000_000, Some(old)).unwrap();
        assert_eq!(names(dir.path()), ["a.txt"]);
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"new");
        assert_eq!(fs::read_dir(disk.tmp_dir()).unwrap().count(), 0);
    }

    /// Recovery after plain renames (FAT, exFAT): both files may have new IDs, and the landing is still finished.
    /// After hard links the IDs must match.
    #[test]
    fn recovery_compares_ids_only_when_the_moves_kept_them() {
        for ids_kept in [false, true] {
            let (dir, mut disk, old) = landing_setup();
            let new = disk.write(&craft_cloud_sync::rel("a.txt"), b"new", false, 1_700_000_000_000).unwrap();
            let renumber = |m: FileMeta| FileMeta { id: m.id.map(|id| id ^ 1), ..m };
            record_with(&disk, renumber(old), renumber(new), ids_kept);
            disk.recover().unwrap();
            let want: &[&str] = if ids_kept { &["a.txt", "a.txt.1.cc-aside"] } else { &["a.txt"] };
            assert_eq!(names(dir.path()), want, "ids kept: {ids_kept}");
            assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"new");
        }
    }

    #[test]
    fn a_moved_file_whose_place_was_taken_stays() {
        let (dir, mut disk, old) = landing_setup();
        // The download's record says one file, but a user saved another there after the crash.
        let new = FileMeta { size: 3, mtime_ms: 1, exec: false, id: Some(1) };
        fs::write(dir.path().join("a.txt"), b"the user's").unwrap();
        record(&disk, old, new);
        disk.recover().unwrap();
        assert_eq!(names(dir.path()), ["a.txt", "a.txt.1.cc-aside"]);
        assert_eq!(fs::read(dir.path().join("a.txt.1.cc-aside")).unwrap(), b"old");
        assert!(!disk.tmp_dir().join("r.landing").exists());
    }

    /// A rename through a hard link that couldn't remove the old name left one file under both names: recovery
    /// removes the extra name, and the file stays.
    #[test]
    fn a_file_left_with_two_names_loses_the_extra_one() {
        let (dir, mut disk, old) = landing_setup();
        fs::hard_link(dir.path().join("a.txt.1.cc-aside"), dir.path().join("a.txt")).unwrap();
        let incoming = FileMeta { size: 3, mtime_ms: 1, exec: false, id: Some(1) };
        record(&disk, old, incoming);
        disk.recover().unwrap();
        assert_eq!(names(dir.path()), ["a.txt"]);
        assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"old");
        assert!(!disk.tmp_dir().join("r.landing").exists());
    }

    /// A record recovery can't read would fail every pass: it is set aside as `<record>.bad` instead, touching
    /// nothing, and listed (with the target, if that much can be read) until the user removes it.
    #[test]
    fn a_damaged_landing_record_is_set_aside_and_touches_nothing() {
        for (content, check) in [
            (&b"not json"[..], "check the folder for files whose names end in .cc-aside"),
            (&br#"{"target":"a.txt","aside":7}"#[..], "check the files next to a.txt"),
            (&br#"{"target":"../a.txt","aside":"x","expected":{"size":1,"mtime_ms":1,"exec":false,"id":null},"incoming":{"size":1,"mtime_ms":1,"exec":false,"id":null}}"#[..], "check the files next to ../a.txt"),
        ] {
            let (dir, mut disk, _) = landing_setup();
            fs::write(disk.tmp_dir().join("r.landing"), content).unwrap();
            disk.recover().unwrap();
            assert_eq!(names(dir.path()), ["a.txt.1.cc-aside"]);
            assert!(!disk.tmp_dir().join("r.landing").exists());
            assert!(disk.tmp_dir().join("r.landing.bad").exists());
            let want = (
                ".craft-cloud/tmp/r.landing.bad".to_owned(),
                format!("a record of an interrupted download can't be read and was set aside; {check}, then remove .craft-cloud/tmp/r.landing.bad"),
            );
            assert_eq!(disk.notices(), std::slice::from_ref(&want));
            assert_eq!(disk.needs_you().unwrap(), [want]);
            // Later passes go on, and still list it until it is gone.
            disk.recover().unwrap();
            assert_eq!(disk.notices().len(), 1);
            fs::remove_file(disk.tmp_dir().join("r.landing.bad")).unwrap();
            disk.recover().unwrap();
            assert!(disk.notices().is_empty());
        }
    }

    /// A damaged swap record: set aside the same way, and its swap file (perhaps an app's save) is kept as `.rescued`
    /// and listed too.
    #[test]
    fn a_damaged_swap_record_is_set_aside_and_its_file_kept() {
        let (_dir, mut disk, _) = landing_setup();
        fs::write(disk.tmp_dir().join("s.swapping"), b"{").unwrap();
        fs::write(disk.tmp_dir().join("s.swap"), b"a save").unwrap();
        disk.recover().unwrap();
        assert_eq!(fs::read(disk.tmp_dir().join("s.rescued")).unwrap(), b"a save");
        let listed: Vec<String> = disk.notices().into_iter().map(|(path, _)| path).collect();
        assert_eq!(listed, [".craft-cloud/tmp/s.rescued", ".craft-cloud/tmp/s.swapping.bad"]);
        assert_eq!(disk.clean_tmp().unwrap(), 0, "never cleaned up");
    }

    #[test]
    fn names_round_trip_unless_they_need_normalising() {
        assert!(name_round_trips("café"));
        assert!(name_round_trips("a:b"), "invalid names are the engine's to report");
        assert_eq!(name_round_trips("cafe\u{301}"), cfg!(target_vendor = "apple"));
    }
}
