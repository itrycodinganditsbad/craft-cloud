//! Sealing the TOTP secrets that the database keeps.
//!
//! - **The key** is 32 random bytes in a file the server's config names (`[two_factor] key_file`, see
//!   [`crate::two_factor`]). It is never used as it is: the AES-256 key is [`blake3::derive_key`] of it under a fixed
//!   context. A key shows in no log, no `Debug` output and no error message, and its bytes are wiped when it is dropped.
//! - **A sealed secret** is one byte naming the key (see below), a 12-byte random nonce and the AES-256-GCM ciphertext
//!   with its tag. The associated data is `craft-cloud:totp:v1:` and the 16 bytes of the account's ID, so a ciphertext
//!   copied to another account does not open.
//! - **The key's name** is the first byte of the BLAKE3 hash of the key file's bytes. It is a function of the key, so a
//!   name is never given to a second key (two keys that share one are refused when the server starts, and the
//!   authentication tag makes a wrong guess fail anyway). With a second, older key (`key_file_old`) configured, the
//!   name picks the key to open a secret with; sealing always uses the current key. The `two-factor rekey` command
//!   rewrites the secrets under the current key, after which the old key can go.
//! - **Errors say nothing about their input**: not the key, not the plaintext, not the ciphertext.

use std::io::Read;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use craft_cloud_meta::UserId;
use zeroize::Zeroizing;

/// The bytes a key file holds.
pub const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const KDF_CONTEXT: &str = "craft-cloud 2026 totp secret v1";
const AAD_PREFIX: &[u8] = b"craft-cloud:totp:v1:";

/// A secret could not be sealed or opened. Carries nothing of what was sealed or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a two-factor secret could not be sealed or opened")]
pub struct SecretBoxError;

/// A key file that cannot be used. The message names the file and what is wrong with it, never its content.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("the two-factor key file {path} does not exist")]
    Missing { path: PathBuf },
    #[error("can't read the two-factor key file {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("the two-factor key file {path} must hold exactly 32 bytes")]
    Length { path: PathBuf },
    #[error(
        "key_file and key_file_old are the same key, or two keys that share a name: make a new key for key_file (the first byte of the keys' BLAKE3 \
         hashes must differ)"
    )]
    SameName,
}

/// A key as read from a key file. `Debug` shows nothing of it.
pub struct TwoFactorKey {
    bytes: Zeroizing<[u8; KEY_BYTES]>,
    id: u8,
}

impl std::fmt::Debug for TwoFactorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TwoFactorKey(<redacted>)")
    }
}

impl TwoFactorKey {
    /// A key from its 32 bytes.
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        let id = blake3::hash(&bytes).as_bytes().first().copied().unwrap_or(0);
        Self { bytes: Zeroizing::new(bytes), id }
    }

    /// A new random key (for `init`).
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; KEY_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self::from_bytes(bytes))
    }

    /// The key file's bytes: only for writing a new key file.
    pub fn to_file_bytes(&self) -> Zeroizing<[u8; KEY_BYTES]> {
        self.bytes.clone()
    }

    /// Reads a key file: exactly [`KEY_BYTES`] bytes (a longer file is not read further).
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        let mut file = std::fs::File::open(path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => KeyError::Missing { path: path.to_owned() },
            _ => KeyError::Read { path: path.to_owned(), source },
        })?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(KEY_BYTES + 1));
        (&mut file).take(KEY_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|source| KeyError::Read { path: path.to_owned(), source })?;
        let bytes: [u8; KEY_BYTES] = bytes.as_slice().try_into().map_err(|_| KeyError::Length { path: path.to_owned() })?;
        Ok(Self::from_bytes(bytes))
    }

    /// The name sealed secrets carry for this key.
    pub fn id(&self) -> u8 {
        self.id
    }
}

struct KeyEntry {
    id: u8,
    cipher: Aes256Gcm,
}

impl KeyEntry {
    fn new(key: &TwoFactorKey) -> Result<Self, SecretBoxError> {
        let derived = Zeroizing::new(blake3::derive_key(KDF_CONTEXT, key.bytes.as_slice()));
        let cipher = Aes256Gcm::new_from_slice(derived.as_slice()).map_err(|_| SecretBoxError)?;
        Ok(Self { id: key.id, cipher })
    }
}

/// Seals and opens TOTP secrets with the current key and, for reading, an older one.
pub struct SecretBox {
    current: KeyEntry,
    old: Option<KeyEntry>,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(<redacted>)")
    }
}

fn aad(user: UserId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_PREFIX.len() + 16);
    aad.extend_from_slice(AAD_PREFIX);
    aad.extend_from_slice(user.0.as_bytes());
    aad
}

impl SecretBox {
    /// A box for `current` and, optionally, the `old` key secrets are still sealed with. Two keys with one name are
    /// refused.
    pub fn new(current: &TwoFactorKey, old: Option<&TwoFactorKey>) -> Result<Self, KeyError> {
        if old.is_some_and(|old| old.id == current.id) {
            return Err(KeyError::SameName);
        }
        let entry = |key: &TwoFactorKey| KeyEntry::new(key).map_err(|_| KeyError::SameName);
        Ok(Self { current: entry(current)?, old: old.map(entry).transpose()? })
    }

    /// The name of the current key.
    pub fn current_id(&self) -> u8 {
        self.current.id
    }

    /// The name of the older key, if one is configured.
    pub fn old_id(&self) -> Option<u8> {
        self.old.as_ref().map(|old| old.id)
    }

    /// The name of the key a sealed secret was sealed with (`None` for something too short to be one).
    pub fn key_id(sealed: &[u8]) -> Option<u8> {
        (sealed.len() >= 1 + NONCE_BYTES + TAG_BYTES).then(|| sealed.first().copied()).flatten()
    }

    /// Seals `plaintext` for `user` with the current key and a fresh random nonce.
    pub fn seal(&self, user: UserId, plaintext: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|_| SecretBoxError)?;
        let ciphertext =
            self.current.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &aad(user) }).map_err(|_| SecretBoxError)?;
        let mut sealed = Vec::with_capacity(1 + NONCE_BYTES + ciphertext.len());
        sealed.push(self.current.id);
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Opens a secret sealed for `user`. A secret of another account, a damaged or truncated one, one sealed with a key this
    /// box doesn't hold: all the same error.
    pub fn open(&self, user: UserId, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretBoxError> {
        let id = Self::key_id(sealed).ok_or(SecretBoxError)?;
        let entry = if id == self.current.id { &self.current } else { self.old.as_ref().filter(|old| old.id == id).ok_or(SecretBoxError)? };
        let (nonce, ciphertext) = sealed.get(1..).and_then(|rest| rest.split_at_checked(NONCE_BYTES)).ok_or(SecretBoxError)?;
        let plaintext = entry.cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad: &aad(user) }).map_err(|_| SecretBoxError)?;
        Ok(Zeroizing::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> UserId {
        UserId(uuid::Uuid::now_v7())
    }

    fn key(byte: u8) -> TwoFactorKey {
        TwoFactorKey::from_bytes([byte; KEY_BYTES])
    }

    #[test]
    fn a_sealed_secret_opens_again() {
        let sb = SecretBox::new(&key(1), None).unwrap();
        let ana = user();
        let sealed = sb.seal(ana, b"12345678901234567890").unwrap();
        assert_eq!(sb.open(ana, &sealed).unwrap().as_slice(), b"12345678901234567890");
        assert_eq!(sealed.first().copied(), Some(key(1).id()));
        assert_eq!(sealed.len(), 1 + NONCE_BYTES + 20 + TAG_BYTES);
        assert_ne!(sealed, sb.seal(ana, b"12345678901234567890").unwrap(), "a fresh nonce each time");
        assert!(!sealed.windows(20).any(|w| w == b"12345678901234567890"), "no plaintext in the ciphertext");
    }

    #[test]
    fn a_wrong_key_another_account_or_a_damaged_ciphertext_fails() {
        let sb = SecretBox::new(&key(1), None).unwrap();
        let (ana, bob) = (user(), user());
        let sealed = sb.seal(ana, b"secret").unwrap();
        assert_eq!(sb.open(bob, &sealed), Err(SecretBoxError), "the account is bound in");
        let other = SecretBox::new(&key(2), None).unwrap();
        assert_eq!(other.open(ana, &sealed), Err(SecretBoxError), "wrong key");
        // A key with the same name but other bytes fails on the tag.
        let mut bytes = [1u8; KEY_BYTES];
        let mut same_name = None;
        for i in 0..=255u8 {
            bytes[0] = i;
            if TwoFactorKey::from_bytes(bytes).id() == key(1).id() && bytes != [1u8; KEY_BYTES] {
                same_name = Some(TwoFactorKey::from_bytes(bytes));
                break;
            }
        }
        if let Some(same_name) = same_name {
            assert_eq!(SecretBox::new(&same_name, None).unwrap().open(ana, &sealed), Err(SecretBoxError));
        }
        for cut in [0, 1, 12, 13, sealed.len() - 1] {
            assert_eq!(sb.open(ana, &sealed[..cut]), Err(SecretBoxError), "cut at {cut}");
        }
        let mut flipped = sealed.clone();
        if let Some(last) = flipped.last_mut() {
            *last ^= 1;
        }
        assert_eq!(sb.open(ana, &flipped), Err(SecretBoxError));
    }

    #[test]
    fn the_older_key_opens_what_it_sealed_and_the_new_one_seals() {
        let ana = user();
        let old_only = SecretBox::new(&key(1), None).unwrap();
        let sealed_old = old_only.seal(ana, b"secret").unwrap();
        let rotated = SecretBox::new(&key(2), Some(&key(1))).unwrap();
        assert_eq!(rotated.open(ana, &sealed_old).unwrap().as_slice(), b"secret");
        assert_eq!(rotated.old_id(), Some(key(1).id()));
        let sealed_new = rotated.seal(ana, b"secret").unwrap();
        assert_eq!(SecretBox::key_id(&sealed_new), Some(key(2).id()));
        assert_eq!(old_only.open(ana, &sealed_new), Err(SecretBoxError), "the old key alone does not open the new");
        // Without the old key configured, what it sealed is a stranger's.
        assert_eq!(SecretBox::new(&key(2), None).unwrap().open(ana, &sealed_old), Err(SecretBoxError));
    }

    #[test]
    fn two_keys_with_one_name_are_refused() {
        assert!(matches!(SecretBox::new(&key(1), Some(&key(1))), Err(KeyError::SameName)));
    }

    #[test]
    fn a_key_is_read_from_a_file_of_exactly_32_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.key");
        std::fs::write(&good, [7u8; KEY_BYTES]).unwrap();
        assert_eq!(TwoFactorKey::from_file(&good).unwrap().id(), key(7).id());
        for (name, len) in [("short.key", 31), ("long.key", 33), ("empty.key", 0), ("huge.key", 100_000)] {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![7u8; len]).unwrap();
            assert!(matches!(TwoFactorKey::from_file(&path), Err(KeyError::Length { .. })), "{name}");
        }
        assert!(matches!(TwoFactorKey::from_file(&dir.path().join("nope.key")), Err(KeyError::Missing { .. })));
    }

    #[test]
    fn keys_and_boxes_show_nothing_in_debug_and_errors_say_nothing() {
        let k = key(9);
        assert_eq!(format!("{k:?}"), "TwoFactorKey(<redacted>)");
        assert_eq!(format!("{:?}", SecretBox::new(&k, None).unwrap()), "SecretBox(<redacted>)");
        assert_eq!(SecretBoxError.to_string(), "a two-factor secret could not be sealed or opened");
    }
}
