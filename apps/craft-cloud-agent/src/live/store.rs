//! Where the live hub keeps its documents between runs: one file per document, written whole and atomically
//! (temporary file, flush, rename), so a crash leaves the old file or the new one. A document's file holds its state,
//! what the agent has of the server's log and its local updates not yet acked, so an add is on disk before the app
//! is told it is kept.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::account::{remove_atomic_temps, write_atomic};

/// A place for the documents of one account.
pub trait DocStore: Send {
    /// Every document file there is, by name.
    fn load(&self) -> io::Result<BTreeMap<String, Vec<u8>>>;
    /// Writes one document file whole, atomically and durably.
    fn save(&mut self, name: &str, bytes: &[u8]) -> io::Result<()>;
    /// Copies a document file that doesn't read aside, durably, under a name of its own (for a person to look at:
    /// diagnosis only, the caller then writes the document it could rebuild over the original); returns that name.
    /// At most [`MAX_SET_ASIDE`] are kept, the oldest going first.
    fn set_aside(&mut self, name: &str) -> io::Result<String>;
    /// Deletes a document file (one that doesn't read at all, once set aside).
    fn remove(&mut self, name: &str) -> io::Result<()>;

    // Documents of files: files of their own kind (an older agent, which reads only the files above, leaves them
    // alone), each with a journal of the local updates made since it was last written.

    // (By default a store has none, and takes none: one for recents only.)

    /// Every document of a file there is, by name.
    fn load_documents(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        Ok(BTreeMap::new())
    }
    /// One document of a file, if there is one.
    fn load_document(&self, _name: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Writes a document of a file whole, atomically and durably.
    fn save_document(&mut self, _name: &str, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    /// Appends `record` to the document's journal, durably, before this returns.
    fn append_journal(&mut self, _name: &str, _record: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    /// The document's journal (empty if it has none).
    fn load_journal(&self, _name: &str) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    /// Empties the document's journal (its updates are in its file now).
    fn clear_journal(&mut self, _name: &str) -> io::Result<()> {
        Ok(())
    }
    /// [`DocStore::set_aside`], for a document of a file.
    fn set_aside_document(&mut self, _name: &str) -> io::Result<String> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    /// [`DocStore::remove`], for a document of a file (and its journal).
    fn remove_document(&mut self, _name: &str) -> io::Result<()> {
        Ok(())
    }
}

/// Documents of files end in this…
const DOCUMENT_SUFFIX: &str = ".doc";
/// …and their journals in this.
const JOURNAL_SUFFIX: &str = ".journal";

/// Most set-aside files kept per store.
pub const MAX_SET_ASIDE: usize = 3;

/// The documents on disk: `<data folder>/live/<space>/`, owner-only.
#[derive(Debug, Clone)]
pub struct DiskStore {
    dir: PathBuf,
}

/// Document files end in this.
const SUFFIX: &str = ".live";

impl DiskStore {
    /// The store in `dir` (made owner-only if missing; interrupted writes cleaned up).
    pub fn open(dir: &Path) -> io::Result<Self> {
        make_private_dirs(dir)?;
        // (Interrupted writes of documents and of set-aside copies.)
        remove_atomic_temps(dir, |of| of.ends_with(SUFFIX) || of.ends_with(DOCUMENT_SUFFIX) || of.contains(ASIDE), std::time::Duration::ZERO)?;
        Ok(Self { dir: dir.to_owned() })
    }

    fn load_suffix(&self, suffix: &str) -> io::Result<BTreeMap<String, Vec<u8>>> {
        let mut out = BTreeMap::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if name.ends_with(suffix) && !name.starts_with('.') && entry.file_type()?.is_file() {
                out.insert(name.trim_end_matches(suffix).to_owned(), std::fs::read(entry.path())?);
            }
        }
        Ok(out)
    }

    fn set_aside_suffix(&mut self, name: &str, suffix: &str) -> io::Result<String> {
        let bytes = std::fs::read(self.dir.join(format!("{name}{suffix}")))?;
        // Unique: the time, then random bytes (two in one millisecond, or a clock gone back, don't collide). Written
        // like a document (temporary file, fsync, rename, folder fsync).
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let to = self.dir.join(format!("{name}{suffix}{ASIDE}{stamp:013}-{count:08}-{}", crate::account::random_hex(4)?));
        write_atomic(&to, &bytes, true)?;
        // Oldest first: the name sorts by its time.
        let mut aside: Vec<(String, PathBuf)> = std::fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let file = e.file_name().to_str()?.to_owned();
                let at = file.find(ASIDE)?;
                Some((file[at..].to_owned(), e.path()))
            })
            .collect();
        aside.sort();
        let extra = aside.len().saturating_sub(MAX_SET_ASIDE);
        for (_, path) in aside.into_iter().take(extra) {
            std::fs::remove_file(path)?;
        }
        Ok(to.display().to_string())
    }

    fn sync_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

/// Makes `dir` and any missing folder above it owner-only (they sit inside the data folder, which exists).
fn make_private_dirs(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        make_private_dirs(parent)?;
    }
    match crate::disk::create_private_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        other => other,
    }
}

impl DocStore for DiskStore {
    fn load(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        let mut out = BTreeMap::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if name.ends_with(SUFFIX) && !name.starts_with('.') && entry.file_type()?.is_file() {
                out.insert(name.trim_end_matches(SUFFIX).to_owned(), std::fs::read(entry.path())?);
            }
        }
        Ok(out)
    }

    fn save(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        write_atomic(&self.dir.join(format!("{name}{SUFFIX}")), bytes, true)
    }

    fn set_aside(&mut self, name: &str) -> io::Result<String> {
        let bytes = std::fs::read(self.dir.join(format!("{name}{SUFFIX}")))?;
        // Unique: the time, then random bytes (two in one millisecond, or a clock gone back, don't collide). Written
        // like a document (temporary file, fsync, rename, folder fsync).
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let to = self.dir.join(format!("{name}{SUFFIX}{ASIDE}{stamp:013}-{count:08}-{}", crate::account::random_hex(4)?));
        write_atomic(&to, &bytes, true)?;
        // Oldest first: the name sorts by its time.
        let mut aside: Vec<(String, PathBuf)> = std::fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let file = e.file_name().to_str()?.to_owned();
                let at = file.find(ASIDE)?;
                Some((file[at..].to_owned(), e.path()))
            })
            .collect();
        aside.sort();
        let extra = aside.len().saturating_sub(MAX_SET_ASIDE);
        for (_, path) in aside.into_iter().take(extra) {
            std::fs::remove_file(path)?;
        }
        Ok(to.display().to_string())
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        std::fs::remove_file(self.dir.join(format!("{name}{SUFFIX}")))?;
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    fn load_documents(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        self.load_suffix(DOCUMENT_SUFFIX)
    }

    fn load_document(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.dir.join(format!("{name}{DOCUMENT_SUFFIX}"))) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn save_document(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        write_atomic(&self.dir.join(format!("{name}{DOCUMENT_SUFFIX}")), bytes, true)
    }

    fn append_journal(&mut self, name: &str, record: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let path = self.dir.join(format!("{name}{JOURNAL_SUFFIX}"));
        let new = !path.exists();
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(record)?;
        file.sync_data()?;
        // A journal made just now: its name is durable too.
        if new {
            self.sync_dir()?;
        }
        Ok(())
    }

    fn load_journal(&self, name: &str) -> io::Result<Vec<u8>> {
        match std::fs::read(self.dir.join(format!("{name}{JOURNAL_SUFFIX}"))) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn clear_journal(&mut self, name: &str) -> io::Result<()> {
        match std::fs::remove_file(self.dir.join(format!("{name}{JOURNAL_SUFFIX}"))) {
            Ok(()) => self.sync_dir(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn set_aside_document(&mut self, name: &str) -> io::Result<String> {
        self.set_aside_suffix(name, DOCUMENT_SUFFIX)
    }

    fn remove_document(&mut self, name: &str) -> io::Result<()> {
        std::fs::remove_file(self.dir.join(format!("{name}{DOCUMENT_SUFFIX}")))?;
        self.clear_journal(name)?;
        self.sync_dir()
    }
}

/// What a set-aside file's name has after the document's.
const ASIDE: &str = ".unreadable-";

/// Documents in memory, shared by clones: what a crashed machine of a simulation still has on its disk.
#[derive(Debug, Clone, Default)]
pub struct MemStore {
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    aside: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    set_aside_count: Arc<std::sync::atomic::AtomicU64>,
    documents: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    journals: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    /// Journal appends fail from now on (a full disk, for tests).
    failing: Arc<std::sync::atomic::AtomicBool>,
}

impl MemStore {
    /// Makes every journal append fail (`true`) or work again: a full disk, for tests.
    pub fn fail_journal(&self, fail: bool) {
        self.failing.store(fail, std::sync::atomic::Ordering::Relaxed);
    }

    /// The journals, by document (for tests that look inside).
    pub fn journals(&self) -> BTreeMap<String, Vec<u8>> {
        self.journals.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The documents of files, by name (for tests that look inside).
    pub fn documents(&self) -> BTreeMap<String, Vec<u8>> {
        self.documents.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl MemStore {
    /// The files set aside, by the name they went to.
    pub fn set_aside_files(&self) -> BTreeMap<String, Vec<u8>> {
        self.aside.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl DocStore for MemStore {
    fn load(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        Ok(self.files.lock().unwrap_or_else(PoisonError::into_inner).clone())
    }

    fn save(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn set_aside(&mut self, name: &str) -> io::Result<String> {
        let bytes = self.files.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned().ok_or_else(|| io::Error::other("no such file"))?;
        let mut aside = self.aside.lock().unwrap_or_else(PoisonError::into_inner);
        let n = self.set_aside_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let to = format!("{n:013}{ASIDE}{name}");
        aside.insert(to.clone(), bytes);
        while aside.len() > MAX_SET_ASIDE {
            aside.pop_first();
        }
        Ok(to)
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner).remove(name);
        Ok(())
    }

    fn load_documents(&self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        Ok(self.documents())
    }

    fn load_document(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.documents.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned())
    }

    fn save_document(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.documents.lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn append_journal(&mut self, name: &str, record: &[u8]) -> io::Result<()> {
        if self.failing.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::other("the disk is full (test)"));
        }
        self.journals.lock().unwrap_or_else(PoisonError::into_inner).entry(name.to_owned()).or_default().extend_from_slice(record);
        Ok(())
    }

    fn load_journal(&self, name: &str) -> io::Result<Vec<u8>> {
        Ok(self.journals.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned().unwrap_or_default())
    }

    fn clear_journal(&mut self, name: &str) -> io::Result<()> {
        self.journals.lock().unwrap_or_else(PoisonError::into_inner).remove(name);
        Ok(())
    }

    fn set_aside_document(&mut self, name: &str) -> io::Result<String> {
        let bytes = self.load_document(name)?.ok_or_else(|| io::Error::other("no such file"))?;
        let mut aside = self.aside.lock().unwrap_or_else(PoisonError::into_inner);
        let n = self.set_aside_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let to = format!("{n:013}{ASIDE}{name}{DOCUMENT_SUFFIX}");
        aside.insert(to.clone(), bytes);
        while aside.len() > MAX_SET_ASIDE {
            aside.pop_first();
        }
        Ok(to)
    }

    fn remove_document(&mut self, name: &str) -> io::Result<()> {
        self.documents.lock().unwrap_or_else(PoisonError::into_inner).remove(name);
        self.clear_journal(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Setting a file aside copies it (renaming would lose the document to a crash until the next load rebuilt it), the
    /// original stays until the caller writes over it, names are unique (not just by the millisecond), and only the
    /// newest few are kept.
    #[test]
    fn set_aside_copies_under_unique_names_and_keeps_a_few() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = DiskStore::open(dir.path()).unwrap();
        store.save("doc", b"damaged").unwrap();
        let mut names = Vec::new();
        for _ in 0..MAX_SET_ASIDE + 2 {
            names.push(store.set_aside("doc").unwrap());
        }
        assert_eq!(store.load().unwrap().get("doc").map(Vec::as_slice), Some(&b"damaged"[..]), "the original stays");
        let kept: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str().map(str::to_owned))
            .filter(|n| n.contains(ASIDE))
            .collect();
        assert_eq!(kept.len(), MAX_SET_ASIDE, "{kept:?}");
        for name in names.iter().skip(2) {
            assert!(std::path::Path::new(name).exists(), "the newest are kept: {name}");
            assert_eq!(std::fs::read(name).unwrap(), b"damaged");
        }
        store.remove("doc").unwrap();
        assert!(store.load().unwrap().is_empty());
        // An interrupted set-aside copy (its temporary file) is cleaned up like an interrupted document.
        let temp = dir.path().join(format!(".doc{SUFFIX}{ASIDE}0000000000001-00000000-abcd.0011223344556677.tmp"));
        std::fs::write(&temp, b"half").unwrap();
        DiskStore::open(dir.path()).unwrap();
        assert!(!temp.exists(), "the interrupted copy is left behind");
    }
}
