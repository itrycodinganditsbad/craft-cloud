//! SFTP backend: one file per object under a root directory on a file server the operator rents or owns.
//!
//! The layout is the disk backend's (`chunks/ab/cd/<hex>` below the root). A write goes to `<name>.tmp-<UUIDv7>` in
//! the target's directory and is published with the `posix-rename@openssh.com` extension, which replaces an existing
//! target in one step. A server that does not offer the extension is refused when the backend connects: replacing
//! by deleting and then renaming could lose the old object, and the plain SFTP rename does not promise to replace.
//!
//! The server is authenticated by a pinned host key (a fingerprint, a public key, or a `known_hosts` file) and
//! there is no mode without one. The client authenticates with a private key; there is no password mode. SFTP has
//! no portable equivalent of `fsync`, so a successful write means the server acknowledged it (and, when the server
//! offers `fsync@openssh.com`, flushed the file); the remote store needs its own backups.
//!
//! A small pool of connections is shared by all operations. A request that fails because the connection broke, timed
//! out or was refused for now is retried a bounded number of times on a fresh connection with exponential backoff;
//! a refusal (permissions, a missing extension, a wrong key) is not retried.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use russh::client;
use russh::keys::ssh_key::{self, Fingerprint, HashAlg};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as ClientError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::{Backend, StoreError};

/// The OpenSSH extension that renames over an existing target in one step.
const POSIX_RENAME: &str = "posix-rename@openssh.com";
/// The OpenSSH extension that flushes an open file to the server's disk.
const FSYNC: &str = "fsync@openssh.com";

/// One read or write request moves this many bytes: the size every SFTP server accepts.
const CHUNK: usize = 32 * 1024;
/// How many read or write requests of one object are in flight at once.
const WINDOW: usize = 16;
/// The longest path (in bytes) a caller may name, root excluded. A blob path is at most 80.
const MAX_PATH_BYTES: usize = 512;
/// The longest listing cursor, the longest path the store's cursors carry.
const MAX_CURSOR_BYTES: usize = 4096;
/// An object is read into memory up to the caller's limit, but not preallocated beyond this.
const MAX_PREALLOCATE: usize = 8 * 1024 * 1024;

/// Connections the pool opens at most, unless the config says another number.
pub const DEFAULT_CONNECTIONS: usize = 4;
/// The most connections a config may ask for: storage boxes allow few, and each holds a session.
pub const MAX_CONNECTIONS: usize = 16;
/// Seconds one connect, or one request, may take unless the config says another number.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// The longest timeout a config may ask for, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 300;
/// Retries after a transient failure unless the config says another number.
pub const DEFAULT_RETRIES: u32 = 3;
/// The most retries a config may ask for.
pub const MAX_RETRIES: u32 = 8;

/// Where and how to reach the file server. Credentials come from the server's config, never from clients.
#[derive(Clone)]
pub struct SftpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The directory that belongs to Craft Cloud: absolute (`/craft-cloud/blobs`), or relative to the account's home
    /// directory (`craft-cloud/blobs`). Created if it is missing. Nothing outside it is read or written.
    pub root: String,
    /// The private key to sign in with (OpenSSH or PKCS#8 format; Ed25519 or ECDSA).
    pub key_file: PathBuf,
    pub key_passphrase: Option<String>,
    /// The server's pinned host key: `SHA256:` and its fingerprint (as `ssh-keygen -lf` prints it), or a public key
    /// line (`ssh-ed25519 AAAA…`).
    pub host_key: Option<String>,
    /// A `known_hosts` file whose entries for the host (and port) are pinned.
    pub known_hosts: Option<PathBuf>,
    /// The most connections open at once, 1 to [`MAX_CONNECTIONS`].
    pub connections: usize,
    /// How long a connect or a request may take.
    pub timeout: Duration,
    /// How often a request that failed transiently is tried again (0 to [`MAX_RETRIES`]).
    pub retries: u32,
}

impl std::fmt::Debug for SftpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("root", &self.root)
            .field("key_file", &self.key_file)
            .field("key_passphrase", &self.key_passphrase.as_ref().map(|_| "<redacted>"))
            .field("host_key", &self.host_key)
            .field("known_hosts", &self.known_hosts)
            .field("connections", &self.connections)
            .field("timeout", &self.timeout)
            .field("retries", &self.retries)
            .finish()
    }
}

/// What the backend may be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// The blob store: paths below `chunks/` and `manifests/` only.
    Blobs,
    /// Any plain relative path (the backup's layout).
    Any,
}

/// Why an operation failed, as far as retrying is concerned.
#[derive(Debug)]
enum Fail {
    /// The path does not exist.
    NoSuchFile,
    /// Worth trying again on a fresh connection: a dropped connection, a timeout, a server that is busy.
    Transient(String),
    /// Trying again changes nothing: permissions, a refused key, a missing extension, a bad path.
    Permanent(String),
}

impl Fail {
    fn into_store_error(self) -> StoreError {
        match self {
            Fail::NoSuchFile => StoreError::Backend("the SFTP server reports that a file or directory does not exist".into()),
            Fail::Transient(message) => StoreError::Backend(format!("SFTP failed after retries: {message}")),
            Fail::Permanent(message) => StoreError::Backend(format!("SFTP failed: {message}")),
        }
    }
}

impl From<ClientError> for Fail {
    fn from(error: ClientError) -> Self {
        match &error {
            ClientError::Status(status) => match status.status_code {
                StatusCode::NoSuchFile => Fail::NoSuchFile,
                StatusCode::NoConnection | StatusCode::ConnectionLost => Fail::Transient(error.to_string()),
                _ => Fail::Permanent(error.to_string()),
            },
            ClientError::Timeout | ClientError::IO(_) | ClientError::UnexpectedBehavior(_) => Fail::Transient(error.to_string()),
            ClientError::Limited(_) | ClientError::UnexpectedPacket => Fail::Permanent(error.to_string()),
        }
    }
}

/// An SSH error while connecting or signing in.
fn ssh_fail(error: russh::Error) -> Fail {
    match error {
        russh::Error::UnknownKey => Fail::Permanent("the server's host key is not the pinned one; the connection was refused".into()),
        russh::Error::NotAuthenticated | russh::Error::NoAuthMethod => Fail::Permanent("the server did not accept the SSH key".into()),
        russh::Error::NoCommonAlgo { .. } => Fail::Permanent(format!("no SSH algorithm in common with the server ({error})")),
        other => Fail::Transient(format!("SSH: {other}")),
    }
}

/// What a host key must be to be trusted.
struct HostPin {
    keys: Vec<ssh_key::PublicKey>,
    fingerprints: Vec<Fingerprint>,
}

impl HostPin {
    /// The pinned keys and fingerprints a config names. At least one is required.
    fn resolve(config: &SftpConfig) -> Result<Self, StoreError> {
        let mut pin = HostPin { keys: Vec::new(), fingerprints: Vec::new() };
        if let Some(text) = &config.host_key {
            let text = text.trim();
            if text.starts_with("SHA256:") {
                let fingerprint =
                    text.parse::<Fingerprint>().map_err(|_| StoreError::Backend("sftp_host_key is not a valid SHA256 fingerprint".into()))?;
                pin.fingerprints.push(fingerprint);
            } else {
                let key = ssh_key::PublicKey::from_openssh(text).map_err(|_| {
                    StoreError::Backend("sftp_host_key is neither a SHA256:… fingerprint nor a public key line like `ssh-ed25519 AAAA…`".into())
                })?;
                pin.keys.push(key);
            }
        }
        if let Some(path) = &config.known_hosts {
            let found = russh::keys::known_hosts::known_host_keys_path(&config.host, config.port, path)
                .map_err(|e| StoreError::Backend(format!("sftp_known_hosts could not be read: {e}")))?;
            if found.is_empty() {
                return Err(StoreError::Backend("sftp_known_hosts has no key for this host and port".into()));
            }
            pin.keys.extend(found.into_iter().map(|(_, key)| key));
        }
        if pin.keys.is_empty() && pin.fingerprints.is_empty() {
            return Err(StoreError::Backend(
                "SFTP needs a pinned host key: set sftp_host_key (a SHA256: fingerprint or a public key line) or sftp_known_hosts; there is no mode without one".into(),
            ));
        }
        Ok(pin)
    }

    fn matches(&self, presented: &ssh_key::PublicKey) -> bool {
        self.keys.iter().any(|key| key.key_data() == presented.key_data()) || self.fingerprints.contains(&presented.fingerprint(HashAlg::Sha256))
    }

    /// The host key algorithms to offer: the pinned keys' own, so a server with several keys presents the pinned one.
    /// With only a fingerprint the algorithm is not known and the client's default list is used.
    fn algorithms(&self) -> Option<Vec<ssh_key::Algorithm>> {
        if !self.fingerprints.is_empty() || self.keys.is_empty() {
            return None;
        }
        let mut algorithms: Vec<ssh_key::Algorithm> = Vec::new();
        for key in &self.keys {
            let algorithm = key.algorithm();
            if !algorithms.contains(&algorithm) {
                algorithms.push(algorithm);
            }
        }
        Some(algorithms)
    }
}

/// The SSH client's callbacks: only the host key is checked, against the pin.
struct Pinned {
    pin: Arc<HostPin>,
}

impl client::Handler for Pinned {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => Ok(self.pin.matches(key)),
            // A certificate needs authorities to trust, which the config cannot name.
            russh::keys::PublicKeyOrCertificate::Certificate(_) => Ok(false),
        }
    }
}

/// One signed-in SFTP session.
struct Conn {
    sftp: RawSftpSession,
    /// The server flushes files on request (`fsync@openssh.com`).
    fsync: bool,
    /// Keeps the SSH connection open; dropping it closes the session.
    _ssh: client::Handle<Pinned>,
}

/// The extensions a server must offer, from its `SSH_FXP_VERSION` reply. Returns whether it also offers `fsync`.
fn check_extensions(extensions: &HashMap<String, String>) -> Result<bool, Fail> {
    if extensions.get(POSIX_RENAME).map(String::as_str) != Some("1") {
        return Err(Fail::Permanent(
            "the SFTP server does not offer the posix-rename@openssh.com extension, which Craft Cloud needs to publish an object atomically \
             (it never deletes and renames instead); use an OpenSSH-based SFTP server or another storage kind"
                .into(),
        ));
    }
    Ok(extensions.get(FSYNC).map(String::as_str) == Some("1"))
}

struct Inner {
    host: String,
    port: u16,
    user: String,
    /// The root with no trailing slash; empty for the account's home directory, `/` for the server's root.
    root: String,
    key: Arc<PrivateKey>,
    pin: Arc<HostPin>,
    timeout: Duration,
    retries: u32,
    permits: Semaphore,
    idle: Mutex<Vec<Arc<Conn>>>,
}

/// The SFTP blob store backend. Clones share one pool of connections.
#[derive(Clone)]
pub struct SftpBackend {
    inner: Arc<Inner>,
    layout: Layout,
}

impl SftpBackend {
    /// Connects (so a wrong host key, key, user or a server without `posix-rename@openssh.com` fails now), makes sure
    /// the root directory exists, and returns a backend for the blob store: it only touches `chunks/` and `manifests/`.
    pub async fn connect(config: &SftpConfig) -> Result<Self, StoreError> {
        Self::open(config, Layout::Blobs, true).await
    }

    /// Like [`SftpBackend::connect`], for a command that only reads: the root directory must exist already and nothing
    /// is created, so a config that points at the wrong place is an error and not a new empty store.
    pub async fn connect_existing(config: &SftpConfig) -> Result<Self, StoreError> {
        Self::open(config, Layout::Blobs, false).await
    }

    /// Like [`SftpBackend::connect`], for the server's backup: any plain relative path below the root may be named.
    pub async fn connect_for_backup(config: &SftpConfig) -> Result<Self, StoreError> {
        Self::open(config, Layout::Any, true).await
    }

    /// Like [`SftpBackend::connect_for_backup`], for a restore: the root directory must exist already and nothing is created.
    pub async fn connect_for_restore(config: &SftpConfig) -> Result<Self, StoreError> {
        Self::open(config, Layout::Any, false).await
    }

    async fn open(config: &SftpConfig, layout: Layout, create_root: bool) -> Result<Self, StoreError> {
        let bad = |message: &str| StoreError::Backend(message.to_owned());
        if config.host.trim().is_empty() || config.host.contains(['\0', '/', ' ']) {
            return Err(bad("sftp_host is empty or not a host name"));
        }
        if config.user.trim().is_empty() || config.user.contains('\0') {
            return Err(bad("sftp_user is empty"));
        }
        if config.port == 0 {
            return Err(bad("sftp_port must be 1 to 65535"));
        }
        if config.connections == 0 || config.connections > MAX_CONNECTIONS {
            return Err(StoreError::Backend(format!("sftp_connections must be 1 to {MAX_CONNECTIONS}")));
        }
        if config.timeout.is_zero() || config.timeout > Duration::from_secs(MAX_TIMEOUT_SECS) {
            return Err(StoreError::Backend(format!("sftp_timeout_secs must be 1 to {MAX_TIMEOUT_SECS}")));
        }
        if config.retries > MAX_RETRIES {
            return Err(StoreError::Backend(format!("sftp_retries must be 0 to {MAX_RETRIES}")));
        }
        let root = normalise_root(&config.root)?;
        let pin = Arc::new(HostPin::resolve(config)?);
        let key = load_key(config).await?;
        let backend = SftpBackend {
            inner: Arc::new(Inner {
                host: config.host.trim().to_owned(),
                port: config.port,
                user: config.user.clone(),
                root,
                key,
                pin,
                timeout: config.timeout,
                retries: config.retries,
                permits: Semaphore::new(config.connections),
                idle: Mutex::new(Vec::new()),
            }),
            layout,
        };
        let root = backend.inner.root.clone();
        backend
            .inner
            .run(|conn| {
                let root = root.clone();
                async move {
                    if create_root {
                        return mkdir_p(&conn, &root).await;
                    }
                    let probe = if root.is_empty() { "." } else { root.as_str() };
                    if is_dir(&conn, probe).await? {
                        Ok(())
                    } else {
                        Err(Fail::Permanent("the storage root directory does not exist on the SFTP server (is sftp_root right?)".into()))
                    }
                }
            })
            .await?;
        Ok(backend)
    }

    /// Checks a path the caller names and joins it to the root.
    fn full(&self, path: &str) -> Result<String, StoreError> {
        check_relative(path, self.layout)?;
        Ok(self.inner.join(path))
    }

    /// Uploads the file at `local` to `path`, in chunks, with the same temporary name and rename as [`Backend::put_raw`].
    pub async fn put_file_raw(&self, path: &str, local: &Path) -> Result<(), StoreError> {
        let full = self.full(path)?;
        self.inner.put(&full, Source::File(local.to_owned())).await
    }

    /// Downloads `path` to the file `local` (written as `local` plus `.part`, then renamed). `false`: no such object. An
    /// object the server reports as larger than `max_bytes` is refused before anything is read.
    pub async fn get_file_raw(&self, path: &str, local: &Path, max_bytes: u64) -> Result<bool, StoreError> {
        let full = self.full(path)?;
        let part = {
            let mut name = local.as_os_str().to_owned();
            name.push(".part");
            PathBuf::from(name)
        };
        let found = self
            .inner
            .run(|conn| {
                let (full, part) = (full.clone(), part.clone());
                async move { download(&conn, &full, &part, max_bytes).await }
            })
            .await;
        match found {
            Ok(true) => {
                std::fs::rename(&part, local)?;
                Ok(true)
            }
            Ok(false) => {
                let _ = std::fs::remove_file(&part);
                Ok(false)
            }
            Err(error) => {
                let _ = std::fs::remove_file(&part);
                Err(error)
            }
        }
    }

    /// Removes the temporary files of uploads that were cut off (a killed process, a connection lost for good) below
    /// `prefix` (empty: the whole root, for a backup root; the blob store takes `chunks` or `manifests`) that are older
    /// than `older_than`. Only the backend's own exact `.tmp-<UUIDv7>` names count, and age is read from the name. Walks
    /// every directory below `prefix`, so on a big store it takes a while. Returns how many it removed.
    pub async fn remove_stale_temp(&self, prefix: &str, older_than: Duration) -> Result<u64, StoreError> {
        if prefix.is_empty() {
            if self.layout == Layout::Blobs {
                return Err(StoreError::Backend("the prefix must be chunks or manifests".into()));
            }
        } else {
            check_relative(prefix, self.layout)?;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let cutoff = now.saturating_sub(older_than.as_secs());
        let dir = if prefix.is_empty() { self.inner.root_dir() } else { self.inner.join(prefix) };
        let removed = AtomicU64::new(0);
        self.inner
            .run(|conn| {
                let (dir, removed) = (dir.clone(), &removed);
                async move { sweep(&conn, dir, cutoff, removed).await.map(|_| ()) }
            })
            .await?;
        Ok(removed.load(Ordering::Relaxed))
    }

    /// Removes an empty directory; a directory that is not there or not empty is left alone. For the backup's old generations.
    pub async fn remove_dir_raw(&self, path: &str) -> Result<(), StoreError> {
        let full = self.full(path)?;
        self.inner
            .run(|conn| {
                let full = full.clone();
                async move {
                    match conn.sftp.rmdir(full).await {
                        Ok(_) => Ok(()),
                        Err(error) => match Fail::from(error) {
                            // Not empty (or not removable by this account): the directory stays.
                            Fail::NoSuchFile | Fail::Permanent(_) => Ok(()),
                            transient => Err(transient),
                        },
                    }
                }
            })
            .await
    }
}

/// `root` with no trailing slash and no way out: no NUL, no `..`.
fn normalise_root(root: &str) -> Result<String, StoreError> {
    let bad = |message: &str| StoreError::Backend(format!("sftp_root {message}"));
    if root.contains('\0') || root.contains('\\') {
        return Err(bad("must not contain NUL or a backslash"));
    }
    if root.split('/').any(|part| part == "..") {
        return Err(bad("must not contain .."));
    }
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        // "" and "." are the home directory; "/" (and "//") the server's root, which is usually the account's own jail.
        return Ok(if root.starts_with('/') { "/".to_owned() } else { String::new() });
    }
    Ok(if trimmed == "." { String::new() } else { trimmed.to_owned() })
}

/// A path the caller names must be plain and relative, and for the blob store below `chunks/` or `manifests/`.
fn check_relative(path: &str, layout: Layout) -> Result<(), StoreError> {
    let bad = |why: &str| StoreError::Backend(format!("object path is refused: {why}"));
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(bad("empty or too long"));
    }
    if path.starts_with('/') || path.contains('\\') || path.chars().any(char::is_control) {
        return Err(bad("not a relative path"));
    }
    let mut segments = path.split('/');
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(bad("empty, . or .. segment"));
        }
    }
    if layout == Layout::Blobs && !matches!(segments.next(), Some("chunks" | "manifests")) {
        return Err(bad("outside chunks/ and manifests/"));
    }
    Ok(())
}

async fn load_key(config: &SftpConfig) -> Result<Arc<PrivateKey>, StoreError> {
    let path = config.key_file.clone();
    let passphrase = config.key_passphrase.clone();
    // Reading and (for an encrypted key) deriving the passphrase key is blocking work.
    let key = tokio::task::spawn_blocking(move || russh::keys::load_secret_key(&path, passphrase.as_deref()))
        .await
        .map_err(|e| StoreError::Backend(format!("loading the SFTP key failed: {e}")))?
        .map_err(|e| StoreError::Backend(format!("the SFTP key file could not be read or decrypted ({e}); Ed25519 and ECDSA keys are supported")))?;
    Ok(Arc::new(key))
}

impl Inner {
    fn join(&self, relative: &str) -> String {
        match self.root.as_str() {
            "" => relative.to_owned(),
            "/" => format!("/{relative}"),
            root => format!("{root}/{relative}"),
        }
    }

    /// The root as a directory to open: `.` for the home directory.
    fn root_dir(&self) -> String {
        match self.root.as_str() {
            "" => ".".to_owned(),
            root => root.to_owned(),
        }
    }

    fn take_idle(&self) -> Option<Arc<Conn>> {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner).pop()
    }

    /// Closes every pooled connection that is not in use.
    fn drop_idle(&self) {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    fn give_back(&self, conn: Arc<Conn>) {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner).push(conn);
    }

    async fn dial(&self) -> Result<Conn, Fail> {
        match tokio::time::timeout(self.timeout, self.dial_inner()).await {
            Ok(result) => result,
            Err(_) => Err(Fail::Transient("connecting timed out".into())),
        }
    }

    async fn dial_inner(&self) -> Result<Conn, Fail> {
        let mut config = client::Config {
            // A generic name and version, like the User-Agent of every other request this program makes.
            client_id: russh::SshId::Standard(Cow::Owned(format!("SSH-2.0-CraftCloud_{}", env!("CARGO_PKG_VERSION")))),
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            nodelay: true,
            ..client::Config::default()
        };
        if let Some(algorithms) = self.pin.algorithms() {
            config.preferred.key = Cow::Owned(algorithms);
        }
        let mut ssh =
            client::connect(Arc::new(config), (self.host.as_str(), self.port), Pinned { pin: Arc::clone(&self.pin) }).await.map_err(ssh_fail)?;
        let signed_in = ssh.authenticate_publickey(&self.user, PrivateKeyWithHashAlg::new(Arc::clone(&self.key), None)).await.map_err(ssh_fail)?;
        if !signed_in.success() {
            return Err(Fail::Permanent("the server did not accept the SSH key".into()));
        }
        let channel = ssh.channel_open_session().await.map_err(ssh_fail)?;
        channel.request_subsystem(true, "sftp").await.map_err(ssh_fail)?;
        let config = russh_sftp::client::Config { request_timeout_secs: self.timeout.as_secs().max(1), ..Default::default() };
        let sftp = RawSftpSession::new_with_config(channel.into_stream(), config);
        let version = sftp.init().await.map_err(Fail::from)?;
        let fsync = check_extensions(&version.extensions)?;
        Ok(Conn { sftp, fsync, _ssh: ssh })
    }

    /// Runs `op` on a pooled connection; a transient failure is tried again on a fresh one, with backoff.
    async fn run<T, Fut>(&self, mut op: impl FnMut(Arc<Conn>) -> Fut) -> Result<T, StoreError>
    where
        Fut: Future<Output = Result<T, Fail>>,
    {
        let mut retried = 0u32;
        let mut stale_retried = false;
        let mut fresh = false;
        loop {
            let mut from_idle = false;
            match self.once(&mut op, &mut from_idle, std::mem::take(&mut fresh)).await {
                Ok(value) => return Ok(value),
                // A pooled connection can have died while it was idle (the server closed it, a firewall forgot it). That
                // is no reason to count a retry or to wait: one connection made for the purpose is tried at once, whatever
                // `retries` is. It is never another pooled one, which is as likely to be dead: the others are dropped.
                Err(Fail::Transient(_)) if from_idle && !stale_retried => {
                    stale_retried = true;
                    fresh = true;
                    self.drop_idle();
                }
                Err(Fail::Transient(_)) if retried < self.retries => {
                    if from_idle {
                        self.drop_idle();
                    }
                    retried = retried.saturating_add(1);
                    tokio::time::sleep(backoff(retried)).await;
                }
                Err(fail) => return Err(fail.into_store_error()),
            }
        }
    }

    /// One try of `op`. A connection comes from the pool unless `fresh` asks for a new one (which also empties the pool).
    async fn once<T, Fut>(&self, op: &mut impl FnMut(Arc<Conn>) -> Fut, from_idle: &mut bool, fresh: bool) -> Result<T, Fail>
    where
        Fut: Future<Output = Result<T, Fail>>,
    {
        let _permit = self.permits.acquire().await.map_err(|_| Fail::Permanent("the connection pool is closed".into()))?;
        if fresh {
            self.drop_idle();
        }
        let idle = if fresh { None } else { self.take_idle() };
        let conn = match idle {
            Some(conn) => {
                *from_idle = true;
                conn
            }
            None => Arc::new(self.dial().await?),
        };
        let result = op(Arc::clone(&conn)).await;
        match &result {
            // The connection answered, whatever the answer was.
            Ok(_) | Err(Fail::NoSuchFile | Fail::Permanent(_)) => self.give_back(conn),
            // A broken connection is dropped, which closes it.
            Err(Fail::Transient(_)) => {}
        }
        result
    }

    async fn put(&self, full: &str, source: Source) -> Result<(), StoreError> {
        let leftovers = Mutex::new(Vec::new());
        let result = self
            .run(|conn| {
                let (full, source) = (full.to_owned(), source.clone());
                let leftovers = &leftovers;
                async move { put_object(&conn, &full, &source, leftovers).await }
            })
            .await;
        // The attempts are over. A temporary file one of them could not remove (its connection was gone) is removed on a
        // fresh connection now, once; what still fails is left for `remove_stale_temp`.
        if result.is_err() && !leftovers.lock().unwrap_or_else(PoisonError::into_inner).is_empty() {
            let _ = self
                .once(
                    &mut |conn: Arc<Conn>| {
                        let leftovers = &leftovers;
                        async move { remove_leftovers(&conn, leftovers).await }
                    },
                    &mut false,
                    false,
                )
                .await;
        }
        result
    }
}

/// 200 ms, doubling to 5 s, then up to a quarter off at random so that clients do not retry in step.
fn backoff(retried: u32) -> Duration {
    let base = 200u64.saturating_mul(1u64 << retried.saturating_sub(1).min(5)).min(5_000);
    let jitter = u64::try_from(uuid::Uuid::now_v7().as_u128() % 256).unwrap_or(0);
    Duration::from_millis(base.saturating_sub(base.saturating_mul(jitter) / 1024))
}

/// What an upload sends. Cloned for each attempt: a retry starts the upload again.
#[derive(Clone)]
enum Source {
    Bytes(Bytes),
    File(PathBuf),
}

/// Reads an upload's source in chunks, in order.
enum Reader {
    Bytes { data: Bytes, offset: usize },
    File { file: tokio::fs::File, offset: u64 },
}

impl Reader {
    async fn open(source: &Source) -> Result<Self, Fail> {
        match source {
            Source::Bytes(data) => Ok(Reader::Bytes { data: data.clone(), offset: 0 }),
            Source::File(path) => {
                let file = tokio::fs::File::open(path).await.map_err(|e| Fail::Permanent(format!("the local file could not be opened: {e}")))?;
                Ok(Reader::File { file, offset: 0 })
            }
        }
    }

    /// The next chunk and the offset it belongs at, or `None` at the end.
    async fn next(&mut self) -> Result<Option<(u64, Vec<u8>)>, Fail> {
        match self {
            Reader::Bytes { data, offset } => {
                let end = offset.saturating_add(CHUNK).min(data.len());
                let Some(part) = data.get(*offset..end).filter(|part| !part.is_empty()) else { return Ok(None) };
                let chunk = (u64::try_from(*offset).unwrap_or(u64::MAX), part.to_vec());
                *offset = end;
                Ok(Some(chunk))
            }
            Reader::File { file, offset } => {
                let mut buffer = vec![0u8; CHUNK];
                let mut filled = 0usize;
                while filled < buffer.len() {
                    let Some(rest) = buffer.get_mut(filled..) else { break };
                    let read = file.read(rest).await.map_err(|e| Fail::Permanent(format!("the local file could not be read: {e}")))?;
                    if read == 0 {
                        break;
                    }
                    filled = filled.saturating_add(read);
                }
                if filled == 0 {
                    return Ok(None);
                }
                buffer.truncate(filled);
                let at = *offset;
                *offset = at.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX));
                Ok(Some((at, buffer)))
            }
        }
    }
}

/// `parent/` and the name of `full`: the parent keeps its trailing slash so that `/name` has a parent too.
fn split_parent(full: &str) -> (&str, &str) {
    match full.rfind('/') {
        Some(index) => full.split_at(index.saturating_add(1)),
        None => ("", full),
    }
}

/// Creates `dir` and the directories above it that are missing. A directory that exists is not an error.
async fn mkdir_p(conn: &Conn, dir: &str) -> Result<(), Fail> {
    let mut missing: Vec<&str> = Vec::new();
    let mut current = dir.trim_end_matches('/');
    loop {
        if current.is_empty() {
            break;
        }
        match conn.sftp.mkdir(current, FileAttributes::default()).await {
            Ok(_) => break,
            Err(error) => match Fail::from(error) {
                // The parent is missing: make it first.
                Fail::NoSuchFile => {
                    missing.push(current);
                    match split_parent(current).0.trim_end_matches('/') {
                        "" => return Err(Fail::Permanent("a directory could not be created: no parent".into())),
                        parent => current = parent,
                    }
                }
                // Probably it exists already (the protocol has no "exists" status): check.
                Fail::Permanent(message) => {
                    if is_dir(conn, current).await? {
                        break;
                    }
                    return Err(Fail::Permanent(message));
                }
                transient => return Err(transient),
            },
        }
    }
    while let Some(next) = missing.pop() {
        if let Err(error) = conn.sftp.mkdir(next, FileAttributes::default()).await {
            match Fail::from(error) {
                Fail::Permanent(message) => {
                    if !is_dir(conn, next).await? {
                        return Err(Fail::Permanent(message));
                    }
                }
                other => return Err(other),
            }
        }
    }
    Ok(())
}

async fn is_dir(conn: &Conn, path: &str) -> Result<bool, Fail> {
    match conn.sftp.stat(path).await {
        Ok(attrs) => Ok(kind_of(&attrs.attrs) == Some(Kind::Dir)),
        Err(error) => match Fail::from(error) {
            Fail::NoSuchFile => Ok(false),
            other => Err(other),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Dir,
    File,
    /// A symbolic link, a device, a socket: never followed and never reported as an object.
    Other,
}

/// What kind of entry the attributes describe; `None` when the server sent no permission bits.
fn kind_of(attrs: &FileAttributes) -> Option<Kind> {
    let mode = attrs.permissions?;
    Some(match mode & 0o170_000 {
        0o040_000 => Kind::Dir,
        0o100_000 => Kind::File,
        _ => Kind::Other,
    })
}

/// An SFTP string: a 32-bit big-endian length and the bytes.
fn push_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// Renames `from` over `to` in one step.
async fn posix_rename(conn: &Conn, from: &str, to: &str) -> Result<(), Fail> {
    let mut data = Vec::new();
    push_string(&mut data, from);
    push_string(&mut data, to);
    match conn.sftp.extended(POSIX_RENAME, data).await.map_err(Fail::from)? {
        Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
        Packet::Status(status) => Err(Fail::from(ClientError::Status(status))),
        _ => Err(Fail::Permanent("unexpected reply to posix-rename".into())),
    }
}

/// Removes the temporary files that earlier attempts of an upload left because their connection was gone. One that
/// cannot be removed now stays on the list.
async fn remove_leftovers(conn: &Conn, leftovers: &Mutex<Vec<String>>) -> Result<(), Fail> {
    let earlier: Vec<String> = std::mem::take(&mut *leftovers.lock().unwrap_or_else(PoisonError::into_inner));
    for path in earlier {
        match conn.sftp.remove(path.clone()).await {
            Ok(_) => {}
            Err(error) => match Fail::from(error) {
                Fail::NoSuchFile => {}
                _ => leftovers.lock().unwrap_or_else(PoisonError::into_inner).push(path),
            },
        }
    }
    Ok(())
}

/// Writes `source` to a temporary name beside `full` and renames it into place. The temporary file is removed when
/// anything fails; when that removal fails too (the connection is gone), the name goes on `leftovers` and the next
/// attempt, on a fresh connection, removes it. A failure of the removal never hides the original error.
async fn put_object(conn: &Conn, full: &str, source: &Source, leftovers: &Mutex<Vec<String>>) -> Result<(), Fail> {
    remove_leftovers(conn, leftovers).await?;
    let (parent, _) = split_parent(full);
    let temporary = format!("{parent}{}", crate::disk::temp_name(uuid::Uuid::now_v7()));
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE;
    let handle = match conn.sftp.open(temporary.clone(), flags, FileAttributes::default()).await {
        Ok(handle) => handle.handle,
        Err(error) => match Fail::from(error) {
            Fail::NoSuchFile => {
                mkdir_p(conn, parent).await?;
                conn.sftp.open(temporary.clone(), flags, FileAttributes::default()).await.map_err(Fail::from)?.handle
            }
            other => return Err(other),
        },
    };
    let written = write_all(conn, &handle, source).await;
    let closed = conn.sftp.close(handle).await.map_err(Fail::from);
    let outcome = match (written, closed) {
        (Ok(()), Ok(_)) => posix_rename(conn, &temporary, full).await,
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    };
    if outcome.is_err() {
        match conn.sftp.remove(temporary.clone()).await {
            Ok(_) => {}
            Err(error) => {
                if !matches!(Fail::from(error), Fail::NoSuchFile) {
                    leftovers.lock().unwrap_or_else(PoisonError::into_inner).push(temporary);
                }
            }
        }
    }
    outcome
}

/// Removes the backend's own temporary files below `dir` that are older than `cutoff` (seconds since the epoch, by the
/// time in their UUIDv7 names). Counts them in `removed`.
fn sweep<'a>(conn: &'a Conn, dir: String, cutoff: u64, removed: &'a AtomicU64) -> WalkFuture<'a> {
    Box::pin(async move {
        let Some(entries) = read_dir(conn, &dir).await? else { return Ok(false) };
        for (name, kind) in entries {
            let child = if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") };
            match kind {
                Kind::Dir => {
                    sweep(conn, child, cutoff, removed).await?;
                }
                Kind::File if crate::disk::temp_created_secs(&name).is_some_and(|created| created < cutoff) => match conn.sftp.remove(child).await {
                    Ok(_) => {
                        removed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => match Fail::from(error) {
                        Fail::NoSuchFile => {}
                        other => return Err(other),
                    },
                },
                Kind::File | Kind::Other => {}
            }
        }
        Ok(false)
    })
}

async fn write_all(conn: &Conn, handle: &str, source: &Source) -> Result<(), Fail> {
    let reader = Reader::open(source).await?;
    stream::try_unfold(reader, |mut reader| async move {
        match reader.next().await? {
            Some(chunk) => Ok::<_, Fail>(Some((chunk, reader))),
            None => Ok(None),
        }
    })
    .map(|chunk| async move {
        let (offset, data) = chunk?;
        conn.sftp.write(handle, offset, data).await.map(|_| ()).map_err(Fail::from)
    })
    .buffer_unordered(WINDOW)
    .try_for_each(|()| async { Ok::<(), Fail>(()) })
    .await?;
    if conn.fsync {
        conn.sftp.fsync(handle).await.map_err(Fail::from)?;
    }
    Ok(())
}

/// Opens `full` for reading, `None` when it does not exist.
async fn open_read(conn: &Conn, full: &str) -> Result<Option<String>, Fail> {
    match conn.sftp.open(full, OpenFlags::READ, FileAttributes::default()).await {
        Ok(handle) => Ok(Some(handle.handle)),
        Err(error) => match Fail::from(error) {
            Fail::NoSuchFile => Ok(None),
            other => Err(other),
        },
    }
}

/// The chunks (offset, length) that cover `total` bytes, made as they are asked for: the size a server reports is not
/// trusted to be small enough to list every chunk of it at once.
fn chunk_plan(total: usize) -> impl Iterator<Item = (usize, usize)> {
    (0..total.div_ceil(CHUNK)).map(move |index| {
        let offset = index.saturating_mul(CHUNK);
        (offset, CHUNK.min(total.saturating_sub(offset)))
    })
}

/// Reads one chunk; a server that answers with less than was asked, before the end the file's size promised, is not trusted.
async fn read_chunk(conn: &Conn, handle: &str, offset: usize, len: usize) -> Result<Vec<u8>, Fail> {
    let at = u64::try_from(offset).unwrap_or(u64::MAX);
    let want = u32::try_from(len).unwrap_or(u32::MAX);
    match conn.sftp.read(handle, at, want).await {
        Ok(data) if data.data.len() == len => Ok(data.data),
        Ok(_) => Err(Fail::Transient("the server returned a short read".into())),
        Err(ClientError::Status(status)) if status.status_code == StatusCode::Eof => Err(Fail::Transient("the object ended early".into())),
        Err(error) => Err(Fail::from(error)),
    }
}

/// Reads at most `max_bytes` plus one byte of `full`, in parallel requests. `None`: no such object.
async fn read_object(conn: &Conn, full: &str, max_bytes: usize) -> Result<Option<Vec<u8>>, Fail> {
    let Some(handle) = open_read(conn, full).await? else { return Ok(None) };
    let result = read_open(conn, &handle, max_bytes).await;
    let _ = conn.sftp.close(handle).await;
    result.map(Some)
}

async fn read_open(conn: &Conn, handle: &str, max_bytes: usize) -> Result<Vec<u8>, Fail> {
    let size = conn.sftp.fstat(handle).await.map_err(Fail::from)?.attrs.size.ok_or_else(|| Fail::Permanent("the server gave no file size".into()))?;
    let total = usize::try_from(size).unwrap_or(usize::MAX).min(max_bytes.saturating_add(1));
    let mut out = Vec::with_capacity(total.min(MAX_PREALLOCATE));
    let mut parts = std::pin::pin!(stream::iter(chunk_plan(total)).map(|(offset, len)| read_chunk(conn, handle, offset, len)).buffered(WINDOW));
    while let Some(part) = parts.try_next().await? {
        out.extend_from_slice(&part);
    }
    Ok(out)
}

/// Copies `full` to the local file `part`. `false`: no such object.
async fn download(conn: &Conn, full: &str, part: &Path, max_bytes: u64) -> Result<bool, Fail> {
    let Some(handle) = open_read(conn, full).await? else { return Ok(false) };
    let result = download_open(conn, &handle, part, max_bytes).await;
    let _ = conn.sftp.close(handle).await;
    result.map(|()| true)
}

async fn download_open(conn: &Conn, handle: &str, part: &Path, max_bytes: u64) -> Result<(), Fail> {
    let local = |e: std::io::Error| Fail::Permanent(format!("the local file could not be written: {e}"));
    let size = conn.sftp.fstat(handle).await.map_err(Fail::from)?.attrs.size.ok_or_else(|| Fail::Permanent("the server gave no file size".into()))?;
    if size > max_bytes {
        return Err(Fail::Permanent(format!("the object is {size} bytes on the server, more than the {max_bytes} expected; it is not downloaded")));
    }
    let total = usize::try_from(size).map_err(|_| Fail::Permanent("the object is too large for this machine".into()))?;
    let mut file = tokio::fs::File::create(part).await.map_err(local)?;
    let mut parts = std::pin::pin!(stream::iter(chunk_plan(total)).map(|(offset, len)| read_chunk(conn, handle, offset, len)).buffered(WINDOW));
    while let Some(part) = parts.try_next().await? {
        file.write_all(&part).await.map_err(local)?;
    }
    file.flush().await.map_err(local)?;
    file.sync_all().await.map_err(local)?;
    Ok(())
}

/// A plain name from a directory listing: anything else is a hostile or broken server.
fn plain_name(name: &str) -> Result<(), Fail> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Fail::Permanent("the server listed a name that is not a plain file name".into()));
    }
    Ok(())
}

/// The entries of a directory: (name, kind). `None` when the directory does not exist.
async fn read_dir(conn: &Conn, dir: &str) -> Result<Option<Vec<(String, Kind)>>, Fail> {
    let handle = match conn.sftp.opendir(dir).await {
        Ok(handle) => handle.handle,
        Err(error) => {
            return match Fail::from(error) {
                Fail::NoSuchFile => Ok(None),
                other => Err(other),
            };
        }
    };
    let result = read_dir_open(conn, dir, &handle).await;
    let _ = conn.sftp.close(handle).await;
    result.map(Some)
}

async fn read_dir_open(conn: &Conn, dir: &str, handle: &str) -> Result<Vec<(String, Kind)>, Fail> {
    let mut entries = Vec::new();
    loop {
        let names = match conn.sftp.readdir(handle).await {
            Ok(names) => names,
            Err(ClientError::Status(status)) if status.status_code == StatusCode::Eof => break,
            Err(error) => return Err(Fail::from(error)),
        };
        if names.files.is_empty() {
            break;
        }
        for file in names.files {
            if file.filename == "." || file.filename == ".." {
                continue;
            }
            plain_name(&file.filename)?;
            let kind = match kind_of(&file.attrs) {
                Some(kind) => kind,
                // The listing carried no mode: ask for it.
                None => {
                    let child = if dir.ends_with('/') { format!("{dir}{}", file.filename) } else { format!("{dir}/{}", file.filename) };
                    match conn.sftp.lstat(child).await {
                        Ok(attrs) => kind_of(&attrs.attrs).unwrap_or(Kind::Other),
                        Err(error) => match Fail::from(error) {
                            Fail::NoSuchFile => continue,
                            other => return Err(other),
                        },
                    }
                }
            };
            entries.push((file.filename, kind));
        }
    }
    Ok(entries)
}

type WalkFuture<'a> = Pin<Box<dyn Future<Output = Result<bool, Fail>> + Send + 'a>>;

/// Collects up to `limit` file paths below `dir` that sort strictly after `after`, in byte order of the relative path,
/// as the disk backend does (the same rules: symbolic links are skipped, the backend's own temporary names are
/// hidden, a subtree entirely before `after` is not opened). `relative` is `dir`'s path below the root. `true`: full.
fn walk<'a>(conn: &'a Conn, dir: String, relative: String, after: Option<&'a str>, limit: usize, out: &'a mut Vec<String>) -> WalkFuture<'a> {
    Box::pin(async move {
        if out.len() >= limit {
            return Ok(true);
        }
        let Some(listed) = read_dir(conn, &dir).await? else { return Ok(false) };
        // (sort key, name, kind): a directory sorts as `name/`.
        let mut entries: Vec<(String, String, Kind)> = Vec::new();
        for (name, kind) in listed {
            match kind {
                Kind::Dir => entries.push((format!("{name}/"), name, kind)),
                Kind::File if !crate::disk::is_temp_name(&name) => entries.push((name.clone(), name, kind)),
                Kind::File | Kind::Other => {}
            }
        }
        entries.sort_unstable();
        for (_, name, kind) in entries {
            let path = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
            if kind == Kind::Dir {
                let subtree = format!("{path}/");
                if after.is_some_and(|cursor| cursor > subtree.as_str() && !cursor.starts_with(&subtree)) {
                    continue;
                }
                let child = if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") };
                if walk(conn, child, path, after, limit, out).await? {
                    return Ok(true);
                }
            } else if after.is_none_or(|cursor| path.as_str() > cursor) {
                out.push(path);
                if out.len() >= limit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })
}

#[async_trait]
impl Backend for SftpBackend {
    fn kind(&self) -> &'static str {
        "sftp"
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<(), StoreError> {
        let full = self.full(path)?;
        self.inner.put(&full, Source::Bytes(data)).await
    }

    async fn get_raw(&self, path: &str) -> Result<Option<Bytes>, StoreError> {
        self.get_raw_limited(path, usize::MAX).await
    }

    async fn get_raw_limited(&self, path: &str, max_bytes: usize) -> Result<Option<Bytes>, StoreError> {
        let full = self.full(path)?;
        let found = self
            .inner
            .run(|conn| {
                let full = full.clone();
                async move { read_object(&conn, &full, max_bytes).await }
            })
            .await?;
        Ok(found.map(Bytes::from))
    }

    async fn size_raw(&self, path: &str) -> Result<Option<u64>, StoreError> {
        let full = self.full(path)?;
        self.inner
            .run(|conn| {
                let full = full.clone();
                async move {
                    match conn.sftp.lstat(full).await {
                        Ok(attrs) => match kind_of(&attrs.attrs) {
                            Some(Kind::Dir | Kind::Other) => Ok(None),
                            Some(Kind::File) | None => {
                                attrs.attrs.size.map(Some).ok_or_else(|| Fail::Permanent("the server gave no file size".into()))
                            }
                        },
                        Err(error) => match Fail::from(error) {
                            Fail::NoSuchFile => Ok(None),
                            other => Err(other),
                        },
                    }
                }
            })
            .await
    }

    async fn delete_raw(&self, path: &str) -> Result<(), StoreError> {
        let full = self.full(path)?;
        self.inner
            .run(|conn| {
                let full = full.clone();
                async move {
                    match conn.sftp.remove(full).await {
                        Ok(_) => Ok(()),
                        Err(error) => match Fail::from(error) {
                            Fail::NoSuchFile => Ok(()),
                            other => Err(other),
                        },
                    }
                }
            })
            .await
    }

    async fn list_raw(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>, StoreError> {
        if !prefix.is_empty() {
            check_relative(prefix, self.layout)?;
        } else if self.layout == Layout::Blobs {
            return Err(StoreError::Backend("listing prefix must be chunks or manifests".into()));
        }
        if let Some(cursor) = after {
            // A cursor is only compared with names, never sent to the server, and it can be any name the listing returned
            // (a foreign file may have a backslash or a control character in its name): only its size and NUL are checked.
            if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES || cursor.contains('\0') {
                return Err(StoreError::Backend("listing cursor is refused: empty, too long or with a NUL".into()));
            }
        }
        let dir = if prefix.is_empty() { self.inner.root_dir() } else { self.inner.join(prefix) };
        self.inner
            .run(|conn| {
                let (dir, prefix) = (dir.clone(), prefix.to_owned());
                async move {
                    let mut out = Vec::new();
                    walk(&conn, dir, prefix, after, limit, &mut out).await?;
                    Ok(out)
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPAV72SkMYdWcs+Y19sIQJFahyXAm1LZbNGMuu76zEl4";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH72VOenl6FhiXMuD2gKb2s9qxxWEnmdLLgAL2MsiEDE";

    fn config() -> SftpConfig {
        SftpConfig {
            host: "files.example".into(),
            port: 22,
            user: "craft".into(),
            root: "/blobs".into(),
            key_file: PathBuf::from("key"),
            key_passphrase: Some("hunter2".into()),
            host_key: None,
            known_hosts: None,
            connections: 2,
            timeout: Duration::from_secs(5),
            retries: 1,
        }
    }

    fn key(line: &str) -> ssh_key::PublicKey {
        ssh_key::PublicKey::from_openssh(line).unwrap()
    }

    #[test]
    fn paths_that_leave_the_root_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "chunks/../../etc/passwd",
            "chunks/..",
            "chunks//ab",
            "chunks/./ab",
            "chunks\\ab",
            "chunks/ab\0",
            "chunks/ab\n",
            "other/ab",
            "chunksx/ab",
            "C:/chunks/ab",
        ] {
            assert!(check_relative(bad, Layout::Blobs).is_err(), "{bad:?} must be refused");
        }
        for good in ["chunks/ab/cd/0123", "manifests/ab/cd/0123", "chunks/foreign-object"] {
            assert!(check_relative(good, Layout::Blobs).is_ok(), "{good:?}");
        }
        assert!(check_relative("generations/2026/meta.sqlite", Layout::Any).is_ok());
        assert!(check_relative("generations/../x", Layout::Any).is_err());
        assert!(check_relative(&"a/".repeat(300), Layout::Any).is_err());
    }

    #[test]
    fn roots_are_normalised_and_cannot_climb() {
        assert_eq!(normalise_root("/srv/craft/").unwrap(), "/srv/craft");
        assert_eq!(normalise_root("craft/blobs").unwrap(), "craft/blobs");
        assert_eq!(normalise_root("").unwrap(), "");
        assert_eq!(normalise_root(".").unwrap(), "");
        assert_eq!(normalise_root("/").unwrap(), "/");
        assert!(normalise_root("/srv/../etc").is_err());
        assert!(normalise_root("/srv\0").is_err());
        assert!(normalise_root("C:\\x").is_err());
    }

    #[test]
    fn a_server_without_posix_rename_is_refused() {
        let mut extensions = HashMap::new();
        extensions.insert("fsync@openssh.com".to_owned(), "1".to_owned());
        let Err(Fail::Permanent(message)) = check_extensions(&extensions) else { panic!("must be refused") };
        assert!(message.contains("posix-rename@openssh.com"), "{message}");
        // Another version of the extension is not the one this code speaks.
        extensions.insert(POSIX_RENAME.to_owned(), "2".to_owned());
        assert!(check_extensions(&extensions).is_err());
        extensions.insert(POSIX_RENAME.to_owned(), "1".to_owned());
        assert!(check_extensions(&extensions).unwrap());
        extensions.remove(FSYNC);
        assert!(!check_extensions(&extensions).unwrap());
    }

    #[test]
    fn only_the_pinned_host_key_is_accepted() {
        let mut with_key = config();
        with_key.host_key = Some(KEY_A.into());
        let pin = HostPin::resolve(&with_key).unwrap();
        assert!(pin.matches(&key(KEY_A)));
        assert!(!pin.matches(&key(KEY_B)));
        assert!(pin.algorithms().is_some());

        let mut with_fingerprint = config();
        with_fingerprint.host_key = Some(key(KEY_A).fingerprint(HashAlg::Sha256).to_string());
        let pin = HostPin::resolve(&with_fingerprint).unwrap();
        assert!(pin.matches(&key(KEY_A)));
        assert!(!pin.matches(&key(KEY_B)));
        assert!(pin.algorithms().is_none());
    }

    #[test]
    fn a_missing_or_malformed_pin_is_refused() {
        let message = |config: &SftpConfig| match HostPin::resolve(config) {
            Err(StoreError::Backend(message)) => message,
            Err(other) => panic!("unexpected {other:?}"),
            Ok(_) => panic!("must be refused"),
        };
        assert!(message(&config()).contains("pinned host key"));
        let mut malformed = config();
        malformed.host_key = Some("SHA256:not-a-fingerprint".into());
        assert!(message(&malformed).contains("fingerprint"));
        malformed.host_key = Some("not a key".into());
        assert!(message(&malformed).contains("public key line"));
        let mut missing_file = config();
        missing_file.known_hosts = Some(PathBuf::from("does-not-exist-known-hosts"));
        assert!(message(&missing_file).contains("sftp_known_hosts"));
    }

    #[test]
    fn known_hosts_pins_the_host_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, format!("files.example {KEY_A}\n[other.example]:2222 {KEY_B}\n")).unwrap();
        let mut config = config();
        config.known_hosts = Some(path.clone());
        let pin = HostPin::resolve(&config).unwrap();
        assert!(pin.matches(&key(KEY_A)) && !pin.matches(&key(KEY_B)));
        config.host = "other.example".into();
        config.port = 2222;
        let pin = HostPin::resolve(&config).unwrap();
        assert!(pin.matches(&key(KEY_B)) && !pin.matches(&key(KEY_A)));
        // A host that is not in the file has no pin, which is a refusal and not "any key".
        config.host = "stranger.example".into();
        assert!(HostPin::resolve(&config).is_err());
    }

    #[test]
    fn the_config_does_not_show_the_passphrase() {
        let shown = format!("{:?}", config());
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn chunks_cover_exactly_the_requested_bytes() {
        assert_eq!(chunk_plan(0).count(), 0);
        assert_eq!(chunk_plan(1).collect::<Vec<_>>(), vec![(0, 1)]);
        assert_eq!(chunk_plan(CHUNK).collect::<Vec<_>>(), vec![(0, CHUNK)]);
        assert_eq!(chunk_plan(CHUNK + 1).collect::<Vec<_>>(), vec![(0, CHUNK), (CHUNK, 1)]);
        assert_eq!(chunk_plan(5 * CHUNK + 7).map(|(_, len)| len).sum::<usize>(), 5 * CHUNK + 7);
        // A server that claims an absurd size costs nothing until the chunks are asked for.
        assert_eq!(chunk_plan(usize::MAX).take(3).collect::<Vec<_>>(), vec![(0, CHUNK), (CHUNK, CHUNK), (2 * CHUNK, CHUNK)]);
    }

    #[test]
    fn temporary_names_carry_their_age() {
        let id = uuid::Uuid::now_v7();
        let name = crate::disk::temp_name(id);
        let created = crate::disk::temp_created_secs(&name).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert!(now.abs_diff(created) < 5);
        for foreign in [".tmp-", ".tmp-x", "blob", ".tmp-018f4d50-0000-4000-8000-000000000000"] {
            assert_eq!(crate::disk::temp_created_secs(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn parents_keep_their_slash() {
        assert_eq!(split_parent("/a/b/c"), ("/a/b/", "c"));
        assert_eq!(split_parent("/c"), ("/", "c"));
        assert_eq!(split_parent("a/b"), ("a/", "b"));
        assert_eq!(split_parent("c"), ("", "c"));
    }

    #[test]
    fn backoff_grows_and_stays_bounded() {
        for retried in 1..=12 {
            let wait = backoff(retried);
            assert!(wait <= Duration::from_millis(5_000), "{wait:?}");
            assert!(wait >= Duration::from_millis(100), "{wait:?}");
        }
    }

    #[test]
    fn listed_names_must_be_plain() {
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(plain_name(bad).is_err(), "{bad:?}");
        }
        assert!(plain_name("0123abcd").is_ok());
        assert!(plain_name(".tmp-018f4d50-0000-7000-8000-000000000000").is_ok());
    }
}
