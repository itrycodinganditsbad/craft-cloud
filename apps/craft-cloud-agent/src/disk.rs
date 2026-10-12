//! The real filesystem: [`DiskFs`] gives the sync engine a folder on this computer.
//!
//! - **Links are never followed.** Symlinks, junctions and other special files are [`FsKind::Other`]. Every
//!   operation checks that the folders above its path are real folders, so a folder the user replaced with a
//!   link is not walked into. (The check and the operation are separate system calls, so a link swapped in
//!   between them is still followed; closing that needs `openat`-style calls and comes with the watcher.)
//! - **The root itself may be a link.** A synced folder that is a symlink or junction (the user's choice) is
//!   followed and is a folder; links *inside* it are still [`FsKind::Other`].
//! - **Writes are atomic.** New content goes to a temporary file in `<root>/.craft-cloud/tmp/` (same volume,
//!   so the rename is atomic), is flushed to disk, gets the replaced file's permissions (Unix), its modification
//!   time and executable bit, and is then renamed over the target. Any error, or a writer dropped without
//!   [`Fs::finish_write`], removes the temporary file.
//! - **In-flight downloads are private (Unix).** `.craft-cloud/` and `tmp/` are created with mode 0700 when the
//!   agent makes them, so other users can't read a partly downloaded file. (Folders that already exist keep their
//!   mode.) Windows folders get the default ACL of the synced folder.
//! - **Renames never replace.** On Linux and macOS the OS's no-replace rename (`renameat2(RENAME_NOREPLACE)`,
//!   `renameatx_np(RENAME_EXCL)`) does it atomically. Elsewhere (Windows), and on filesystems that refuse the
//!   flag, the target is checked right before a plain rename: a file another program creates at the target in
//!   that instant is replaced. The window is a few microseconds and needs a program racing the agent for a
//!   name the agent just checked.
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
//!   goes up again; history keeps the old node), and files are hashed more often. Windows read-only files can't
//!   be replaced by a rename: a remote edit to one fails and is retried, the local file is never overwritten.
//! - **The root lists only with its marker.** Listing the root fails unless `.craft-cloud/root.json` is there as
//!   a regular file, so a folder emptied or unmounted in the middle of a pass fails the pass instead of looking
//!   like everything was deleted.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use craft_cloud_proto::Name;
use craft_cloud_sync::{FileMeta, Fs, FsEntry, FsKind, RelPath};
use file_id::FileId;

/// The agent's own folder inside the sync root. The engine never syncs it.
pub const AGENT_DIR: &str = ".craft-cloud";
/// Temporary files, inside [`AGENT_DIR`].
const TMP_DIR: &str = "tmp";
/// Extension of temporary files (also ignored by the engine).
const TMP_EXT: &str = "part";
/// Largest buffer `read_at` allocates up front; longer reads grow as data arrives.
const MAX_READ_PREALLOC: usize = 4 * 1024 * 1024;

/// A folder on disk, as the engine sees it.
#[derive(Debug, Clone)]
pub struct DiskFs {
    root: PathBuf,
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
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    use rustix::io::Errno;
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // The filesystem or kernel doesn't support the flag (EINVAL on Linux, ENOTSUP on macOS).
        Err(e) if [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP].contains(&e) => check_then_rename(from, to),
        // A case-only rename on a case-insensitive filesystem: the "existing" target is the source itself.
        Err(e) if e == Errno::EXIST && same_entry(from, to) => fs::rename(from, to),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    check_then_rename(from, to)
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

/// Windows: the new file gets the folder's default ACL, like any file an app saves with temp + rename. The
/// read-only attribute isn't copied: a read-only file can't be replaced by a rename, so its download fails and
/// is retried (the user's read-only file is never overwritten).
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
        Self { root: root.into() }
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

    /// Removes temporary files a crashed pass left behind. Call it before a pass, while no other pass runs on
    /// this folder. Returns how many it removed.
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

    fn finish_write(&mut self, path: &RelPath, mut writer: DiskWriter, exec: bool, mtime_ms: i64) -> io::Result<FileMeta> {
        let full = self.file_target(path)?;
        let file = writer.file.take().ok_or_else(|| io::Error::other("the write already finished"))?;
        set_mode(&file, Self::lstat(&full)?.as_ref(), exec)?;
        file.set_modified(system_time(mtime_ms)?)?;
        file.sync_all()?;
        let md = file.metadata()?;
        // The ID is read before the rename (Windows reads it by path); a rename on one volume keeps it.
        let meta = file_meta(&md, &writer.temp);
        drop(file);
        fs::rename(&writer.temp, &full)?;
        writer.armed = false;
        if let Some(parent) = full.parent() {
            sync_dir(parent);
        }
        Ok(meta)
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

    #[test]
    fn names_round_trip_unless_they_need_normalising() {
        assert!(name_round_trips("café"));
        assert!(name_round_trips("a:b"), "invalid names are the engine's to report");
        assert_eq!(name_round_trips("cafe\u{301}"), cfg!(target_vendor = "apple"));
    }
}
