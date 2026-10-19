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
}

/// What an agent from before blobs answers.
fn not_found() -> Error {
    Error::Refused { code: craft_cloud_api::api::codes::NOT_FOUND.into(), message: "no such call".into() }
}

/// An older agent's "no such call" as [`codes::AGENT_TOO_OLD`].
fn too_old(e: Error) -> Error {
    match e {
        Error::Refused { code, .. } if code == craft_cloud_api::api::codes::NOT_FOUND => Error::Refused {
            code: codes::AGENT_TOO_OLD.into(),
            message: "the Craft Cloud agent on this computer is too old for binary content in live documents: update Craft Cloud".into(),
        },
        other => other,
    }
}

impl Blobs {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
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
        if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
            return Err(Error::Refused { code: "too_large".into(), message: format!("a blob is 1 byte to {MAX_BLOB_BYTES} bytes") });
        }
        self.check_agent()?;
        let answer =
            self.client.binary("PUT", paths::BLOBS, Some(bytes), craft_cloud_api::local::MAX_LOCAL_JSON_BYTES, PUT_TIMEOUT).map_err(too_old)?;
        let stored: Stored = serde_json::from_slice(&answer.body).map_err(|e| Error::Protocol(e.to_string()))?;
        if stored.len != bytes.len() as u64 {
            return Err(Error::Protocol("the agent named a blob of another length".into()));
        }
        Ok(stored)
    }

    /// The bytes named `hash`, of `len` bytes (checked).
    pub fn get(&self, hash: &[u8; 32], len: u64) -> Result<Vec<u8>, Error> {
        self.check_agent()?;
        let path = format!("{}/{}", paths::BLOBS, hex(hash));
        let answer = self.client.binary("GET", &path, None, MAX_BLOB_BYTES, GET_TIMEOUT).map_err(too_old)?;
        if answer.body.len() as u64 != len {
            return Err(Error::Protocol(format!("the agent's blob is {} bytes, not {len}", answer.body.len())));
        }
        Ok(answer.body)
    }
}
