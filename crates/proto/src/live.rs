//! The live protocol: `GET /api/v1/live`, upgraded to a WebSocket, between an agent and the server.
//!
//! Every frame is one binary WebSocket message holding one [`ClientFrame`] or [`ServerFrame`], encoded with
//! `postcard` ([`ClientFrame::encode`], [`ServerFrame::decode`] and their twins). The connection is signed in with the
//! device token like every other request, before the upgrade.
//!
//! - **`hello`** opens it: the agent sends the protocol versions it speaks, the server answers `welcome` with the one
//!   they share, or `error {protocol_mismatch}` and closes.
//! - **`create {doc, kind}`** makes a document if it isn't there yet (answered `created`). Today the only kind is an
//!   app's recents document, whose ID is derived from the space and the app ([`recents_doc`]), so two machines
//!   creating it offline make the same one.
//! - **`subscribe {doc, after}`**: the server sends every update of the document with a sequence number above `after`
//!   (the highest one the agent holds every update up to), then `subscribed {doc, head}`, then each new update as it
//!   is appended. The server never parses an update: catching up by sequence number needs nothing but its log, where
//!   catching up by Loro version would make it read every update.
//! - **`update {doc, client_seq, bytes}`** appends an update; once it is durable the server answers
//!   `ack {doc, client_seq, seq}` and sends it to every other subscriber as `update {doc, seq, bytes, device}`. An
//!   update sent again with the same `client_seq` and the same bytes (its ack was lost) is acked with its first `seq`.
//! - **`unsubscribe {doc}`** stops the updates.
//! - **`error {code, message, doc, client_seq}`** answers a frame the server refused, with a code of
//!   [`crate::api::codes`]; `doc` and `client_seq` name what it refers to. A document of another account is
//!   `not_found`, exactly like one that doesn't exist.
//!
//! From protocol [`LIVE_DOCUMENTS_PROTOCOL`] on (an agent and a server that both speak it; an older peer never sees
//! these frames):
//!
//! - **Snapshots never travel in frames**: a snapshot (a Loro snapshot of a whole document, up to
//!   [`MAX_SNAPSHOT_BYTES`]) goes up and down as a file's content does, through the chunk API (cut into chunks named by
//!   a manifest, with that API's body limits, hash checks and the account's storage cap), and frames name it by its
//!   manifest. The agent uploads it before the frame that names it; the server checks that the manifest is the
//!   account's before recording anything.
//! - **Documents linked to files**: `create {doc, node, app, snapshot}` ([`ClientFrame::CreateDocument`], on the wire
//!   a `create` of [`DocKind::Document`] with the snapshot's manifest appended) makes the document of a file node by
//!   import; its ID is [`node_doc`]. The server keeps the first snapshot it sees and answers `created {doc, kept}`
//!   ([`ServerFrame::CreatedDocument`], a `created` with `kept` appended): an agent whose import wasn't kept drops it
//!   and catches up from the start. A recents `create` and its `created` are byte for byte what they were.
//! - **Snapshots and compaction**: once a document's log above its snapshot passes [`COMPACT_UPDATES`] updates or
//!   [`COMPACT_BYTES`], the server asks one device that holds every update up to `upto` for `compact {doc, upto}`; it
//!   answers `snapshot {doc, upto, manifest}`, naming a Loro snapshot of exactly the updates 1..=`upto`. A `subscribe`
//!   from below the snapshot (or from 0) gets `snapshot {doc, base, manifest}` first (the agent downloads it), then the
//!   log from `base` on (the update at `base` too, so the agent learns its hash for the next `at`). `subscribe_log` is
//!   a `subscribe` that never gets a snapshot, for an agent that couldn't read one: the log's rows below the snapshot
//!   are kept.
//! - **Presence**: `presence {doc, bytes}` both ways, at most [`MAX_PRESENCE_BYTES`] and [`PRESENCE_PER_SEC`] a second
//!   per document per device, relayed to the document's other subscribers and never stored.
//!
//! Limits ([`MAX_UPDATE_BYTES`], [`MAX_SUBSCRIPTIONS`], [`MAX_FRAME_BYTES`], [`MAX_SNAPSHOT_BYTES`]) are checked when a
//! frame is decoded and again by the server; over them the answer is `too_large` or `rate_limited`, like the HTTP API.
//!
//! **Why postcard**: frames carry opaque binary updates, so a binary encoding avoids base64 (a third more bytes per
//! keystroke batch, once documents are edited live). postcard (MIT OR Apache-2.0, with `cobs`, MIT OR Apache-2.0) is compact, has no schema
//! compiler, decodes untrusted input without allocating more than the input holds, and is serde, like the rest of
//! this crate. It isn't self-describing, so a later protocol version that adds a frame or a field is negotiated at
//! `hello` (as the binary manifest format has its own version) instead of being skipped by older peers; frame
//! variants are only ever appended, never reordered. CBOR (`ciborium`, Apache-2.0) was the other candidate: it can
//! skip unknown fields, at the cost of bigger frames and three crates instead of two.

use serde::{Deserialize, Serialize};

use crate::ManifestId;
use crate::ids::{DeviceId, DocId, Epoch, NodeId, SpaceId};

/// Live protocol versions this build speaks. An agent and a server must share at least one.
pub const LIVE_PROTOCOL_MIN: u32 = 1;
pub const LIVE_PROTOCOL_MAX: u32 = 3;
/// The protocol that adds documents linked to files, snapshots, compaction and presence. A connection on an older one
/// carries recents only.
pub const LIVE_DOCUMENTS_PROTOCOL: u32 = 2;
/// The protocol that adds changes in fragments and bundles (`crate::fragments`): the server stages an agent's
/// fragments and stores a set whole or not at all (acking each fragment's `client_seq` then), and never sends a
/// fragment or a bundle to a connection on an older protocol (it refuses that connection the document instead: its
/// agent can't read them). An agent sends fragments only on it.
pub const LIVE_FRAGMENTS_PROTOCOL: u32 = 3;

/// Largest update, in bytes; largest snapshot of a document (an import, or a compaction), in bytes (it travels through
/// the chunk API, never in a frame); largest presence message (Loro's ephemeral state: a cursor, a selection, a machine
/// name), in bytes. Apps see them too, so they live in `craft-cloud-api`.
pub use craft_cloud_api::limits::{MAX_PRESENCE_BYTES, MAX_SNAPSHOT_BYTES, MAX_UPDATE_BYTES};
/// Most documents one device may be subscribed to at once.
pub const MAX_SUBSCRIPTIONS: usize = 256;
/// Largest encoded frame: an update and its envelope (IDs, numbers, an error message).
pub const MAX_FRAME_BYTES: usize = MAX_UPDATE_BYTES + 4096;
/// Most presence messages one device may send a second for one document (more are dropped, never stored anyway).
pub const PRESENCE_PER_SEC: u32 = 20;
/// A document's log above its snapshot is compacted once it holds more than this many updates…
pub const COMPACT_UPDATES: u64 = 1000;
/// …or more than this many bytes.
pub const COMPACT_BYTES: u64 = 1024 * 1024;
/// Longest error message in a frame, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 1024;
/// Longest error code in a frame, in bytes.
pub const MAX_CODE_BYTES: usize = 64;

/// Error code: the agent and the server share no live protocol version (the connection is closed).
pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";

/// An app's machine name, as in `recents:<app>`: 1 to 32 bytes of lowercase ASCII letters, digits and `-`, starting
/// with a letter (`wordcraft`, `photocraft`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AppName(String);

/// Why an app name was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("app names are 1-32 lowercase letters, digits or '-', starting with a letter")]
pub struct AppNameError;

impl AppName {
    pub fn new(name: &str) -> Result<Self, AppNameError> {
        let ok = (1..=32).contains(&name.len())
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok { Ok(Self(name.to_owned())) } else { Err(AppNameError) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AppName {
    type Error = AppNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(&name)
    }
}

impl From<AppName> for String {
    fn from(name: AppName) -> Self {
        name.0
    }
}

impl std::fmt::Display for AppName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a live document is. Only appended to, never reordered (the variant's index is on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocKind {
    /// An app's recent files: one per app per account, linked to no node. Its ID is [`recents_doc`].
    Recents { app: AppName },
    /// A file of the synced folder opened live: linked to its node (a rename or move keeps it), made by the app that
    /// first opened it. Its ID is [`node_doc`]. Only from protocol [`LIVE_DOCUMENTS_PROTOCOL`] on, and only created with
    /// its snapshot ([`ClientFrame::CreateDocument`]).
    Document { node: NodeId, app: AppName },
}

/// What a stored document kind starts with: `document:<app>` (its node is stored beside it).
pub const DOCUMENT_KIND_PREFIX: &str = "document:";

impl DocKind {
    /// The kind as stored on the server: `recents:<app>`, `document:<app>` (a document's node is stored beside it).
    pub fn as_text(&self) -> String {
        match self {
            DocKind::Recents { app } => format!("recents:{app}"),
            DocKind::Document { app, .. } => format!("{DOCUMENT_KIND_PREFIX}{app}"),
        }
    }

    /// A recents kind from its stored form, `None` for one this build doesn't know (a document needs its node too:
    /// [`DocKind::parse_stored`]).
    pub fn parse(text: &str) -> Option<Self> {
        let app = text.strip_prefix("recents:")?;
        AppName::new(app).ok().map(|app| DocKind::Recents { app })
    }

    /// The kind from its stored form and the node stored beside it.
    pub fn parse_stored(text: &str, node: Option<NodeId>) -> Option<Self> {
        match (text.strip_prefix(DOCUMENT_KIND_PREFIX), node) {
            (Some(app), Some(node)) => AppName::new(app).ok().map(|app| DocKind::Document { node, app }),
            (Some(_), None) => None,
            (None, _) => Self::parse(text),
        }
    }

    /// The ID a document of this kind must have in `space`: every kind fixes it (recents from the app, documents from
    /// the node), so two machines that make the same one offline make one document.
    pub fn fixed_id(&self, space: SpaceId) -> Option<DocId> {
        match self {
            DocKind::Recents { app } => Some(recents_doc(space, app)),
            DocKind::Document { node, .. } => Some(node_doc(space, *node)),
        }
    }
}

/// The ID of the live document of the file node `node` in `space`: the first 16 bytes of a BLAKE3 hash of both (with
/// a domain of its own), as a UUID of version 8. Two machines that import the same file make the same document.
pub fn node_doc(space: SpaceId, node: NodeId) -> DocId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"craft-cloud live document v1\0");
    hasher.update(space.as_uuid().as_bytes());
    hasher.update(node.as_uuid().as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(hash.as_bytes().get(..16).unwrap_or(&[0u8; 16]));
    DocId::from_uuid(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// The ID of `app`'s recents document in `space`: the first 16 bytes of a BLAKE3 hash of both, as a UUID of version 8
/// (custom). Every machine of the account computes the same one, so a recents document made offline on two machines
/// is one document.
pub fn recents_doc(space: SpaceId, app: &AppName) -> DocId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"craft-cloud live recents v1\0");
    hasher.update(space.as_uuid().as_bytes());
    hasher.update(app.as_str().as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(hash.as_bytes().get(..16).unwrap_or(&[0u8; 16]));
    DocId::from_uuid(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// The hash a `subscribe` names the update at `after` by: BLAKE3 of its bytes.
pub fn update_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// A frame from an agent to the server. Variants are only appended (their index is on the wire).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientFrame {
    /// The first frame: the protocol versions the agent speaks.
    Hello {
        min: u32,
        max: u32,
    },
    /// Make `doc` of `kind` if the space doesn't have it yet.
    Create {
        doc: DocId,
        kind: DocKind,
    },
    /// Send every update above `after`, then `subscribed`, then new ones as they come. `at` is the
    /// [`update_hash`] of the update the agent has at `after`: if the server's differs (it was restored from a
    /// backup and its epoch not reset yet), it answers `resync_required` for the document.
    Subscribe {
        doc: DocId,
        after: u64,
        at: Option<[u8; 32]>,
    },
    Unsubscribe {
        doc: DocId,
    },
    /// Append an update (an opaque Loro update, at most [`MAX_UPDATE_BYTES`]). `client_seq` names it in the `ack`.
    Update {
        doc: DocId,
        client_seq: u64,
        bytes: Vec<u8>,
    },
    /// The answer to `compact {doc, upto}`: the manifest of a Loro snapshot of exactly the updates 1..=`upto` of the
    /// document (and the snapshot it was made from), at most [`MAX_SNAPSHOT_BYTES`], uploaded already. Never answered.
    Snapshot {
        doc: DocId,
        upto: u64,
        manifest: ManifestId,
    },
    /// Ephemeral state (a cursor, a selection) for the document's other subscribers, at most [`MAX_PRESENCE_BYTES`];
    /// relayed, never stored, never answered.
    Presence {
        doc: DocId,
        bytes: Vec<u8>,
    },
    /// `subscribe` that never gets a snapshot: the whole log above `after`, its compacted rows too (for an agent that
    /// couldn't read the document's snapshot).
    SubscribeLog {
        doc: DocId,
        after: u64,
        at: Option<[u8; 32]>,
    },
    // Frames that are only appended to on the wire go above this line. The one below is a `create` of a document with
    // its snapshot's manifest appended (see [`ClientFrame::encode`]): it has no index of its own, so it stays last.
    /// Make the document of the file node `node` by import, with the snapshot `snapshot` names (a Loro snapshot, at
    /// most [`MAX_SNAPSHOT_BYTES`], uploaded already) as its content if the space doesn't have it yet. Answered
    /// `created {doc, kept}`. On the wire: `create {doc, kind: document {node, app}}` with the manifest appended.
    #[serde(skip)]
    CreateDocument {
        doc: DocId,
        node: NodeId,
        app: AppName,
        snapshot: ManifestId,
    },
}

/// A frame from the server to an agent. Variants are only appended (their index is on the wire).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerFrame {
    /// `hello` accepted: the protocol version both speak, and the space's epoch. A new epoch means the server was
    /// restored from a backup: its logs may have gone back (and grown again since), so the agent catches up from the
    /// start and sends its whole documents up again.
    Welcome { protocol: u32, epoch: Epoch },
    /// `create` done (or the document was there already, of the same kind).
    Created { doc: DocId },
    /// An update of a subscribed document: from the catch-up, or appended by another device.
    Update { doc: DocId, seq: u64, bytes: Vec<u8>, device: DeviceId },
    /// The catch-up of `subscribe` is done; `head` is the document's last sequence number.
    Subscribed { doc: DocId, head: u64 },
    /// The update the agent named `client_seq` is durable as `seq`.
    Ack { doc: DocId, client_seq: u64, seq: u64 },
    /// A frame was refused: `code` is one of [`crate::api::codes`] or [`PROTOCOL_MISMATCH`].
    Error { code: String, message: String, doc: Option<DocId>, client_seq: Option<u64> },
    /// The document's log above its snapshot is long: answer `snapshot {doc, upto, bytes}` once this device holds
    /// every update up to `upto` (or don't; another device is asked later).
    Compact { doc: DocId, upto: u64 },
    /// The catch-up of a `subscribe` from below the document's snapshot: the manifest of the snapshot (to download),
    /// which holds the updates 1..=`base` (and, for a document made by import, its import). The log from `base` on
    /// follows.
    Snapshot { doc: DocId, base: u64, manifest: ManifestId },
    /// Another device's presence in a subscribed document, as it sent it.
    Presence { doc: DocId, bytes: Vec<u8> },
    // Frames that are only appended to on the wire go above this line; see [`ClientFrame::CreateDocument`].
    /// `create {doc, node, app, snapshot}` done: `kept` says whether this snapshot is the document's (else another
    /// machine's import was there first, and this one is to be dropped). On the wire: `created {doc}` with `kept`
    /// appended.
    #[serde(skip)]
    CreatedDocument { doc: DocId, kept: bool },
}

/// A frame that can't be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("the frame is {0} bytes; the limit is {MAX_FRAME_BYTES}")]
    TooLarge(usize),
    #[error("the frame doesn't decode: {0}")]
    Malformed(String),
    #[error("the frame breaks a limit: {0}")]
    Invalid(&'static str),
}

fn malformed(e: postcard::Error) -> FrameError {
    FrameError::Malformed(e.to_string())
}

/// Encodes `frame`, and `trailer` after it (a field appended to an older frame), within the frame's limit.
fn encode<T: Serialize, A: Serialize>(frame: &T, trailer: Option<&A>, limit: usize) -> Result<Vec<u8>, FrameError> {
    let mut bytes = postcard::to_stdvec(frame).map_err(malformed)?;
    if let Some(trailer) = trailer {
        bytes.extend(postcard::to_stdvec(trailer).map_err(malformed)?);
    }
    if bytes.len() > limit {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Decodes the start of a frame, returning what follows it (a trailer, or nothing).
fn decode_head<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<(T, &'a [u8]), FrameError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    postcard::take_from_bytes(bytes).map_err(malformed)
}

/// A trailer that must be all of `rest`.
fn decode_trailer<'a, T: Deserialize<'a>>(rest: &'a [u8]) -> Result<T, FrameError> {
    let (value, after) = postcard::take_from_bytes(rest).map_err(malformed)?;
    if !after.is_empty() {
        return Err(FrameError::Malformed(format!("{} bytes after the frame", after.len())));
    }
    Ok(value)
}

fn presence_ok(bytes: &[u8]) -> Result<(), FrameError> {
    if bytes.is_empty() || bytes.len() > MAX_PRESENCE_BYTES { Err(FrameError::Invalid("a presence message is 1 byte to 4 KiB")) } else { Ok(()) }
}

impl ClientFrame {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        self.check()?;
        match self {
            ClientFrame::CreateDocument { doc, node, app, snapshot } => {
                let head = ClientFrame::Create { doc: *doc, kind: DocKind::Document { node: *node, app: app.clone() } };
                encode(&head, Some(snapshot), MAX_FRAME_BYTES)
            }
            frame => encode::<_, ()>(frame, None, MAX_FRAME_BYTES),
        }
    }

    /// Decodes and checks a frame from the network (hostile: every length is checked).
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let (head, rest): (Self, &[u8]) = decode_head(bytes)?;
        let frame = match head {
            ClientFrame::Create { doc, kind: DocKind::Document { node, app } } => {
                ClientFrame::CreateDocument { doc, node, app, snapshot: decode_trailer(rest)? }
            }
            head => {
                if !rest.is_empty() {
                    return Err(FrameError::Malformed(format!("{} bytes after the frame", rest.len())));
                }
                head
            }
        };
        frame.check()?;
        Ok(frame)
    }

    fn check(&self) -> Result<(), FrameError> {
        match self {
            ClientFrame::Update { bytes, .. } if bytes.len() > MAX_UPDATE_BYTES => Err(FrameError::Invalid("an update is at most 1 MiB")),
            ClientFrame::Update { bytes, .. } if bytes.is_empty() => Err(FrameError::Invalid("an update can't be empty")),
            ClientFrame::Create { kind: DocKind::Document { .. }, .. } => Err(FrameError::Invalid("a document is created with its snapshot")),
            ClientFrame::Presence { bytes, .. } => presence_ok(bytes),
            _ => Ok(()),
        }
    }
}

impl ServerFrame {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        self.check()?;
        match self {
            ServerFrame::CreatedDocument { doc, kept } => encode(&ServerFrame::Created { doc: *doc }, Some(kept), MAX_FRAME_BYTES),
            frame => encode::<_, ()>(frame, None, MAX_FRAME_BYTES),
        }
    }

    /// Decodes and checks a frame from the network (hostile: every length is checked).
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let (head, rest): (Self, &[u8]) = decode_head(bytes)?;
        let frame = match head {
            ServerFrame::Created { doc } if !rest.is_empty() => ServerFrame::CreatedDocument { doc, kept: decode_trailer(rest)? },
            head => {
                if !rest.is_empty() {
                    return Err(FrameError::Malformed(format!("{} bytes after the frame", rest.len())));
                }
                head
            }
        };
        frame.check()?;
        Ok(frame)
    }

    fn check(&self) -> Result<(), FrameError> {
        match self {
            ServerFrame::Update { bytes, .. } if bytes.len() > MAX_UPDATE_BYTES || bytes.is_empty() => {
                Err(FrameError::Invalid("an update is 1 byte to 1 MiB"))
            }
            ServerFrame::Error { code, message, .. } if code.len() > MAX_CODE_BYTES || message.len() > MAX_MESSAGE_BYTES => {
                Err(FrameError::Invalid("an error's code or message is too long"))
            }
            ServerFrame::Presence { bytes, .. } => presence_ok(bytes),
            _ => Ok(()),
        }
    }

    /// An error frame, its message cut to [`MAX_MESSAGE_BYTES`] (at a character boundary).
    pub fn error(code: &str, message: impl Into<String>, doc: Option<DocId>, client_seq: Option<u64>) -> Self {
        let mut message: String = message.into();
        if message.len() > MAX_MESSAGE_BYTES {
            let mut end = MAX_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            message.truncate(end);
        }
        let code = code.get(..code.len().min(MAX_CODE_BYTES)).unwrap_or_default().to_owned();
        ServerFrame::Error { code, message, doc, client_seq }
    }
}

/// Frames are logged by kind and IDs only: never an update's bytes (a document's content).
impl std::fmt::Debug for ClientFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientFrame::Hello { min, max } => write!(f, "Hello({min}-{max})"),
            ClientFrame::Create { doc, kind } => write!(f, "Create({doc}, {})", kind.as_text()),
            ClientFrame::Subscribe { doc, after, at } => write!(f, "Subscribe({doc}, after {after}, at {})", if at.is_some() { "hash" } else { "-" }),
            ClientFrame::Unsubscribe { doc } => write!(f, "Unsubscribe({doc})"),
            ClientFrame::Update { doc, client_seq, bytes } => write!(f, "Update({doc}, client_seq {client_seq}, {} bytes)", bytes.len()),
            ClientFrame::Snapshot { doc, upto, manifest } => write!(f, "Snapshot({doc}, upto {upto}, manifest {manifest})"),
            ClientFrame::Presence { doc, bytes } => write!(f, "Presence({doc}, {} bytes)", bytes.len()),
            ClientFrame::SubscribeLog { doc, after, at } => {
                write!(f, "SubscribeLog({doc}, after {after}, at {})", if at.is_some() { "hash" } else { "-" })
            }
            ClientFrame::CreateDocument { doc, node, app, snapshot } => {
                write!(f, "CreateDocument({doc}, node {node}, {app}, manifest {snapshot})")
            }
        }
    }
}

impl std::fmt::Debug for ServerFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerFrame::Welcome { protocol, epoch } => write!(f, "Welcome({protocol}, epoch {epoch})"),
            ServerFrame::Created { doc } => write!(f, "Created({doc})"),
            ServerFrame::Update { doc, seq, bytes, device } => write!(f, "Update({doc}, seq {seq}, {} bytes, from {device})", bytes.len()),
            ServerFrame::Subscribed { doc, head } => write!(f, "Subscribed({doc}, head {head})"),
            ServerFrame::Ack { doc, client_seq, seq } => write!(f, "Ack({doc}, client_seq {client_seq}, seq {seq})"),
            ServerFrame::Error { code, message, doc, client_seq } => write!(f, "Error({code}: {message}; {doc:?}, {client_seq:?})"),
            ServerFrame::Compact { doc, upto } => write!(f, "Compact({doc}, upto {upto})"),
            ServerFrame::Snapshot { doc, base, manifest } => write!(f, "Snapshot({doc}, base {base}, manifest {manifest})"),
            ServerFrame::Presence { doc, bytes } => write!(f, "Presence({doc}, {} bytes)", bytes.len()),
            ServerFrame::CreatedDocument { doc, kept } => write!(f, "CreatedDocument({doc}, kept {kept})"),
        }
    }
}

/// Whether two live protocol ranges overlap, and on which version: the highest both speak.
pub fn shared_version(min: u32, max: u32) -> Option<u32> {
    let high = max.min(LIVE_PROTOCOL_MAX);
    (min <= max && high >= min.max(LIVE_PROTOCOL_MIN)).then_some(high)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppName {
        AppName::new(name).unwrap()
    }

    #[test]
    fn app_names_are_plain() {
        for good in ["wordcraft", "photo-craft", "a", "x1"] {
            assert!(AppName::new(good).is_ok(), "{good}");
        }
        for bad in ["", "WordCraft", "1craft", "-x", "word craft", "wörd", &"a".repeat(33), "a/b", "a:b"] {
            assert!(AppName::new(bad).is_err(), "{bad:?}");
        }
        assert!(serde_json::from_str::<AppName>("\"Bad\"").is_err(), "checked when read, too");
    }

    #[test]
    fn kinds_round_trip_as_text() {
        let kind = DocKind::Recents { app: app("wordcraft") };
        assert_eq!(kind.as_text(), "recents:wordcraft");
        assert_eq!(DocKind::parse("recents:wordcraft"), Some(kind));
        assert_eq!(DocKind::parse("recents:Bad"), None);
        assert_eq!(DocKind::parse("document:x"), None);
    }

    #[test]
    fn a_recents_document_has_one_id_per_space_and_app() {
        let (space, other) = (SpaceId::generate(), SpaceId::generate());
        let id = recents_doc(space, &app("wordcraft"));
        assert_eq!(id, recents_doc(space, &app("wordcraft")), "every machine finds the same one");
        assert_ne!(id, recents_doc(space, &app("gridcraft")));
        assert_ne!(id, recents_doc(other, &app("wordcraft")));
        assert_eq!(id.as_uuid().get_version_num(), 8);
        assert_eq!(DocKind::Recents { app: app("wordcraft") }.fixed_id(space), Some(id));
    }

    #[test]
    fn frames_round_trip() {
        let doc = DocId::generate();
        let device = DeviceId::generate();
        let client = [
            ClientFrame::Hello { min: 1, max: 1 },
            ClientFrame::Create { doc, kind: DocKind::Recents { app: app("wordcraft") } },
            ClientFrame::Subscribe { doc, after: 7, at: None },
            ClientFrame::Subscribe { doc, after: 7, at: Some(update_hash(b"u")) },
            ClientFrame::Unsubscribe { doc },
            ClientFrame::Update { doc, client_seq: 3, bytes: vec![1, 2, 3] },
        ];
        for frame in client {
            assert_eq!(ClientFrame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
        let server = [
            ServerFrame::Welcome { protocol: 1, epoch: Epoch::generate() },
            ServerFrame::Created { doc },
            ServerFrame::Update { doc, seq: 9, bytes: vec![4; 10], device },
            ServerFrame::Subscribed { doc, head: 9 },
            ServerFrame::Ack { doc, client_seq: 3, seq: 10 },
            ServerFrame::error("not_found", "no such document", Some(doc), None),
        ];
        for frame in server {
            assert_eq!(ServerFrame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
    }

    /// The wire format is pinned: a frame variant's index and field order are part of the protocol.
    #[test]
    fn the_encoding_is_stable() {
        assert_eq!(ClientFrame::Hello { min: 1, max: 1 }.encode().unwrap(), vec![0, 1, 1]);
        let doc = DocId::from_uuid(uuid::Uuid::from_bytes([7; 16]));
        let mut want = vec![4, 16];
        want.extend([7; 16]);
        want.extend([5, 2, 0xaa, 0xbb]);
        assert_eq!(ClientFrame::Update { doc, client_seq: 5, bytes: vec![0xaa, 0xbb] }.encode().unwrap(), want);
        let epoch = Epoch::from_uuid(uuid::Uuid::from_bytes([3; 16]));
        let mut want = vec![0, 1, 16];
        want.extend([3; 16]);
        assert_eq!(ServerFrame::Welcome { protocol: 1, epoch }.encode().unwrap(), want);
    }

    #[test]
    fn hostile_frames_are_refused() {
        let doc = DocId::generate();
        let big = ClientFrame::Update { doc, client_seq: 1, bytes: vec![0; MAX_UPDATE_BYTES + 1] };
        assert!(big.encode().is_err());
        // Encoded by hand (no checks) and decoded: refused.
        let raw = postcard::to_stdvec(&big).unwrap();
        assert!(matches!(ClientFrame::decode(&raw), Err(FrameError::Invalid(_))));
        assert!(matches!(ClientFrame::decode(&vec![0; MAX_FRAME_BYTES + 1]), Err(FrameError::TooLarge(_))));
        let empty = postcard::to_stdvec(&ClientFrame::Update { doc, client_seq: 1, bytes: vec![] }).unwrap();
        assert!(ClientFrame::decode(&empty).is_err());
        for junk in [&[][..], &[9][..], &[4, 200][..], &[0, 1, 1, 0][..], &[1, 16, 0][..]] {
            assert!(ClientFrame::decode(junk).is_err(), "{junk:?}");
        }
        // A length prefix far beyond the input allocates nothing and fails.
        assert!(ClientFrame::decode(&[4, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0x7f]).is_err());
        // A bad app name inside a frame.
        let mut create = vec![1, 16];
        create.extend([1; 16]);
        create.extend([0, 3]);
        create.extend(b"BAD");
        assert!(ClientFrame::decode(&create).is_err());
    }

    #[test]
    fn long_error_messages_are_cut() {
        let ServerFrame::Error { message, .. } = ServerFrame::error("invalid", "é".repeat(MAX_MESSAGE_BYTES), None, None) else { panic!() };
        assert!(message.len() <= MAX_MESSAGE_BYTES && message.chars().all(|c| c == 'é'));
    }

    #[test]
    fn debug_output_never_shows_update_bytes() {
        let doc = DocId::generate();
        let shown = format!("{:?}", ClientFrame::Update { doc, client_seq: 1, bytes: b"secret text".to_vec() });
        assert!(!shown.contains("115") && !shown.contains("secret"), "{shown}");
        assert!(shown.contains("11 bytes"), "{shown}");
    }

    #[test]
    fn versions_are_negotiated_to_the_highest_shared() {
        assert_eq!(shared_version(1, 1), Some(1));
        assert_eq!(shared_version(1, 5), Some(LIVE_PROTOCOL_MAX));
        assert_eq!(shared_version(2, 5), Some(LIVE_FRAGMENTS_PROTOCOL));
        assert_eq!(shared_version(1, 2), Some(LIVE_DOCUMENTS_PROTOCOL), "an agent from before fragments");
        assert_eq!(shared_version(LIVE_PROTOCOL_MAX + 1, LIVE_PROTOCOL_MAX + 4), None);
        assert_eq!(shared_version(0, 0), None);
        assert_eq!(shared_version(3, 1), None, "an empty range");
        // An agent from before documents (1-1) and a server that has them agree on 1, and so do a new agent and an old
        // server (its range is 1-1): recents only.
        assert_eq!(shared_version(1, 1), Some(1));
    }

    #[test]
    fn a_document_has_one_id_per_space_and_node() {
        let (space, other) = (SpaceId::generate(), SpaceId::generate());
        let (node, node2) = (NodeId::generate(), NodeId::generate());
        let id = node_doc(space, node);
        assert_eq!(id, node_doc(space, node), "two machines importing the same file make one document");
        assert_ne!(id, node_doc(space, node2));
        assert_ne!(id, node_doc(other, node));
        assert_eq!(id.as_uuid().get_version_num(), 8);
        let kind = DocKind::Document { node, app: app("wordcraft") };
        assert_eq!(kind.fixed_id(space), Some(id));
        assert_eq!(kind.as_text(), "document:wordcraft");
        assert_eq!(DocKind::parse_stored("document:wordcraft", Some(node)), Some(kind));
        assert_eq!(DocKind::parse_stored("document:wordcraft", None), None, "a document's node is stored beside it");
        assert_eq!(DocKind::parse_stored("recents:wordcraft", None), Some(DocKind::Recents { app: app("wordcraft") }));
    }

    fn doc7() -> DocId {
        DocId::from_uuid(uuid::Uuid::from_bytes([7; 16]))
    }

    /// The frames of documents round-trip, and a document's `create` and `created` are an older `create` and
    /// `created` with a field appended.
    #[test]
    fn document_frames_round_trip() {
        let (doc, node) = (DocId::generate(), NodeId::generate());
        let manifest = ManifestId::from_bytes([0xab; 32]);
        let client = [
            ClientFrame::CreateDocument { doc, node, app: app("wordcraft"), snapshot: manifest },
            ClientFrame::Snapshot { doc, upto: 1001, manifest },
            ClientFrame::Presence { doc, bytes: vec![2; 40] },
            ClientFrame::SubscribeLog { doc, after: 3, at: Some(update_hash(b"x")) },
        ];
        for frame in client {
            assert_eq!(ClientFrame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
        let server = [
            ServerFrame::CreatedDocument { doc, kept: true },
            ServerFrame::CreatedDocument { doc, kept: false },
            ServerFrame::Compact { doc, upto: 1001 },
            ServerFrame::Snapshot { doc, base: 1001, manifest },
            ServerFrame::Presence { doc, bytes: vec![4; MAX_PRESENCE_BYTES] },
        ];
        for frame in server {
            assert_eq!(ServerFrame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
        // `created {doc, kept}` is `created {doc}` and one byte.
        let kept = ServerFrame::CreatedDocument { doc: doc7(), kept: true }.encode().unwrap();
        let mut want = ServerFrame::Created { doc: doc7() }.encode().unwrap();
        want.push(1);
        assert_eq!(kept, want);
        // A document's `create` is a `create` of kind 1 (`document {node, app}`), the snapshot's manifest (in hex) after
        // it.
        let node = NodeId::from_uuid(uuid::Uuid::from_bytes([8; 16]));
        let made = ClientFrame::CreateDocument { doc: doc7(), node, app: app("ab"), snapshot: manifest }.encode().unwrap();
        let mut want = vec![1, 16];
        want.extend([7; 16]);
        want.extend([1, 16]);
        want.extend([8; 16]);
        want.extend([2, b'a', b'b', 64]);
        want.extend("ab".repeat(32).bytes());
        assert_eq!(made, want);
    }

    /// What an agent and a server from before documents send (their bytes written out here) still decodes, to the same
    /// frames: nothing they know changed on the wire.
    #[test]
    fn frames_in_the_older_encoding_still_decode() {
        let doc = doc7();
        let id = |bytes: &mut Vec<u8>| bytes.extend([16].iter().chain([7u8; 16].iter()));
        let mut create = vec![1];
        id(&mut create);
        create.extend([0, 9]);
        create.extend(b"wordcraft");
        assert_eq!(ClientFrame::decode(&create).unwrap(), ClientFrame::Create { doc, kind: DocKind::Recents { app: app("wordcraft") } });
        let mut subscribe = vec![2];
        id(&mut subscribe);
        subscribe.extend([7, 0]);
        assert_eq!(ClientFrame::decode(&subscribe).unwrap(), ClientFrame::Subscribe { doc, after: 7, at: None });
        let mut unsubscribe = vec![3];
        id(&mut unsubscribe);
        assert_eq!(ClientFrame::decode(&unsubscribe).unwrap(), ClientFrame::Unsubscribe { doc });
        let mut created = vec![1];
        id(&mut created);
        assert_eq!(ServerFrame::decode(&created).unwrap(), ServerFrame::Created { doc });
        let mut ack = vec![4];
        id(&mut ack);
        ack.extend([3, 10]);
        assert_eq!(ServerFrame::decode(&ack).unwrap(), ServerFrame::Ack { doc, client_seq: 3, seq: 10 });
        let mut subscribed = vec![3];
        id(&mut subscribed);
        subscribed.push(9);
        assert_eq!(ServerFrame::decode(&subscribed).unwrap(), ServerFrame::Subscribed { doc, head: 9 });
        let device = DeviceId::from_uuid(uuid::Uuid::from_bytes([2; 16]));
        let mut update = vec![2];
        id(&mut update);
        update.extend([9, 2, 0xaa, 0xbb, 16]);
        update.extend([2; 16]);
        assert_eq!(ServerFrame::decode(&update).unwrap(), ServerFrame::Update { doc, seq: 9, bytes: vec![0xaa, 0xbb], device });
        let mut error = vec![5, 9];
        error.extend(b"not_found");
        error.extend([2, b'n', b'o', 1]);
        id(&mut error);
        error.push(0);
        assert_eq!(ServerFrame::decode(&error).unwrap(), ServerFrame::error("not_found", "no", Some(doc), None));
        // And the frames an older peer sends encode the same way here.
        assert_eq!(ClientFrame::Create { doc, kind: DocKind::Recents { app: app("wordcraft") } }.encode().unwrap(), create);
        assert_eq!(ServerFrame::Created { doc }.encode().unwrap(), created);
    }

    /// Every new limit at its edge.
    #[test]
    fn document_frames_are_checked_at_their_limits() {
        let doc = DocId::generate();
        let node = NodeId::generate();
        let presence = |n| ClientFrame::Presence { doc, bytes: vec![1; n] };
        assert!(presence(MAX_PRESENCE_BYTES).encode().is_ok());
        assert!(presence(MAX_PRESENCE_BYTES + 1).encode().is_err());
        assert!(presence(0).encode().is_err());
        let raw = postcard::to_stdvec(&presence(MAX_PRESENCE_BYTES + 1)).unwrap();
        assert!(matches!(ClientFrame::decode(&raw), Err(FrameError::Invalid(_))));
        let create = |m: [u8; 32]| ClientFrame::CreateDocument { doc, node, app: app("wordcraft"), snapshot: ManifestId::from_bytes(m) };
        // A manifest that isn't one (not 64 lowercase hex digits) doesn't decode.
        let mut bad = postcard::to_stdvec(&ClientFrame::Create { doc, kind: DocKind::Document { node, app: app("wordcraft") } }).unwrap();
        bad.extend(postcard::to_stdvec(&"AB".repeat(32)).unwrap());
        assert!(ClientFrame::decode(&bad).is_err());
        // A document `create` without its snapshot, or with bytes after it, is refused; so is one of a recents kind with
        // a snapshot after it.
        let bare = postcard::to_stdvec(&ClientFrame::Create { doc, kind: DocKind::Document { node, app: app("x") } }).unwrap();
        assert!(ClientFrame::decode(&bare).is_err());
        assert!(ClientFrame::Create { doc, kind: DocKind::Document { node, app: app("x") } }.encode().is_err());
        let mut extra = create([1; 32]).encode().unwrap();
        assert!(ClientFrame::decode(&extra).is_ok());
        extra.push(0);
        assert!(ClientFrame::decode(&extra).is_err());
        let mut recents = ClientFrame::Create { doc, kind: DocKind::Recents { app: app("x") } }.encode().unwrap();
        recents.extend(postcard::to_stdvec(&vec![1u8; 3]).unwrap());
        assert!(ClientFrame::decode(&recents).is_err());
        // Every frame is at most the small limit, either way (no frame carries a snapshot).
        let mut padded = postcard::to_stdvec(&ClientFrame::Unsubscribe { doc }).unwrap();
        padded.resize(MAX_FRAME_BYTES + 1, 0);
        assert!(matches!(ClientFrame::decode(&padded), Err(FrameError::TooLarge(_))));
        assert!(matches!(ServerFrame::decode(&padded), Err(FrameError::TooLarge(_))));
        // `created` with a trailer that isn't a flag.
        let mut created = ServerFrame::Created { doc }.encode().unwrap();
        created.push(2);
        assert!(ServerFrame::decode(&created).is_err());
        let p = ServerFrame::Presence { doc, bytes: vec![1; MAX_PRESENCE_BYTES + 1] };
        assert!(p.encode().is_err());
    }

    #[test]
    fn debug_output_never_shows_document_bytes() {
        let doc = DocId::generate();
        let frames = [
            format!("{:?}", ClientFrame::Update { doc, client_seq: 1, bytes: b"secret".to_vec() }),
            format!("{:?}", ClientFrame::Presence { doc, bytes: b"secret".to_vec() }),
            format!("{:?}", ServerFrame::Presence { doc, bytes: b"secret".to_vec() }),
        ];
        for shown in frames {
            assert!(!shown.contains("secret") && shown.contains("6 bytes"), "{shown}");
        }
    }
}
