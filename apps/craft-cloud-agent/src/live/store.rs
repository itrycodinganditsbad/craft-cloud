//! Where the live hub keeps its documents between runs: one file per document, written whole and atomically
//! (temporary file, flush, rename), so a crash leaves the old file or the new one. A document's file holds its state,
//! what the agent has of the server's log and its local updates not yet acked, so an add is on disk before the app
//! is told it is kept.
//!
//! A document's journal is one file for as long as the document is here: appended to, cut back, and emptied in place
//! once its updates are in the document's file, never deleted and made again. (On Windows a virus scanner or the
//! search indexer opens files the agent just wrote; one that doesn't share the journal for deletion made the
//! deletion fail, and a deleted name another program still holds can stay "delete pending" and refuse a new file of
//! that name.) A holder that doesn't share the file for what the store does is waited for, about a second and a half
//! at most ([`crate::account::retry_busy`]). Files that do go (a document that doesn't read) are renamed to a name of
//! their own first, so their names are free at once ([`crate::account::retire`]).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::account::{remove_atomic_temps, retire, retry_busy, write_atomic};

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

    /// The names of every document of a file there is (each read with [`DocStore::load_document`], one at a time).
    fn document_names(&self) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// One document of a file, if there is one.
    fn load_document(&self, _name: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Writes a document of a file whole, atomically and durably.
    fn save_document(&mut self, _name: &str, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    /// Appends `record` to the document's journal, durably, before this returns. On an error the journal is cut back
    /// to where it was, as far as the store can (the caller cuts it back too, and writes the document whole).
    fn append_journal(&mut self, _name: &str, _record: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    /// Cuts the document's journal to its first `len` bytes (what follows doesn't read, or was never kept), durably.
    fn truncate_journal(&mut self, _name: &str, _len: usize) -> io::Result<()> {
        Ok(())
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
        // (Interrupted writes of documents and of set-aside copies, and removals a crash interrupted.)
        remove_atomic_temps(
            dir,
            |of| of.ends_with(SUFFIX) || of.ends_with(DOCUMENT_SUFFIX) || of.ends_with(JOURNAL_SUFFIX) || of.contains(ASIDE),
            std::time::Duration::ZERO,
        )?;
        Ok(Self { dir: dir.to_owned() })
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
        // Oldest first: the name sorts by its time. Each kind of file keeps its own few.
        let kind = format!("{suffix}{ASIDE}");
        let mut aside: Vec<(String, PathBuf)> = std::fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let file = e.file_name().to_str()?.to_owned();
                let at = file.find(&kind)?;
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
        self.set_aside_suffix(name, SUFFIX)
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        // (Written again soon: the document is filled from the server.)
        if !retire(&self.dir.join(format!("{name}{SUFFIX}")))? {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        self.sync_dir()
    }

    fn document_names(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if let Some(doc) = name.strip_suffix(DOCUMENT_SUFFIX)
                && !name.starts_with('.')
                && entry.file_type()?.is_file()
            {
                out.push(doc.to_owned());
            }
        }
        out.sort();
        Ok(out)
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
        let mut file = retry_busy(|| std::fs::OpenOptions::new().create(true).append(true).open(&path))?;
        let before = file.metadata()?.len();
        if let Err(e) = file.write_all(record).and_then(|()| file.sync_data()) {
            // Not kept: whatever part of it got there goes (a record cut off, or one whose flush failed, must not come
            // back after a crash as a change the app was told wasn't kept, nor hide the records after it).
            let _ = file.set_len(before).and_then(|()| file.sync_data());
            return Err(e);
        }
        // A journal made just now: its name is durable too.
        if new {
            self.sync_dir()?;
        }
        Ok(())
    }

    fn truncate_journal(&mut self, name: &str, len: usize) -> io::Result<()> {
        cut(&self.dir.join(format!("{name}{JOURNAL_SUFFIX}")), len as u64)
    }

    fn load_journal(&self, name: &str) -> io::Result<Vec<u8>> {
        match std::fs::read(self.dir.join(format!("{name}{JOURNAL_SUFFIX}"))) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn clear_journal(&mut self, name: &str) -> io::Result<()> {
        // Emptied in place, not deleted: the next change's append finds the same file (see the module's doc). A crash
        // before the cut is durable leaves the records, which the document's file has now, and a load skips them.
        cut(&self.dir.join(format!("{name}{JOURNAL_SUFFIX}")), 0)
    }

    fn set_aside_document(&mut self, name: &str) -> io::Result<String> {
        self.set_aside_suffix(name, DOCUMENT_SUFFIX)
    }

    fn remove_document(&mut self, name: &str) -> io::Result<()> {
        // (Written again soon, with a new journal: the document is filled from the server.)
        if !retire(&self.dir.join(format!("{name}{DOCUMENT_SUFFIX}")))? {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        retire(&self.dir.join(format!("{name}{JOURNAL_SUFFIX}")))?;
        self.sync_dir()
    }
}

/// Cuts the file at `path` to its first `len` bytes if it is longer, durably (a missing file is empty already). A
/// holder that doesn't share it for writing is waited for.
fn cut(path: &Path, len: u64) -> io::Result<()> {
    let file = match retry_busy(|| std::fs::OpenOptions::new().write(true).open(path)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if file.metadata()?.len() > len {
        file.set_len(len)?;
        file.sync_data()?;
    }
    Ok(())
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
    /// How journal appends fail from now on ([`JournalFailure`] as a number; 0: they don't), for tests.
    failing: Arc<std::sync::atomic::AtomicU8>,
}

/// How a [`MemStore`]'s journal appends fail, for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalFailure {
    /// Nothing is written (a full disk).
    Nothing,
    /// Half the record is written, and stays (a store that couldn't cut it back).
    Torn,
    /// The whole record is written, and stays, but the flush fails.
    Unflushed,
}

impl MemStore {
    /// Makes every journal append fail (`true`, writing nothing) or work again: a full disk, for tests.
    pub fn fail_journal(&self, fail: bool) {
        self.fail_journal_as(fail.then_some(JournalFailure::Nothing));
    }

    /// Makes every journal append fail as `how`, or work again (`None`).
    pub fn fail_journal_as(&self, how: Option<JournalFailure>) {
        let n = match how {
            None => 0,
            Some(JournalFailure::Nothing) => 1,
            Some(JournalFailure::Torn) => 2,
            Some(JournalFailure::Unflushed) => 3,
        };
        self.failing.store(n, std::sync::atomic::Ordering::Relaxed);
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

    fn document_names(&self) -> io::Result<Vec<String>> {
        Ok(self.documents().into_keys().collect())
    }

    fn load_document(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.documents.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned())
    }

    fn save_document(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.documents.lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn append_journal(&mut self, name: &str, record: &[u8]) -> io::Result<()> {
        let mut journals = self.journals.lock().unwrap_or_else(PoisonError::into_inner);
        match self.failing.load(std::sync::atomic::Ordering::Relaxed) {
            0 => {}
            1 => return Err(io::Error::other("the disk is full (test)")),
            2 => {
                journals.entry(name.to_owned()).or_default().extend_from_slice(record.get(..record.len() / 2).unwrap_or_default());
                return Err(io::Error::other("the disk filled up mid-write (test)"));
            }
            _ => {
                journals.entry(name.to_owned()).or_default().extend_from_slice(record);
                return Err(io::Error::other("the flush failed (test)"));
            }
        }
        journals.entry(name.to_owned()).or_default().extend_from_slice(record);
        Ok(())
    }

    fn truncate_journal(&mut self, name: &str, len: usize) -> io::Result<()> {
        if let Some(journal) = self.journals.lock().unwrap_or_else(PoisonError::into_inner).get_mut(name) {
            journal.truncate(len);
        }
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

    /// Another program (a virus scanner, the search indexer, a backup tool) holding `path` open, sharing it as
    /// `share` (`FILE_SHARE_READ` 1, `FILE_SHARE_WRITE` 2, `FILE_SHARE_DELETE` 4).
    #[cfg(windows)]
    fn hold(path: &Path, share: u32) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).share_mode(share).open(path).unwrap()
    }

    /// A journal is emptied in place once its updates are in the document's file (the same file takes the next
    /// change); removing a document leaves neither its file, nor its journal, nor a temporary name behind; and a
    /// removal a crash cut short (the renamed file) is cleaned up at the next open.
    #[test]
    fn a_journal_is_emptied_in_place_and_a_removed_document_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join(format!("doc{JOURNAL_SUFFIX}"));
        let mut store = DiskStore::open(dir.path()).unwrap();
        store.save_document("doc", b"document").unwrap();
        store.append_journal("doc", b"first").unwrap();
        store.clear_journal("doc").unwrap();
        assert_eq!(std::fs::metadata(&journal).unwrap().len(), 0, "the journal stays, empty");
        assert!(store.load_journal("doc").unwrap().is_empty());
        store.append_journal("doc", b"second").unwrap();
        assert_eq!(store.load_journal("doc").unwrap(), b"second");
        store.clear_journal("never-journalled").unwrap();
        assert!(!dir.path().join(format!("never-journalled{JOURNAL_SUFFIX}")).exists());
        store.remove_document("doc").unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(store.remove_document("doc").is_err(), "a document that isn't there");
        let interrupted = dir.path().join(format!(".doc{JOURNAL_SUFFIX}.0011223344556677.tmp"));
        std::fs::write(&interrupted, b"records").unwrap();
        DiskStore::open(dir.path()).unwrap();
        assert!(!interrupted.exists(), "the interrupted removal is left behind");
    }

    /// A scanner holding a document's journal without delete sharing doesn't fail the document's write (its journal
    /// emptied once its updates are in its file) nor the next change's append.
    #[cfg(windows)]
    #[test]
    fn a_journal_held_without_delete_sharing_is_still_emptied_and_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = DiskStore::open(dir.path()).unwrap();
        store.save_document("doc", b"document").unwrap();
        store.append_journal("doc", b"first").unwrap();
        let _scanner = hold(&dir.path().join(format!("doc{JOURNAL_SUFFIX}")), 1 | 2);
        store.save_document("doc", b"document, first in").unwrap();
        store.clear_journal("doc").unwrap();
        store.append_journal("doc", b"second").unwrap();
        assert_eq!(store.load_journal("doc").unwrap(), b"second");
    }

    /// A scanner holding the journal for a moment without sharing it for writing delays an append, a cut and an
    /// emptying; it doesn't fail them.
    #[cfg(windows)]
    #[test]
    fn a_journal_held_for_a_moment_delays_its_writes() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join(format!("doc{JOURNAL_SUFFIX}"));
        let mut store = DiskStore::open(dir.path()).unwrap();
        store.save_document("doc", b"document").unwrap();
        store.append_journal("doc", b"first").unwrap();
        let held_for_a_moment = |store: &mut DiskStore, what: &dyn Fn(&mut DiskStore) -> io::Result<()>| {
            let scanner = hold(&journal, 1);
            let release = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(150));
                drop(scanner);
            });
            let done = what(store);
            release.join().unwrap();
            done
        };
        held_for_a_moment(&mut store, &|s| s.append_journal("doc", b"second")).unwrap();
        assert_eq!(store.load_journal("doc").unwrap(), b"firstsecond");
        held_for_a_moment(&mut store, &|s| s.truncate_journal("doc", 5)).unwrap();
        assert_eq!(store.load_journal("doc").unwrap(), b"first");
        held_for_a_moment(&mut store, &|s| s.clear_journal("doc")).unwrap();
        assert!(store.load_journal("doc").unwrap().is_empty());
    }

    /// The lead this was found from: a file deleted while another program holds it (sharing it for deletion, as
    /// scanners do) used to keep its name "delete pending" on Windows, and making a file of that name failed with
    /// ERROR_ACCESS_DENIED until the last handle closed. A journal is never deleted and made again now; this checks
    /// the store's own way of removing a document frees its names at once either way.
    #[cfg(windows)]
    #[test]
    fn a_document_removed_while_held_can_be_written_again_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = DiskStore::open(dir.path()).unwrap();
        store.save_document("doc", b"damaged").unwrap();
        store.append_journal("doc", b"first").unwrap();
        let _scanners =
            (hold(&dir.path().join(format!("doc{DOCUMENT_SUFFIX}")), 1 | 2 | 4), hold(&dir.path().join(format!("doc{JOURNAL_SUFFIX}")), 1 | 2 | 4));
        store.remove_document("doc").unwrap();
        store.save_document("doc", b"rebuilt").unwrap();
        store.append_journal("doc", b"again").unwrap();
        assert_eq!(store.load_document("doc").unwrap().as_deref(), Some(&b"rebuilt"[..]));
        assert_eq!(store.load_journal("doc").unwrap(), b"again");
    }

    /// Recents files and documents of files each keep their own few set-aside copies: many of one kind never push out
    /// the other's.
    #[test]
    fn recents_and_documents_keep_set_aside_copies_of_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = DiskStore::open(dir.path()).unwrap();
        store.save("recents", b"damaged list").unwrap();
        store.save_document("doc", b"damaged document").unwrap();
        store.set_aside("recents").unwrap();
        for _ in 0..MAX_SET_ASIDE + 2 {
            store.set_aside_document("doc").unwrap();
        }
        let aside: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str().map(str::to_owned))
            .filter(|n| n.contains(ASIDE))
            .collect();
        assert_eq!(aside.iter().filter(|n| n.contains(&format!("{SUFFIX}{ASIDE}"))).count(), 1, "the recents copy stays: {aside:?}");
        assert_eq!(aside.iter().filter(|n| n.contains(&format!("{DOCUMENT_SUFFIX}{ASIDE}"))).count(), MAX_SET_ASIDE, "{aside:?}");
    }

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
