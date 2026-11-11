//! What makes a backup trustworthy when the server that holds it is not: the backup key, the manifest's MAC and the
//! database dump's encryption.
//!
//! A backup server sees everything it stores and can change any of it. Hashes inside the backup do not help, because the
//! server that rewrites a file rewrites the hash beside it. So the parts that decide what a restore trusts are bound to a
//! secret the backup server never sees:
//!
//! - **The backup key** is 32 random bytes, written as 64 hexadecimal digits and a line break in a file of its own
//!   (`[backup] backup_key_file`, by default `backup.key` beside `config.toml`), made on the first `backup` and never
//!   overwritten. It is not in the backup. Without it a backup cannot be restored, so it must be kept apart from the machine:
//!   the text can be copied into a password manager and written back to a file with any editor (white space around it is
//!   ignored; upper case digits are read too).
//! - **The manifest** carries a MAC: keyed BLAKE3 under a key derived from the backup key with a context of its own. It
//!   covers every field, the dump's hash and sizes, and the hash, count and total of the blob inventory, so a restore
//!   trusts none of them before the MAC has been checked.
//! - **The database dump** is sealed with AES-256-GCM in segments of 1 MiB. The segment key is derived from the backup
//!   key and a random 16-byte salt in the file's header; a segment's nonce is its number; its associated data names the
//!   generation, the schema version, the segment number and whether it is the last, so segments cannot be moved between
//!   backups, reordered, dropped from the end or taken from another generation.
//! - **The blob inventory** (`inventory.txt`: one `kind id size` line per blob, sorted) is bound by its hash in the MAC'd
//!   manifest. A restore reads its blobs from the inventory, not from a listing the backup server makes up.
//!
//! Blobs themselves are not encrypted: they are content-addressed and every read checks the hash against the ID, so a
//! changed blob is found, but the backup server can read them. Sealing blobs is a follow-up.

use std::io::{Read, Write};
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The bytes of a backup key (its file holds them as hexadecimal digits).
pub const KEY_BYTES: usize = 32;
/// The hexadecimal digits of a key.
const KEY_HEX_BYTES: usize = KEY_BYTES * 2;
/// The default name of the key file, beside `config.toml`.
pub const DEFAULT_KEY_FILE: &str = "backup.key";
const SEGMENT: usize = 1024 * 1024;
const TAG: usize = 16;
const SALT: usize = 16;
const MAGIC: &[u8; 8] = b"CCBKDB02";
/// Header bytes before the first segment.
pub const HEADER_BYTES: u64 = (MAGIC.len() + SALT) as u64;
const MAC_CONTEXT: &str = "craft-cloud 2026 backup manifest mac v1";
const DB_CONTEXT: &str = "craft-cloud 2026 backup database v1";
const AAD_PREFIX: &[u8] = b"craft-cloud:backup-db:v1\0";
/// The manifest's `schema_version` that carries a MAC.
pub const AUTHENTICATED_SCHEMA: u32 = 2;

/// The backup key. `Debug` shows nothing of it; its bytes are wiped when it is dropped.
#[derive(Clone)]
pub struct BackupKey(Zeroizing<[u8; KEY_BYTES]>);

impl std::fmt::Debug for BackupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackupKey(<redacted>)")
    }
}

impl BackupKey {
    /// Reads a key file: 64 hexadecimal digits, with white space around them ignored.
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut file = std::fs::File::open(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("the backup key file {} does not exist", path.display()),
            _ => format!("can't read the backup key file {}: {e}", path.display()),
        })?;
        // A key file is 65 bytes; read a little more than that to tell a file that is too long.
        let mut text = Zeroizing::new(Vec::with_capacity(KEY_HEX_BYTES * 2));
        (&mut file)
            .take(KEY_HEX_BYTES as u64 * 2)
            .read_to_end(&mut text)
            .map_err(|e| format!("can't read the backup key file {}: {e}", path.display()))?;
        let bad =
            || format!("the backup key file {} must hold the 64 hexadecimal digits of the key (and nothing else but white space)", path.display());
        let digits = text.trim_ascii();
        if digits.len() != KEY_HEX_BYTES {
            return Err(bad());
        }
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            let value = |digit: &u8| char::from(*digit).to_digit(16);
            let (Some(high), Some(low)) = (pair.first().and_then(value), pair.get(1).and_then(value)) else { return Err(bad()) };
            *byte = u8::try_from(high * 16 + low).map_err(|_| bad())?;
        }
        Ok(Self(bytes))
    }

    /// Reads the key file, or makes it when there is none: 32 random bytes, owner-only on Unix, never over a file that is
    /// there. The bytes are written to a file of their own and published with a hard link, which fails if the key file
    /// appeared in the meantime; two first backups that start together therefore end with the same key, and neither
    /// reads a half-written file. `true`: the key was made now.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool), String> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Self::load(path).map(|key| (key, false)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("can't look at the backup key file {}: {e}", path.display())),
        }
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        getrandom::fill(bytes.as_mut_slice()).map_err(|e| format!("the system random source failed: {e}"))?;
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let temp = parent.join(format!(".backup-key-{}.tmp", uuid::Uuid::now_v7().simple()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut text = Zeroizing::new(String::with_capacity(KEY_HEX_BYTES + 1));
        for byte in bytes.iter() {
            text.push_str(&format!("{byte:02x}"));
        }
        text.push('\n');
        let written = options.open(&temp).and_then(|mut file| file.write_all(text.as_bytes()).and_then(|()| file.sync_all()));
        let published = written.and_then(|()| std::fs::hard_link(&temp, path));
        let _ = std::fs::remove_file(&temp);
        match published {
            Ok(()) => Ok((Self(bytes), true)),
            // Someone else made it first: theirs is the key.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Self::load(path).map(|key| (key, false)),
            Err(e) => Err(format!("can't make the backup key file {}: {e}", path.display())),
        }
    }

    fn mac_key(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(blake3::derive_key(MAC_CONTEXT, self.0.as_slice()))
    }

    fn db_cipher(&self, salt: &[u8; SALT]) -> Result<Aes256Gcm, String> {
        let mut material = Zeroizing::new(Vec::with_capacity(KEY_BYTES + SALT));
        material.extend_from_slice(self.0.as_slice());
        material.extend_from_slice(salt);
        let derived = Zeroizing::new(blake3::derive_key(DB_CONTEXT, &material));
        Aes256Gcm::new_from_slice(derived.as_slice()).map_err(|_| "the backup key could not be used".to_owned())
    }
}

/// The size of a sealed dump of `plain` bytes.
pub fn sealed_len(plain: u64) -> u64 {
    let segments = plain.div_ceil(SEGMENT as u64).max(1);
    HEADER_BYTES.saturating_add(plain).saturating_add(segments.saturating_mul(TAG as u64))
}

fn aad(generation: &str, schema: u32, index: u64, last: bool) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_PREFIX.len() + generation.len() + 17);
    aad.extend_from_slice(AAD_PREFIX);
    aad.extend_from_slice(&u32::try_from(generation.len()).unwrap_or(u32::MAX).to_be_bytes());
    aad.extend_from_slice(generation.as_bytes());
    aad.extend_from_slice(&schema.to_be_bytes());
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(u8::from(last));
    aad
}

fn nonce(index: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    if let Some(tail) = nonce.get_mut(4..) {
        tail.copy_from_slice(&index.to_be_bytes());
    }
    nonce
}

/// Fills `buf` as far as the reader goes; returns how many bytes it got (less than `buf.len()` only at the end).
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let Some(rest) = buf.get_mut(filled..) else { break };
        match reader.read(rest) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Seals the file `src` into `dst` for `generation`. Returns the plain size.
pub fn seal_file(key: &BackupKey, generation: &str, schema: u32, src: &Path, dst: &Path) -> Result<u64, String> {
    let io = |e: std::io::Error| format!("sealing the database dump failed: {e}");
    let mut salt = [0u8; SALT];
    getrandom::fill(&mut salt).map_err(|e| format!("the system random source failed: {e}"))?;
    let cipher = key.db_cipher(&salt)?;
    let mut input = std::fs::File::open(src).map_err(io)?;
    let mut output = std::io::BufWriter::new(std::fs::File::create(dst).map_err(io)?);
    output.write_all(MAGIC).map_err(io)?;
    output.write_all(&salt).map_err(io)?;
    let mut current = vec![0u8; SEGMENT];
    let mut next = vec![0u8; SEGMENT];
    let mut have = read_full(&mut input, &mut current).map_err(io)?;
    let (mut index, mut total) = (0u64, 0u64);
    loop {
        let ahead = read_full(&mut input, &mut next).map_err(io)?;
        let last = ahead == 0;
        let plain = current.get(..have).unwrap_or_default();
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce(index)), Payload { msg: plain, aad: &aad(generation, schema, index, last) })
            .map_err(|_| "sealing the database dump failed".to_owned())?;
        output.write_all(&sealed).map_err(io)?;
        total = total.saturating_add(u64::try_from(have).unwrap_or(u64::MAX));
        if last {
            break;
        }
        std::mem::swap(&mut current, &mut next);
        have = ahead;
        index = index.saturating_add(1);
    }
    let file = output.into_inner().map_err(|e| format!("sealing the database dump failed: {}", e.error()))?;
    file.sync_all().map_err(io)?;
    Ok(total)
}

/// Why a sealed dump was not opened.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Wrong key, a changed file, another generation's file or a cut-off one: the checks cannot tell these apart.
    NotAuthentic,
    /// The plain text would be bigger than the limit.
    TooBig,
    Io(String),
}

/// Opens the sealed dump `src` into `dst`, stopping with [`OpenError::TooBig`] before it writes more than `max_plain`
/// bytes. Returns the plain size. The caller removes `dst` on an error.
pub fn open_file(key: &BackupKey, generation: &str, schema: u32, src: &Path, dst: &Path, max_plain: u64) -> Result<u64, OpenError> {
    let io = |e: std::io::Error| OpenError::Io(format!("opening the database dump failed: {e}"));
    let mut input = std::fs::File::open(src).map_err(io)?;
    let mut header = [0u8; MAGIC.len() + SALT];
    if read_full(&mut input, &mut header).map_err(io)? != header.len() || header.get(..MAGIC.len()) != Some(MAGIC.as_slice()) {
        return Err(OpenError::NotAuthentic);
    }
    let mut salt = [0u8; SALT];
    salt.copy_from_slice(header.get(MAGIC.len()..).ok_or(OpenError::NotAuthentic)?);
    let cipher = key.db_cipher(&salt).map_err(OpenError::Io)?;
    let mut output = std::io::BufWriter::new(std::fs::File::create(dst).map_err(io)?);
    let block = SEGMENT + TAG;
    let mut current = vec![0u8; block];
    let mut next = vec![0u8; block];
    let mut have = read_full(&mut input, &mut current).map_err(io)?;
    let (mut index, mut total) = (0u64, 0u64);
    loop {
        let ahead = read_full(&mut input, &mut next).map_err(io)?;
        let last = ahead == 0;
        // Only the last segment may be short.
        if have < TAG || (!last && have != block) {
            return Err(OpenError::NotAuthentic);
        }
        let plain_len = u64::try_from(have - TAG).unwrap_or(u64::MAX);
        if total.saturating_add(plain_len) > max_plain {
            return Err(OpenError::TooBig);
        }
        let sealed = current.get(..have).unwrap_or_default();
        let plain = cipher
            .decrypt(Nonce::from_slice(&nonce(index)), Payload { msg: sealed, aad: &aad(generation, schema, index, last) })
            .map_err(|_| OpenError::NotAuthentic)?;
        output.write_all(&plain).map_err(io)?;
        total = total.saturating_add(plain_len);
        if last {
            break;
        }
        std::mem::swap(&mut current, &mut next);
        have = ahead;
        index = index.saturating_add(1);
    }
    let file = output.into_inner().map_err(|e| OpenError::Io(format!("opening the database dump failed: {}", e.error())))?;
    file.sync_all().map_err(io)?;
    Ok(total)
}

/// The manifest of a generation with a MAC (`schema_version` 2). Every field is covered; a field the code does not know
/// is refused, so nothing can ride along unauthenticated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifest {
    pub schema_version: u32,
    pub generation: String,
    pub created: String,
    pub created_ms: i64,
    pub server_version: String,
    pub database: DatabaseEntry,
    pub inventory: InventoryEntry,
    pub blobs: BlobCounts,
    /// Keyed BLAKE3 over all of the above, as 64 hex digits.
    pub mac: String,
}

/// The sealed database dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseEntry {
    pub kind: String,
    pub file: String,
    /// Size and BLAKE3 of the sealed file as it lies on the backup server.
    pub bytes: u64,
    pub blake3: String,
    /// Size of the dump once opened.
    pub plain_bytes: u64,
}

/// The list of blobs this backup stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryEntry {
    pub file: String,
    pub bytes: u64,
    pub blake3: String,
    /// Lines in the file.
    pub count: u64,
    /// The sizes in the file added up (the objects' sizes on the backup server).
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobCounts {
    pub chunks: KindCounts,
    pub manifests: KindCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindCounts {
    pub found: u64,
    pub copied: u64,
    pub already_there: u64,
}

impl SignedManifest {
    fn mac_of(&self, key: &BackupKey) -> blake3::Hash {
        fn text(out: &mut Vec<u8>, value: &str) {
            out.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            out.extend_from_slice(value.as_bytes());
        }
        fn number(out: &mut Vec<u8>, value: u64) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        let mut input = b"craft-cloud backup manifest v2\0".to_vec();
        number(&mut input, u64::from(self.schema_version));
        text(&mut input, &self.generation);
        text(&mut input, &self.created);
        number(&mut input, u64::from_be_bytes(self.created_ms.to_be_bytes()));
        text(&mut input, &self.server_version);
        let database = &self.database;
        text(&mut input, &database.kind);
        text(&mut input, &database.file);
        number(&mut input, database.bytes);
        text(&mut input, &database.blake3);
        number(&mut input, database.plain_bytes);
        let inventory = &self.inventory;
        text(&mut input, &inventory.file);
        number(&mut input, inventory.bytes);
        text(&mut input, &inventory.blake3);
        number(&mut input, inventory.count);
        number(&mut input, inventory.total_bytes);
        for counts in [self.blobs.chunks, self.blobs.manifests] {
            number(&mut input, counts.found);
            number(&mut input, counts.copied);
            number(&mut input, counts.already_there);
        }
        blake3::keyed_hash(&key.mac_key(), &input)
    }

    /// Sets `mac` for the other fields.
    pub fn sign(mut self, key: &BackupKey) -> Self {
        self.mac = self.mac_of(key).to_hex().to_string();
        self
    }

    /// `true` when `mac` is the MAC of the other fields under `key`. The comparison takes the same time whatever differs.
    pub fn verify(&self, key: &BackupKey) -> bool {
        match blake3::Hash::from_hex(&self.mac) {
            Ok(given) => given == self.mac_of(key),
            Err(_) => false,
        }
    }
}

/// One line of the inventory: `m|c <64 hex digits> <size>`.
pub fn inventory_line(manifest: bool, id: &[u8; 32], size: u64) -> String {
    format!("{} {} {size}\n", if manifest { 'm' } else { 'c' }, blake3::Hash::from_bytes(*id).to_hex())
}

/// What a line of the inventory says.
pub fn parse_inventory_line(line: &str) -> Option<(bool, [u8; 32], u64)> {
    let mut parts = line.split(' ');
    let (kind, id, size) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || id.len() != 64 {
        return None;
    }
    let manifest = match kind {
        "m" => true,
        "c" => false,
        _ => return None,
    };
    // `from_hex` takes upper case too; the writer never makes it, so a line that has it is not ours.
    if id.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let id = *blake3::Hash::from_hex(id).ok()?.as_bytes();
    if size.is_empty() || (size.starts_with('0') && size.len() > 1) || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((manifest, id, size.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> BackupKey {
        BackupKey(Zeroizing::new([byte; KEY_BYTES]))
    }

    fn sample() -> SignedManifest {
        SignedManifest {
            schema_version: AUTHENTICATED_SCHEMA,
            generation: "20231114T221320Z-00000000000000000000000000000000".into(),
            created: "2023-11-14T22:13:20Z".into(),
            created_ms: 1_700_000_000_000,
            server_version: "0.0.0".into(),
            database: DatabaseEntry { kind: "sqlite".into(), file: "g/meta.sqlite.enc".into(), bytes: 100, blake3: "ab".repeat(32), plain_bytes: 60 },
            inventory: InventoryEntry { file: "g/inventory.txt".into(), bytes: 9, blake3: "cd".repeat(32), count: 1, total_bytes: 5 },
            blobs: BlobCounts {
                chunks: KindCounts { found: 1, copied: 1, already_there: 0 },
                manifests: KindCounts { found: 0, copied: 0, already_there: 0 },
            },
            mac: String::new(),
        }
    }

    #[test]
    fn a_manifest_verifies_under_its_key_only_and_every_field_counts() {
        let signed = sample().sign(&key(1));
        assert!(signed.verify(&key(1)));
        assert!(!signed.verify(&key(2)));
        let changes: [fn(&mut SignedManifest); 12] = [
            |m| m.schema_version += 1,
            |m| m.generation.push('x'),
            |m| m.created.push('x'),
            |m| m.created_ms += 1,
            |m| m.server_version.push('x'),
            |m| m.database.bytes += 1,
            |m| m.database.blake3 = "00".repeat(32),
            |m| m.database.plain_bytes += 1,
            |m| m.inventory.blake3 = "00".repeat(32),
            |m| m.inventory.count += 1,
            |m| m.inventory.total_bytes += 1,
            |m| m.blobs.chunks.copied += 1,
        ];
        for change in changes {
            let mut forged = signed.clone();
            change(&mut forged);
            assert!(!forged.verify(&key(1)));
        }
        let mut no_mac = signed;
        no_mac.mac = "not hex".into();
        assert!(!no_mac.verify(&key(1)));
    }

    #[test]
    fn a_manifest_with_a_field_of_its_own_is_refused() {
        let text = serde_json::to_string(&sample().sign(&key(1))).unwrap();
        assert!(serde_json::from_str::<SignedManifest>(&text).is_ok());
        let extra = text.replacen('{', "{\"extra\":1,", 1);
        assert!(serde_json::from_str::<SignedManifest>(&extra).is_err());
    }

    #[test]
    fn a_dump_opens_with_its_key_for_its_generation_only() {
        let dir = tempfile::tempdir().unwrap();
        let (plain, sealed, back) = (dir.path().join("plain"), dir.path().join("sealed"), dir.path().join("back"));
        // Empty, one byte, a whole segment, and a bit more than one.
        for len in [0usize, 1, SEGMENT, SEGMENT + 7] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            std::fs::write(&plain, &data).unwrap();
            assert_eq!(seal_file(&key(1), "g1", 2, &plain, &sealed).unwrap(), len as u64);
            assert_eq!(std::fs::metadata(&sealed).unwrap().len(), sealed_len(len as u64), "{len}");
            assert_eq!(open_file(&key(1), "g1", 2, &sealed, &back, u64::MAX), Ok(len as u64));
            assert_eq!(std::fs::read(&back).unwrap(), data);
            assert_eq!(open_file(&key(2), "g1", 2, &sealed, &back, u64::MAX), Err(OpenError::NotAuthentic), "wrong key");
            assert_eq!(open_file(&key(1), "g2", 2, &sealed, &back, u64::MAX), Err(OpenError::NotAuthentic), "other generation");
            assert_eq!(open_file(&key(1), "g1", 3, &sealed, &back, u64::MAX), Err(OpenError::NotAuthentic), "other schema");
        }
    }

    #[test]
    fn a_changed_cut_or_extended_dump_is_refused_and_the_limit_stops_the_writing() {
        let dir = tempfile::tempdir().unwrap();
        let (plain, sealed, back) = (dir.path().join("plain"), dir.path().join("sealed"), dir.path().join("back"));
        std::fs::write(&plain, vec![9u8; SEGMENT * 2 + 5]).unwrap();
        seal_file(&key(1), "g", 2, &plain, &sealed).unwrap();
        let good = std::fs::read(&sealed).unwrap();
        let open = |bytes: &[u8]| {
            std::fs::write(&sealed, bytes).unwrap();
            open_file(&key(1), "g", 2, &sealed, &back, u64::MAX)
        };
        assert!(open(&good).is_ok());
        for at in [0, MAGIC.len() + 1, HEADER_BYTES as usize + 3, good.len() - 1] {
            let mut changed = good.clone();
            changed[at] ^= 1;
            assert_eq!(open(&changed), Err(OpenError::NotAuthentic), "byte {at}");
        }
        // Dropping the last segment, or the last bytes, or adding any, is found whatever the length says.
        assert_eq!(open(&good[..good.len() - 1]), Err(OpenError::NotAuthentic));
        assert_eq!(open(&good[..good.len() - (5 + TAG)]), Err(OpenError::NotAuthentic));
        assert_eq!(open(&good[..HEADER_BYTES as usize]), Err(OpenError::NotAuthentic));
        assert_eq!(open(&good[..4]), Err(OpenError::NotAuthentic));
        let mut longer = good.clone();
        longer.push(0);
        assert_eq!(open(&longer), Err(OpenError::NotAuthentic));
        // Two segments swapped.
        let (first, rest) = good[HEADER_BYTES as usize..].split_at(SEGMENT + TAG);
        let (second, last) = rest.split_at(SEGMENT + TAG);
        let swapped = [&good[..HEADER_BYTES as usize], second, first, last].concat();
        assert_eq!(open(&swapped), Err(OpenError::NotAuthentic));
        // The limit is checked before a segment is written.
        std::fs::write(&sealed, &good).unwrap();
        assert_eq!(open_file(&key(1), "g", 2, &sealed, &back, SEGMENT as u64), Err(OpenError::TooBig));
        assert!(std::fs::metadata(&back).map_or(0, |m| m.len()) <= SEGMENT as u64);
    }

    #[test]
    fn the_key_file_is_made_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.key");
        let (first, made) = BackupKey::load_or_create(&path).unwrap();
        assert!(made);
        let bytes = std::fs::read(&path).unwrap();
        // 64 hexadecimal digits and a line break: text that can be copied.
        assert_eq!(bytes.len(), KEY_HEX_BYTES + 1);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.ends_with('\n') && text.trim_end().bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{text:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let (again, made) = BackupKey::load_or_create(&path).unwrap();
        assert!(!made);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(first.mac_key().as_slice(), again.mac_key().as_slice());
        // No temporary file is left, and a key file of the wrong size is an error, not a new key.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        std::fs::write(&path, b"short").unwrap();
        assert!(BackupKey::load_or_create(&path).unwrap_err().contains("64 hexadecimal digits"));
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
        // The text can be kept anywhere and written back with any editor: white space and capitals do not matter.
        let same = format!("  {}\r\n\r\n", text.trim_end().to_uppercase());
        std::fs::write(&path, same).unwrap();
        assert_eq!(BackupKey::load(&path).unwrap().mac_key().as_slice(), first.mac_key().as_slice());
        // Anything else is refused: the raw bytes, a digit too few or many, a letter that is no digit, text around.
        let digits = text.trim_end().to_owned();
        for bad in [
            String::from_utf8_lossy(&[7u8; 32]).into_owned(),
            digits[..63].to_owned(),
            format!("{digits}0"),
            format!("g{}", &digits[1..]),
            format!("{digits} {digits}"),
            format!("key: {digits}"),
            String::new(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            let refused = BackupKey::load(&path).unwrap_err();
            assert!(refused.contains("64 hexadecimal digits"), "{bad:?}: {refused}");
        }
        std::fs::write(&path, vec![b'a'; 100_000]).unwrap();
        assert!(BackupKey::load(&path).is_err());
        std::fs::write(&path, &text).unwrap();
        assert!(BackupKey::load(&dir.path().join("none.key")).unwrap_err().contains("does not exist"));
        assert_eq!(format!("{first:?}"), "BackupKey(<redacted>)");
    }

    #[test]
    fn inventory_lines_round_trip_and_odd_ones_are_refused() {
        let id = [0xabu8; 32];
        let line = inventory_line(true, &id, 42);
        assert_eq!(parse_inventory_line(line.trim_end()), Some((true, id, 42)));
        let line = inventory_line(false, &id, 0);
        assert_eq!(parse_inventory_line(line.trim_end()), Some((false, id, 0)));
        let hex = "ab".repeat(32);
        for bad in [
            String::new(),
            format!("x {hex} 1"),
            format!("c {hex}"),
            format!("c {hex} 1 2"),
            format!("c {} 1", &hex[..62]),
            format!("c {} 1", hex.to_uppercase()),
            format!("c {hex} 01"),
            format!("c {hex} -1"),
            format!("c {hex} 99999999999999999999999"),
            format!("c  {hex} 1"),
        ] {
            assert_eq!(parse_inventory_line(&bad), None, "{bad:?}");
        }
    }
}
