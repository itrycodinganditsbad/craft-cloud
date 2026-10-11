//! Converting IDs to and from the byte columns they are stored in.

use uuid::Uuid;

use crate::MetaError;

pub(crate) fn uuid_bytes(u: &Uuid) -> Vec<u8> {
    u.as_bytes().to_vec()
}

pub(crate) fn uuid_from(bytes: &[u8]) -> Result<Uuid, MetaError> {
    Uuid::from_slice(bytes).map_err(|_| MetaError::Corrupt("a stored ID is not 16 bytes"))
}

pub(crate) fn hash_from(bytes: &[u8]) -> Result<[u8; 32], MetaError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| MetaError::Corrupt("a stored hash is not 32 bytes"))
}

pub(crate) fn to_i64(n: u64) -> Result<i64, MetaError> {
    i64::try_from(n).map_err(|_| MetaError::Invalid("number too large"))
}

pub(crate) fn to_u64(n: i64) -> Result<u64, MetaError> {
    u64::try_from(n).map_err(|_| MetaError::Corrupt("a stored count is negative"))
}

/// Flags are stored as 0/1 integers on every backend (the `Any` driver can't decode SQLite booleans).
pub(crate) fn flag(row: &sqlx::any::AnyRow, column: &str) -> Result<bool, MetaError> {
    use sqlx::Row;
    match row.try_get::<i64, _>(column)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(MetaError::Corrupt("a stored flag is not 0 or 1")),
    }
}
