//! Binary content kept out of live documents, through the agent's `/v1/blobs` ([`craft_cloud_api::local::blobs`]): an
//! app puts the bytes and keeps the [`Stored`] name in its document; any machine of the account gets the bytes back by
//! that name. Blocking; a get may wait for a download (up to [`GET_TIMEOUT`]), so call it off the UI thread.
//!
//! An agent from before blobs answers [`Error::Refused`] with [`codes::AGENT_TOO_OLD`] here (and only here: the rest
//! of the local API works with it as before).

use std::time::Duration;

use craft_cloud_api::limits::MAX_BLOB_BYTES;
use craft_cloud_api::local::blobs::hex;
use craft_cloud_api::local::{codes, paths};

pub use craft_cloud_api::local::blobs::Stored;

use crate::{Client, Error};

/// How long a put may take (the agent writes the bytes to its disk and answers; the upload follows by itself).
pub const PUT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a get may take (it may download the blob first).
pub const GET_TIMEOUT: Duration = Duration::from_secs(120);

/// The agent's blob store.
#[derive(Debug, Clone)]
pub struct Blobs {
    client: Client,
    /// The file whose space every call goes to ([`Blobs::for_path`]); none: the personal space.
    bound: Option<String>,
}

/// What an agent from before blobs answers.
fn not_found() -> Error {
    Error::Refused { code: craft_cloud_api::api::codes::NOT_FOUND.into(), message: "no such call".into(), name_taken: None }
}

/// `url` with the query `for=<path>`, if there is a path.
fn with_for(url: &str, path: Option<&str>) -> String {
    match path {
        Some(path) => format!("{url}?for={}", crate::query_component(path)),
        None => url.to_owned(),
    }
}

/// An older agent's "no such call" as [`codes::AGENT_TOO_OLD`].
fn too_old(e: Error) -> Error {
    match e {
        Error::Refused { code, .. } if code == craft_cloud_api::api::codes::NOT_FOUND => Error::Refused {
            code: codes::AGENT_TOO_OLD.into(),
            message: "the Craft Cloud agent on this computer is too old for binary content in live documents: update Craft Cloud".into(),
            name_taken: None,
        },
        other => other,
    }
}

impl Blobs {
    pub(crate) fn new(client: Client) -> Self {
        Self { client, bound: None }
    }

    /// This store, bound to the space of the file at `path` (absolute, inside a synced folder, as a live document of a
    /// team space is opened): [`Blobs::put`] and [`Blobs::get`] on the result go to that space, as [`Blobs::put_for`]
    /// and [`Blobs::get_for`] do for every call. A path in the personal folder, or a relative one, is the personal
    /// space's, as the agent routes it.
    pub fn for_path(&self, path: &str) -> Self {
        Self { client: self.client.clone(), bound: Some(path.to_owned()) }
    }

    /// The file this store is bound to ([`Blobs::for_path`]); none for a store of the personal space.
    pub fn bound_path(&self) -> Option<&str> {
        self.bound.as_deref()
    }

    /// Whether the agent knows `/v1/blobs`, asked once per client with a cheap `GET` of a name no blob has (an agent
    /// from before blobs answers its `not_found` for an unknown call before reading a body, so a big put to it would
    /// only end in a broken connection). A question that gets no answer is asked again next time.
    fn check_agent(&self) -> Result<(), Error> {
        if let Some(known) = self.client.blobs_known.get() {
            return if *known { Ok(()) } else { Err(too_old(not_found())) };
        }
        let path = format!("{}/{}", paths::BLOBS, hex(&[0; 32]));
        let known = match self.client.binary("GET", &path, None, MAX_BLOB_BYTES, crate::http::QUICK_TIMEOUT) {
            Err(Error::Refused { code, .. }) if code == craft_cloud_api::api::codes::NOT_FOUND => false,
            // `blob_not_found`, `not_signed_in`, `offline`: an agent that knows blobs.
            Ok(_) | Err(Error::Refused { .. }) => true,
            Err(e) => return Err(e),
        };
        let _ = self.client.blobs_known.set(known);
        if known { Ok(()) } else { Err(too_old(not_found())) }
    }

    /// Keeps `bytes` (1 byte to [`MAX_BLOB_BYTES`]) and names them. Answered once they are on the agent's disk. An
    /// agent too old for blobs is found out before a byte of them is sent ([`codes::AGENT_TOO_OLD`]).
    pub fn put(&self, bytes: &[u8]) -> Result<Stored, Error> {
        self.put_inner(bytes, self.bound.as_deref())
    }

    /// [`Blobs::put`] for a document of the file at `path` (absolute, inside a synced folder): the blob goes to that file's
    /// space, so every member of a team space gets it back ([`Blobs::get_for`]). An agent from before team spaces ignores
    /// the path and keeps the blob in the personal space.
    pub fn put_for(&self, bytes: &[u8], path: &str) -> Result<Stored, Error> {
        self.put_inner(bytes, Some(path))
    }

    fn put_inner(&self, bytes: &[u8], path: Option<&str>) -> Result<Stored, Error> {
        if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
            return Err(Error::Refused {
                code: "too_large".into(),
                message: format!("a blob is 1 byte to {MAX_BLOB_BYTES} bytes"),
                name_taken: None,
            });
        }
        self.check_agent()?;
        let url = with_for(paths::BLOBS, path);
        let answer = self.client.binary("PUT", &url, Some(bytes), craft_cloud_api::local::MAX_LOCAL_JSON_BYTES, PUT_TIMEOUT).map_err(too_old)?;
        let stored: Stored = serde_json::from_slice(&answer.body).map_err(|e| Error::Protocol(e.to_string()))?;
        if stored.len != bytes.len() as u64 {
            return Err(Error::Protocol("the agent named a blob of another length".into()));
        }
        Ok(stored)
    }

    /// The bytes named `hash`, of `len` bytes (checked).
    pub fn get(&self, hash: &[u8; 32], len: u64) -> Result<Vec<u8>, Error> {
        self.get_inner(hash, len, self.bound.as_deref())
    }

    /// [`Blobs::get`] from the space of the file at `path` ([`Blobs::put_for`]).
    pub fn get_for(&self, hash: &[u8; 32], len: u64, path: &str) -> Result<Vec<u8>, Error> {
        self.get_inner(hash, len, Some(path))
    }

    fn get_inner(&self, hash: &[u8; 32], len: u64, path: Option<&str>) -> Result<Vec<u8>, Error> {
        self.check_agent()?;
        let url = with_for(&format!("{}/{}", paths::BLOBS, hex(hash)), path);
        let answer = self.client.binary("GET", &url, None, MAX_BLOB_BYTES, GET_TIMEOUT).map_err(too_old)?;
        if answer.body.len() as u64 != len {
            return Err(Error::Protocol(format!("the agent's blob is {} bytes, not {len}", answer.body.len())));
        }
        Ok(answer.body)
    }
}
