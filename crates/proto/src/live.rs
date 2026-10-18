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
//! Limits ([`MAX_UPDATE_BYTES`], [`MAX_SUBSCRIPTIONS`], [`MAX_FRAME_BYTES`]) are checked when a frame is decoded and
//! again by the server; over them the answer is `too_large` or `rate_limited`, like the HTTP API.
//!
//! **Why postcard**: frames carry opaque binary updates, so a binary encoding avoids base64 (a third more bytes per
//! keystroke batch, once documents are edited live). postcard (MIT OR Apache-2.0, with `cobs`, MIT OR Apache-2.0) is compact, has no schema
//! compiler, decodes untrusted input without allocating more than the input holds, and is serde, like the rest of
//! this crate. It isn't self-describing, so a later protocol version that adds a frame or a field is negotiated at
//! `hello` (as the binary manifest format has its own version) instead of being skipped by older peers; frame
//! variants are only ever appended, never reordered. CBOR (`ciborium`, Apache-2.0) was the other candidate: it can
//! skip unknown fields, at the cost of bigger frames and three crates instead of two.

use serde::{Deserialize, Serialize};

use crate::ids::{DeviceId, DocId, Epoch, SpaceId};

/// Live protocol versions this build speaks. An agent and a server must share at least one.
pub const LIVE_PROTOCOL_MIN: u32 = 1;
pub const LIVE_PROTOCOL_MAX: u32 = 1;

/// Largest update, in bytes.
pub const MAX_UPDATE_BYTES: usize = 1024 * 1024;
/// Most documents one device may be subscribed to at once.
pub const MAX_SUBSCRIPTIONS: usize = 256;
/// Largest encoded frame: an update and its envelope (IDs, numbers, an error message).
pub const MAX_FRAME_BYTES: usize = MAX_UPDATE_BYTES + 4096;
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
}

impl DocKind {
    /// The kind as stored on the server: `recents:<app>`.
    pub fn as_text(&self) -> String {
        match self {
            DocKind::Recents { app } => format!("recents:{app}"),
        }
    }

    /// The kind from its stored form, `None` for one this build doesn't know.
    pub fn parse(text: &str) -> Option<Self> {
        let app = text.strip_prefix("recents:")?;
        AppName::new(app).ok().map(|app| DocKind::Recents { app })
    }

    /// The ID a document of this kind must have in `space`, if the kind fixes it (recents do).
    pub fn fixed_id(&self, space: SpaceId) -> Option<DocId> {
        match self {
            DocKind::Recents { app } => Some(recents_doc(space, app)),
        }
    }
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

fn encode<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameError> {
    let bytes = postcard::to_stdvec(frame).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, FrameError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    let (frame, rest) = postcard::take_from_bytes(bytes).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if !rest.is_empty() {
        return Err(FrameError::Malformed(format!("{} bytes after the frame", rest.len())));
    }
    Ok(frame)
}

impl ClientFrame {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        self.check()?;
        encode(self)
    }

    /// Decodes and checks a frame from the network (hostile: every length is checked).
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let frame: Self = decode(bytes)?;
        frame.check()?;
        Ok(frame)
    }

    fn check(&self) -> Result<(), FrameError> {
        match self {
            ClientFrame::Update { bytes, .. } if bytes.len() > MAX_UPDATE_BYTES => Err(FrameError::Invalid("an update is at most 1 MiB")),
            ClientFrame::Update { bytes, .. } if bytes.is_empty() => Err(FrameError::Invalid("an update can't be empty")),
            _ => Ok(()),
        }
    }
}

impl ServerFrame {
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        self.check()?;
        encode(self)
    }

    /// Decodes and checks a frame from the network (hostile: every length is checked).
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let frame: Self = decode(bytes)?;
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
        assert_eq!(shared_version(2, 5), None);
        assert_eq!(shared_version(0, 0), None);
        assert_eq!(shared_version(3, 1), None, "an empty range");
    }
}
