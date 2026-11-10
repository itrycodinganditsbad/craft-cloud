//! `backup` and `restore`: the server's data on a file server that can be reached over SFTP.
//!
//! A backup is a consistent copy of the SQLite database (`VACUUM INTO`, taken while the server runs) and a copy of the
//! blob store. Blobs are content-addressed and never change, so they are copied incrementally: a name that is already
//! on the backup server is skipped. The layout below the backup directory:
//!
//! ```text
//! chunks/ab/cd/<id>, manifests/ab/cd/<id>   the blobs, as the store holds them
//! generations/<UTC stamp>-<id>/meta.sqlite.enc  one database dump, sealed (see [`crate::backup_auth`])
//! generations/<UTC stamp>-<id>/inventory.txt    every blob of the backup: kind, ID and size, sorted
//! generations/<UTC stamp>-<id>/manifest.json    written last: its presence marks the generation as complete; carries a MAC
//!
//! A generation's name is the second the backup began in and a random UUIDv7, so two backups that run at once (or in
//! the same second) write to directories of their own and never into each other's files. Names without the `-<id>`
//! (written by the first version) are still read.
//! ```
//!
//! The database is dumped first and the blobs after it: a blob the dump refers to was uploaded before the commit that
//! names it, so the copy that follows contains it. A restore reads the newest complete generation (or the one asked
//! for), checks the manifest's MAC under the backup key before it believes any field, opens the dump, and copies the
//! blobs the inventory lists into an empty data directory. The backup server is not trusted: it can read the blobs, but
//! not the database, and it cannot make a restore accept anything it changed. Generations from before backups were
//! authenticated (`meta.sqlite`, schema 1) are refused unless `--allow-unauthenticated` is given.
//!
//! Not in a backup, on purpose: `config.toml` (it holds secrets), the two-factor key and the backup key. Keep copies of
//! all three apart from the backup (the backup key above all: without it nothing can be restored). Postgres is not dumped here; use `pg_dump` (see `docs/server.md`).

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use craft_cloud_meta::Meta;
use craft_cloud_store::sftp::{DEFAULT_CONNECTIONS, DEFAULT_RETRIES, DEFAULT_TIMEOUT_SECS, SftpBackend, SftpConfig};
use craft_cloud_store::{Backend, BlobKey, BlobKind, MAX_LIST_PAGE_SIZE, Store, StoreError};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::Value;

use crate::backup_auth::{self, BackupKey};
use crate::cli::{CliError, create_data_dir, open_existing, restrict, tuned};
use crate::config::{BackupConfig, Config};

/// How many database generations stay on the backup server when `--keep` and `[backup] keep` are not given.
pub const DEFAULT_KEEP: u32 = 7;
/// Blobs copied at once.
const COPY_PARALLEL: usize = 8;
/// The most a generation's manifest may be.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const GENERATIONS: &str = "generations";
/// The plain dump (the name schema 1 generations carry, and the working file of a backup).
const DUMP_NAME: &str = "meta.sqlite";
const SEALED_DUMP_NAME: &str = "meta.sqlite.enc";
const INVENTORY_NAME: &str = "inventory.txt";
const MANIFEST_NAME: &str = "manifest.json";

/// What a backup target must do besides the store's byte operations: whole files, and removing an empty directory.
pub trait Remote: Backend + Clone {
    fn put_file(&self, path: &str, local: &Path) -> impl Future<Output = Result<(), StoreError>> + Send;
    /// `false`: no such object. An object larger than `max_bytes` is refused before it is read.
    fn get_file(&self, path: &str, local: &Path, max_bytes: u64) -> impl Future<Output = Result<bool, StoreError>> + Send;
    fn remove_dir(&self, path: &str) -> impl Future<Output = Result<(), StoreError>> + Send;
}

impl Remote for SftpBackend {
    async fn put_file(&self, path: &str, local: &Path) -> Result<(), StoreError> {
        self.put_file_raw(path, local).await
    }

    async fn get_file(&self, path: &str, local: &Path, max_bytes: u64) -> Result<bool, StoreError> {
        self.get_file_raw(path, local, max_bytes).await
    }

    async fn remove_dir(&self, path: &str) -> Result<(), StoreError> {
        self.remove_dir_raw(path).await
    }
}

/// `sftp://user@host[:port]/directory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpUrl {
    pub user: String,
    pub host: String,
    pub port: u16,
    /// Absolute (`/srv/backup`), or relative to the account's home directory (`/~/backup` in the URL).
    pub root: String,
}

/// Reads `sftp://user@host[:port]/directory`. A password in the URL is refused: keys are the only way in.
pub fn parse_sftp_url(text: &str) -> Result<SftpUrl, String> {
    let bad = |why: &str| format!("{why}; write the address as sftp://user@host[:port]/directory (/~/directory for one in the account's home)");
    let rest = text.trim().strip_prefix("sftp://").ok_or_else(|| bad("the address does not start with sftp://"))?;
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(authority, path)| (authority, path));
    let (user, host_port) = authority.rsplit_once('@').ok_or_else(|| bad("the address has no user name"))?;
    if user.contains(':') {
        return Err("a password in the address is refused: Craft Cloud signs in with a key (--key-file)".into());
    }
    let decode =
        |text: &str| percent_encoding::percent_decode_str(text).decode_utf8().map(|decoded| decoded.into_owned()).map_err(|_| bad("not valid UTF-8"));
    let user = decode(user)?;
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or_else(|| bad("an unclosed ["))?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port),
            None if after.is_empty() => None,
            None => return Err(bad("text after the ] of the host")),
        };
        (host.to_owned(), port)
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (host_port.to_owned(), None),
        }
    };
    let port = match port {
        Some(port) => port.parse::<u16>().ok().filter(|port| *port != 0).ok_or_else(|| bad("the port is not 1 to 65535"))?,
        None => 22,
    };
    if user.is_empty() || host.is_empty() {
        return Err(bad("the user or the host is empty"));
    }
    let path = decode(path)?;
    let root = match path.as_str() {
        "" => return Err(bad("the address names no directory")),
        "~" => String::new(),
        other => match other.strip_prefix("~/") {
            Some(home_relative) => home_relative.trim_end_matches('/').to_owned(),
            None => format!("/{}", other.trim_end_matches('/')),
        },
    };
    Ok(SftpUrl { user, host, port, root })
}

/// Where the backup goes and how to sign in, from the command line, then the `[backup]` section, then the environment.
#[derive(Debug, Clone, Default)]
pub struct RemoteOptions {
    pub url: Option<String>,
    pub key_file: Option<PathBuf>,
    pub host_key: Option<String>,
    pub known_hosts: Option<PathBuf>,
    /// The backup key (`--backup-key-file`); else `[backup] backup_key_file`; else `backup.key` beside the config.
    pub backup_key_file: Option<PathBuf>,
}

/// The SFTP settings for the backup server. `config` is the server's config when there is one (a restore has none).
pub fn sftp_settings(options: &RemoteOptions, config: Option<&Config>) -> Result<SftpConfig, CliError> {
    let section: Option<&BackupConfig> = config.and_then(|config| config.backup.as_ref());
    let resolve = |path: &Path| config.map_or_else(|| path.to_owned(), |config| config.key_path(path));
    let url_text = options
        .url
        .clone()
        .or_else(|| section.and_then(|section| section.to.clone()))
        .ok_or_else(|| CliError::Usage("no backup address: give --to/--from sftp://user@host/directory (or set [backup] to)".into()))?;
    let url = parse_sftp_url(&url_text).map_err(CliError::Usage)?;
    let key_file = match options.key_file.clone().or_else(|| section.and_then(|section| section.key_file.as_deref().map(resolve))) {
        Some(path) => path,
        None => std::env::var_os("CRAFT_CLOUD_SFTP_KEY_FILE")
            .map(PathBuf::from)
            .ok_or_else(|| CliError::Usage("no SSH key: give --key-file (or set [backup] key_file or CRAFT_CLOUD_SFTP_KEY_FILE)".into()))?,
    };
    let key_passphrase = section
        .and_then(|section| section.key_passphrase.as_ref().map(|secret| secret.0.clone()))
        .or_else(|| std::env::var("CRAFT_CLOUD_SFTP_KEY_PASSPHRASE").ok());
    let host_key = options.host_key.clone().or_else(|| section.and_then(|section| section.host_key.clone()));
    let known_hosts = options.known_hosts.clone().or_else(|| section.and_then(|section| section.known_hosts.as_deref().map(resolve)));
    if host_key.is_none() && known_hosts.is_none() {
        return Err(CliError::Usage(
            "the backup server's host key must be pinned: give --host-key (a SHA256: fingerprint or a public key line) or --known-hosts; there is no mode that connects without checking the server"
                .into(),
        ));
    }
    Ok(SftpConfig {
        host: url.host,
        port: url.port,
        user: url.user,
        root: url.root,
        key_file,
        key_passphrase,
        host_key,
        known_hosts,
        connections: DEFAULT_CONNECTIONS,
        timeout: std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        retries: DEFAULT_RETRIES,
    })
}

/// Where the backup key is: `[backup] backup_key_file`, or `backup.key` beside `config.toml`.
pub fn backup_key_path(config: &Config) -> PathBuf {
    let named = config.backup.as_ref().and_then(|section| section.backup_key_file.as_deref());
    config.key_path(named.unwrap_or(Path::new(backup_auth::DEFAULT_KEY_FILE)))
}

/// `backup` to an SFTP server.
pub async fn backup(
    config: &Config,
    options: &RemoteOptions,
    keep: Option<u32>,
    work_dir: Option<&Path>,
    out: &mut impl Write,
) -> Result<(), CliError> {
    require_sqlite(config)?;
    let settings = sftp_settings(options, Some(config))?;
    let key_path = options.backup_key_file.clone().unwrap_or_else(|| backup_key_path(config));
    let remote = SftpBackend::connect_for_backup(&settings).await?;
    let keep = keep.or_else(|| config.backup.as_ref().and_then(|section| section.keep)).unwrap_or(DEFAULT_KEEP);
    backup_with_key(config, &key_path, remote, keep, work_dir, crate::state::now_ms(), out).await
}

/// `restore` from an SFTP server into `dir`. The backup key comes from `options.backup_key_file`, else from the config
/// (`config` is optional: a restore usually runs on a machine that has none yet).
pub async fn restore(
    options: &RemoteOptions,
    config: Option<&Config>,
    limits: RestoreLimits,
    allow_unauthenticated: bool,
    dir: &Path,
    generation: Option<&str>,
    out: &mut impl Write,
) -> Result<(), CliError> {
    check_empty(dir)?;
    let settings = sftp_settings(options, config)?;
    let key_file = options.backup_key_file.clone().or_else(|| config.map(backup_key_path));
    let remote = SftpBackend::connect_for_restore(&settings).await?;
    let restore_options = RestoreOptions { key_file, allow_unauthenticated, limits, ..RestoreOptions::default() };
    restore_from(remote, dir, generation, &restore_options, out).await
}

fn require_sqlite(config: &Config) -> Result<(), CliError> {
    if config.database.url.starts_with("sqlite:") {
        return Ok(());
    }
    Err(CliError::Usage(
        "backup copies a SQLite database. This server uses Postgres: dump it with `pg_dump` (see docs/server.md). This command does not run pg_dump and does not back up the blobs of a Postgres server"
            .into(),
    ))
}

fn check_empty(dir: &Path) -> Result<(), CliError> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        return Err(CliError::Usage(format!("{} is not empty; restore needs an empty (or missing) data directory", dir.display())));
    }
    Ok(())
}

/// What one pass over a kind of blob did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Copied {
    pub seen: u64,
    pub copied: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub foreign: u64,
}

/// What `restore` may use up, whatever the backup server says it holds. Every limit is enforced while the data arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreLimits {
    /// The most the database may be once opened.
    pub max_db_bytes: u64,
    /// The most the whole restore may write: the database and all blobs.
    pub max_total_bytes: u64,
    /// The most blobs a restore fetches.
    pub max_objects: u64,
    /// Free space that must remain on the target's filesystem, before and during the restore.
    pub min_free_bytes: u64,
}

pub const DEFAULT_MAX_DB_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_OBJECTS: u64 = 50_000_000;
pub const DEFAULT_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

impl Default for RestoreLimits {
    fn default() -> Self {
        Self {
            max_db_bytes: DEFAULT_MAX_DB_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_objects: DEFAULT_MAX_OBJECTS,
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
        }
    }
}

/// Free bytes on the filesystem that holds a directory; `None` when it cannot be told.
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

fn real_free_space() -> FreeSpace {
    Arc::new(|dir: &Path| fs4::statvfs(dir).ok().map(|stats| stats.available_space()))
}

/// How a restore runs.
#[derive(Clone)]
pub struct RestoreOptions {
    /// The backup key file. Needed for every generation made with authentication.
    pub key_file: Option<PathBuf>,
    /// Restore a generation from before backups were authenticated (nothing proves it is what was backed up).
    pub allow_unauthenticated: bool,
    pub limits: RestoreLimits,
    /// How free space is measured; the real measure unless a test swaps it.
    pub free_space: FreeSpace,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self { key_file: None, allow_unauthenticated: false, limits: RestoreLimits::default(), free_space: real_free_space() }
    }
}

/// What a restore has used so far, against its limits.
struct Budget {
    limits: RestoreLimits,
    free_space: FreeSpace,
    dir: PathBuf,
    bytes: u64,
    objects: u64,
}

impl Budget {
    fn new(options: &RestoreOptions, dir: &Path) -> Self {
        Self { limits: options.limits, free_space: options.free_space.clone(), dir: dir.to_owned(), bytes: 0, objects: 0 }
    }

    /// The disk must hold `more` bytes and still leave the reserve.
    fn check_free(&self, more: u64) -> Result<(), CliError> {
        let Some(free) = (self.free_space)(&self.dir) else { return Ok(()) };
        if free < self.limits.min_free_bytes.saturating_add(more) {
            return Err(CliError::Usage(format!(
                "not enough free space on the target's disk: {free} bytes are free, the restore needs {more} more and {} must stay free \
                 (--min-free-bytes); nothing was restored",
                self.limits.min_free_bytes
            )));
        }
        Ok(())
    }

    /// Counts one blob that is about to be fetched and looks at the disk for it. `expected` is the size the backup server
    /// reports (or, for an authenticated backup, the manifest lists): a guide for the free-space check, not what is counted
    /// against `--max-total-bytes` (that is what is written, see [`Budget::charge_bytes`]).
    fn begin_object(&mut self, expected: u64) -> Result<(), CliError> {
        self.objects = self.objects.saturating_add(1);
        if self.objects > self.limits.max_objects {
            return Err(CliError::Usage(format!(
                "the backup holds more than {} blobs (--max-objects); nothing was restored",
                self.limits.max_objects
            )));
        }
        self.check_free(expected)
    }

    fn charge_bytes(&mut self, bytes: u64) -> Result<(), CliError> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.max_total_bytes {
            return Err(CliError::Usage(format!(
                "the restore would write more than {} bytes (--max-total-bytes); nothing was restored",
                self.limits.max_total_bytes
            )));
        }
        Ok(())
    }
}

/// The list of blobs a backup stands for, written while the blobs are copied.
struct Inventory {
    out: std::io::BufWriter<std::fs::File>,
    hasher: blake3::Hasher,
    bytes: u64,
    count: u64,
    total: u64,
    last: Option<(bool, [u8; 32])>,
}

struct InventoryDone {
    bytes: u64,
    blake3: String,
    count: u64,
    total: u64,
}

impl Inventory {
    fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            out: std::io::BufWriter::new(std::fs::File::create(path)?),
            hasher: blake3::Hasher::new(),
            bytes: 0,
            count: 0,
            total: 0,
            last: None,
        })
    }

    /// Blobs must come in order (manifests, then chunks; by ID within a kind): the restore relies on it.
    fn add(&mut self, key: BlobKey, size: u64) -> Result<(), CliError> {
        let at = (key.kind == BlobKind::Chunk, key.id);
        if self.last.is_some_and(|last| last >= at) {
            return Err(CliError::Inconsistent("the blob store listed its blobs out of order".into()));
        }
        self.last = Some(at);
        let line = backup_auth::inventory_line(key.kind == BlobKind::Manifest, &key.id, size);
        self.out.write_all(line.as_bytes())?;
        self.hasher.update(line.as_bytes());
        self.bytes = self.bytes.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(size);
        Ok(())
    }

    fn finish(mut self) -> Result<InventoryDone, CliError> {
        self.out.flush()?;
        self.out.get_ref().sync_all()?;
        Ok(InventoryDone { bytes: self.bytes, blake3: self.hasher.finalize().to_hex().to_string(), count: self.count, total: self.total })
    }
}

/// What a copy pass also does: count against a restore's limits before a blob is fetched, or record a backup's blobs.
#[derive(Default)]
struct CopyTools<'a> {
    budget: Option<&'a Mutex<Budget>>,
    inventory: Option<&'a Mutex<Inventory>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Copies the blobs of one kind from `from` to `to`; a name that `to` has is skipped. A blob that cannot be read or
/// written stops the copy: a backup that silently leaves some out is not a backup.
async fn copy_blobs(from: &Store, to: &Store, kind: BlobKind, tools: &CopyTools<'_>) -> Result<Copied, CliError> {
    let mut totals = Copied::default();
    let mut cursor = None;
    loop {
        let page = from.list(kind, cursor, MAX_LIST_PAGE_SIZE).await?;
        totals.foreign = totals.foreign.saturating_add(u64::try_from(page.foreign.len()).unwrap_or(u64::MAX));
        // In the order of the listing, so the inventory is sorted.
        let results: Vec<(BlobKey, Option<u64>, u64)> =
            stream::iter(page.blobs)
                .map(|key| async move {
                    copy_one(from, to, key, tools.budget).await.map(|(copied, size)| (key, copied, size)).map_err(|e| e.in_blob(key))
                })
                .buffered(COPY_PARALLEL)
                .try_collect()
                .await?;
        for (key, result, size) in results {
            totals.seen = totals.seen.saturating_add(1);
            match result {
                Some(len) => {
                    totals.copied = totals.copied.saturating_add(1);
                    totals.bytes = totals.bytes.saturating_add(len);
                }
                None => totals.skipped = totals.skipped.saturating_add(1),
            }
            if let Some(inventory) = tools.inventory {
                lock(inventory).add(key, size)?;
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(totals);
        }
    }
}

/// A blob that failed: on the store, or on a limit of the restore.
enum CopyError {
    Store(StoreError),
    Limit(CliError),
}

impl CopyError {
    fn in_blob(self, key: BlobKey) -> CliError {
        match self {
            CopyError::Store(e) => CliError::Inconsistent(format!("{key}: {e}")),
            CopyError::Limit(e) => e,
        }
    }
}

impl From<StoreError> for CopyError {
    fn from(e: StoreError) -> Self {
        CopyError::Store(e)
    }
}

/// The bytes copied (`None`: `to` has the blob already) and the blob's size on `to`. Otherwise the bytes are read (and
/// checked against the ID) from `from` and written. With a budget, the blob is counted against it before it is read.
async fn copy_one(from: &Store, to: &Store, key: BlobKey, budget: Option<&Mutex<Budget>>) -> Result<(Option<u64>, u64), CopyError> {
    if let Some(len) = to.object_len(key).await? {
        return Ok((None, len));
    }
    if let Some(budget) = budget {
        // The size the backup server reports is only a guide for the free-space check: it is not trusted for the total.
        let size = from.object_len(key).await?.unwrap_or(0);
        lock(budget).begin_object(size).map_err(CopyError::Limit)?;
    }
    let bytes = from.get(key).await?;
    let written = to.put(key, &bytes).await?;
    if let Some(budget) = budget {
        // What was written here is what counts.
        lock(budget).charge_bytes(written.physical_len).map_err(CopyError::Limit)?;
    }
    Ok((Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)), written.physical_len))
}

/// The backup itself, on any target. The backup key is `backup.key` beside the config (see [`backup_key_path`]).
pub async fn backup_to<R: Remote>(
    config: &Config,
    remote: R,
    keep: u32,
    work_dir: Option<&Path>,
    now_ms: i64,
    out: &mut impl Write,
) -> Result<(), CliError> {
    backup_with_key(config, &backup_key_path(config), remote, keep, work_dir, now_ms, out).await
}

/// [`backup_to`] with the backup key file named.
pub async fn backup_with_key<R: Remote>(
    config: &Config,
    key_path: &Path,
    remote: R,
    keep: u32,
    work_dir: Option<&Path>,
    now_ms: i64,
    out: &mut impl Write,
) -> Result<(), CliError> {
    require_sqlite(config)?;
    if keep == 0 {
        return Err(CliError::Usage("--keep must be at least 1".into()));
    }
    // The key comes first: a backup that cannot be authenticated is not made.
    let (key, made) = BackupKey::load_or_create(key_path).map_err(CliError::Usage)?;
    if made {
        writeln!(
            out,
            "made the backup key {}. Keep a copy of it apart from this machine and from the backup server (a password manager will do): \
             without it no backup can be restored.",
            key_path.display()
        )?;
    }
    let (meta, local) = open_existing(config).await?;
    let (second, iso) = utc_stamp(now_ms);
    // The second alone is not a name: another backup may begin in it. The random part makes the directory this backup's own.
    let stamp = format!("{second}-{}", uuid::Uuid::now_v7().simple());
    let dump_path = format!("{GENERATIONS}/{stamp}/{SEALED_DUMP_NAME}");
    let inventory_path = format!("{GENERATIONS}/{stamp}/{INVENTORY_NAME}");
    let manifest_path = format!("{GENERATIONS}/{stamp}/{MANIFEST_NAME}");
    let work = tempfile::Builder::new().prefix("craft-cloud-backup-").tempdir_in(work_dir.map_or_else(std::env::temp_dir, Path::to_owned))?;
    let dump = work.path().join(DUMP_NAME);
    meta.dump_sqlite(&dump).await?;
    // A dump that does not open as a current database is not worth uploading.
    Meta::open_existing(&format!("sqlite:{}", dump.display())).await?;
    // What goes to the backup server is sealed: it cannot read the database, and cannot change it unseen.
    let sealed = work.path().join(SEALED_DUMP_NAME);
    let plain_bytes = {
        let (key, stamp, dump, sealed) = (key.clone(), stamp.clone(), dump.clone(), sealed.clone());
        tokio::task::spawn_blocking(move || backup_auth::seal_file(&key, &stamp, backup_auth::AUTHENTICATED_SCHEMA, &dump, &sealed))
            .await
            .map_err(|e| CliError::Inconsistent(format!("sealing failed: {e}")))?
            .map_err(CliError::Inconsistent)?
    };
    let _ = std::fs::remove_file(&dump);
    let (dump_bytes, dump_hash) = hash_file(&sealed).await?;
    remote.put_file(&dump_path, &sealed).await?;
    writeln!(out, "database: {plain_bytes} bytes sealed and copied to the backup server")?;

    let inventory_file = work.path().join(INVENTORY_NAME);
    let inventory = Mutex::new(Inventory::create(&inventory_file)?);
    let tools = CopyTools { budget: None, inventory: Some(&inventory) };
    let remote_store = tuned(Store::new(remote.clone()), config);
    let manifests = copy_blobs(&local, &remote_store, BlobKind::Manifest, &tools).await?;
    let chunks = copy_blobs(&local, &remote_store, BlobKind::Chunk, &tools).await?;
    for (name, counts) in [("chunks", chunks), ("manifests", manifests)] {
        writeln!(out, "{name}: {} found, {} copied ({} bytes), {} already there", counts.seen, counts.copied, counts.bytes, counts.skipped)?;
        if counts.foreign > 0 {
            writeln!(out, "{name}: {} object(s) in the store are not blobs and were left out", counts.foreign)?;
        }
    }
    let inventory_done = inventory.into_inner().unwrap_or_else(PoisonError::into_inner).finish()?;
    remote.put_file(&inventory_path, &inventory_file).await?;

    let counts = |c: Copied| backup_auth::KindCounts { found: c.seen, copied: c.copied, already_there: c.skipped };
    let record = backup_auth::SignedManifest {
        schema_version: backup_auth::AUTHENTICATED_SCHEMA,
        generation: stamp.clone(),
        created: iso,
        created_ms: now_ms,
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        database: backup_auth::DatabaseEntry { kind: "sqlite".into(), file: dump_path, bytes: dump_bytes, blake3: dump_hash, plain_bytes },
        inventory: backup_auth::InventoryEntry {
            file: inventory_path,
            bytes: inventory_done.bytes,
            blake3: inventory_done.blake3,
            count: inventory_done.count,
            total_bytes: inventory_done.total,
        },
        blobs: backup_auth::BlobCounts { chunks: counts(chunks), manifests: counts(manifests) },
        mac: String::new(),
    }
    .sign(&key);
    let text = serde_json::to_vec_pretty(&record).map_err(|e| CliError::Inconsistent(format!("the manifest could not be written: {e}")))?;
    remote.put_raw(&manifest_path, Bytes::from(text)).await?;
    writeln!(out, "backup {stamp} is complete")?;

    let removed = prune(&remote, keep, &stamp, now_ms).await?;
    if removed > 0 {
        writeln!(out, "removed {removed} older database generation(s); {keep} are kept")?;
    }
    Ok(())
}

/// What a restore trusts, once the manifest has been read.
enum Source {
    /// Schema 2: the MAC under the backup key was right, so every number and name in it is the backup's own.
    Authenticated(BackupKey, Box<backup_auth::SignedManifest>),
    /// Schema 1, from before backups were authenticated, and only when the operator asked for it.
    Unauthenticated { dump_path: String, hash: String, bytes: u64 },
}

const NOT_AUTHENTIC: &str = "the backup's manifest does not match the backup key: the key is another one, or the manifest was changed. \
                             Nothing was restored";

/// Reads the manifest of generation `stamp` and decides what to trust. Nothing in it is used before the MAC is right.
/// `None`: a generation from before backups were authenticated, which is only restored when it is named (`explicit`) and
/// the flag is given; the newest generation is never one of those, whatever the backup server calls newest.
fn read_manifest(raw: &[u8], stamp: &str, options: &RestoreOptions, explicit: bool, out: &mut impl Write) -> Result<Option<Source>, CliError> {
    let record: Value = serde_json::from_slice(raw).map_err(|_| CliError::Inconsistent("the backup's manifest is not readable".into()))?;
    match record["schema_version"].as_u64() {
        Some(2) => {
            let key_path = options.key_file.as_deref().ok_or_else(|| {
                CliError::Usage(
                    "this backup is authenticated: give the backup key it was made with (--backup-key-file, or [backup] backup_key_file in a config). \
                     Without the key it cannot be restored"
                        .into(),
                )
            })?;
            let key = BackupKey::load(key_path).map_err(|e| CliError::Usage(format!("{e}; the backup can only be restored with its key")))?;
            let signed: backup_auth::SignedManifest = serde_json::from_value(record)
                .map_err(|_| CliError::Inconsistent("the backup's manifest is not readable, or has a field this version does not know".into()))?;
            if !signed.verify(&key) {
                return Err(CliError::Inconsistent(NOT_AUTHENTIC.into()));
            }
            // From here on the fields are the backup's own. They must still describe this generation.
            let own = |name: &str| format!("{GENERATIONS}/{stamp}/{name}");
            if signed.schema_version != backup_auth::AUTHENTICATED_SCHEMA
                || signed.generation != stamp
                || signed.database.kind != "sqlite"
                || signed.database.file != own(SEALED_DUMP_NAME)
                || signed.inventory.file != own(INVENTORY_NAME)
            {
                return Err(CliError::Inconsistent("the manifest names another generation or other files than its own".into()));
            }
            Ok(Some(Source::Authenticated(key, Box::new(signed))))
        }
        Some(1) if record["database"]["kind"] == "sqlite" => {
            if !explicit {
                return Ok(None);
            }
            if !options.allow_unauthenticated {
                return Err(CliError::Usage(
                    "this generation was made by an older version that did not authenticate backups: nothing proves that the backup server did not \
                     change it, and a changed database can hand an attacker every account. It is refused. If it is your own test data, \
                     restore it with --allow-unauthenticated"
                        .into(),
                ));
            }
            writeln!(
                out,
                "WARNING: this generation is not authenticated. Whoever controls the backup server could have changed it. Use it for test data only, \
                 and make a new backup with this version."
            )?;
            let hash =
                record["database"]["blake3"].as_str().ok_or_else(|| CliError::Inconsistent("the manifest has no database hash".into()))?.to_owned();
            let bytes = record["database"]["bytes"].as_u64().ok_or_else(|| CliError::Inconsistent("the manifest has no database size".into()))?;
            let dump_path = format!("{GENERATIONS}/{stamp}/{DUMP_NAME}");
            if record["database"]["file"] != dump_path {
                return Err(CliError::Inconsistent("the manifest names another database file than its own generation".into()));
            }
            Ok(Some(Source::Unauthenticated { dump_path, hash, bytes }))
        }
        _ => Err(CliError::Inconsistent("the backup was made by a version this one does not understand".into())),
    }
}

/// A restore on any source.
pub async fn restore_from<R: Remote>(
    remote: R,
    dir: &Path,
    generation: Option<&str>,
    options: &RestoreOptions,
    out: &mut impl Write,
) -> Result<(), CliError> {
    check_empty(dir)?;
    let generations = list_generations(&remote).await?;
    let complete: Vec<&String> = generations.iter().filter(|(_, files)| files.iter().any(|f| f == MANIFEST_NAME)).map(|(stamp, _)| stamp).collect();
    // The generation asked for, or the complete ones from the newest down. An old unauthenticated one is taken only when it
    // is asked for by name: when none is named, such a generation is passed over (the backup server decides what looks newest,
    // so a plain dump it plants must not become the restore).
    let candidates: Vec<String> = match generation {
        // The whole name, or just its stamp (the newest backup that began in that second).
        Some(wanted) => vec![
            complete
                .iter()
                .rev()
                .find(|stamp| {
                    stamp.as_str() == wanted
                        || (stamp.len() > wanted.len() && stamp.starts_with(wanted) && stamp.get(wanted.len()..=wanted.len()) == Some("-"))
                })
                .map(|stamp| (*stamp).clone())
                .ok_or_else(|| CliError::Usage(format!("the backup server has no complete generation {wanted}")))?,
        ],
        None => complete.iter().rev().map(|stamp| (*stamp).clone()).collect(),
    };
    if candidates.is_empty() {
        return Err(CliError::Usage("the backup server holds no complete backup".into()));
    }
    let mut passed_over = Vec::new();
    let mut chosen = None;
    for stamp in candidates {
        let manifest_path = format!("{GENERATIONS}/{stamp}/{MANIFEST_NAME}");
        let raw = remote
            .get_raw_limited(&manifest_path, MAX_MANIFEST_BYTES)
            .await?
            .ok_or_else(|| CliError::Inconsistent("the manifest disappeared".into()))?;
        if raw.len() > MAX_MANIFEST_BYTES {
            return Err(CliError::Inconsistent("the backup's manifest is too large to be one of ours".into()));
        }
        // Before anything is written, and before any field is believed. A manifest that fails is an error, never a reason
        // to fall back to an older generation.
        match read_manifest(&raw, &stamp, options, generation.is_some(), out)? {
            Some(source) => {
                chosen = Some((stamp, source));
                break;
            }
            None => passed_over.push(stamp),
        }
    }
    let Some((stamp, source)) = chosen else {
        return Err(CliError::Usage(format!(
            "the backup server holds no authenticated backup; {} older generation(s) are from before backups were authenticated and are restored only              when named (--generation <stamp> --allow-unauthenticated): {}",
            passed_over.len(),
            passed_over.join(", ")
        )));
    };
    if !passed_over.is_empty() {
        writeln!(out, "passed over {} newer generation(s) that are not authenticated: {}", passed_over.len(), passed_over.join(", "))?;
    }

    // Everything is made in a directory beside the target and moved into place at the end, so a restore that is killed
    // leaves the target as it was (missing or empty, and a re-run is not refused) and a leftover `….restore-<id>` directory
    // that can be deleted. A restore that fails removes its own.
    let abs = std::path::absolute(dir)?;
    let name = abs.file_name().ok_or_else(|| CliError::Usage("the data directory needs a name".into()))?.to_string_lossy().into_owned();
    let staging = abs.with_file_name(format!("{name}.restore-{}", uuid::Uuid::now_v7().simple()));
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result = match &source {
        Source::Authenticated(key, signed) => restore_authenticated(&remote, &staging, key, signed, options, out).await,
        Source::Unauthenticated { dump_path, hash, bytes } => restore_legacy(&remote, &staging, dump_path, hash, *bytes, options, out).await,
    };
    let result = match result {
        Ok(()) => publish(&staging, &abs),
        Err(e) => Err(e),
    };
    if result.is_err() {
        // A blob that was being written when another failed may still be finishing on a blocking thread, so the first
        // attempt can meet a file in use or one that appears meanwhile: try again after a moment.
        for attempt in 0..10u64 {
            if std::fs::remove_dir_all(&staging).is_ok() && !staging.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
        }
    }
    result?;
    writeln!(out, "restored generation {stamp} into {}", abs.display())?;
    writeln!(
        out,
        "next: put your config.toml and two-factor key beside them (storage path {}, database sqlite:{}), run `craft-cloud-server verify`, \
         and, because the database is older than what the computers hold, `craft-cloud-server reset-epoch --all` before starting the server",
        abs.join("blobs").display(),
        abs.join("meta.db").display()
    )?;
    Ok(())
}

/// Moves the finished `staging` directory to `target`, which may exist if it is empty.
fn publish(staging: &Path, target: &Path) -> Result<(), CliError> {
    if target.exists() {
        // Fails, and leaves the directory, if something was put into it since the check.
        std::fs::remove_dir(target)
            .map_err(|_| CliError::Usage(format!("{} is not empty any more; nothing was restored into it", target.display())))?;
    }
    std::fs::rename(staging, target)?;
    Ok(())
}

/// The longest line the inventory has (`c`, 64 hex digits, a 20-digit size, two spaces and the newline), with room to spare.
const INVENTORY_LINE_MAX: u64 = 96;
/// Blobs fetched before the limits are looked at again (they are looked at before each blob, too).
const RESTORE_BATCH: usize = COPY_PARALLEL * 8;

/// Restores an authenticated generation. The sizes, the hashes and the inventory are the backup's own (the MAC said so);
/// the backup server only supplies bytes, and each is checked against them as it arrives.
async fn restore_authenticated<R: Remote>(
    remote: &R,
    dir: &Path,
    key: &BackupKey,
    manifest: &backup_auth::SignedManifest,
    options: &RestoreOptions,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let (database, inventory) = (&manifest.database, &manifest.inventory);
    let limits = options.limits;
    // The limits are looked at before anything is downloaded.
    if database.plain_bytes > limits.max_db_bytes {
        return Err(CliError::Usage(format!(
            "the database is {} bytes, more than --max-db-bytes ({}); nothing was restored",
            database.plain_bytes, limits.max_db_bytes
        )));
    }
    if inventory.count > limits.max_objects {
        return Err(CliError::Usage(format!(
            "the backup holds {} blobs, more than --max-objects ({}); nothing was restored",
            inventory.count, limits.max_objects
        )));
    }
    if database.plain_bytes.saturating_add(inventory.total_bytes) > limits.max_total_bytes {
        return Err(CliError::Usage(format!(
            "the backup is {} bytes, more than --max-total-bytes ({}); nothing was restored",
            database.plain_bytes.saturating_add(inventory.total_bytes),
            limits.max_total_bytes
        )));
    }
    if database.bytes != backup_auth::sealed_len(database.plain_bytes) || inventory.bytes > inventory.count.saturating_mul(INVENTORY_LINE_MAX) {
        return Err(CliError::Inconsistent("the sizes in the manifest do not agree with each other".into()));
    }
    create_data_dir(dir)?;
    let budget = Mutex::new(Budget::new(options, dir));
    let needed = database.bytes.saturating_add(database.plain_bytes).saturating_add(inventory.bytes).saturating_add(inventory.total_bytes);
    lock(&budget).check_free(needed)?;

    // The inventory first: it names every blob, and nothing else is fetched.
    let inventory_file = dir.join(".inventory.tmp");
    if !remote.get_file(&inventory.file, &inventory_file, inventory.bytes).await? {
        return Err(CliError::Inconsistent("the backup's blob inventory is missing".into()));
    }
    let (bytes, hash) = hash_file(&inventory_file).await?;
    if bytes != inventory.bytes || hash != inventory.blake3 {
        return Err(CliError::Inconsistent("the blob inventory does not match the manifest; it was changed or is damaged".into()));
    }

    let sealed = dir.join(".meta.db.sealed.tmp");
    if !remote.get_file(&database.file, &sealed, database.bytes).await? {
        return Err(CliError::Inconsistent("the backup's database dump is missing".into()));
    }
    let (bytes, hash) = hash_file(&sealed).await?;
    if bytes != database.bytes || hash != database.blake3 {
        return Err(CliError::Inconsistent("the database dump does not match the hash in the manifest; it was changed or is damaged".into()));
    }
    let database_file = dir.join("meta.db");
    let opened = {
        let (key, generation, sealed, target, max) =
            (key.clone(), manifest.generation.clone(), sealed.clone(), database_file.clone(), database.plain_bytes);
        tokio::task::spawn_blocking(move || backup_auth::open_file(&key, &generation, backup_auth::AUTHENTICATED_SCHEMA, &sealed, &target, max))
            .await
            .map_err(|e| CliError::Inconsistent(format!("opening the dump failed: {e}")))?
    };
    let plain = match opened {
        Ok(plain) => plain,
        Err(backup_auth::OpenError::NotAuthentic) => {
            return Err(CliError::Inconsistent("the database dump does not open with this key; it was changed or is damaged".into()));
        }
        Err(backup_auth::OpenError::TooBig) => return Err(CliError::Inconsistent("the database dump is bigger than the manifest says".into())),
        Err(backup_auth::OpenError::Io(message)) => return Err(CliError::Inconsistent(message)),
    };
    let _ = std::fs::remove_file(&sealed);
    if plain != database.plain_bytes {
        return Err(CliError::Inconsistent("the database dump is not the size the manifest says".into()));
    }
    Meta::open_existing(&format!("sqlite:{}", database_file.display())).await?;
    restrict(&database_file, 0o600)?;
    lock(&budget).charge_bytes(plain)?;
    writeln!(out, "database: {plain} bytes restored and checked")?;

    let local = Store::new(craft_cloud_store::disk::DiskBackend::new(dir.join("blobs"))?);
    let source = Store::new(remote.clone());
    let restored = restore_listed_blobs(&source, &local, &inventory_file, manifest, &budget).await?;
    let _ = std::fs::remove_file(&inventory_file);
    writeln!(out, "manifests: {} restored; chunks: {} restored ({} bytes in all)", restored.manifests, restored.chunks, restored.bytes)?;
    Ok(())
}

struct Restored {
    manifests: u64,
    chunks: u64,
    bytes: u64,
}

/// Fetches exactly the blobs the (verified) inventory lists, in its order, each checked against its ID and against the size
/// listed. The inventory must be complete: its line count and total are the manifest's, or nothing is published.
async fn restore_listed_blobs(
    source: &Store,
    local: &Store,
    inventory_file: &Path,
    manifest: &backup_auth::SignedManifest,
    budget: &Mutex<Budget>,
) -> Result<Restored, CliError> {
    use std::io::BufRead;
    let bad = |what: &str| CliError::Inconsistent(format!("the blob inventory is not in the form this version writes: {what}"));
    let mut lines = std::io::BufReader::new(std::fs::File::open(inventory_file)?).lines();
    let (mut count, mut total, mut last) = (0u64, 0u64, None::<(bool, [u8; 32])>);
    let mut restored = Restored { manifests: 0, chunks: 0, bytes: 0 };
    loop {
        let mut batch: Vec<(BlobKey, u64)> = Vec::new();
        while batch.len() < RESTORE_BATCH {
            let Some(line) = lines.next() else { break };
            let line = line?;
            let (is_manifest, id, size) = backup_auth::parse_inventory_line(&line).ok_or_else(|| bad("a line is malformed"))?;
            let at = (!is_manifest, id);
            if last.is_some_and(|last| last >= at) {
                return Err(bad("the lines are not in order, or one is repeated"));
            }
            last = Some(at);
            count = count.saturating_add(1);
            total = total.saturating_add(size);
            if count > manifest.inventory.count || total > manifest.inventory.total_bytes {
                return Err(bad("it lists more than the manifest says"));
            }
            let kind = if is_manifest { BlobKind::Manifest } else { BlobKind::Chunk };
            batch.push((BlobKey { kind, id }, size));
        }
        if batch.is_empty() {
            break;
        }
        // The limits are looked at for every blob before it is fetched, so a restore stops where it crosses one.
        let fetched: Vec<(BlobKey, u64)> = stream::iter(batch)
            .map(|(key, size)| async move { fetch_listed(source, local, key, size, budget).await.map(|len| (key, len)) })
            .buffered(COPY_PARALLEL)
            .try_collect()
            .await?;
        for (key, len) in fetched {
            match key.kind {
                BlobKind::Manifest => restored.manifests = restored.manifests.saturating_add(1),
                BlobKind::Chunk => restored.chunks = restored.chunks.saturating_add(1),
            }
            restored.bytes = restored.bytes.saturating_add(len);
        }
    }
    if count != manifest.inventory.count || total != manifest.inventory.total_bytes {
        return Err(bad("it lists fewer blobs than the manifest says"));
    }
    Ok(restored)
}

async fn fetch_listed(source: &Store, local: &Store, key: BlobKey, size: u64, budget: &Mutex<Budget>) -> Result<u64, CliError> {
    lock(budget).begin_object(size)?;
    let on_server = source
        .object_len(key)
        .await?
        .ok_or_else(|| CliError::Inconsistent(format!("{key} is listed in the backup but is not on the backup server")))?;
    if on_server != size {
        return Err(CliError::Inconsistent(format!("{key} is {on_server} bytes on the backup server, the manifest lists {size}")));
    }
    let bytes = source.get(key).await.map_err(|e| CliError::Inconsistent(format!("{key}: {e}")))?;
    let written = local.put(key, &bytes).await?;
    // What is written here counts against --max-total-bytes, whatever the manifest lists: the local copy is not stored the
    // way the backup server stores it.
    lock(budget).charge_bytes(written.physical_len)?;
    Ok(size)
}

/// Restores a generation from before backups were authenticated, which only an explicit flag allows. The limits apply.
async fn restore_legacy<R: Remote>(
    remote: &R,
    dir: &Path,
    dump_path: &str,
    expected_hash: &str,
    expected_bytes: u64,
    options: &RestoreOptions,
    out: &mut impl Write,
) -> Result<(), CliError> {
    if expected_bytes > options.limits.max_db_bytes {
        return Err(CliError::Usage(format!(
            "the database is {expected_bytes} bytes, more than --max-db-bytes ({}); nothing was restored",
            options.limits.max_db_bytes
        )));
    }
    create_data_dir(dir)?;
    let budget = Mutex::new(Budget::new(options, dir));
    lock(&budget).check_free(expected_bytes)?;
    let database = dir.join("meta.db");
    if !remote.get_file(dump_path, &database, expected_bytes).await? {
        return Err(CliError::Inconsistent("the backup's database dump is missing".into()));
    }
    let (bytes, hash) = hash_file(&database).await?;
    if hash != expected_hash || bytes != expected_bytes {
        return Err(CliError::Inconsistent("the database dump does not match the hash in the manifest; it is damaged".into()));
    }
    Meta::open_existing(&format!("sqlite:{}", database.display())).await?;
    restrict(&database, 0o600)?;
    lock(&budget).charge_bytes(bytes)?;
    writeln!(out, "database: {bytes} bytes restored and checked")?;
    let local = Store::new(craft_cloud_store::disk::DiskBackend::new(dir.join("blobs"))?);
    let source = Store::new(remote.clone());
    let tools = CopyTools { budget: Some(&budget), inventory: None };
    for (name, kind) in [("manifests", BlobKind::Manifest), ("chunks", BlobKind::Chunk)] {
        let counts = copy_blobs(&source, &local, kind, &tools).await?;
        writeln!(out, "{name}: {} found, {} restored ({} bytes)", counts.seen, counts.copied, counts.bytes)?;
    }
    Ok(())
}

/// Generation stamp → the file names in it, oldest first.
async fn list_generations<R: Remote>(remote: &R) -> Result<BTreeMap<String, Vec<String>>, CliError> {
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let page = remote.list_raw(GENERATIONS, after.as_deref(), MAX_LIST_PAGE_SIZE).await?;
        let last = page.last().cloned();
        for path in &page {
            let mut parts = path.split('/');
            if let (Some(GENERATIONS), Some(stamp), Some(name), None) = (parts.next(), parts.next(), parts.next(), parts.next())
                && is_generation(stamp)
            {
                found.entry(stamp.to_owned()).or_default().push(name.to_owned());
            }
        }
        if page.len() < MAX_LIST_PAGE_SIZE {
            return Ok(found);
        }
        after = last;
    }
}

/// How old a generation without its manifest must be before it is taken for a backup that died, not one still running.
const ABANDONED_AFTER_MS: i64 = 24 * 3600 * 1000;

/// Deletes the complete generations beyond the `keep` newest, and the incomplete ones that are over a day old. It runs
/// after this backup's own manifest is written, and only touches generations that began in an earlier second than
/// `current`: the generation just written is never deleted, whatever the clock said, one that began in the same second
/// or later belongs to a backup that may be running, and a generation without its manifest that is younger than a day
/// is left alone for the same reason. Returns how many it removed.
async fn prune<R: Remote>(remote: &R, keep: u32, current: &str, now_ms: i64) -> Result<usize, CliError> {
    let generations = list_generations(remote).await?;
    let complete: Vec<&String> = generations.iter().filter(|(_, files)| files.iter().any(|f| f == MANIFEST_NAME)).map(|(stamp, _)| stamp).collect();
    let keep = usize::try_from(keep).unwrap_or(usize::MAX).max(1);
    let kept: Vec<&String> = complete.iter().rev().take(keep).copied().collect();
    let abandoned_before = utc_stamp(now_ms.saturating_sub(ABANDONED_AFTER_MS)).0;
    let own_second = current.get(..16).unwrap_or(current);
    let doomed = |stamp: &String, files: &Vec<String>| {
        if stamp == current || kept.contains(&stamp) {
            return false;
        }
        let second = stamp.get(..16).unwrap_or(stamp);
        if second >= own_second {
            return false;
        }
        files.iter().any(|f| f == MANIFEST_NAME) || second < abandoned_before.as_str()
    };
    let mut removed = 0usize;
    for (stamp, files) in generations.iter().filter(|(stamp, files)| doomed(stamp, files)) {
        // The manifest goes first: from then on the generation is not offered by a restore.
        for name in files.iter().filter(|name| name.as_str() == MANIFEST_NAME).chain(files.iter().filter(|name| name.as_str() != MANIFEST_NAME)) {
            remote.delete_raw(&format!("{GENERATIONS}/{stamp}/{name}")).await?;
        }
        remote.remove_dir(&format!("{GENERATIONS}/{stamp}")).await?;
        removed = removed.saturating_add(1);
    }
    Ok(removed)
}

/// A generation's name: a stamp, and the `-<32 hex digits>` of a UUIDv7 unless it was written by the first version.
fn is_generation(text: &str) -> bool {
    match text.get(..16) {
        Some(stamp) if text.len() == 16 => is_stamp(stamp),
        Some(stamp) => {
            text.len() == 49
                && is_stamp(stamp)
                && text.get(16..17) == Some("-")
                && text.get(17..).is_some_and(|id| id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        }
        None => false,
    }
}

/// `YYYYMMDDTHHMMSSZ`.
fn is_stamp(text: &str) -> bool {
    text.len() == 16
        && text.bytes().enumerate().all(|(i, b)| {
            if i == 8 {
                b == b'T'
            } else if i == 15 {
                b == b'Z'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// The compact stamp (a directory name that sorts by time) and the ISO 8601 text of a time in milliseconds since the epoch.
pub fn utc_stamp(ms: i64) -> (String, String) {
    let days = ms.div_euclid(86_400_000);
    let secs = ms.rem_euclid(86_400_000) / 1000;
    // Civil date from days since 1970-01-01 (proleptic Gregorian).
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (hour, minute, second) = (secs / 3600, secs % 3600 / 60, secs % 60);
    (
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
    )
}

/// The size and BLAKE3 (hex) of a file.
async fn hash_file(path: &Path) -> Result<(u64, String), CliError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || -> std::io::Result<(u64, String)> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut total = 0u64;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                return Ok((total, hasher.finalize().to_hex().to_string()));
            }
            hasher.update(buffer.get(..read).unwrap_or_default());
            total = total.saturating_add(u64::try_from(read).unwrap_or(0));
        }
    })
    .await
    .map_err(|e| CliError::Inconsistent(format!("hashing failed: {e}")))?
    .map_err(CliError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_read() {
        let url = parse_sftp_url("sftp://box@files.example.org/srv/craft").unwrap();
        assert_eq!(url, SftpUrl { user: "box".into(), host: "files.example.org".into(), port: 22, root: "/srv/craft".into() });
        let url = parse_sftp_url("sftp://box@[::1]:2222/~/backups/").unwrap();
        assert_eq!((url.host.as_str(), url.port, url.root.as_str()), ("::1", 2222, "backups"));
        assert_eq!(parse_sftp_url("sftp://u@h:23/%7Euser/x").unwrap().root, "/~user/x");
        assert_eq!(parse_sftp_url("sftp://u@h/~").unwrap().root, "");
        for bad in [
            "http://u@h/x",
            "sftp://h/x",
            "sftp://u:secret@h/x",
            "sftp://u@h",
            "sftp://u@h/",
            "sftp://u@:22/x",
            "sftp://@h/x",
            "sftp://u@h:0/x",
            "sftp://u@h:99999/x",
            "sftp://u@[::1/x",
        ] {
            assert!(parse_sftp_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_password_in_the_address_is_never_echoed() {
        let message = parse_sftp_url("sftp://u:hunter2@h/x").unwrap_err();
        assert!(!message.contains("hunter2"), "{message}");
    }

    #[test]
    fn stamps_are_utc_and_sort_by_time() {
        assert_eq!(utc_stamp(0), ("19700101T000000Z".into(), "1970-01-01T00:00:00Z".into()));
        assert_eq!(utc_stamp(1_700_000_000_000).0, "20231114T221320Z");
        assert_eq!(utc_stamp(951_782_400_000).1, "2000-02-29T00:00:00Z");
        assert_eq!(utc_stamp(-1).0, "19691231T235959Z");
        assert!(utc_stamp(1_700_000_000_000).0 < utc_stamp(1_700_000_001_000).0);
        assert!(is_stamp(&utc_stamp(1_700_000_000_000).0));
        let id = uuid::Uuid::now_v7().simple().to_string();
        assert!(is_generation("20231114T221320Z") && is_generation(&format!("20231114T221320Z-{id}")));
        for bad in [
            "20231114T221320Z-",
            "20231114T221320Z-xyz",
            "20231114T221320Zx",
            format!("20231114T221320Z_{id}").as_str(),
            format!("2023-11-14T221320Z-{id}").as_str(),
        ] {
            assert!(!is_generation(bad), "{bad}");
        }
        assert!(!is_stamp("2023-11-14"));
        assert!(!is_stamp("20231114T221320Zx"));
    }
}
