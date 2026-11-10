//! `backup` and `restore`: the server's data on a file server that can be reached over SFTP.
//!
//! A backup is a consistent copy of the SQLite database (`VACUUM INTO`, taken while the server runs) and a copy of the
//! blob store. Blobs are content-addressed and never change, so they are copied incrementally: a name that is already
//! on the backup server is skipped. The layout below the backup directory:
//!
//! ```text
//! chunks/ab/cd/<id>, manifests/ab/cd/<id>   the blobs, as the store holds them
//! generations/<UTC stamp>-<id>/meta.sqlite     one database dump
//! generations/<UTC stamp>-<id>/manifest.json   written last: its presence marks the generation as complete
//!
//! A generation's name is the second the backup began in and a random UUIDv7, so two backups that run at once (or in
//! the same second) write to directories of their own and never into each other's files. Names without the `-<id>`
//! (written by the first version) are still read.
//! ```
//!
//! The database is dumped first and the blobs after it: a blob the dump refers to was uploaded before the commit that
//! names it, so the copy that follows contains it. A restore reads the newest complete generation (or the one asked
//! for), checks the dump against the hash in its manifest, and copies every blob into an empty data directory.
//!
//! Not in a backup, on purpose: `config.toml` (it holds secrets) and the two-factor key. Keep copies of both apart from
//! the backup. Postgres is not dumped here; use `pg_dump` (see `docs/server.md`).

use std::collections::BTreeMap;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use craft_cloud_meta::Meta;
use craft_cloud_store::sftp::{DEFAULT_CONNECTIONS, DEFAULT_RETRIES, DEFAULT_TIMEOUT_SECS, SftpBackend, SftpConfig};
use craft_cloud_store::{Backend, BlobKey, BlobKind, MAX_LIST_PAGE_SIZE, Store, StoreError};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};

use crate::cli::{CliError, create_data_dir, open_existing, restrict, tuned};
use crate::config::{BackupConfig, Config};

/// How many database generations stay on the backup server when `--keep` and `[backup] keep` are not given.
pub const DEFAULT_KEEP: u32 = 7;
/// Blobs copied at once.
const COPY_PARALLEL: usize = 8;
/// The most a generation's manifest may be.
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const GENERATIONS: &str = "generations";
const DUMP_NAME: &str = "meta.sqlite";
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
    let remote = SftpBackend::connect_for_backup(&settings).await?;
    let keep = keep.or_else(|| config.backup.as_ref().and_then(|section| section.keep)).unwrap_or(DEFAULT_KEEP);
    backup_to(config, remote, keep, work_dir, crate::state::now_ms(), out).await
}

/// `restore` from an SFTP server into `dir`.
pub async fn restore(options: &RemoteOptions, dir: &Path, generation: Option<&str>, out: &mut impl Write) -> Result<(), CliError> {
    check_empty(dir)?;
    let settings = sftp_settings(options, None)?;
    let remote = SftpBackend::connect_for_restore(&settings).await?;
    restore_from(remote, dir, generation, out).await
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

/// Copies the blobs of one kind from `from` to `to`; a name that `to` has is skipped. A blob that cannot be read or
/// written stops the copy: a backup that silently leaves some out is not a backup.
pub async fn copy_blobs(from: &Store, to: &Store, kind: BlobKind) -> Result<Copied, CliError> {
    let mut totals = Copied::default();
    let mut cursor = None;
    loop {
        let page = from.list(kind, cursor, MAX_LIST_PAGE_SIZE).await?;
        totals.foreign = totals.foreign.saturating_add(u64::try_from(page.foreign.len()).unwrap_or(u64::MAX));
        let results: Vec<Option<u64>> = stream::iter(page.blobs)
            .map(|key| async move { copy_one(from, to, key).await.map_err(|e| CliError::Inconsistent(format!("{key}: {e}"))) })
            .buffer_unordered(COPY_PARALLEL)
            .try_collect()
            .await?;
        for result in results {
            totals.seen = totals.seen.saturating_add(1);
            match result {
                Some(len) => {
                    totals.copied = totals.copied.saturating_add(1);
                    totals.bytes = totals.bytes.saturating_add(len);
                }
                None => totals.skipped = totals.skipped.saturating_add(1),
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(totals);
        }
    }
}

/// `None`: `to` has it already. Otherwise the bytes are read (and checked against the ID) from `from` and written.
async fn copy_one(from: &Store, to: &Store, key: BlobKey) -> Result<Option<u64>, StoreError> {
    if to.object_len(key).await?.is_some() {
        return Ok(None);
    }
    let bytes = from.get(key).await?;
    to.put(key, &bytes).await?;
    Ok(Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)))
}

/// The backup itself, on any target.
pub async fn backup_to<R: Remote>(
    config: &Config,
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
    let (meta, local) = open_existing(config).await?;
    let (second, iso) = utc_stamp(now_ms);
    // The second alone is not a name: another backup may begin in it. The random part makes the directory this backup's own.
    let stamp = format!("{second}-{}", uuid::Uuid::now_v7().simple());
    let dump_path = format!("{GENERATIONS}/{stamp}/{DUMP_NAME}");
    let manifest_path = format!("{GENERATIONS}/{stamp}/{MANIFEST_NAME}");
    let work = tempfile::Builder::new().prefix("craft-cloud-backup-").tempdir_in(work_dir.map_or_else(std::env::temp_dir, Path::to_owned))?;
    let dump = work.path().join(DUMP_NAME);
    meta.dump_sqlite(&dump).await?;
    // A dump that does not open as a current database is not worth uploading.
    Meta::open_existing(&format!("sqlite:{}", dump.display())).await?;
    let (dump_bytes, dump_hash) = hash_file(&dump).await?;
    remote.put_file(&dump_path, &dump).await?;
    writeln!(out, "database: {dump_bytes} bytes copied to the backup server")?;

    let remote_store = tuned(Store::new(remote.clone()), config);
    let manifests = copy_blobs(&local, &remote_store, BlobKind::Manifest).await?;
    let chunks = copy_blobs(&local, &remote_store, BlobKind::Chunk).await?;
    for (name, counts) in [("chunks", chunks), ("manifests", manifests)] {
        writeln!(out, "{name}: {} found, {} copied ({} bytes), {} already there", counts.seen, counts.copied, counts.bytes, counts.skipped)?;
        if counts.foreign > 0 {
            writeln!(out, "{name}: {} object(s) in the store are not blobs and were left out", counts.foreign)?;
        }
    }

    let record = json!({
        "schema_version": 1,
        "created": iso,
        "created_ms": now_ms,
        "server_version": env!("CARGO_PKG_VERSION"),
        "database": {"kind": "sqlite", "file": dump_path, "bytes": dump_bytes, "blake3": dump_hash},
        "blobs": {
            "chunks": {"found": chunks.seen, "copied": chunks.copied, "already_there": chunks.skipped},
            "manifests": {"found": manifests.seen, "copied": manifests.copied, "already_there": manifests.skipped},
        },
    });
    let text = serde_json::to_vec_pretty(&record).map_err(|e| CliError::Inconsistent(format!("the manifest could not be written: {e}")))?;
    remote.put_raw(&manifest_path, Bytes::from(text)).await?;
    writeln!(out, "backup {stamp} is complete")?;

    let removed = prune(&remote, keep, &stamp, now_ms).await?;
    if removed > 0 {
        writeln!(out, "removed {removed} older database generation(s); {keep} are kept")?;
    }
    Ok(())
}

/// A restore on any source.
pub async fn restore_from<R: Remote>(remote: R, dir: &Path, generation: Option<&str>, out: &mut impl Write) -> Result<(), CliError> {
    check_empty(dir)?;
    let generations = list_generations(&remote).await?;
    let complete: Vec<&String> = generations.iter().filter(|(_, files)| files.iter().any(|f| f == MANIFEST_NAME)).map(|(stamp, _)| stamp).collect();
    let stamp = match generation {
        // The whole name, or just its stamp (the newest backup that began in that second).
        Some(wanted) => complete
            .iter()
            .rev()
            .find(|stamp| {
                stamp.as_str() == wanted
                    || (stamp.len() > wanted.len() && stamp.starts_with(wanted) && stamp.get(wanted.len()..=wanted.len()) == Some("-"))
            })
            .map(|stamp| (*stamp).clone())
            .ok_or_else(|| CliError::Usage(format!("the backup server has no complete generation {wanted}")))?,
        None => complete.last().map(|stamp| (*stamp).clone()).ok_or_else(|| CliError::Usage("the backup server holds no complete backup".into()))?,
    };
    let manifest_path = format!("{GENERATIONS}/{stamp}/{MANIFEST_NAME}");
    let raw =
        remote.get_raw_limited(&manifest_path, MAX_MANIFEST_BYTES).await?.ok_or_else(|| CliError::Inconsistent("the manifest disappeared".into()))?;
    if raw.len() > MAX_MANIFEST_BYTES {
        return Err(CliError::Inconsistent("the backup's manifest is too large to be one of ours".into()));
    }
    let record: Value = serde_json::from_slice(&raw).map_err(|_| CliError::Inconsistent("the backup's manifest is not readable".into()))?;
    if record["schema_version"] != 1 || record["database"]["kind"] != "sqlite" {
        return Err(CliError::Inconsistent("the backup was made by a version this one does not understand".into()));
    }
    let expected_hash =
        record["database"]["blake3"].as_str().ok_or_else(|| CliError::Inconsistent("the manifest has no database hash".into()))?.to_owned();
    let expected_bytes = record["database"]["bytes"].as_u64().ok_or_else(|| CliError::Inconsistent("the manifest has no database size".into()))?;
    let dump_path = format!("{GENERATIONS}/{stamp}/{DUMP_NAME}");
    if record["database"]["file"] != dump_path {
        return Err(CliError::Inconsistent("the manifest names another database file than its own generation".into()));
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
    let result = match restore_into(&remote, &staging, &dump_path, &expected_hash, expected_bytes, out).await {
        Ok(()) => publish(&staging, &abs),
        Err(e) => Err(e),
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
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

async fn restore_into<R: Remote>(
    remote: &R,
    dir: &Path,
    dump_path: &str,
    expected_hash: &str,
    expected_bytes: u64,
    out: &mut impl Write,
) -> Result<(), CliError> {
    create_data_dir(dir)?;
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
    writeln!(out, "database: {bytes} bytes restored and checked")?;
    let local = Store::new(craft_cloud_store::disk::DiskBackend::new(dir.join("blobs"))?);
    let source = Store::new(remote.clone());
    for (name, kind) in [("manifests", BlobKind::Manifest), ("chunks", BlobKind::Chunk)] {
        let counts = copy_blobs(&source, &local, kind).await?;
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
