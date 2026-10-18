//! `run.log`: where the agent writes what it says when it has no console to say it on: the background build
//! (`craft-cloud-agent-bg`, every command) and `run --log` (what every start-at-login entry runs). It is kept under
//! [`MAX_LOG_BYTES`]: past that, it becomes `run.log.1` (replacing an older one) and a new `run.log` starts, so at most
//! twice that is kept.
//!
//! One log takes everything: the command's output and errors (the [`RunLog`] is handed to it as both), and the notes
//! parts of the agent make on their own ([`note`]: a lock that can't be held, the live link's state, a status file
//! that can't be written), which go to standard error while no log is [`install`]ed.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// The log's name in the data folder.
pub const LOG_FILE: &str = "run.log";
/// The size at which the log moves to `run.log.1`: 10 MiB.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// The process's log, once one is installed.
static INSTALLED: Mutex<Option<RunLog>> = Mutex::new(None);

/// Makes `log` the place [`note`] writes to, for the rest of the process.
pub fn install(log: RunLog) {
    *INSTALLED.lock().unwrap_or_else(PoisonError::into_inner) = Some(log);
}

/// Says `line` (one line, without its line break) in the installed log, or on standard error without one. A failed
/// write is dropped: a note never stops the agent.
pub fn note(line: std::fmt::Arguments<'_>) {
    let log = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner).clone();
    match log {
        Some(mut log) => {
            let _ = writeln!(log, "{line}");
            let _ = log.flush();
        }
        None => {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
    }
}

/// Opens `run.log` in the data folder `data` (creating the folder) and installs it ([`install`]); `None` if it can't
/// be opened (the agent then runs without one, its output going nowhere).
pub fn open_in(data: &crate::account::DataDir) -> Option<RunLog> {
    data.create().ok()?;
    let log = RunLog::open(&data.path().join(LOG_FILE), MAX_LOG_BYTES).ok()?;
    install(log.clone());
    Some(log)
}

/// An appending log file that rotates at a size. Cheap to clone: clones write to the same file, a write at a time.
#[derive(Clone)]
pub struct RunLog {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    path: PathBuf,
    file: File,
    written: u64,
    max: u64,
}

impl RunLog {
    /// Opens (or creates) the log at `path`, rotating it at `max` bytes. A log already over `max` is rotated first.
    pub fn open(path: &Path, max: u64) -> std::io::Result<Self> {
        let written = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if written >= max {
            rotate(path)?;
        }
        let file = append(path)?;
        let written = file.metadata()?.len();
        Ok(Self { inner: Arc::new(Mutex::new(Inner { path: path.to_owned(), file, written, max })) })
    }

    /// The rotated log: `run.log.1` next to `path`.
    pub fn rotated(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".1");
        path.with_file_name(name)
    }
}

fn append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotate(path: &Path) -> std::io::Result<()> {
    match std::fs::rename(path, RunLog::rotated(path)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl Write for RunLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if inner.written > 0 && inner.written.saturating_add(len) > inner.max {
            inner.file.flush()?;
            rotate(&inner.path)?;
            inner.file = append(&inner.path)?;
            inner.written = 0;
        }
        inner.file.write_all(buf)?;
        inner.written = inner.written.saturating_add(len);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_rotates_at_its_size_and_keeps_one_old_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let mut log = RunLog::open(&path, 100).unwrap();
        let line = [b'a'; 44];
        for _ in 0..2 {
            log.write_all(&line).unwrap();
            log.write_all(b"\n").unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 90);
        assert!(!RunLog::rotated(&path).exists());
        // The next line would take it past 100 bytes: the log moves to run.log.1 first.
        log.write_all(b"third line\n").unwrap();
        assert_eq!(std::fs::read(RunLog::rotated(&path)).unwrap().len(), 90);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "third line\n");
        // And again: the older one is replaced, so one is kept.
        for _ in 0..10 {
            log.write_all(b"0123456789\n").unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() <= 100);
        assert!(std::fs::read_to_string(RunLog::rotated(&path)).unwrap().starts_with("third line\n"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // A log over the size when it is opened is rotated first.
        drop(log);
        std::fs::write(&path, vec![b'x'; 150]).unwrap();
        let mut log = RunLog::open(&path, 100).unwrap();
        log.write_all(b"fresh\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        assert_eq!(std::fs::metadata(RunLog::rotated(&path)).unwrap().len(), 150);
    }

    /// What used to go straight to standard error (`eprintln!`) goes to the installed log once there is one.
    #[test]
    fn notes_go_to_the_installed_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        install(RunLog::open(&path, 1000).unwrap());
        note(format_args!("a note for the log"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("a note for the log\n"));
    }

    #[test]
    fn clones_write_to_the_same_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let mut a = RunLog::open(&path, 1000).unwrap();
        let mut b = a.clone();
        a.write_all(b"one\n").unwrap();
        b.write_all(b"two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }
}
