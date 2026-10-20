//! The live hub's protocol state for one account on this machine: its recents documents, what it has of each one's
//! log on the server, its local updates not yet acked, and the frames to send. No I/O but its [`DocStore`] and no
//! clock: the hub ([`super::hub`]) carries the frames over the WebSocket and ticks it ([`Replica::resend`]), and the
//! recents simulation drives it directly against the server's own sessions.
//!
//! - **An add** ([`Replica::add`]) changes the document, becomes an update with the next `client_seq`, and is written
//!   to the document's file (with the update in its list of unacked ones) before the app is told it is kept; if the
//!   write fails, the document goes back to what its file holds. It is sent at once if the link is up, else when it
//!   comes up.
//! - **Connecting** ([`Replica::connected`]): `hello`; `welcome` names the space's epoch. Every document is created (if
//!   the server lacks it) and subscribed from the number of the last update it has of the log, with that update's hash
//!   (`at`); after `subscribed`, every unacked update is sent (again: the server acks one it already has with its first
//!   number).
//! - **What it has of the log** ([`Have`]) is one number: every update up to it is in the document's file, applied or
//!   waiting in Loro for its history. The server sends a document's log in order without gaps, so an update counts as
//!   had as soon as it is taken. Loro applies a waiting one when its history comes; the waiting ones are kept in the
//!   file (Loro doesn't keep them in a snapshot) and imported again on load. An update Loro can't decode (or that made
//!   it fail) has no operations anything can depend on: it counts as had, and is said once.
//! - **What the server holds is known by version, not by acks**: an unacked update all of whose operations are in the
//!   rows taken (and the snapshot a catch-up started with) is stored, and leaves the unacked list, whatever its ack
//!   does. Its own acks may come before the updates of others numbered below them: such an ack is kept (on the unacked
//!   update, or beside it once the update is stored) until the number is reached, then it counts as had. On every new
//!   connection those notes are forgotten and the updates sent again: the catch-up brings the rows themselves, the
//!   server acks one it has with its number, and stores one a restore lost. A gap in what the server sends otherwise
//!   (a frame lost on a connection given up on) is caught up by subscribing again from the number it has.
//! - **A server restored from a backup, or a log the admin emptied**: the epoch is new (a reset starts one too), or
//!   the update at `after` isn't the one this machine has there (`resync_required`), or `subscribed` names a head below
//!   the number it has. The document starts over: its waiting updates go, it catches up from the start, and then sends
//!   up what the log lacks of it (the ranges of operations of every update it took are noted on the way), a part of at
//!   most 64 KiB at a time, in causal order (each part applies once the ones before it are in the log), a run of
//!   others' operations only after a random wait of a few ticks (their machines may push them), so every update it
//!   holds, its own acked ones too, is on the server again without each machine sending the same history. Unacked
//!   updates travel in those parts.
//! - **Refusals**: `rate_limited` sends again later (a create or subscribe: again with a growing pause, so do
//!   `not_found` after a refused create, and `invalid`); `too_large` sends the document instead, as a push in smaller
//!   parts (halving each time, three times at most); `quota_exceeded` (the document is full on the server) keeps every
//!   unacked update and tries again after a growing pause, and says so. A document whose own history is over the cap
//!   stays full until log compaction (later).
//! - **Waiting updates are capped** at [`MAX_WAITING_BYTES`] per document: an update past it isn't taken, and the
//!   document stops taking the server's updates until the next connection, said once (an honest log never comes near;
//!   a device that does this is revoked, and the admin resets the document).
//! - **Loro failing** on an update from the server: the document is read back from its file and that update skipped (a
//!   catch-up's batch Loro fails on is taken again one row at a time, so only the row at fault is); on a waiting update
//!   imported again: that one is dropped. A machine that dropped a row never compacts past it. A second failure in a
//!   run stops the document until the agent restarts (its local changes are kept), and its apps are told. A file that
//!   doesn't read is set aside (kept); its document starts afresh from its unacked updates, which are kept and sent.
//! - A newer agent's update is taken like any other: once the document says it is of a newer version, this agent only
//!   stops adding to it (the design's rule: an older agent is read-only on a newer document), and says why.
//!
//! **Documents of files** (with a server on live protocol 2) work the same way, with what recents never needed:
//!
//! - **Joining**: a file opened for the first time here is followed from the start to see whether the server has its
//!   document ([`Replica::open_document`]). If it does, the document is live once caught up; if not (`not_found`), the
//!   app is asked for an import ([`DocEvent::NeedsImport`]), which goes up as `create` with its snapshot
//!   ([`Replica::import_document`]); kept, it is the document; not kept (another machine's import was first), it is
//!   dropped and the document caught up from the start. The app hears it is live only once that is settled
//!   ([`DocEvent::Joined`]). A document that isn't joined yet is never written to disk, so a file that has never been
//!   live never opens offline from an empty copy. A joined document whose server lost it (`not_found`) is made again
//!   from this machine's copy. One the server holds nothing of (an admin reset it, and its machines haven't sent it
//!   back) isn't joined: the app opens the file (`not_live {resetting}`).
//! - **Local updates** ([`Replica::local_update`]) are imported into the copy with Loro's checked import (an update
//!   Loro refuses leaves the copy as it was; one it fails on reads the document back from disk), appended to the
//!   document's journal (durable) before the app is told, and sent at once (during a push too: the apps' peers are this
//!   machine's own). The document's file itself is written now and then ([`Replica::flush_documents`]), not per
//!   keystroke; the journal is cleared after. A journal append that fails is cut back and refused, and the document is
//!   written whole before its journal takes another; a journal is read up to its first bad record, and cut there.
//! - **Snapshots**: a catch-up from below the server's snapshot starts with it; one this agent can't read makes it
//!   follow the whole log instead (`subscribe_log`). When the server asks to compact, the agent answers once it holds
//!   every update up to `upto` and nothing waits, with a snapshot of exactly those updates ([`LogVersion`]), or not at
//!   all (another device is asked).
//! - **Presence** goes both ways as it is; remote updates and presence are handed to the apps ([`Replica::take_events`]).
//! - **At most [`MAX_SUBSCRIPTIONS`] followed and [`MAX_LOADED_DOCUMENTS`] in memory**: past those, the least recently
//!   used document no app has open and with nothing to send or waiting is unfollowed, and then written and let go of
//!   (it stays on disk, and is loaded and followed again when opened). At start, only documents with something to send
//!   or waiting are loaded (and followed).

use std::collections::{BTreeMap, BTreeSet};

use craft_cloud_proto::api::codes;
use craft_cloud_proto::fragments::{MAX_WHOLE_BYTES, is_bundle, is_fragment, join, parse, split};
use craft_cloud_proto::live::{
    AppName, ClientFrame, DocKind, LIVE_DOCUMENTS_PROTOCOL, LIVE_PROTOCOL_MAX, LIVE_PROTOCOL_MIN, MAX_SNAPSHOT_BYTES, MAX_SUBSCRIPTIONS,
    MAX_UPDATE_BYTES, PROTOCOL_MISMATCH, ServerFrame, node_doc, recents_doc, update_hash,
};
use craft_cloud_proto::local::{NotLiveReason, RecentEntry};
use craft_cloud_proto::{DocId, Epoch, ManifestId, NodeId, SpaceId};
use serde::{Deserialize, Serialize};

use super::blobs::{Upload, cut};
use super::copy::{LiveCopy, LogVersion, Taken as CopyTaken, Version, version_of};
use super::recents::{AddKey, Imported, Recents, Spans, UpdateError, add_spans, spans_of};
use super::store::DocStore;

/// The format of a document's file. Fixed from the first release on.
const DOC_FILE_FORMAT: u32 = 1;
/// Most bytes of updates waiting in Loro for their history, per document.
pub const MAX_WAITING_BYTES: usize = 8 * 1024 * 1024;
/// How often a `too_large` refusal halves the size of the parts before the document stays here.
const MAX_STRIKES: u32 = 3;
/// The longest pause, in ticks of [`Replica::resend`] (about 2 s each in the agent), before a refused subscribe is
/// tried again.
const MAX_RETRY_TICKS: u32 = 32;
/// The most bytes of one part of a push after a catch-up from the start.
const PUSH_PART: usize = 64 * 1024;
/// A push waits a random 1 to this many ticks before each part of other machines' operations (theirs may push them
/// first). With delivery within a tick, a part goes twice only when two machines draw the same lowest wait: about 1 in
/// this many per other machine.
const PUSH_PATIENCE: u32 = 8;
/// The Loro peers a push counts as this machine's own, latest first: its own copy's in its latest runs, and those its
/// apps wrote a document of a file as (older ones count as others': they still go up, after a wait). A few per run, so
/// without a cap the list would grow for good.
const MAX_OWN_PEERS: usize = 64;
/// Documents of files a replica keeps loaded (in memory) at most: past it, the least recently used one with nothing to
/// send and no app holding it is written and let go of ([`Replica::unload`]), and loaded again when opened.
pub const MAX_LOADED_DOCUMENTS: usize = 256;
/// Heals of one document in a run after which its push stops (the same state fails the same way again).
const MAX_HEALS: u32 = 2;
/// The format of a document of a file's file (`.doc`).
const DOCUMENT_FILE_FORMAT: u32 = 1;
/// A document's journal is written into its file (and cleared) once it holds this many bytes, besides now and then.
const MAX_JOURNAL_BYTES: usize = 4 * 1024 * 1024;
/// A catch-up's updates are taken a batch at a time, at most this many bytes of them at once.
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Most bytes of rows of a document's log that are fragments of changes not whole yet, kept until they are.
pub const MAX_FRAGMENT_BYTES: usize = 2 * MAX_WHOLE_BYTES;
/// Ticks of [`Replica::resend`] (about 2 s each in the agent) without a new fragment after which the fragments of a
/// change that never came whole are dropped, never applied (about a week of the agent running: its sender's agent
/// sends every fragment it has again until the server has it, so only one that lost its own can leave one half-way).
pub const FRAGMENT_PATIENCE_TICKS: u32 = 300_000;
/// The most operations a copy may be past the version a compaction asks for and still make its snapshot (Loro checks
/// the document out there, in time growing with the square of the text it takes back).
const MAX_CHECKOUT_OPS: i64 = 64 * 1024;

/// Every update of the log up to `upto` is in the document's file; `upto_hash` is the hash of the update at `upto`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Have {
    upto: u64,
    upto_hash: Option<[u8; 32]>,
}

/// A local update not yet acked; `acked`: the number an ack gave it, above what the document has of the log (the
/// updates below it on their way).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    client_seq: u64,
    bytes: Vec<u8>,
    acked: Option<u64>,
    /// A part of what the log lacks of the document, after a catch-up from the start ([`Doc::push_next`]).
    push: bool,
}

/// An update from the server that Loro holds until its history comes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Waiting {
    hash: [u8; 32],
    /// How far each peer's operations must reach for it to be in ([`Recents::covers`]).
    needs: Vec<(u64, i32)>,
    bytes: Vec<u8>,
}

/// A document's file.
#[derive(Debug, Serialize, Deserialize)]
struct DocFile {
    format: u32,
    space: SpaceId,
    doc: DocId,
    app: AppName,
    have: Have,
    next_client_seq: u64,
    pending: Vec<Pending>,
    /// The document as a Loro snapshot (its unacked updates are in it too, and in `pending`).
    snapshot: Vec<u8>,
    /// The server's updates in `have` that Loro holds for their history (not in a snapshot).
    waiting: Vec<Waiting>,
    /// The epoch the server had when `have` was learnt (`None`: never connected).
    epoch: Option<Epoch>,
    /// What the log holds since a catch-up from the start, while what it lacks of the document goes up.
    pushing: Option<Spans>,
    /// The Loro peers this machine edited the document as, latest first (one per run, at most [`MAX_OWN_PEERS`]).
    own_peers: Vec<u64>,
    /// Ticks left before the push's next part of others' operations.
    push_wait: Option<u32>,
}

/// What a recents file holds beyond [`DocFile`], written after it (an older agent reads the file and ignores what
/// follows; a file an older agent wrote has nothing after it).
#[derive(Debug, Default, Serialize, Deserialize)]
struct DocFileMore {
    /// [`Doc::dropped`].
    dropped: Option<u64>,
}

/// A recents file: [`DocFile`], and [`DocFileMore`] when it has it.
fn read_doc_file(bytes: &[u8]) -> postcard::Result<(DocFile, DocFileMore)> {
    let (file, rest) = postcard::take_from_bytes::<DocFile>(bytes)?;
    let more = if rest.is_empty() { DocFileMore::default() } else { postcard::from_bytes(rest).unwrap_or_default() };
    Ok((file, more))
}

/// A document of a file's file (`<id>.doc`, beside its journal): what a recents file holds, and its node. A file of its
/// own kind, so an older agent (which reads only recents files) leaves it alone.
#[derive(Debug, Serialize, Deserialize)]
struct DocumentFile {
    format: u32,
    space: SpaceId,
    doc: DocId,
    node: NodeId,
    app: AppName,
    have: Have,
    next_client_seq: u64,
    pending: Vec<Pending>,
    snapshot: Vec<u8>,
    waiting: Vec<Waiting>,
    epoch: Option<Epoch>,
    pushing: Option<Spans>,
    own_peers: Vec<u64>,
    push_wait: Option<u32>,
    /// Follow it with `subscribe_log` (the server's snapshot didn't read here).
    log_only: bool,
    /// [`Doc::dropped`].
    dropped: Option<u64>,
}

/// What a document of a file's file holds beyond [`DocumentFile`], written after it when there is any (an older agent
/// reads the file and ignores what follows).
#[derive(Debug, Default, Serialize, Deserialize)]
struct DocumentFileMore {
    /// [`Doc::fragments`].
    fragments: Vec<(u64, Vec<u8>)>,
}

/// A document of a file's file: [`DocumentFile`], and [`DocumentFileMore`] when it has it.
fn read_document_file(bytes: &[u8]) -> postcard::Result<(DocumentFile, DocumentFileMore)> {
    let (file, rest) = postcard::take_from_bytes::<DocumentFile>(bytes)?;
    let more = if rest.is_empty() { DocumentFileMore::default() } else { postcard::from_bytes(rest).unwrap_or_default() };
    Ok((file, more))
}

/// The updates `pending` (this machine's unacked updates, in order) make whole: one as it is, a change in fragments
/// once (at its first fragment's place), and none of a change whose fragments aren't all there.
fn wholes(pending: &[Pending]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut done = BTreeSet::new();
    for (i, p) in pending.iter().enumerate() {
        let Some(f) = parse(&p.bytes) else {
            out.push(p.bytes.clone());
            continue;
        };
        if done.contains(&f.set) {
            continue;
        }
        let parts: Vec<&[u8]> =
            pending.iter().skip(i).filter(|q| parse(&q.bytes).is_some_and(|g| g.set == f.set)).map(|q| q.bytes.as_slice()).collect();
        if let Some(whole) = join(parts) {
            done.insert(f.set);
            out.push(whole);
        }
    }
    out
}

/// Imports `bytes` into `body`, an update of any size up to a whole change's (the frame limit is checked where an
/// update comes in, a whole change's when its fragments are joined).
fn import_whole(body: &Body, bytes: &[u8]) -> Result<Imported, UpdateError> {
    if bytes.len() > MAX_UPDATE_BYTES { body.import_any_size(bytes) } else { body.import(bytes) }
}

/// What a file of either kind holds, as [`Doc::from_parts`] reads it.
struct Parts {
    id: DocId,
    kind: Kind,
    have: Have,
    next_client_seq: u64,
    pending: Vec<Pending>,
    snapshot: Vec<u8>,
    waiting: Vec<Waiting>,
    epoch: Option<Epoch>,
    pushing: Option<Spans>,
    own_peers: Vec<u64>,
    push_wait: Option<u32>,
    dropped: Option<u64>,
    fragments: Vec<(u64, Vec<u8>)>,
}

/// One record of a document's journal: a local update, on disk before the app is told it is kept.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct JournalRecord {
    client_seq: u64,
    bytes: Vec<u8>,
}

/// A journal record as it is written: its length, the record, and the first 8 bytes of its BLAKE3 (a record cut off
/// by a crash, never acked to the app, is where the journal ends).
fn journal_record(record: &JournalRecord) -> std::io::Result<Vec<u8>> {
    let body = postcard::to_stdvec(record).map_err(std::io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(|_| std::io::Error::other("a journal record over 4 GiB"))?;
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(update_hash(&body).get(..8).unwrap_or_default());
    Ok(out)
}

/// The records of a journal, up to the first that doesn't read, and how many bytes they take (where it is cut).
fn journal_records(mut bytes: &[u8]) -> (Vec<JournalRecord>, usize) {
    let mut out = Vec::new();
    let mut read = 0;
    while bytes.len() >= 4 {
        let Some(len) = bytes.get(..4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_le_bytes) else { break };
        let len = len as usize;
        let (Some(body), Some(check)) = (bytes.get(4..4 + len), bytes.get(4 + len..12 + len)) else { break };
        if update_hash(body).get(..8) != Some(check) {
            break;
        }
        match postcard::from_bytes::<JournalRecord>(body) {
            Ok(record) => out.push(record),
            Err(_) => break,
        }
        bytes = bytes.get(12 + len..).unwrap_or_default();
        read += 12 + len;
    }
    (out, read)
}

/// Which document it is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Recents(AppName),
    Document { node: NodeId, app: AppName },
}

impl Kind {
    fn app(&self) -> &AppName {
        match self {
            Kind::Recents(app) | Kind::Document { app, .. } => app,
        }
    }

    fn is_document(&self) -> bool {
        matches!(self, Kind::Document { .. })
    }
}

/// A document's content: an app's recents list, or a document of a file (whose content nothing here reads).
enum Body {
    Recents(Recents),
    Document(LiveCopy),
}

impl Body {
    fn new(kind: &Kind, peer: u64) -> Self {
        match kind {
            Kind::Recents(_) => Body::Recents(Recents::new(peer)),
            Kind::Document { .. } => Body::Document(LiveCopy::new(peer)),
        }
    }

    fn load(kind: &Kind, snapshot: &[u8], peer: u64) -> Result<Self, UpdateError> {
        Ok(match kind {
            Kind::Recents(_) => Body::Recents(Recents::load(snapshot, peer)?),
            Kind::Document { .. } => Body::Document(LiveCopy::load(snapshot, peer)?),
        })
    }

    fn copy(&self) -> &LiveCopy {
        match self {
            Body::Recents(recents) => recents,
            Body::Document(copy) => copy,
        }
    }

    /// Imports an update; `changed`: the list changed (recents), or it brought something an app hasn't seen (a
    /// document: it goes to the apps).
    fn import(&self, bytes: &[u8]) -> Result<Imported, UpdateError> {
        match self {
            Body::Recents(recents) => recents.import(bytes),
            Body::Document(copy) => copy.import(bytes).map(document_imported),
        }
    }

    fn import_any_size(&self, bytes: &[u8]) -> Result<Imported, UpdateError> {
        match self {
            Body::Recents(recents) => recents.import_any_size(bytes),
            Body::Document(copy) => copy.import_any_size(bytes).map(document_imported),
        }
    }

    /// A recents document a newer agent wrote: its version.
    fn newer(&self) -> Option<i64> {
        match self {
            Body::Recents(recents) => recents.newer(),
            Body::Document(_) => None,
        }
    }

    fn view(&self) -> Vec<RecentEntry> {
        match self {
            Body::Recents(recents) => recents.view(),
            Body::Document(_) => Vec::new(),
        }
    }

    /// Marks the document as one Loro failed in (for tests).
    #[cfg(test)]
    fn poison(&self) {
        self.copy().poison();
    }
}

fn document_imported(taken: CopyTaken) -> Imported {
    match taken {
        CopyTaken::Applied { new } => Imported::Applied { changed: new },
        CopyTaken::Waiting { needs } => Imported::Waiting { needs, changed: true },
    }
}

/// Where a document of a file that isn't live here yet stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Join {
    /// Followed from the start, to see whether the server has it.
    Probing,
    /// The server doesn't have it: waiting for the app's import.
    NeedsImport,
    /// The import went up as `create`: waiting for `created`.
    Creating(Vec<u8>),
    /// Another machine's import was kept: caught up from the start.
    Following,
}

/// What happened to a document of a file, for the apps that have it open (or wait to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocEvent {
    /// It is live here now (caught up, or made by this machine's import): the apps that wait for it get its state.
    Joined(DocId),
    /// The server doesn't have it: an app is to send its import.
    NeedsImport(DocId),
    /// It can't be made live now: the apps that wait open the file as a plain file.
    NotLive(DocId, NotLiveReason, String),
    /// Another machine's change, to apply as it is.
    Remote(DocId, Vec<u8>),
    /// Another machine's presence, as it sent it.
    Presence(DocId, Vec<u8>),
}

/// Why a local update was refused.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    /// The update isn't taken (Loro refused it, or it is over a limit): the document is as it was.
    #[error("{0}")]
    Invalid(String),
    #[error("the document couldn't be written to disk: {0}")]
    Io(#[from] std::io::Error),
}

/// How opening a document of a file went ([`Replica::open_document`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opening {
    /// Live here: its state is ready ([`Replica::document_state`]).
    Live,
    /// Being joined: a [`DocEvent`] says how it ends.
    Joining,
    /// It has never been live here and the server can't be asked: a plain file for now.
    NotLive(NotLiveReason, String),
}

/// Where a replica gets the Loro peer ID of each document it opens or makes: random in the agent (a fresh one per
/// run, so a peer ID is never reused with a different history), from the seed in the simulation.
pub type Peers = Box<dyn FnMut() -> u64 + Send>;

/// Random peer IDs, for the agent.
pub fn random_peers() -> Peers {
    Box::new(|| {
        let mut bytes = [0u8; 8];
        // A failed random source still gives distinct IDs per document within this run, and Loro's own fresh
        // document had one random ID already; this only replaces it.
        let _ = getrandom::fill(&mut bytes);
        u64::from_le_bytes(bytes)
    })
}

/// How far a document is on the current connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    /// Not subscribed (no connection, not yet welcomed, or refused for a moment).
    No,
    /// `subscribe` sent; the catch-up is under way.
    CatchingUp,
    /// Caught up: updates go out as they are made.
    Live,
}

/// What taking an update from the server did.
enum Taken {
    /// It counts as had.
    Had,
    /// Past the cap on waiting updates: not taken, and the document takes nothing more on this connection.
    Stuck,
    /// Loro failed on it: the document may be half-changed and is read back from its file.
    Panicked,
    /// Loro failed on a waiting update imported again (hash): that one is dropped, and the document read back.
    WaitingPanicked([u8; 32]),
}

struct Doc {
    id: DocId,
    kind: Kind,
    body: Body,
    have: Have,
    pending: Vec<Pending>,
    waiting: Vec<Waiting>,
    next_client_seq: u64,
    epoch: Option<Epoch>,
    /// After a catch-up from the start: the ranges of operations the log is known to hold, while the parts it lacks
    /// go up one at a time (each one computed when the last is acked, so what other machines push meanwhile isn't
    /// sent again).
    pushing: Option<Spans>,
    /// Ticks the push still waits before its next part of other machines' operations (`None`: not drawn yet).
    push_wait: Option<u32>,
    /// An update came in since the wait was drawn: someone is pushing, and this machine queues behind it.
    queued: bool,
    /// The Loro peers this machine edited the document as, latest first.
    own_peers: Vec<u64>,
    /// The update (number and hash) Loro failed on: counted as had, not imported, when it comes again.
    skip: Option<(u64, [u8; 32])>,
    /// The server said the document is full: ticks until its unacked updates are tried again (the admin may have
    /// emptied it), nothing being sent meanwhile; and the pause it had.
    full: Option<u32>,
    full_pause: u32,
    /// Past the cap on waiting updates: nothing is taken until the next connection.
    stuck: bool,
    /// Loro failed on a second row of it in this run: not followed until the agent restarts ([`Replica::stopped`]).
    stopped: bool,
    /// Loro failed outside an import (an export): the document is read back from its file.
    broken: bool,
    sub: Sub,
    /// Unacked updates sent on this connection (by client_seq).
    sent: BTreeSet<u64>,
    /// The operations the server is known to hold up to `have`: every row taken (and own row acked) and the snapshot
    /// a catch-up started with, since the document was loaded or its log learnt afresh. An unacked update all of whose
    /// operations are in it is stored ([`Doc::settle_pending`]), whatever its ack does.
    seen: Spans,
    /// Own updates sent on this connection that the log turned out to hold before their ack (a row or the server's
    /// snapshot brought their operations back): the ack still comes, numbering a row that holds them and is never sent
    /// to this machine, so it counts as had when it comes (by client_seq: the update's bytes).
    taken_back: BTreeMap<u64, Vec<u8>>,
    /// Acks of own rows above what is had (rows below them on their way), whose updates left the unacked list: by the
    /// row's number, its bytes, counted when the rows below it are here (as [`Pending::acked`]; this connection only,
    /// a catch-up sends the rows themselves).
    back_acked: BTreeMap<u64, Vec<u8>>,
    /// The lowest row of the log this machine dropped (Loro failed on it, or it can never apply): its copy lacks it,
    /// so it never answers a `compact` at or above it (another device does). Kept in its file.
    dropped: Option<u64>,
    /// `too_large` refusals since the last ack.
    strikes: u32,
    /// A refused create or subscribe: ticks until it is tried again, and the pause it had.
    retry: Option<(u32, u32)>,
    /// The retry is after a refused create (the account's limit), which was said: the `not_found` that the subscribe
    /// after it gets is not said again.
    create_refused: bool,
    /// Changed since its file was last written.
    dirty: bool,
    /// What was said about this document's updates already.
    said: BTreeSet<String>,
    /// What the server's log holds of it, as a version per number: what a compaction's snapshot is made at.
    log: LogVersion,
    /// The server asked for a snapshot of the updates up to this: answered once they are all here.
    compact_ask: Option<u64>,
    /// Live here: it has a file and opens offline. Recents always are; a document of a file once joined.
    joined: bool,
    /// A document of a file being joined.
    join: Option<Join>,
    /// A joined document the server lost is being made again from this copy (`create` sent).
    creating: bool,
    /// Not followed (the device's cap on subscriptions): it stays on disk.
    parked: bool,
    /// When an app last opened it (a counter), and how many have it open now.
    last_used: u64,
    open: u32,
    /// Followed with `subscribe_log`: the server's snapshot didn't read here.
    log_only: bool,
    /// The catch-up under way was given up on (its snapshot didn't read): its `subscribed` starts the next one.
    stale: bool,
    /// Bytes appended to its journal since its file was last written.
    journal: usize,
    /// An append to its journal failed (or its end couldn't be cut): it is written whole before its journal takes
    /// another change, so nothing after a bad record is ever hidden by it.
    journal_failed: bool,
    /// Its file must be written at the next flush (not only now and then).
    urgent: bool,
    /// The waiting update Loro just failed on, imported again ([`Doc::rewait`]).
    waiting_failed: Option<[u8; 32]>,
    /// The catch-up under way started with the server's snapshot ([`Doc::check_snapshot`]).
    from_snapshot: bool,
    /// It has a file (written, or loaded from one).
    on_disk: bool,
    /// The catch-up's updates not taken yet ([`Replica::take_batch`]), and their bytes.
    batch: Vec<(u64, Vec<u8>)>,
    batched: usize,
    /// The catch-up started with the server's snapshot (its base and manifest), being downloaded: its updates wait in
    /// `batch` (and its `subscribed`, in `deferred_head`) until it is here ([`Replica::on_snapshot`]).
    fetching: Option<(u64, ManifestId)>,
    deferred_head: Option<u64>,
    /// Snapshots to upload before the frames that name them go ([`Replica::take_uploads`]).
    uploads: Vec<Upload>,
    /// The rows of the log (taken from the server, or this machine's own acked) that are fragments of a change not
    /// whole yet: its number and bytes. They count as had; the change goes into the copy (and to the apps) once every
    /// fragment of it is here, never before. Kept in its file. No snapshot is made for the server while any is here.
    fragments: Vec<(u64, Vec<u8>)>,
    /// Ticks since the last fragment came, while some are here ([`FRAGMENT_PATIENCE_TICKS`]).
    fragment_ticks: u32,
    /// The change the row just taken made whole, for the apps ([`Doc::take`]).
    joined_now: Option<Vec<u8>>,
}

/// What a row that is a fragment did ([`Doc::take_fragment`]).
enum Fragmented {
    /// Kept: more of its change is to come.
    Waiting,
    /// It was the last: the change whole, and the row its first fragment took.
    Whole { first: u64, update: Vec<u8> },
    /// It doesn't read, or its change's fragments don't make the change they name: never applied.
    Broken,
}

impl Doc {
    fn empty(app: &AppName, peer: u64, id: DocId) -> Self {
        Self::with(id, Kind::Recents(app.clone()), Body::Recents(Recents::new(peer)), Have::default(), Vec::new(), Vec::new(), 1, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn with(
        id: DocId,
        kind: Kind,
        body: Body,
        have: Have,
        pending: Vec<Pending>,
        waiting: Vec<Waiting>,
        next_client_seq: u64,
        epoch: Option<Epoch>,
        pushing: Option<Spans>,
    ) -> Self {
        Self {
            id,
            own_peers: vec![body.copy().peer()],
            kind,
            body,
            have,
            pending,
            waiting,
            next_client_seq,
            epoch,
            pushing,
            push_wait: None,
            queued: false,
            skip: None,
            full: None,
            full_pause: 0,
            stuck: false,
            stopped: false,
            broken: false,
            sub: Sub::No,
            sent: BTreeSet::new(),
            seen: Spans::new(),
            taken_back: BTreeMap::new(),
            back_acked: BTreeMap::new(),
            dropped: None,
            strikes: 0,
            retry: None,
            create_refused: false,
            dirty: false,
            said: BTreeSet::new(),
            log: LogVersion::unknown(),
            compact_ask: None,
            joined: true,
            join: None,
            creating: false,
            parked: false,
            last_used: 0,
            open: 0,
            log_only: false,
            stale: false,
            journal: 0,
            journal_failed: false,
            urgent: false,
            waiting_failed: None,
            from_snapshot: false,
            on_disk: false,
            batch: Vec::new(),
            batched: 0,
            fetching: None,
            deferred_head: None,
            uploads: Vec::new(),
            fragments: Vec::new(),
            fragment_ticks: 0,
            joined_now: None,
        }
    }

    /// Row `seq` of the log is a fragment: kept until its change is whole (rows taken again are kept once).
    fn take_fragment(&mut self, seq: u64, bytes: &[u8]) -> Fragmented {
        let Some(f) = parse(bytes) else { return Fragmented::Broken };
        if !self.fragments.iter().any(|(s, _)| *s == seq) {
            self.fragments.push((seq, bytes.to_vec()));
            self.dirty = true;
        }
        self.fragment_ticks = 0;
        let of_set = |b: &[u8]| parse(b).is_some_and(|g| g.set == f.set);
        let indexes: BTreeSet<u16> = self.fragments.iter().filter(|(_, b)| of_set(b)).filter_map(|(_, b)| parse(b).map(|g| g.index)).collect();
        if indexes.len() < usize::from(f.count) {
            return Fragmented::Waiting;
        }
        let first = self.fragments.iter().filter(|(_, b)| of_set(b)).map(|(s, _)| *s).min().unwrap_or(seq);
        let whole = join(self.fragments.iter().filter(|(_, b)| of_set(b)).map(|(_, b)| b.as_slice()));
        self.fragments.retain(|(_, b)| !of_set(b));
        match whole {
            Some(update) => Fragmented::Whole { first, update },
            None => Fragmented::Broken,
        }
    }

    /// Row `seq` of the log is this machine's own update `bytes` (acked, or taken back): the server holds its
    /// operations. An own change in fragments is in the log from its last fragment's row on (it is in the copy
    /// already).
    fn note_own_row(&mut self, seq: u64, bytes: &[u8]) {
        let ranges = if self.kind.is_document() && is_fragment(bytes) {
            match self.take_fragment(seq, bytes) {
                Fragmented::Whole { first, update } => {
                    self.log.note_set(first, seq);
                    self.on_server(&update);
                    spans_of(&update).unwrap_or_default()
                }
                Fragmented::Waiting | Fragmented::Broken => Vec::new(),
            }
        } else {
            spans_of(bytes).unwrap_or_default()
        };
        self.saw(&ranges);
        self.log.note(seq, ranges);
    }

    /// The bytes of rows kept as fragments.
    fn fragment_bytes(&self) -> usize {
        self.fragments.iter().map(|(_, b)| b.len()).sum()
    }

    /// The app this document belongs to.
    fn app(&self) -> &AppName {
        self.kind.app()
    }

    /// The document as the user is told about it: `recents` names an app's recents (as every recents note always has),
    /// `document` a document of a file.
    fn told(&self, recents: impl FnOnce(&AppName) -> String, document: impl FnOnce(DocId) -> String) -> String {
        match &self.kind {
            Kind::Recents(app) => recents(app),
            Kind::Document { .. } => document(self.id),
        }
    }

    /// What the server's log holds of the document up to `have`, worked out from the copy: everything in it, when
    /// nothing in it may be out of the log (no unacked update: one may or may not be stored, and its ack doesn't say
    /// which; nothing waits; no push under way; no row dropped). Unknown otherwise, until those are gone.
    fn derive_log(&mut self) {
        self.log = LogVersion::unknown();
        if !self.waiting.is_empty() || self.pushing.is_some() || !self.pending.is_empty() || self.dropped.is_some() || !self.fragments.is_empty() {
            return;
        }
        let Ok(mut version) = self.body.copy().version() else { return };
        version.retain(|_, end| *end > 0);
        self.log = LogVersion::anchored(self.have.upto, version);
    }

    /// Unacked updates whose operations the server is known to hold ([`Doc::seen`]) are stored: they leave the list
    /// (and so count in a compaction, being in the log). An ack that came for one already is kept until the rows
    /// below it are here; one still on its way is counted when it comes ([`Doc::taken_back`]).
    fn settle_pending(&mut self) {
        if self.seen.is_empty() || self.pending.is_empty() {
            return;
        }
        let mut i = 0;
        while let Some(p) = self.pending.get(i) {
            let held = spans_of(&p.bytes).is_some_and(|ranges| !ranges.is_empty() && held_by(&self.seen, &ranges));
            if !held {
                i += 1;
                continue;
            }
            let own = self.pending.remove(i);
            self.stored(own);
        }
    }

    /// An unacked update, out of the list, is stored (the log holds its operations): an ack that came for it already
    /// is kept until the rows below it are here, one on its way is counted when it comes.
    fn stored(&mut self, own: Pending) {
        let sent = self.sent.remove(&own.client_seq);
        match own.acked {
            Some(n) if n > self.have.upto => {
                self.back_acked.insert(n, own.bytes);
            }
            Some(_) => {}
            None if sent => {
                self.taken_back.insert(own.client_seq, own.bytes);
            }
            None => {}
        }
        self.dirty = true;
    }

    /// `bytes` (an update or snapshot) were written by this machine's apps: their peers are its own in a push (which
    /// sends its own operations without a wait).
    fn own(&mut self, bytes: &[u8]) {
        let Some(ranges) = spans_of(bytes) else { return };
        for (peer, start, end) in ranges {
            if start >= end || self.own_peers.first() == Some(&peer) {
                continue;
            }
            self.own_peers.retain(|p| *p != peer);
            self.own_peers.insert(0, peer);
            self.own_peers.truncate(MAX_OWN_PEERS);
            self.dirty = true;
        }
    }

    /// The server holds `ranges` (up to `have`).
    fn saw(&mut self, ranges: &[(u64, i32, i32)]) {
        add_spans(&mut self.seen, ranges);
    }

    /// The log is learnt afresh (from the start, or a snapshot that replaces what was known): nothing seen yet, nothing
    /// dropped.
    fn forget_seen(&mut self) {
        self.seen = Spans::new();
        self.dropped = None;
    }

    /// The answer to the server's `compact`, once this machine holds every update up to `upto`: a snapshot of exactly
    /// those, or nothing (something waits, it dropped a row at or below `upto`, or the version there isn't known;
    /// another device is asked). The question is dropped either way once it can be answered.
    fn try_compact(&mut self) -> Vec<ClientFrame> {
        let Some(upto) = self.compact_ask else { return Vec::new() };
        if self.sub != Sub::Live || self.have.upto < upto {
            return Vec::new();
        }
        self.compact_ask = None;
        // (A push under way doesn't stop it: the log was learnt from the start then, and is noted from there.) Nor is
        // one made while a change's fragments are coming: the snapshot would lack the change, and the log would lose
        // its first fragments.
        if !self.waiting.is_empty() || self.stuck || !self.joined || self.is_broken() || !self.fragments.is_empty() {
            return Vec::new();
        }
        // A row it dropped is below: its copy lacks that row, and a snapshot from it would too.
        if self.dropped.is_some_and(|d| d <= upto) {
            return Vec::new();
        }
        if !self.log.is_known() && upto == self.have.upto {
            self.derive_log();
        }
        let Some(version) = self.log.at(upto) else { return Vec::new() };
        // Not from a copy far past that version: a snapshot of an earlier one makes Loro check the document out there,
        // which for a big paste since (an own change not in the log yet) takes time growing with the square of its
        // text, tens of seconds on the hub's thread. Another device answers, or the next ask finds this one there.
        let Ok(now) = self.body.copy().version() else { return Vec::new() };
        let past: i64 = now.iter().map(|(peer, end)| i64::from(*end) - i64::from(version.get(peer).copied().unwrap_or(0))).filter(|n| *n > 0).sum();
        if past > MAX_CHECKOUT_OPS {
            return Vec::new();
        }
        match self.body.copy().snapshot_at(&version) {
            Ok(bytes) if !bytes.is_empty() && bytes.len() <= MAX_SNAPSHOT_BYTES => {
                // Uploaded first (through the chunk API), then named.
                let Ok(upload) = cut(self.id, &bytes) else { return Vec::new() };
                self.log.rebase(upto, version);
                let manifest = upload.id();
                self.uploads.push(upload);
                vec![ClientFrame::Snapshot { doc: self.id, upto, manifest }]
            }
            Err(UpdateError::Panicked) => {
                self.broken = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The document as its file holds it, editing as `peer`. The unacked updates are in the snapshot already (made
    /// before it was written); imported again, they change nothing, but a snapshot written before them can't lose
    /// them. The waiting ones go back into Loro's queue. One Loro fails on is left out (the document read again from
    /// the snapshot without it): a waiting one is dropped (the server has it, and a catch-up from the start brings it
    /// if it is ever needed), an unacked one stays unacked (it is sent, and its operations come back with the log).
    /// An error only when the snapshot itself doesn't read.
    fn from_file(file: DocFile, more: DocFileMore, peer: u64, notes: &mut Vec<String>) -> Result<Self, UpdateError> {
        let parts = Parts {
            dropped: more.dropped,
            id: file.doc,
            kind: Kind::Recents(file.app),
            have: file.have,
            next_client_seq: file.next_client_seq,
            pending: file.pending,
            snapshot: file.snapshot,
            waiting: file.waiting,
            epoch: file.epoch,
            pushing: file.pushing,
            own_peers: file.own_peers,
            push_wait: file.push_wait,
            fragments: Vec::new(),
        };
        Self::from_parts(parts, peer, notes)
    }

    /// [`Doc::from_file`], for a file of either kind.
    fn from_parts(mut file: Parts, peer: u64, notes: &mut Vec<String>) -> Result<Self, UpdateError> {
        let (mut bad_pending, mut bad_waiting) = (BTreeSet::new(), BTreeSet::new());
        // (Unacked changes in fragments go in whole.)
        let own = wholes(&file.pending);
        let body = 'load: loop {
            let body = Body::load(&file.kind, &file.snapshot, peer)?;
            for (i, pending) in own.iter().enumerate() {
                if !bad_pending.contains(&i) && body.import_any_size(pending) == Err(UpdateError::Panicked) {
                    bad_pending.insert(i);
                    continue 'load;
                }
            }
            for (i, waiting) in file.waiting.iter().enumerate() {
                if !bad_waiting.contains(&i) && import_whole(&body, &waiting.bytes) == Err(UpdateError::Panicked) {
                    bad_waiting.insert(i);
                    continue 'load;
                }
            }
            break body;
        };
        if !bad_pending.is_empty() || !bad_waiting.is_empty() {
            let count = bad_pending.len() + bad_waiting.len();
            notes.push(match &file.kind {
                Kind::Recents(app) => format!(
                    "Loro failed on {count} stored recents update(s) of {app}; the waiting ones were dropped, the unsent ones are kept and sent"
                ),
                Kind::Document { .. } => format!(
                    "Loro failed on {count} stored update(s) of the live document {}; the waiting ones were dropped, the unsent ones are kept and sent",
                    file.id
                ),
            });
        }
        let mut i = 0;
        file.waiting.retain(|_| {
            i += 1;
            !bad_waiting.contains(&(i - 1))
        });
        let mut doc =
            Self::with(file.id, file.kind, body, file.have, file.pending, file.waiting, file.next_client_seq.max(1), file.epoch, file.pushing);
        doc.waiting.retain(|w| !doc.body.copy().covers(&w.needs));
        for peer in file.own_peers {
            if !doc.own_peers.contains(&peer) && doc.own_peers.len() < MAX_OWN_PEERS {
                doc.own_peers.push(peer);
            }
        }
        doc.push_wait = file.push_wait;
        doc.dropped = file.dropped;
        doc.fragments = file.fragments;
        doc.on_disk = true;
        doc.derive_log();
        Ok(doc)
    }

    /// A document whose file doesn't read (the snapshot is damaged, or Loro fails on it): started afresh from its
    /// unacked updates, which are kept and sent; it catches up from the start, so nothing the server has is missed.
    fn from_pending(id: DocId, kind: Kind, pending: Vec<Pending>, next_client_seq: u64, peer: u64) -> Self {
        let body = Body::new(&kind, peer);
        for update in wholes(&pending) {
            let _ = body.import_any_size(&update);
        }
        let pending = pending.into_iter().map(|p| Pending { acked: None, push: false, ..p }).collect();
        let mut doc = Self::with(id, kind, body, Have::default(), pending, Vec::new(), next_client_seq.max(1), None, None);
        doc.dirty = true;
        doc.urgent = true;
        doc
    }

    /// Takes update `seq` (the next one of the log) from the server.
    fn take(&mut self, seq: u64, bytes: &[u8], notes: &mut Vec<String>, changed: &mut bool) -> Taken {
        let hash = update_hash(bytes);
        self.dirty = true;
        // Something came in for the document while this machine pushes: someone else is pushing (or adding).
        // Its wait for the next part of others' operations starts over, so pushers queue behind whoever is pushing
        // instead of sending the same parts.
        // (Not the catch-up's own updates: those are the log, not a pusher.)
        if self.pushing.is_some() && self.sub == Sub::Live {
            self.push_wait = None;
            self.queued = true;
        }
        if self.skip == Some((seq, hash)) {
            self.skip = None;
            self.drop_row(seq);
            // A fragment Loro failed on (its change whole): the rest of that change goes too.
            if let Some(f) = parse(bytes) {
                self.fragments.retain(|(_, b)| parse(b).is_none_or(|g| g.set != f.set));
            }
            self.log.note(seq, Vec::new());
            self.advance(seq, hash);
            return Taken::Had;
        }
        // A fragment of a change (a document's): kept, and counted as had, until the change is whole; only then does
        // the change go into the copy, and to the apps.
        let joined;
        let bytes: &[u8] = if self.kind.is_document() && is_fragment(bytes) {
            match self.take_fragment(seq, bytes) {
                Fragmented::Waiting => {
                    if self.fragment_bytes() > MAX_FRAGMENT_BYTES {
                        self.fragments.retain(|(s, _)| *s != seq);
                        self.stuck = true;
                        let line = format!(
                            "more than {} MiB of fragments of changes of the live document {} wait for the rest; this machine takes no more of them until it connects again",
                            MAX_FRAGMENT_BYTES >> 20,
                            self.id
                        );
                        self.say(notes, line);
                        return Taken::Stuck;
                    }
                    self.log.note(seq, Vec::new());
                    self.advance(seq, hash);
                    return Taken::Had;
                }
                Fragmented::Broken => {
                    let line = format!(
                        "a change of the live document {} came in fragments that don't make it; it was skipped, and this machine leaves compacting it to other devices",
                        self.id
                    );
                    self.say(notes, line);
                    self.drop_row(seq);
                    self.log.note(seq, Vec::new());
                    self.advance(seq, hash);
                    return Taken::Had;
                }
                Fragmented::Whole { first, update } => {
                    self.log.note_set(first, seq);
                    joined = update;
                    self.joined_now = Some(joined.clone());
                    &joined
                }
            }
        } else {
            bytes
        };
        let had_history = self.body.copy().has_history();
        let mut in_log = true;
        match import_whole(&self.body, bytes) {
            Ok(Imported::Applied { changed: list }) => {
                *changed |= list;
                if !had_history && self.rewait(changed).is_err() {
                    return self.waiting_panicked();
                }
                if let Some(version) = self.body.newer() {
                    let line = format!(
                        "the recents of {} were written by a newer Craft Cloud agent (version {version}); they are shown, and this agent adds to them again once it is updated",
                        self.app()
                    );
                    self.say(notes, line);
                }
            }
            Ok(Imported::Waiting { needs, changed: list }) => {
                *changed |= list;
                if !self.waiting.iter().any(|w| w.hash == hash) {
                    let held: usize = self.waiting.iter().map(|w| w.bytes.len()).sum();
                    if held.saturating_add(bytes.len()) > MAX_WAITING_BYTES {
                        self.stuck = true;
                        let line = self.told(
                            |app| format!(
                                "more than {} MiB of recents updates for {app} wait for history that never came; this machine takes no more of them until it connects again",
                                MAX_WAITING_BYTES >> 20
                            ),
                            |id| format!(
                                "more than {} MiB of updates of the live document {id} wait for history that never came; this machine takes no more of them until it connects again",
                                MAX_WAITING_BYTES >> 20
                            ),
                        );
                        self.say(notes, line);
                        return Taken::Stuck;
                    }
                    self.waiting.push(Waiting { hash, needs, bytes: bytes.to_vec() });
                }
            }
            Err(UpdateError::Panicked) => return Taken::Panicked,
            Err(e) => {
                in_log = false;
                let line = self.told(
                    |app| format!(
                        "a recents update for {app} from the server can never apply and was skipped: {e}; this machine leaves compacting them to other devices"
                    ),
                    |id| format!(
                        "an update of the live document {id} from the server can never apply and was skipped: {e}; this machine leaves compacting it to other devices"
                    ),
                );
                self.say(notes, line);
                self.drop_row(seq);
            }
        }
        self.waiting.retain(|w| !self.body.copy().covers(&w.needs));
        self.on_server(bytes);
        // What the log holds, as far as this copy has it (an update that never applies isn't in it).
        let ranges = if in_log { spans_of(bytes).unwrap_or_default() } else { Vec::new() };
        self.saw(&ranges);
        self.log.note(seq, ranges);
        // This machine's own unacked updates the log turns out to hold (its ack was lost, or forgotten with the
        // connection, or another machine pushed the same operations) are stored.
        self.settle_pending();
        self.advance(seq, hash);
        Taken::Had
    }

    /// Row `seq` isn't in this copy (Loro failed on it, or it can never apply; the note that says so says this too):
    /// this machine leaves compacting the log past it to other devices.
    fn drop_row(&mut self, seq: u64) {
        self.dropped = Some(self.dropped.map_or(seq, |d| d.min(seq)));
        self.dirty = true;
        self.urgent = true;
    }

    /// After the copy's first history (perhaps a snapshot) Loro dropped what waited: it goes in again. `Err` with the
    /// one Loro failed on (in [`Doc::waiting_failed`]).
    fn rewait(&mut self, changed: &mut bool) -> Result<(), ()> {
        for waiting in &self.waiting {
            match import_whole(&self.body, &waiting.bytes) {
                Ok(Imported::Applied { changed: list } | Imported::Waiting { changed: list, .. }) => *changed |= list,
                Err(UpdateError::Panicked) => {
                    self.waiting_failed = Some(waiting.hash);
                    return Err(());
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    fn waiting_panicked(&mut self) -> Taken {
        match self.waiting_failed.take() {
            Some(hash) => Taken::WaitingPanicked(hash),
            None => Taken::Panicked,
        }
    }

    /// `bytes` are in the log: while a push is under way, their operations needn't go up.
    fn on_server(&mut self, bytes: &[u8]) {
        if let Some(has) = self.pushing.as_mut()
            && let Some(ranges) = spans_of(bytes)
        {
            add_spans(has, &ranges);
        }
    }

    /// The log is had up to `seq` (whose update has `hash`), and up to the own acks that follow it.
    fn advance(&mut self, seq: u64, hash: [u8; 32]) {
        self.have = Have { upto: seq, upto_hash: Some(hash) };
        self.back_acked.retain(|at, _| *at > seq);
        loop {
            let next = self.have.upto.saturating_add(1);
            let bytes = if let Some(i) = self.pending.iter().position(|p| p.acked == Some(next)) {
                let own = self.pending.remove(i);
                self.sent.remove(&own.client_seq);
                self.on_server(&own.bytes);
                own.bytes
            } else if let Some(bytes) = self.back_acked.remove(&next) {
                bytes
            } else {
                break;
            };
            self.have = Have { upto: next, upto_hash: Some(update_hash(&bytes)) };
            self.note_own_row(next, &bytes);
        }
        // Own updates the catch-up brought back count as had too.
        let upto = self.have.upto;
        let (back, keep): (Vec<Pending>, Vec<Pending>) =
            std::mem::take(&mut self.pending).into_iter().partition(|p| p.acked.is_some_and(|a| a <= upto));
        self.pending = keep;
        for own in back {
            self.on_server(&own.bytes);
        }
        self.dirty = true;
    }

    /// Loro failed in this document (an import, export or read panicked): read back from its file, never dropped.
    fn is_broken(&self) -> bool {
        self.broken || self.body.copy().is_poisoned()
    }

    /// Tells the user `line`, once per document.
    fn say(&mut self, notes: &mut Vec<String>, line: String) {
        if self.said.insert(line.clone()) {
            notes.push(line);
        }
    }

    /// While a push is under way and the document is caught up: the next part of what the log lacks, once the last one
    /// is acked (so what the others push meanwhile, which comes in as their updates, isn't sent again). When the log
    /// lacks nothing, the push is done, and every unacked update the log is known to hold is had.
    ///
    /// Parts go in causal order (the changes the log lacks, by Lamport time): each applies on its own once the ones
    /// before it are in the log, so a machine catching up from the log never holds more than about a part waiting.
    /// A part is a run of changes all of this machine's own peers, or all of others'. A run of its own goes at once; a
    /// run of others' only after a random wait of 1 to [`PUSH_PATIENCE`] ticks (drawn anew before each such part, and
    /// kept in the file), and is then computed afresh: what their machines pushed meanwhile has come in as their
    /// updates and isn't sent again, so machines that come back at once after a reset don't push the same history in
    /// lockstep.
    fn push_next(&mut self, id: DocId, notes: &mut Vec<String>, random: &mut Peers) -> Vec<ClientFrame> {
        if self.sub != Sub::Live || self.full.is_some() || self.pending.iter().any(|p| p.push) {
            return Vec::new();
        }
        let Some(has) = &self.pushing else { return Vec::new() };
        let Ok(missing) = self.body.copy().missing(has) else {
            // Loro failed: the document is read back from its file ([`Replica::heal`]).
            self.broken = true;
            return Vec::new();
        };
        let part = match missing.first() {
            None => Ok(None),
            Some((peer, _, _)) => {
                let own = self.own_peers.contains(peer);
                if own {
                    // (A wait that ran down during a run of its own operations is drawn afresh for the next run of
                    // others'.)
                    self.push_wait = None;
                } else {
                    match self.push_wait {
                        None => {
                            // (Mixed, as a simulation's numbers may be close together.)
                            let mut x = random().wrapping_add(0x9e37_79b9_7f4a_7c15);
                            x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                            x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                            x ^= x >> 31;
                            // Behind someone pushing (an update came in while waiting): at least a full patience more,
                            // so a pusher whose parts come a few ticks apart isn't taken over.
                            let base = if self.queued { PUSH_PATIENCE } else { 0 };
                            self.queued = false;
                            self.push_wait = Some(base + 1 + u32::try_from(x % u64::from(PUSH_PATIENCE)).unwrap_or(0));
                            self.dirty = true;
                            return Vec::new();
                        }
                        Some(left) if left > 0 => return Vec::new(),
                        Some(_) => self.push_wait = None,
                    }
                }
                let run = missing.iter().take_while(|(p, _, _)| self.own_peers.contains(p) == own).count();
                let max = PUSH_PART.min(MAX_UPDATE_BYTES >> self.strikes);
                self.body.copy().export_prefix(&missing[..run], max, MAX_UPDATE_BYTES >> self.strikes).map(|(bytes, _)| Some(bytes))
            }
        };
        match part {
            Ok(Some(bytes)) => {
                let frames = split(&bytes, MAX_UPDATE_BYTES >> self.strikes).unwrap_or_else(|_| vec![bytes]);
                for bytes in frames {
                    let client_seq = self.next_client_seq;
                    self.next_client_seq = client_seq.saturating_add(1);
                    self.pending.push(Pending { client_seq, bytes, acked: None, push: true });
                }
                self.dirty = true;
                send_pending(id, self)
            }
            Ok(None) => {
                let has = self.pushing.take().unwrap_or_default();
                // (One acked above what is had stays: it is counted when the numbers below it come. One sent and not
                // acked yet is counted when its ack comes.)
                let (held, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
                    .into_iter()
                    .partition(|p| p.acked.is_none() && spans_of(&p.bytes).is_some_and(|ranges| held_by(&has, &ranges)));
                self.pending = keep;
                for own in held {
                    self.stored(own);
                }
                self.dirty = true;
                send_pending(id, self)
            }
            Err(UpdateError::Panicked) => {
                // Loro failed: the document is read back from its file ([`Replica::heal`]).
                self.broken = true;
                Vec::new()
            }
            Err(e) => {
                let line = self.told(
                    |app| format!("the recents of {app} can't go up to the server ({e}); they stay on this machine"),
                    |id| format!("the live document {id} can't go up to the server ({e}); it stays on this machine"),
                );
                self.say(notes, line);
                Vec::new()
            }
        }
    }

    /// From the start: the log the server has now isn't the one this machine learnt (a restore, an emptied log). The
    /// waiting updates go (the catch-up brings all of the log again), Loro's copies of them too (the document is read
    /// back from its own state), and so does a stuck state; after the catch-up, what the log lacks goes up.
    fn start_over(&mut self) {
        self.have = Have::default();
        self.pushing = Some(Spans::new());
        self.push_wait = None;
        self.waiting.clear();
        // (The catch-up brings every row again, fragments too.)
        self.fragments.clear();
        self.stuck = false;
        if let Ok(snapshot) = self.body.copy().snapshot()
            && let Ok(fresh) = Body::load(&self.kind, &snapshot, self.body.copy().peer())
        {
            self.body = fresh;
            for update in wholes(&self.pending) {
                let _ = self.body.import_any_size(&update);
            }
        }
        self.forget_acks();
        // The log is learnt again from the start.
        self.log = LogVersion::anchored(0, Version::new());
        self.forget_seen();
        self.batch.clear();
        self.batched = 0;
        self.compact_ask = None;
        self.dirty = true;
        self.urgent = true;
    }

    /// Acks of numbers not reached are forgotten: their updates go again (the server acks one it has).
    fn forget_acks(&mut self) {
        for pending in &mut self.pending {
            pending.acked = None;
        }
        self.taken_back.clear();
        self.back_acked.clear();
    }

    /// `subscribe` from what the document has, without `create` (the server has it: on the same connection); a
    /// document whose server snapshot didn't read here follows the whole log instead.
    fn resubscribe(&mut self, id: DocId) -> Vec<ClientFrame> {
        if self.stopped {
            self.sub = Sub::No;
            return Vec::new();
        }
        self.sub = Sub::CatchingUp;
        if self.log_only {
            return vec![ClientFrame::SubscribeLog { doc: id, after: self.have.upto, at: self.at() }];
        }
        vec![ClientFrame::Subscribe { doc: id, after: self.have.upto, at: self.at() }]
    }

    /// The server's snapshot of the document, at the start of a catch-up from below it: imported (Loro merges it with
    /// what is here), and the log is had up to its base. One that doesn't read here (or that Loro fails on: the
    /// document is read back from its file) is given up on: this machine follows the whole log instead
    /// (`subscribe_log`), from where it was. `true` when it was taken.
    fn take_snapshot(&mut self, base: u64, bytes: &[u8], notes: &mut Vec<String>, changed: &mut bool) -> Result<(), UpdateError> {
        let had_history = self.body.copy().has_history();
        match self.body.import_any_size(bytes) {
            Ok(Imported::Applied { changed: list } | Imported::Waiting { changed: list, .. }) => *changed |= list,
            Err(e) => {
                let line = self.told(
                    |app| format!("the server's snapshot of the recents of {app} doesn't read here ({e}); this machine reads the whole log instead"),
                    |id| format!("the server's snapshot of the live document {id} doesn't read here ({e}); this machine reads the whole log instead"),
                );
                self.say(notes, line);
                self.log_only = true;
                self.stale = true;
                self.dirty = true;
                self.urgent = true;
                return Err(e);
            }
        }
        if !had_history && self.rewait(changed).is_err() {
            // (Read back from its file by the caller.)
            return Err(UpdateError::Panicked);
        }
        self.waiting.retain(|w| !self.body.copy().covers(&w.needs));
        self.on_server(bytes);
        let version = version_of(bytes).unwrap_or_default();
        if base >= self.have.upto {
            self.have = Have { upto: base, upto_hash: if base == self.have.upto { self.have.upto_hash } else { None } };
            self.log = LogVersion::anchored(base, version.clone());
            // The rows it holds are in the copy now (Loro merged them), one this machine dropped too.
            if self.dropped.is_some_and(|d| d <= base) {
                self.dropped = None;
            }
        }
        // What the server holds up to its base: this machine's unacked updates in it are stored.
        let ranges: Vec<(u64, i32, i32)> = version.iter().map(|(peer, end)| (*peer, 0, *end)).collect();
        self.saw(&ranges);
        self.settle_pending();
        self.from_snapshot = true;
        self.dirty = true;
        Ok(())
    }

    /// The end of a catch-up that started with the server's snapshot, with updates still waiting for their history:
    /// the snapshot may lack some of what the rows below its base held (a device made it wrong). The whole log is
    /// read again from the start, with the import (`subscribe_log`, kept for this document from now on); Loro takes what
    /// it has already as no news. Frames to send, if it does.
    fn check_snapshot(&mut self, id: DocId, notes: &mut Vec<String>) -> Vec<ClientFrame> {
        if !std::mem::take(&mut self.from_snapshot) || self.waiting.is_empty() || self.log_only {
            return Vec::new();
        }
        let line = self.told(
            |app| format!("updates of the recents of {app} wait for history the server's snapshot should hold; this machine reads the whole log"),
            |id| format!("updates of the live document {id} wait for history the server's snapshot should hold; this machine reads the whole log"),
        );
        self.say(notes, line);
        self.log_only = true;
        self.have = Have::default();
        self.log = LogVersion::anchored(0, Version::new());
        self.forget_seen();
        self.fragments.clear();
        self.compact_ask = None;
        self.dirty = true;
        self.urgent = true;
        self.resubscribe(id)
    }

    fn at(&self) -> Option<[u8; 32]> {
        if self.have.upto > 0 { self.have.upto_hash } else { None }
    }
}

/// What the link to the server is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Down,
    /// `hello` sent, waiting for `welcome`.
    Hello,
    Up,
}

/// Why the replica wants the connection to end.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// Something trying again later may fix: reconnect with backoff.
    #[error("{0}")]
    Retry(String),
    /// The server speaks another live protocol: trying again only helps after an update.
    #[error("{0}")]
    Mismatch(String),
}

/// Why an add was refused.
#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0}")]
    Invalid(String),
    #[error("the recents couldn't be written to disk: {0}")]
    Io(#[from] std::io::Error),
}

/// One account's live documents on this machine.
pub struct Replica<S: DocStore> {
    store: S,
    space: SpaceId,
    docs: BTreeMap<DocId, Doc>,
    link: Link,
    /// The epoch the server named in `welcome`, while the link is up.
    epoch: Option<Epoch>,
    /// Apps whose list changed since [`Replica::take_changed`].
    changed: BTreeSet<AppName>,
    /// Things to tell the user (bad updates, refusals), since [`Replica::take_notes`].
    notes: Vec<String>,
    peers: Peers,
    /// Heals per document in this run ([`Replica::heal`]).
    heals: BTreeMap<DocId, u32>,
    /// Apps whose push stopped ([`Replica::push_stopped`]).
    push_stopped: BTreeSet<AppName>,
    /// Rows of each document Loro failed on in this run ([`Replica::recover_doc`]).
    failures: BTreeMap<DocId, u32>,
    /// Documents whose catch-ups are taken one row at a time in this run (Loro failed on a batch of them).
    one_by_one: BTreeSet<DocId>,
    /// Documents not followed for the rest of this run: Loro failed on a second row ([`Replica::is_stopped`]).
    stopped: BTreeSet<DocId>,
    /// Documents of files on disk and not loaded: nothing to send, nothing waiting, no app holding them
    /// ([`Replica::load`], [`Replica::unload`]).
    unloaded: BTreeSet<DocId>,
    /// Snapshots catch-ups started with, to download ([`Replica::take_fetches`]).
    fetches: Vec<(DocId, ManifestId)>,
    /// Documents let go of without dropping ([`Replica::forgotten`]).
    forgotten: usize,
    /// The live protocol `welcome` named (0 while the link isn't up).
    protocol: u32,
    /// What happened to documents of files since [`Replica::take_events`].
    events: Vec<DocEvent>,
    /// Counts opens, for which document was used least recently.
    uses: u64,
}

impl<S: DocStore> Drop for Replica<S> {
    /// A document Loro failed in is let go of without dropping (dropping its poisoned lock would panic again).
    fn drop(&mut self) {
        for (_, doc) in std::mem::take(&mut self.docs) {
            if doc.is_broken() {
                std::mem::forget(doc);
            }
        }
    }
}

impl<S: DocStore> Replica<S> {
    /// Loads the account's documents from `store`, each editing as a peer from `peers`. A file that doesn't read is set
    /// aside in a note (its document starts empty and is filled from the server); one of another space is ignored.
    pub fn open(mut store: S, space: SpaceId, mut peers: Peers) -> std::io::Result<Self> {
        let files = store.load()?;
        let mut notes = Vec::new();
        let mut docs = BTreeMap::new();
        for (name, bytes) in files {
            if let Some((id, doc)) = load_doc(&mut store, space, &name, &bytes, peers(), None, &mut notes) {
                docs.insert(id, doc);
            }
        }
        // Documents of files are loaded only when they have something to send or that waits; the rest stay on disk
        // until an app opens one ([`Replica::open_document`]).
        let mut unloaded = BTreeSet::new();
        for name in store.document_names()? {
            let Some(bytes) = store.load_document(&name)? else { continue };
            if let Some(id) = idle_on_disk(&store, space, &name, &bytes) {
                unloaded.insert(id);
                continue;
            }
            if let Some((id, mut doc)) = load_document(&mut store, space, &name, &bytes, peers(), None, &mut notes) {
                // Followed again when an app opens it, or now if it has something to send.
                doc.parked = doc.pending.is_empty() && doc.pushing.is_none();
                docs.insert(id, doc);
            }
        }
        Ok(Self {
            store,
            space,
            docs,
            link: Link::Down,
            epoch: None,
            changed: BTreeSet::new(),
            notes,
            peers,
            heals: BTreeMap::new(),
            push_stopped: BTreeSet::new(),
            failures: BTreeMap::new(),
            one_by_one: BTreeSet::new(),
            stopped: BTreeSet::new(),
            unloaded,
            fetches: Vec::new(),
            forgotten: 0,
            protocol: 0,
            events: Vec::new(),
            uses: 0,
        })
    }

    /// Loads document `id` of a file from disk if it is there and not loaded. Whether it is loaded now.
    fn load(&mut self, id: DocId) -> bool {
        if self.docs.contains_key(&id) {
            return true;
        }
        if !self.unloaded.remove(&id) {
            return false;
        }
        let name = id.to_string();
        let Ok(Some(bytes)) = self.store.load_document(&name) else { return false };
        let peer = (self.peers)();
        match load_document(&mut self.store, self.space, &name, &bytes, peer, None, &mut self.notes) {
            Some((loaded, mut doc)) if loaded == id => {
                doc.parked = true;
                doc.stopped = self.stopped.contains(&id);
                self.docs.insert(id, doc);
                true
            }
            _ => false,
        }
    }

    /// Past [`MAX_LOADED_DOCUMENTS`] documents of files in memory: the least recently used ones with nothing unacked or
    /// waiting, no push under way and no app holding them are written (if changed) and let go of (unfollowed first);
    /// they stay on disk, and are loaded again when opened. One that can't be written stays. Frames to send.
    fn unload(&mut self) -> Vec<ClientFrame> {
        let loaded = self.loaded_documents();
        let Some(extra) = loaded.checked_sub(MAX_LOADED_DOCUMENTS).filter(|n| *n > 0) else { return Vec::new() };
        let mut idle: Vec<(u64, DocId)> = self
            .docs
            .iter()
            .filter(|(_, d)| {
                d.kind.is_document()
                    && d.joined
                    && d.open == 0
                    && d.pending.is_empty()
                    && d.waiting.is_empty()
                    && d.fragments.is_empty()
                    && d.pushing.is_none()
                    && !d.is_broken()
                    && d.batch.is_empty()
            })
            .map(|(id, d)| (d.last_used, *id))
            .collect();
        idle.sort_unstable();
        let mut frames = Vec::new();
        let up = self.link == Link::Up;
        for (_, id) in idle.into_iter().take(extra) {
            let Some(doc) = self.docs.get_mut(&id) else { continue };
            if (doc.dirty || !doc.on_disk) && save(&mut self.store, self.space, id, doc).is_err() {
                continue;
            }
            if doc.sub != Sub::No && up {
                frames.push(ClientFrame::Unsubscribe { doc: id });
            }
            self.docs.remove(&id);
            self.unloaded.insert(id);
        }
        frames
    }

    fn doc_id(&self, app: &AppName) -> DocId {
        recents_doc(self.space, app)
    }

    /// Whether `app` has a document here.
    pub fn has(&self, app: &AppName) -> bool {
        self.docs.contains_key(&self.doc_id(app))
    }

    /// `app`'s list, newest first.
    pub fn entries(&self, app: &AppName) -> Vec<RecentEntry> {
        self.docs.get(&self.doc_id(app)).map(|d| d.body.view()).unwrap_or_default()
    }

    /// The newest entry for a file, if the list shows it (to keep its `saved` time when it is only opened).
    pub fn current(&self, app: &AppName, entry: &RecentEntry) -> Option<RecentEntry> {
        self.docs.get(&self.doc_id(app)).and_then(|d| match &d.body {
            Body::Recents(recents) => recents.current(entry.node, &entry.path),
            Body::Document(_) => None,
        })
    }

    /// An app follows its list: its document is opened (and, with the link up, subscribed). Frames to send.
    pub fn follow(&mut self, app: &AppName) -> Vec<ClientFrame> {
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let (id, doc) = doc_in(&mut self.docs, self.space, app, &mut self.peers);
        if up && doc.sub == Sub::No && doc.retry.is_none() { subscribe(id, doc, epoch) } else { Vec::new() }
    }

    /// Adds `entry` to `app`'s list under the new key `key`: written to disk before this returns, sent now if the link
    /// is up. Returns the frames to send. If the write fails, the document is as its file had it, and nothing is sent.
    pub fn add(&mut self, app: &AppName, key: AddKey, entry: RecentEntry) -> Result<Vec<ClientFrame>, AddError> {
        let (space, up, epoch) = (self.space, self.link == Link::Up, self.epoch);
        let (id, doc) = doc_in(&mut self.docs, space, app, &mut self.peers);
        let Body::Recents(recents) = &doc.body else { return Err(AddError::Invalid("not a recents document".into())) };
        let before = recents.view();
        let bytes = recents.add(key, &entry).map_err(|e| AddError::Invalid(e.to_string()))?;
        let client_seq = doc.next_client_seq;
        doc.next_client_seq = client_seq.saturating_add(1);
        doc.pending.push(Pending { client_seq, bytes: bytes.clone(), acked: None, push: false });
        let changed = doc.body.view() != before;
        // On disk before the app hears it is kept.
        if let Err(e) = save(&mut self.store, space, id, doc) {
            self.reload(id);
            return Err(e.into());
        }
        let doc_app = doc.app().clone();
        let frames = match doc.sub {
            // (While a push is under way, its parts carry the add.)
            Sub::Live if doc.full.is_none() && doc.pushing.is_none() => {
                doc.sent.insert(client_seq);
                vec![ClientFrame::Update { doc: id, client_seq, bytes }]
            }
            // A document new since the link came up: subscribed now, and the add goes once it is caught up.
            Sub::No if up && doc.retry.is_none() => subscribe(id, doc, epoch),
            Sub::No | Sub::CatchingUp | Sub::Live => Vec::new(),
        };
        if changed {
            self.changed.insert(doc_app);
        }
        Ok(frames)
    }

    /// Puts a document back as its file has it, or drops it if it has no file yet (or the file doesn't read).
    fn reload(&mut self, id: DocId) {
        self.reload_without(id, None);
    }

    /// [`Replica::reload`], leaving out the waiting update with hash `drop` (Loro failed on it).
    fn reload_without(&mut self, id: DocId, drop: Option<[u8; 32]>) {
        // The document in memory may be one Loro failed in: its lock is poisoned, and dropping it would panic again.
        // It is let go of without dropping (a small leak, after a rare failure).
        if let Some(old) = self.docs.remove(&id)
            && old.is_broken()
        {
            std::mem::forget(old);
            self.forgotten += 1;
        }
        let name = id.to_string();
        let peer = (self.peers)();
        let document = self.store.load_document(&name).ok().flatten();
        let reloaded = match document {
            Some(bytes) => load_document(&mut self.store, self.space, &name, &bytes, peer, drop, &mut self.notes),
            None => {
                let stored = self.store.load().ok().and_then(|files| files.get(&name).cloned());
                stored.and_then(|bytes| load_doc(&mut self.store, self.space, &name, &bytes, peer, drop, &mut self.notes))
            }
        };
        match reloaded {
            Some((_, mut doc)) => {
                doc.dirty |= drop.is_some();
                doc.stopped = self.stopped.contains(&id);
                self.docs.insert(id, doc);
            }
            None => {
                self.docs.remove(&id);
            }
        }
    }

    /// Loro failed on update `seq` (`hash`) of `id`: the document is read back from its file (nothing half-changed
    /// stays), and caught up again from there, with that update counted as had and not imported.
    fn recover_doc(&mut self, id: DocId, kind: Kind, seq: u64, hash: [u8; 32]) -> Vec<ClientFrame> {
        if self.failed_again(id) {
            return self.stop(id, &kind, None);
        }
        self.reload(id);
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let Some(doc) = doc_again(&mut self.docs, self.space, &mut self.peers, id, &kind) else { return Vec::new() };
        doc.skip = Some((seq, hash));
        let line = doc.told(
            |app| format!("a recents update for {app} from the server made Loro fail; it was skipped, and this machine leaves compacting them to other devices"),
            |id| format!("an update of the live document {id} from the server made Loro fail; it was skipped, and this machine leaves compacting it to other devices"),
        );
        let frames = if up { subscribe(id, doc, epoch) } else { Vec::new() };
        doc.say(&mut self.notes, line);
        if let Kind::Recents(app) = kind {
            self.changed.insert(app);
        }
        frames
    }

    /// Loro failed on a waiting update of `id` imported again (`hash`): that one is dropped (the incoming update is
    /// not to blame, and comes again), and the document read back from its file and caught up from there.
    fn recover_waiting_doc(&mut self, id: DocId, kind: Kind, hash: [u8; 32]) -> Vec<ClientFrame> {
        if self.failed_again(id) {
            return self.stop(id, &kind, Some(hash));
        }
        self.reload_without(id, Some(hash));
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let Some(doc) = doc_again(&mut self.docs, self.space, &mut self.peers, id, &kind) else { return Vec::new() };
        let line = doc.told(
            |app| format!("a waiting recents update for {app} made Loro fail; it was dropped"),
            |id| format!("a waiting update of the live document {id} made Loro fail; it was dropped"),
        );
        let frames = if up { subscribe(id, doc, epoch) } else { Vec::new() };
        doc.say(&mut self.notes, line);
        if let Kind::Recents(app) = kind {
            self.changed.insert(app);
        }
        frames
    }

    /// Counts a row Loro failed on in `id`; whether it is the second in this run.
    fn failed_again(&mut self, id: DocId) -> bool {
        let failures = self.failures.entry(id).or_insert(0);
        *failures += 1;
        *failures >= MAX_HEALS
    }

    /// Loro failed on a second row of `id` in this run: read back from its file and not followed again until the agent
    /// restarts (another catch-up would go round the same failures). Its local changes are kept (on disk, and sent
    /// after the restart); its apps are told ([`Replica::is_stopped`]). `drop`: a waiting update to leave out.
    fn stop(&mut self, id: DocId, kind: &Kind, drop: Option<[u8; 32]>) -> Vec<ClientFrame> {
        self.stopped.insert(id);
        self.reload_without(id, drop);
        let mut frames = Vec::new();
        let Some(doc) = doc_again(&mut self.docs, self.space, &mut self.peers, id, kind) else { return frames };
        doc.stopped = true;
        if doc.sub != Sub::No && self.link == Link::Up {
            frames.push(ClientFrame::Unsubscribe { doc: id });
        }
        doc.sub = Sub::No;
        doc.retry = None;
        let line = doc.told(
            |app| format!("Loro failed on the recents of {app} again; this machine stops following them until the agent restarts (what was added here is kept)"),
            |id| format!("Loro failed on the live document {id} again; this machine stops following it until the agent restarts (the changes made here are kept)"),
        );
        doc.say(&mut self.notes, line);
        if let Kind::Recents(app) = kind {
            self.changed.insert(app.clone());
        }
        frames
    }

    /// [`Replica::recover_doc`] of `app`'s recents (as the tests call it).
    #[cfg(test)]
    fn recover(&mut self, id: DocId, app: AppName, seq: u64, hash: [u8; 32]) -> Vec<ClientFrame> {
        self.recover_doc(id, Kind::Recents(app), seq, hash)
    }

    /// [`Replica::recover_waiting_doc`] of `app`'s recents (as the tests call it).
    #[cfg(test)]
    fn recover_waiting(&mut self, id: DocId, app: AppName, hash: [u8; 32]) -> Vec<ClientFrame> {
        self.recover_waiting_doc(id, Kind::Recents(app), hash)
    }

    /// A connection to the server is open: the first frame to send.
    pub fn connected(&mut self) -> Vec<ClientFrame> {
        self.link = Link::Hello;
        vec![ClientFrame::Hello { min: LIVE_PROTOCOL_MIN, max: LIVE_PROTOCOL_MAX }]
    }

    /// The connection ended: everything is subscribed and sent again on the next one. A document of a file still being
    /// joined is given up on (it was never live here): the apps that wait for it open the file as a plain file.
    pub fn disconnected(&mut self) {
        self.link = Link::Down;
        self.epoch = None;
        self.protocol = 0;
        let joining: Vec<DocId> = self.docs.iter().filter(|(_, d)| !d.joined).map(|(id, _)| *id).collect();
        for id in joining {
            self.docs.remove(&id);
            self.events.push(DocEvent::NotLive(id, NotLiveReason::Offline, "the connection to the server went".into()));
        }
        for doc in self.docs.values_mut() {
            doc.sub = Sub::No;
            doc.sent.clear();
            doc.taken_back.clear();
            doc.back_acked.clear();
            doc.retry = None;
            doc.create_refused = false;
            doc.full = None;
            doc.stuck = false;
            doc.creating = false;
            doc.compact_ask = None;
            doc.stale = false;
            doc.from_snapshot = false;
            // (The server sends them again with the next catch-up.)
            doc.batch.clear();
            doc.batched = 0;
            doc.fetching = None;
            doc.deferred_head = None;
            // (The frames that would have named them are gone with the connection.)
            doc.uploads.clear();
        }
        self.fetches.clear();
    }

    /// Whether the server speaks documents of files: `None` while the link isn't up.
    pub fn documents_supported(&self) -> Option<bool> {
        (self.link == Link::Up).then_some(self.protocol >= LIVE_DOCUMENTS_PROTOCOL)
    }

    /// Opens the document of the file node `node` for `app`: one live here already is followed again (if it was put
    /// aside) and opens from its copy, online or not; one that isn't is joined, which needs the server. Frames to send.
    pub fn open_document(&mut self, node: NodeId, app: &AppName) -> (DocId, Opening, Vec<ClientFrame>) {
        let id = node_doc(self.space, node);
        self.uses += 1;
        let (up, epoch, uses) = (self.link == Link::Up, self.epoch, self.uses);
        let documents = self.protocol >= LIVE_DOCUMENTS_PROTOCOL;
        // One on disk is loaded now.
        self.load(id);
        if let Some(doc) = self.docs.get_mut(&id) {
            doc.last_used = uses;
            if !doc.joined {
                return (id, Opening::Joining, Vec::new());
            }
            let was_parked = std::mem::replace(&mut doc.parked, false);
            // Room first (an unsubscribe before the subscribe that needs its slot).
            let mut frames = self.unload();
            if was_parked {
                frames.extend(self.evict());
            }
            let Some(doc) = self.docs.get_mut(&id) else { return (id, Opening::Live, frames) };
            if up && documents && doc.sub == Sub::No && doc.retry.is_none() {
                frames.extend(subscribe(id, doc, epoch));
            }
            return (id, Opening::Live, frames);
        }
        if !up {
            return (id, Opening::NotLive(NotLiveReason::Offline, "the agent can't reach the server".into()), Vec::new());
        }
        if !documents {
            return (id, Opening::NotLive(NotLiveReason::Offline, "live documents need a newer Craft Cloud server".into()), Vec::new());
        }
        let peer = (self.peers)();
        let kind = Kind::Document { node, app: app.clone() };
        let mut doc = Doc::with(id, kind.clone(), Body::new(&kind, peer), Have::default(), Vec::new(), Vec::new(), 1, epoch, None);
        doc.joined = false;
        doc.join = Some(Join::Probing);
        doc.last_used = uses;
        doc.log = LogVersion::anchored(0, Version::new());
        let subscribe = doc.resubscribe(id);
        self.docs.insert(id, doc);
        // Room first (an unsubscribe before the subscribe that needs its slot).
        let mut frames = self.unload();
        frames.extend(self.evict());
        frames.extend(subscribe);
        (id, Opening::Joining, frames)
    }

    /// The app's import of a file whose document the server doesn't have (after [`DocEvent::NeedsImport`]): a Loro
    /// snapshot, checked here, sent up as `create`. Frames to send, or why it was refused (the file then opens as a
    /// plain file).
    pub fn import_document(&mut self, id: DocId, snapshot: &[u8]) -> Result<Vec<ClientFrame>, (NotLiveReason, String)> {
        let Some(doc) = self.docs.get_mut(&id) else { return Err((NotLiveReason::Offline, "the document isn't being opened any more".into())) };
        if doc.join != Some(Join::NeedsImport) {
            return Err((NotLiveReason::Offline, "no import is waited for".into()));
        }
        let Kind::Document { node, app } = doc.kind.clone() else { return Err((NotLiveReason::NotInFolder, "not a document of a file".into())) };
        if snapshot.is_empty() || snapshot.len() > MAX_SNAPSHOT_BYTES {
            self.docs.remove(&id);
            return Err((NotLiveReason::TooLarge, format!("an import is 1 byte to {MAX_SNAPSHOT_BYTES} bytes")));
        }
        // Checked by Loro before it goes anywhere: an import that doesn't read is never made a document.
        let peer = (self.peers)();
        match LiveCopy::load(snapshot, peer) {
            Ok(copy) => {
                // (A copy Loro failed in is let go of without dropping.)
                if copy.is_poisoned() {
                    std::mem::forget(copy);
                }
            }
            Err(e) => {
                self.docs.remove(&id);
                return Err((NotLiveReason::Offline, format!("the import doesn't read: {e}")));
            }
        }
        if self.link != Link::Up {
            self.docs.remove(&id);
            return Err((NotLiveReason::Offline, "the connection to the server went".into()));
        }
        // Uploaded first (through the chunk API), then named in `create`.
        let upload = match cut(id, snapshot) {
            Ok(upload) => upload,
            Err(e) => {
                self.docs.remove(&id);
                return Err((NotLiveReason::TooLarge, e));
            }
        };
        let manifest = upload.id();
        doc.uploads.push(upload);
        doc.join = Some(Join::Creating(snapshot.to_vec()));
        Ok(vec![ClientFrame::CreateDocument { doc: id, node, app, snapshot: manifest }])
    }

    /// The snapshots to upload now, each before the frames of its document that follow it (a frame names one by its
    /// manifest). One that can't be uploaded is said with [`Replica::upload_failed`].
    pub fn take_uploads(&mut self) -> Vec<Upload> {
        self.docs.values_mut().flat_map(|d| std::mem::take(&mut d.uploads)).collect()
    }

    /// A snapshot of `doc` couldn't be uploaded (the frame that names it isn't sent): an import's join is given up on
    /// (the app opens the file as it is), a joined document's making again is tried later, and a compaction's answer
    /// is just not given (another device is asked).
    pub fn upload_failed(&mut self, id: DocId, why: &str) {
        let Some(doc) = self.docs.get_mut(&id) else { return };
        if matches!(doc.join, Some(Join::Creating(_))) {
            self.docs.remove(&id);
            self.events.push(DocEvent::NotLive(id, NotLiveReason::Offline, format!("the import couldn't go up to the server: {why}")));
        } else if doc.creating {
            doc.creating = false;
            doc.sub = Sub::No;
            let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
            doc.retry = Some((pause, pause));
        }
    }

    /// The snapshots of catch-ups to download now, by document and manifest; each is handed back with
    /// [`Replica::on_snapshot`].
    pub fn take_fetches(&mut self) -> Vec<(DocId, ManifestId)> {
        std::mem::take(&mut self.fetches)
    }

    /// An app that was to import a document is gone (it closed its connection mid-import): the join is given up on;
    /// nothing was sent to the server.
    pub fn abandon_join(&mut self, id: DocId) -> Vec<ClientFrame> {
        match self.docs.get(&id) {
            Some(doc) if !doc.joined && matches!(doc.join, Some(Join::NeedsImport | Join::Probing)) => {
                let following = doc.sub != Sub::No;
                self.docs.remove(&id);
                if following { vec![ClientFrame::Unsubscribe { doc: id }] } else { Vec::new() }
            }
            _ => Vec::new(),
        }
    }

    /// A document of a file live here: its state for an app opening it, a Loro snapshot, and the updates to import after
    /// it: the server's still waiting for their history, and this machine's own not yet acked (in the snapshot already,
    /// unless they wait for history too: after a restart, the server's updates they build on may not be on disk yet,
    /// and Loro applies them only once those come again; the app's Loro does the same).
    pub fn document_state(&self, id: DocId) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
        let doc = self.docs.get(&id).filter(|d| d.joined && d.kind.is_document())?;
        let snapshot = doc.body.copy().snapshot().ok()?;
        // (Whole: an unacked change in fragments goes as the change.)
        let after = doc.waiting.iter().map(|w| w.bytes.clone()).chain(wholes(&doc.pending)).collect();
        Some((snapshot, after))
    }

    /// Whether this machine's copy of a document of a file holds every operation `ids` names (a save's frontiers).
    pub fn document_holds(&self, id: DocId, ids: &[(u64, u32)]) -> bool {
        self.docs.get(&id).filter(|d| d.kind.is_document()).is_some_and(|d| d.body.copy().holds(ids))
    }

    /// Whether a document of a file, as frontiers `a` name it, holds every operation of frontiers `b`; `None` when this
    /// machine's copy can't tell (it lacks their operations, or isn't loaded).
    pub fn document_includes(&self, id: DocId, a: &[(u64, u32)], b: &[(u64, u32)]) -> Option<bool> {
        self.docs.get(&id).filter(|d| d.kind.is_document())?.body.copy().includes(a, b)
    }

    /// A document of a file whose saves are asked about: loaded from disk if it isn't, and followed again if it was
    /// put aside, within the same caps as an open (the least recently used make room), so its copy can tell which of
    /// two saves is newer once the server's operations are in. Frames to send.
    pub fn consult_document(&mut self, id: DocId) -> Vec<ClientFrame> {
        self.uses += 1;
        let (up, epoch, uses) = (self.link == Link::Up, self.epoch, self.uses);
        let documents = self.protocol >= LIVE_DOCUMENTS_PROTOCOL;
        self.load(id);
        let Some(doc) = self.docs.get_mut(&id).filter(|d| d.joined && d.kind.is_document()) else { return Vec::new() };
        doc.last_used = uses;
        let was_parked = std::mem::replace(&mut doc.parked, false);
        let mut frames = self.unload();
        if was_parked {
            frames.extend(self.evict());
        }
        let Some(doc) = self.docs.get_mut(&id) else { return frames };
        if up && documents && doc.sub == Sub::No && doc.retry.is_none() {
            frames.extend(subscribe(id, doc, epoch));
        }
        frames
    }

    /// Whether a document being joined waits for an app's import (the server doesn't have it).
    pub fn needs_import(&self, id: DocId) -> bool {
        self.docs.get(&id).is_some_and(|d| d.join == Some(Join::NeedsImport))
    }

    /// Whether a document of a file is live here (it opens offline).
    pub fn has_document(&self, id: DocId) -> bool {
        self.unloaded.contains(&id) || self.docs.get(&id).is_some_and(|d| d.joined && d.kind.is_document())
    }

    /// The node a document of a file belongs to.
    pub fn document_node(&self, id: DocId) -> Option<NodeId> {
        match self.docs.get(&id).map(|d| &d.kind) {
            Some(Kind::Document { node, .. }) => Some(*node),
            _ => None,
        }
    }

    /// How a document stands: whether its edits go out as they are made, and how many of this machine's wait for the
    /// server's ack.
    pub fn document_status(&self, id: DocId) -> Option<(bool, u64)> {
        let doc = self.docs.get(&id)?;
        let online = self.link == Link::Up && doc.sub == Sub::Live && doc.full.is_none();
        Some((online, (doc.pending.len() + usize::from(doc.pushing.is_some())) as u64))
    }

    /// An app has (`true`) or no longer has (`false`) the document open: one no app has open may be unfollowed past
    /// the device's cap.
    pub fn set_open(&mut self, id: DocId, open: bool) {
        if let Some(doc) = self.docs.get_mut(&id) {
            doc.open = if open { doc.open.saturating_add(1) } else { doc.open.saturating_sub(1) };
        }
    }

    /// An app's own change to a document of a file: imported into the copy with Loro's checked import (refused, it
    /// leaves the copy as it was; Loro failing reads the document back from disk), on disk in its journal before this
    /// returns, and sent when the link is up. Returns the frames to send.
    pub fn local_update(&mut self, id: DocId, bytes: &[u8]) -> Result<Vec<ClientFrame>, LocalError> {
        let Some(doc) = self.docs.get_mut(&id).filter(|d| d.joined && d.kind.is_document()) else {
            // (After a disk error the copy may be being read back: for now, worth sending again.)
            return Err(LocalError::Io(std::io::Error::other("the document isn't open here (being read back from disk?)")));
        };
        // (An update over the frame limit came from the app in fragments, joined by its connection: it goes into the
        // journal whole, and up to the server as fragments again.)
        if bytes.is_empty() || bytes.len() > MAX_WHOLE_BYTES {
            return Err(LocalError::Invalid(format!("an update is 1 to {MAX_WHOLE_BYTES} bytes")));
        }
        // Its journal needs its file beside it (a journal alone isn't loaded): a document joined since it was last
        // written is written first (as it is, before this update). So is one whose journal failed: written whole, its
        // journal starts empty.
        if !doc.on_disk || doc.journal_failed {
            let space = self.space;
            save(&mut self.store, space, id, doc)?;
            doc.journal_failed = false;
        }
        // Fragments of it: what goes up (each an update of the log), and its numbers.
        let frames = split(bytes, MAX_UPDATE_BYTES).map_err(|e| LocalError::Invalid(e.to_string()))?;
        match import_whole(&doc.body, bytes) {
            Ok(_) => {}
            Err(UpdateError::Panicked) => {
                self.reload(id);
                self.follow_again_soon(id);
                return Err(LocalError::Invalid("Loro failed on the update; it was dropped and the document read back from disk".into()));
            }
            Err(e) => return Err(LocalError::Invalid(e.to_string())),
        }
        let client_seq = doc.next_client_seq;
        let before = doc.journal;
        let record = JournalRecord { client_seq, bytes: bytes.to_vec() };
        let name = id.to_string();
        let written = journal_record(&record).and_then(|rec| self.store.append_journal(&name, &rec).map(|()| rec.len()));
        let Some(doc) = self.docs.get_mut(&id) else { return Err(LocalError::Invalid("the document is gone".into())) };
        match written {
            Ok(len) => doc.journal = doc.journal.saturating_add(len),
            Err(e) => {
                // Not kept: the journal is cut back to where it was (whatever part of the record got there must not
                // come back after a crash, nor hide what comes after it), the copy goes back to what is on disk, the
                // app is told, and the document is written whole before its journal takes another change.
                let _ = self.store.truncate_journal(&name, before);
                self.reload(id);
                if let Some(doc) = self.docs.get_mut(&id) {
                    doc.journal_failed = true;
                    doc.urgent = true;
                }
                self.follow_again_soon(id);
                return Err(e.into());
            }
        }
        // (The journal's record is the whole update under its first number; its fragments take the numbers after.)
        let seqs = client_seq..client_seq.saturating_add(frames.len() as u64);
        doc.next_client_seq = seqs.end;
        for (seq, frame) in seqs.clone().zip(&frames) {
            doc.pending.push(Pending { client_seq: seq, bytes: frame.clone(), acked: None, push: false });
        }
        doc.own(bytes);
        doc.dirty = true;
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        Ok(match doc.sub {
            // (During a push too: an app's change goes at once, not behind the push's parts. Its operations count as
            // in the log for the push once it is acked.)
            Sub::Live if doc.full.is_none() => seqs
                .zip(frames)
                .map(|(client_seq, bytes)| {
                    doc.sent.insert(client_seq);
                    ClientFrame::Update { doc: id, client_seq, bytes }
                })
                .collect(),
            Sub::No if up && self.protocol >= LIVE_DOCUMENTS_PROTOCOL && doc.retry.is_none() => {
                doc.parked = false;
                subscribe(id, doc, epoch)
            }
            Sub::No | Sub::CatchingUp | Sub::Live => Vec::new(),
        })
    }

    /// A document read back from its file (so unfollowed) while the link is up: followed again at the next tick.
    fn follow_again_soon(&mut self, id: DocId) {
        let up = self.link == Link::Up && self.protocol >= LIVE_DOCUMENTS_PROTOCOL;
        if let Some(doc) = self.docs.get_mut(&id)
            && up
            && doc.sub == Sub::No
        {
            doc.parked = false;
            doc.retry = Some((1, 0));
        }
    }

    /// An app's presence in a document of a file, for the document's other machines (only while it is followed).
    pub fn presence(&mut self, id: DocId, bytes: &[u8]) -> Vec<ClientFrame> {
        match self.docs.get(&id) {
            Some(doc) if self.link == Link::Up && self.protocol >= LIVE_DOCUMENTS_PROTOCOL && doc.sub == Sub::Live && doc.joined => {
                vec![ClientFrame::Presence { doc: id, bytes: bytes.to_vec() }]
            }
            _ => Vec::new(),
        }
    }

    /// What happened to documents of files since the last call.
    pub fn take_events(&mut self) -> Vec<DocEvent> {
        std::mem::take(&mut self.events)
    }

    /// Past the device's cap on followed documents: the least recently used ones no app has open and with nothing to
    /// send are unfollowed (they stay on disk). Frames to send.
    fn evict(&mut self) -> Vec<ClientFrame> {
        let followed = self.docs.values().filter(|d| !d.parked).count();
        let Some(extra) = followed.checked_sub(MAX_SUBSCRIPTIONS).filter(|n| *n > 0) else { return Vec::new() };
        let mut idle: Vec<(u64, DocId)> = self
            .docs
            .iter()
            .filter(|(_, d)| {
                d.kind.is_document() && d.joined && !d.parked && d.open == 0 && d.pending.is_empty() && d.pushing.is_none() && d.fragments.is_empty()
            })
            .map(|(id, d)| (d.last_used, *id))
            .collect();
        idle.sort_unstable();
        let mut frames = Vec::new();
        for (_, id) in idle.into_iter().take(extra) {
            if let Some(doc) = self.docs.get_mut(&id) {
                if doc.sub != Sub::No {
                    frames.push(ClientFrame::Unsubscribe { doc: id });
                }
                doc.sub = Sub::No;
                doc.retry = None;
                doc.parked = true;
            }
        }
        frames
    }

    /// How many documents of files are loaded (in memory), for tests and the status.
    pub fn loaded_documents(&self) -> usize {
        self.docs.values().filter(|d| d.kind.is_document()).count()
    }

    /// Whether Loro failed on the document twice in this run: it isn't followed again until the agent restarts.
    pub fn is_stopped(&self, id: DocId) -> bool {
        self.stopped.contains(&id)
    }

    /// Whether the link is up (welcomed).
    pub fn is_up(&self) -> bool {
        self.link == Link::Up
    }

    /// The unacked local updates: document and `client_seq`.
    pub fn unacked(&self) -> Vec<(DocId, u64)> {
        self.docs.iter().flat_map(|(id, d)| d.pending.iter().map(|p| (*id, p.client_seq))).collect()
    }

    /// Unacked local updates, over every document.
    pub fn waiting(&self) -> usize {
        self.docs.values().map(|d| d.pending.len() + usize::from(d.pushing.is_some())).sum()
    }

    /// Bytes of the server's updates waiting for their history, over every document.
    pub fn waiting_bytes(&self) -> usize {
        self.docs.values().flat_map(|d| d.waiting.iter().map(|w| w.bytes.len())).sum()
    }

    /// How `app`'s document stands, in words (for a simulation that wants to say why it waits).
    pub fn describe(&self, app: &AppName) -> String {
        self.describe_doc(self.doc_id(app))
    }

    /// [`Replica::describe`], of any document.
    pub fn describe_doc(&self, id: DocId) -> String {
        match self.docs.get(&id) {
            None => "no document".into(),
            Some(d) => format!(
                "{:?}, upto {}, {} unacked ({} sent, {} push, acked {:?}), pushing {:?}, wait {:?}, full {:?}, stuck {}, retry {:?}",
                d.sub,
                d.have.upto,
                d.pending.len(),
                d.sent.len(),
                d.pending.iter().filter(|p| p.push).count(),
                d.pending.iter().filter_map(|p| p.acked).collect::<Vec<_>>(),
                d.pushing.as_ref().map(|has| d.body.copy().missing(has)),
                d.push_wait,
                d.full,
                d.stuck,
                d.retry
            ),
        }
    }

    /// [`Replica::waiting_peers`], of `app`'s document.
    pub fn waiting_peers_of(&self, app: &AppName) -> BTreeSet<u64> {
        self.docs.get(&self.doc_id(app)).map(|d| d.waiting.iter().flat_map(|w| w.needs.iter().map(|(peer, _)| *peer)).collect()).unwrap_or_default()
    }

    /// Unacked local updates (and an unfinished push) of `app`'s document.
    pub fn waiting_for(&self, app: &AppName) -> usize {
        self.docs.get(&self.doc_id(app)).map_or(0, |d| d.pending.len() + usize::from(d.pushing.is_some()))
    }

    /// The Loro peers whose operations the server's updates waiting for their history are, over every document (for a
    /// simulation that checks only a rogue's wait).
    pub fn waiting_peers(&self) -> BTreeSet<u64> {
        self.docs.values().flat_map(|d| d.waiting.iter().flat_map(|w| w.needs.iter().map(|(peer, _)| *peer))).collect()
    }

    /// Whether `app`'s document is full on the server (for a simulation that wants to say so).
    pub fn is_full(&self, app: &AppName) -> bool {
        self.docs.get(&self.doc_id(app)).is_some_and(|d| d.full.is_some())
    }

    /// Whether [`Replica::resend`] has something to do: an unacked update of a caught-up document not on its way, a
    /// refused subscribe to try again, or a full document to try again.
    pub fn wants_tick(&self) -> bool {
        self.docs.values().any(|d| {
            d.retry.is_some()
                || d.is_broken()
                || (d.sub == Sub::Live && d.full.is_some())
                || (d.sub == Sub::Live && d.pushing.is_some() && !d.pending.iter().any(|p| p.push))
                || (d.sub == Sub::Live && d.pending.iter().any(|p| !d.sent.contains(&p.client_seq) && (p.push || d.pushing.is_none())))
        })
    }

    /// Every entry of `app`'s document that reads, by key (for a simulation that wants to look inside).
    pub fn readable(&self, app: &AppName) -> Vec<(String, RecentEntry)> {
        self.docs
            .get(&self.doc_id(app))
            .and_then(|d| match &d.body {
                Body::Recents(recents) => Some(recents.readable()),
                Body::Document(_) => None,
            })
            .unwrap_or_default()
    }

    /// A document as a Loro snapshot (for a simulation that wants to look inside).
    pub fn snapshot_of(&self, doc: DocId) -> Option<Vec<u8>> {
        self.docs.get(&doc).and_then(|d| d.body.copy().snapshot().ok())
    }

    /// Documents Loro failed in outside an import: read back from their files, and caught up again from there.
    /// After the second heal of a document in one run, its push stops (the same state would fail the same way again):
    /// its unacked adds go up as plain updates, and [`Replica::push_stopped`] says so for the app to show.
    fn heal(&mut self) -> Vec<ClientFrame> {
        let broken: Vec<(DocId, Kind)> = self.docs.iter().filter(|(_, d)| d.is_broken()).map(|(id, d)| (*id, d.kind.clone())).collect();
        let mut frames = Vec::new();
        for (id, kind) in broken {
            // A document of a file that isn't joined yet has no file: it is given up on, and its apps told.
            if self.docs.get(&id).is_some_and(|d| !d.joined) {
                if let Some(old) = self.docs.remove(&id) {
                    std::mem::forget(old);
                    self.forgotten += 1;
                }
                self.events.push(DocEvent::NotLive(id, NotLiveReason::Offline, "Loro failed on the document".into()));
                continue;
            }
            self.reload(id);
            let heals = self.heals.entry(id).or_insert(0);
            *heals += 1;
            let stop = *heals >= MAX_HEALS;
            let (up, epoch, documents) = (self.link == Link::Up, self.epoch, self.protocol >= LIVE_DOCUMENTS_PROTOCOL);
            let Some(doc) = doc_again(&mut self.docs, self.space, &mut self.peers, id, &kind) else { continue };
            let line = doc.told(
                |app| format!("Loro failed on the recents of {app}; they were read back from disk"),
                |id| format!("Loro failed on the live document {id}; it was read back from disk"),
            );
            doc.say(&mut self.notes, line);
            if stop && doc.pushing.is_some() {
                doc.pushing = None;
                doc.push_wait = None;
                doc.dirty = true;
                let line = doc.told(
                    |app| format!(
                        "the recents of {app} can't be sent up whole after Loro failed again; new ones are sent one by one, and what the server lacks of the rest stays on this machine"
                    ),
                    |id| format!(
                        "the live document {id} can't be sent up whole after Loro failed again; new changes are sent one by one, and what the server lacks of the rest stays on this machine"
                    ),
                );
                doc.say(&mut self.notes, line);
                if let Kind::Recents(app) = &kind {
                    self.push_stopped.insert(app.clone());
                    // The app's followers are told (their list event carries the status).
                    self.changed.insert(app.clone());
                }
            }
            if up && (!kind.is_document() || (documents && !doc.parked)) {
                frames.extend(subscribe(id, doc, epoch));
            }
        }
        frames
    }

    /// Whether `app`'s push stopped after Loro failed on it twice ([`Replica::heal`]): a status for the app to show.
    pub fn push_stopped(&self, app: &AppName) -> bool {
        self.push_stopped.contains(app)
    }

    /// Documents let go of without dropping, Loro having failed in them (for a test that wants to count them).
    pub fn forgotten(&self) -> usize {
        self.forgotten
    }

    /// One tick (about every 2 s while [`Replica::wants_tick`]): the unacked updates of caught-up documents not on
    /// their way (refused for a moment as `rate_limited`), and a refused subscribe once its pause is over.
    pub fn resend(&mut self) -> Vec<ClientFrame> {
        let mut frames = self.resend_inner();
        frames.extend(self.heal());
        frames
    }

    fn resend_inner(&mut self) -> Vec<ClientFrame> {
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let mut frames = Vec::new();
        for (id, doc) in &mut self.docs {
            // The fragments of a change that stopped coming for good: dropped, never applied.
            if !doc.fragments.is_empty() && doc.sub == Sub::Live {
                doc.fragment_ticks = doc.fragment_ticks.saturating_add(1);
                if doc.fragment_ticks > FRAGMENT_PATIENCE_TICKS {
                    let line = format!(
                        "a change of the live document {id} came part of the way and the rest never came; what came of it was dropped, never applied"
                    );
                    doc.say(&mut self.notes, line);
                    doc.fragments.clear();
                    doc.fragment_ticks = 0;
                    doc.dirty = true;
                    doc.urgent = true;
                }
            }
            match doc.sub {
                Sub::Live => {
                    match doc.full {
                        Some(left) if left > 1 => doc.full = Some(left - 1),
                        Some(_) => doc.full = None,
                        None => {}
                    }
                    frames.extend(send_pending(*id, doc));
                    if let Some(left) = doc.push_wait.as_mut()
                        && *left > 0
                    {
                        *left -= 1;
                        doc.dirty = true;
                    }
                    frames.extend(doc.push_next(*id, &mut self.notes, &mut self.peers));
                }
                Sub::No if up => {
                    if let Some((left, pause)) = doc.retry {
                        if left > 1 {
                            doc.retry = Some((left - 1, pause));
                        } else {
                            let next = (pause * 2).min(MAX_RETRY_TICKS);
                            frames.extend(subscribe(*id, doc, epoch));
                            doc.retry = Some((0, next));
                        }
                    }
                }
                Sub::No | Sub::CatchingUp => {}
            }
        }
        frames
    }

    /// Handles one frame from the server. Frames to send back, or why the connection must end.
    pub fn on_frame(&mut self, frame: ServerFrame) -> Result<Vec<ClientFrame>, LinkError> {
        let mut frames = self.on_frame_inner(frame)?;
        frames.extend(self.heal());
        Ok(frames)
    }

    fn on_frame_inner(&mut self, frame: ServerFrame) -> Result<Vec<ClientFrame>, LinkError> {
        match frame {
            ServerFrame::Welcome { protocol, epoch } => {
                self.link = Link::Up;
                self.epoch = Some(epoch);
                self.protocol = protocol;
                let documents = protocol >= LIVE_DOCUMENTS_PROTOCOL;
                if !documents && self.docs.values().any(|d| d.kind.is_document()) {
                    self.notes.push("live documents need a newer Craft Cloud server; their changes stay on this machine until it is updated".into());
                }
                let mut frames = Vec::new();
                for (id, doc) in &mut self.docs {
                    // Documents of files only with a server that has them, and only those followed.
                    if doc.kind.is_document() && (!documents || doc.parked || !doc.joined) {
                        continue;
                    }
                    frames.extend(subscribe(*id, doc, Some(epoch)));
                }
                Ok(frames)
            }
            ServerFrame::Created { .. } => Ok(Vec::new()),
            ServerFrame::Update { doc: id, seq, bytes, .. } => {
                let Some(doc) = self.docs.get_mut(&id) else { return Ok(Vec::new()) };
                // While the snapshot the catch-up started with downloads, its updates wait here (it comes first).
                if doc.fetching.is_some() && doc.sub == Sub::CatchingUp {
                    doc.batched = doc.batched.saturating_add(bytes.len());
                    doc.batch.push((seq, bytes));
                    return Ok(Vec::new());
                }
                // A catch-up of a document of a file is taken a batch at a time (one Loro import for many updates is
                // far quicker on a big document): the updates wait here until `subscribed`, or a few MiB of them.
                if doc.kind.is_document() && doc.sub == Sub::CatchingUp && !doc.stale {
                    doc.batched = doc.batched.saturating_add(bytes.len());
                    doc.batch.push((seq, bytes));
                    return Ok(if doc.batched > MAX_BATCH_BYTES { self.take_batch(id) } else { Vec::new() });
                }
                Ok(self.on_update(id, seq, bytes))
            }
            ServerFrame::Subscribed { doc: id, head } => {
                if let Some(doc) = self.docs.get_mut(&id)
                    && doc.fetching.is_some()
                    && doc.sub == Sub::CatchingUp
                {
                    doc.deferred_head = Some(head);
                    return Ok(Vec::new());
                }
                let mut frames = self.take_batch(id);
                frames.extend(self.on_subscribed(id, head));
                Ok(frames)
            }
            ServerFrame::Ack { doc: id, client_seq, seq } => Ok(self.on_ack(id, client_seq, seq)),
            other => self.on_other(other),
        }
    }

    /// The snapshot `manifest` names, which the catch-up of `id` started with, downloaded (`Ok`), or not (`Err`: it
    /// can't be got or doesn't read; this machine then follows the whole log instead, `subscribe_log`). Then the
    /// catch-up's updates that waited for it are taken, and its `subscribed` if it came. Frames to send.
    pub fn on_snapshot(&mut self, id: DocId, manifest: ManifestId, bytes: Result<Vec<u8>, String>) -> Vec<ClientFrame> {
        let mut frames = self.on_snapshot_inner(id, manifest, bytes);
        frames.extend(self.heal());
        frames
    }

    fn on_snapshot_inner(&mut self, id: DocId, manifest: ManifestId, bytes: Result<Vec<u8>, String>) -> Vec<ClientFrame> {
        let Some(doc) = self.docs.get_mut(&id) else { return Vec::new() };
        let Some((base, _)) = doc.fetching.filter(|(_, m)| *m == manifest) else { return Vec::new() };
        doc.fetching = None;
        let head = doc.deferred_head.take();
        let mut changed = false;
        let taken = match &bytes {
            Ok(bytes) => doc.take_snapshot(base, bytes, &mut self.notes, &mut changed),
            Err(why) => {
                // As one that doesn't read: the whole log instead.
                let line = doc.told(
                    |app| {
                        format!("the server's snapshot of the recents of {app} can't be got here ({why}); this machine reads the whole log instead")
                    },
                    |id| {
                        format!("the server's snapshot of the live document {id} can't be got here ({why}); this machine reads the whole log instead")
                    },
                );
                doc.say(&mut self.notes, line);
                doc.log_only = true;
                doc.stale = true;
                doc.dirty = true;
                doc.urgent = true;
                Err(UpdateError::Invalid("not got"))
            }
        };
        let kind = doc.kind.clone();
        if changed {
            match &kind {
                Kind::Recents(app) => {
                    self.changed.insert(app.clone());
                }
                Kind::Document { .. } => {
                    if let Ok(bytes) = bytes {
                        self.events.push(DocEvent::Remote(id, bytes));
                    }
                }
            }
        }
        match taken {
            Ok(()) => {
                // The updates that waited, and the end of the catch-up.
                let mut frames = self.take_batch(id);
                if let Some(head) = head {
                    frames.extend(self.on_subscribed(id, head));
                }
                frames
            }
            // Read back from its file (Loro failed in it), and caught up again with the whole log.
            Err(UpdateError::Panicked) => {
                self.reload(id);
                let (up, epoch) = (self.link == Link::Up, self.epoch);
                match doc_again(&mut self.docs, self.space, &mut self.peers, id, &kind) {
                    Some(doc) if up => {
                        doc.log_only = true;
                        subscribe(id, doc, epoch)
                    }
                    _ => Vec::new(),
                }
            }
            // Given up on: what waited goes (the next catch-up, of the whole log, brings it); its `subscribed` starts
            // that one.
            Err(_) => {
                if let Some(doc) = self.docs.get_mut(&id) {
                    doc.batch.clear();
                    doc.batched = 0;
                }
                head.map(|head| self.on_subscribed(id, head)).unwrap_or_default()
            }
        }
    }

    /// The updates a catch-up batched: imported into the copy at once, then each taken as it would have been (Loro
    /// knows them then, so that is quick). Remote changes go to the apps as they came. The row Loro failed on before
    /// is left out of the batch (it is skipped when taken). If Loro fails on the batch, the document is read back
    /// from its file and the catch-up taken again one row at a time for the rest of the run, so the row at fault is
    /// found and only it dropped ([`Replica::recover_doc`]).
    fn take_batch(&mut self, id: DocId) -> Vec<ClientFrame> {
        let one_by_one = self.one_by_one.contains(&id);
        let Some(doc) = self.docs.get_mut(&id) else { return Vec::new() };
        let batch = std::mem::take(&mut doc.batch);
        doc.batched = 0;
        if batch.is_empty() {
            return Vec::new();
        }
        let new = if one_by_one {
            // (Each row's own import says what it brought.)
            false
        } else {
            // (Fragments are taken row by row: their change goes in once it is whole. So are bundles: their slices go
            // in one after the other, which Loro's batch import can't do.)
            let updates: Vec<Vec<u8>> = batch
                .iter()
                .filter(|(seq, bytes)| doc.skip != Some((*seq, update_hash(bytes))) && !is_fragment(bytes) && !is_bundle(bytes))
                .map(|(_, bytes)| bytes.clone())
                .collect();
            match doc.body.copy().import_batch(&updates) {
                Ok(new) => new,
                Err(UpdateError::Panicked) => return self.take_one_by_one(id),
                Err(_) => false,
            }
        };
        let kind = doc.kind.clone();
        let mut frames = Vec::new();
        if new && let Kind::Recents(app) = &kind {
            self.changed.insert(app.clone());
        }
        for (seq, bytes) in batch {
            if new && kind.is_document() && !is_fragment(&bytes) && !is_bundle(&bytes) {
                self.events.push(DocEvent::Remote(id, bytes.clone()));
            }
            frames.extend(self.on_update(id, seq, bytes));
        }
        frames
    }

    /// Loro failed on a catch-up's batch of `id`: read back from its file, and caught up again from there one row at a
    /// time for the rest of the run.
    fn take_one_by_one(&mut self, id: DocId) -> Vec<ClientFrame> {
        let Some(kind) = self.docs.get(&id).map(|d| d.kind.clone()) else { return Vec::new() };
        self.one_by_one.insert(id);
        self.reload(id);
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let Some(doc) = doc_again(&mut self.docs, self.space, &mut self.peers, id, &kind) else { return Vec::new() };
        if up { subscribe(id, doc, epoch) } else { Vec::new() }
    }

    /// An update of a document's log from the server.
    fn on_update(&mut self, id: DocId, seq: u64, bytes: Vec<u8>) -> Vec<ClientFrame> {
        let Some(doc) = self.docs.get_mut(&id) else { return Vec::new() };
        // The update at the base of a snapshot the catch-up started with: its hash names it in the next `at`.
        if seq == doc.have.upto && seq > 0 && doc.have.upto_hash.is_none() {
            doc.have.upto_hash = Some(update_hash(&bytes));
            doc.dirty = true;
            return Vec::new();
        }
        if seq <= doc.have.upto || doc.stuck || doc.stopped {
            return Vec::new();
        }
        if seq > doc.have.upto.saturating_add(1) {
            // A gap: a catch-up under way brings it; else the server's updates were lost on the way (an old
            // connection), and a catch-up from what it has brings them.
            return if doc.sub == Sub::Live { doc.resubscribe(id) } else { Vec::new() };
        }
        let mut changed = false;
        let taken = doc.take(seq, &bytes, &mut self.notes, &mut changed);
        // (A row that made a change in fragments whole: the apps get the change, never a fragment.)
        let joined = doc.joined_now.take();
        let kind = doc.kind.clone();
        let mut frames = if matches!(taken, Taken::Had) { doc.try_compact() } else { Vec::new() };
        if changed {
            match &kind {
                Kind::Recents(app) => {
                    self.changed.insert(app.clone());
                }
                Kind::Document { .. } => self.events.push(DocEvent::Remote(id, joined.unwrap_or_else(|| bytes.clone()))),
            }
        }
        frames.extend(match taken {
            Taken::Had | Taken::Stuck => Vec::new(),
            Taken::Panicked => self.recover_doc(id, kind, seq, update_hash(&bytes)),
            Taken::WaitingPanicked(hash) => self.recover_waiting_doc(id, kind, hash),
        });
        frames
    }

    /// `subscribed {doc, head}`: the end of a catch-up.
    fn on_subscribed(&mut self, id: DocId, head: u64) -> Vec<ClientFrame> {
        let Some(doc) = self.docs.get_mut(&id) else { return Vec::new() };
        if doc.stale {
            // The end of a catch-up given up on: the next one, from where it is.
            doc.stale = false;
            return doc.resubscribe(id);
        }
        if head < doc.have.upto {
            // The server has less than this machine read of it (restored without a new epoch): from the start,
            // so `have` names updates of this log, and then what the log lacks goes up.
            doc.from_snapshot = false;
            doc.start_over();
            return doc.resubscribe(id);
        }
        let from_snapshot = doc.from_snapshot;
        let again = doc.check_snapshot(id, &mut self.notes);
        if !again.is_empty() {
            return again;
        }
        if matches!(doc.join, Some(Join::Probing | Join::Following)) && head == 0 && !from_snapshot {
            // The server has the document but nothing of it (no update, no snapshot): an admin reset it, and its
            // machines haven't sent it back yet. An app here would get an empty document and might save it over its
            // file: the file is what to open until then.
            self.docs.remove(&id);
            self.events.push(DocEvent::NotLive(
                id,
                NotLiveReason::Resetting,
                "the server's live document was reset and isn't back yet; open the file as it is".into(),
            ));
            return vec![ClientFrame::Unsubscribe { doc: id }];
        }
        doc.sub = Sub::Live;
        doc.retry = None;
        doc.create_refused = false;
        if matches!(doc.join, Some(Join::Probing | Join::Following)) {
            // The server has it, and this machine all of it: live here from now on.
            doc.join = None;
            // (Written with the next of the documents' writes: until then the server has all of it.)
            doc.joined = true;
            doc.dirty = true;
            self.events.push(DocEvent::Joined(id));
        }
        let mut frames = send_pending(id, doc);
        frames.extend(doc.push_next(id, &mut self.notes, &mut self.peers));
        frames.extend(doc.try_compact());
        frames
    }

    /// `ack {doc, client_seq, seq}`: one of this machine's updates is in the log.
    fn on_ack(&mut self, id: DocId, client_seq: u64, seq: u64) -> Vec<ClientFrame> {
        let Some(doc) = self.docs.get_mut(&id) else { return Vec::new() };
        let Some(i) = doc.pending.iter().position(|p| p.client_seq == client_seq) else {
            // Stored already as far as this machine knew (the log held its operations before the ack came): the acked
            // row holds them, and counts as had like any ack (or waits for the rows below it, as any ack does).
            let Some(bytes) = doc.taken_back.remove(&client_seq) else { return Vec::new() };
            if seq == doc.have.upto.saturating_add(1) {
                doc.note_own_row(seq, &bytes);
                doc.advance(seq, update_hash(&bytes));
            } else if seq > doc.have.upto {
                doc.back_acked.insert(seq, bytes);
            }
            let mut frames = if doc.pushing.is_some() { doc.push_next(id, &mut self.notes, &mut self.peers) } else { Vec::new() };
            frames.extend(doc.try_compact());
            return frames;
        };
        doc.strikes = 0;
        doc.full_pause = 0;
        doc.dirty = true;
        let push = doc.pending.get(i).is_some_and(|p| p.push);
        if let Some(bytes) = doc.pending.get(i).map(|p| p.bytes.clone()) {
            doc.on_server(&bytes);
        }
        if seq <= doc.have.upto {
            doc.pending.remove(i);
            doc.sent.remove(&client_seq);
        } else if seq == doc.have.upto.saturating_add(1) {
            let own = doc.pending.remove(i);
            doc.sent.remove(&client_seq);
            doc.note_own_row(seq, &own.bytes);
            doc.advance(seq, update_hash(&own.bytes));
        } else if let Some(pending) = doc.pending.get_mut(i) {
            // Others' updates below it are on their way: counted once they are here. (A push part counts as
            // sent: the next one is computed without it.)
            pending.acked = Some(seq);
            pending.push = false;
        }
        let mut frames = if push { doc.push_next(id, &mut self.notes, &mut self.peers) } else { Vec::new() };
        frames.extend(doc.try_compact());
        frames
    }

    /// The frames [`Replica::on_frame_inner`] doesn't take itself.
    fn on_other(&mut self, frame: ServerFrame) -> Result<Vec<ClientFrame>, LinkError> {
        match frame {
            ServerFrame::Welcome { .. } | ServerFrame::Update { .. } | ServerFrame::Subscribed { .. } | ServerFrame::Ack { .. } => Ok(Vec::new()),
            ServerFrame::Created { .. } => Ok(Vec::new()),
            ServerFrame::Error { code, message, doc, client_seq } => self.refused(&code, &message, doc, client_seq),
            ServerFrame::Compact { doc: id, upto } => {
                let Some(doc) = self.docs.get_mut(&id) else { return Ok(Vec::new()) };
                doc.compact_ask = Some(upto);
                Ok(doc.try_compact())
            }
            ServerFrame::Snapshot { doc: id, base, manifest } => {
                let Some(doc) = self.docs.get_mut(&id) else { return Ok(Vec::new()) };
                // Only as the start of a catch-up (a stray one, from a connection given up on, is no news). It is
                // downloaded through the chunk API; the catch-up's updates wait for it ([`Replica::on_snapshot`]).
                if doc.sub != Sub::CatchingUp || doc.stale || doc.fetching.is_some() {
                    return Ok(Vec::new());
                }
                doc.fetching = Some((base, manifest));
                self.fetches.push((id, manifest));
                Ok(Vec::new())
            }
            ServerFrame::Presence { doc: id, bytes } => {
                if self.docs.get(&id).is_some_and(|d| d.joined && d.kind.is_document()) {
                    self.events.push(DocEvent::Presence(id, bytes));
                }
                Ok(Vec::new())
            }
            ServerFrame::CreatedDocument { doc: id, kept } => Ok(self.created(id, kept)),
        }
    }

    /// A refused `create` or `subscribe` of a document of a file, where it differs from recents: `Some(frames)` when
    /// handled here.
    fn refused_document(&mut self, id: DocId, code: &str, message: &str) -> Option<Vec<ClientFrame>> {
        let doc = self.docs.get_mut(&id).filter(|d| d.kind.is_document())?;
        let Kind::Document { node, app } = doc.kind.clone() else { return None };
        match (&doc.join, code) {
            // The server doesn't have it: the app is asked for its import.
            (Some(Join::Probing | Join::Following), codes::NOT_FOUND) => {
                doc.sub = Sub::No;
                doc.join = Some(Join::NeedsImport);
                self.events.push(DocEvent::NeedsImport(id));
                Some(Vec::new())
            }
            // The import was refused: the file opens as a plain file (and is tried again at the next open).
            (Some(Join::Creating(_)), _) => {
                let reason = match code {
                    codes::NOT_FOUND => NotLiveReason::NotSynced,
                    codes::TOO_LARGE | codes::QUOTA_EXCEEDED => NotLiveReason::TooLarge,
                    _ => NotLiveReason::Offline,
                };
                self.docs.remove(&id);
                self.events.push(DocEvent::NotLive(id, reason, format!("the server refused the import ({code}: {message})")));
                Some(Vec::new())
            }
            // Refused while joining for another reason: given up on for now.
            (Some(_), _) if code != codes::RATE_LIMITED => {
                self.docs.remove(&id);
                self.events.push(DocEvent::NotLive(id, NotLiveReason::Offline, format!("the server can't open it now ({code}: {message})")));
                Some(Vec::new())
            }
            // A joined document the server doesn't have (a restore from before it was made, or a reset): made again from
            // this machine's copy; nothing it holds is lost. If that is refused too (the file is gone on the server),
            // tried again after a growing pause.
            (None, codes::NOT_FOUND) if !doc.creating => {
                let upload = doc.body.copy().snapshot().map_err(|e| e.to_string()).and_then(|s| cut(id, &s));
                let upload = match upload {
                    Ok(upload) => upload,
                    Err(e) => {
                        // (Over the snapshot limit, say: it can't be made again from here. Said, and tried again later
                        // as any refusal, in case it shrinks.)
                        doc.sub = Sub::No;
                        let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
                        doc.retry = Some((pause, pause));
                        let line =
                            format!("the server lost the live document {id}, and this machine can't make it again ({e}); it stays on this machine");
                        doc.say(&mut self.notes, line);
                        return Some(Vec::new());
                    }
                };
                doc.sub = Sub::No;
                doc.creating = true;
                let manifest = upload.id();
                doc.uploads.push(upload);
                Some(vec![ClientFrame::CreateDocument { doc: id, node, app, snapshot: manifest }])
            }
            (None, codes::NOT_FOUND | codes::QUOTA_EXCEEDED | codes::TOO_LARGE) => {
                doc.creating = false;
                let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
                doc.sub = Sub::No;
                doc.retry = Some((pause, pause));
                let line = format!("the server can't make the live document {id} again ({code}: {message}); it stays on this machine");
                doc.say(&mut self.notes, line);
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// `created {doc, kept}`: this machine's import is the document (kept), or another machine's is (not kept: this one
    /// is dropped, and the document caught up from the start). A joined document made again from its copy (its
    /// server lost it) starts over either way, so what the log lacks of it goes up.
    fn created(&mut self, id: DocId, kept: bool) -> Vec<ClientFrame> {
        let Some(doc) = self.docs.get_mut(&id) else { return Vec::new() };
        if doc.creating {
            doc.creating = false;
            doc.start_over();
            return doc.resubscribe(id);
        }
        let Some(Join::Creating(snapshot)) = doc.join.take() else { return Vec::new() };
        if !kept {
            doc.join = Some(Join::Following);
            return doc.resubscribe(id);
        }
        let peer = doc.body.copy().peer();
        match Body::load(&doc.kind, &snapshot, peer) {
            Ok(body) => {
                doc.body = body;
                doc.own_peers = vec![peer];
                // The import is this machine's app's.
                doc.own(&snapshot);
                let version = version_of(&snapshot).unwrap_or_default();
                let ranges: Vec<(u64, i32, i32)> = version.iter().map(|(peer, end)| (*peer, 0, *end)).collect();
                doc.saw(&ranges);
                doc.log = LogVersion::anchored(0, version);
                doc.joined = true;
                doc.dirty = true;
                self.events.push(DocEvent::Joined(id));
                // Followed from the start: the server sends this import back, then what others add.
                doc.resubscribe(id)
            }
            Err(_) => {
                // (Checked before it went up; Loro reads it differently now.) Caught up from the server instead.
                doc.join = Some(Join::Following);
                doc.resubscribe(id)
            }
        }
    }

    fn refused(&mut self, code: &str, message: &str, doc: Option<DocId>, client_seq: Option<u64>) -> Result<Vec<ClientFrame>, LinkError> {
        if code == PROTOCOL_MISMATCH {
            return Err(LinkError::Mismatch(message.to_owned()));
        }
        let epoch = self.epoch;
        if let (Some(id), None) = (doc, client_seq)
            && let Some(frames) = self.refused_document(id, code, message)
        {
            return Ok(frames);
        }
        let found = doc.and_then(|id| self.docs.get_mut(&id).map(|d| (id, d)));
        match (code, found, client_seq) {
            // Sent again by `resend`, a moment later.
            (codes::RATE_LIMITED, Some((_, doc)), Some(client_seq)) => {
                doc.sent.remove(&client_seq);
                Ok(Vec::new())
            }
            // Too large for this server: the whole document goes up instead, in parts half the size each time.
            (codes::TOO_LARGE, Some((id, doc)), Some(client_seq)) => {
                if !doc.pending.iter().any(|p| p.client_seq == client_seq) {
                    return Ok(Vec::new());
                }
                doc.strikes = doc.strikes.saturating_add(1);
                if doc.strikes > MAX_STRIKES {
                    let line = doc.told(
                        |app| format!("the server refuses even small parts of the recents of {app} ({message}); they stay on this machine"),
                        |id| format!("the server refuses even small parts of the live document {id} ({message}); it stays on this machine"),
                    );
                    doc.say(&mut self.notes, line);
                    return Ok(Vec::new());
                }
                // Its operations go up in the parts of a push (of the whole document, if no push is under way). A
                // fragment takes the rest of its change with it: the push sends the change whole again.
                let set = doc.pending.iter().find(|p| p.client_seq == client_seq).and_then(|p| parse(&p.bytes)).map(|f| f.set);
                let gone: Vec<u64> = doc
                    .pending
                    .iter()
                    .filter(|p| p.client_seq == client_seq || set.is_some_and(|s| parse(&p.bytes).is_some_and(|f| f.set == s)))
                    .map(|p| p.client_seq)
                    .collect();
                doc.pending.retain(|p| !gone.contains(&p.client_seq));
                for seq in gone {
                    doc.sent.remove(&seq);
                }
                doc.pushing.get_or_insert_with(Spans::new);
                doc.dirty = true;
                Ok(doc.push_next(id, &mut self.notes, &mut self.peers))
            }
            // The document is full on the server: every unacked update stays here (none is dropped), and nothing is sent
            // until they are tried again after a growing pause (the admin may have emptied it by then).
            (codes::QUOTA_EXCEEDED, Some((_, doc)), Some(client_seq)) => {
                doc.sent.remove(&client_seq);
                if doc.full.is_none() {
                    doc.full_pause = (doc.full_pause * 2).clamp(1, MAX_RETRY_TICKS);
                    doc.full = Some(doc.full_pause);
                }
                let line = doc.told(
                    |app| format!("the server takes no more recents of {app} for now ({message}); new ones stay on this machine"),
                    |id| format!("the live document {id} is full on the server for now ({message}); new changes stay on this machine"),
                );
                doc.say(&mut self.notes, line);
                Ok(Vec::new())
            }
            // The update at `after` isn't the one this machine has there: the log was restored (its epoch reset hasn't
            // run). From the start, and the whole document up again.
            (codes::RESYNC_REQUIRED, Some((id, doc)), None) => {
                doc.start_over();
                Ok(subscribe(id, doc, epoch))
            }
            // A create refused because the account is at its live-document limit (the server's cap): tried again after a
            // growing pause, and the recents stay here until then. The link stays up for the other lists.
            (codes::QUOTA_EXCEEDED, Some((_, doc)), None) => {
                let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
                doc.sub = Sub::No;
                doc.retry = Some((pause, pause));
                doc.create_refused = true;
                let line = doc.told(
                    |app| {
                        format!("the account has as many live documents as the server allows; the recents of {app} stay on this machine ({message})")
                    },
                    |id| {
                        format!(
                            "the account has as many live documents as the server allows; the live document {id} stays on this machine ({message})"
                        )
                    },
                );
                doc.say(&mut self.notes, line);
                Ok(Vec::new())
            }
            // A create or subscribe refused for a moment (the budget, the device's cap), or a subscribe to a document
            // whose create was refused, or one repeated during a catch-up: tried again after a growing pause.
            (codes::RATE_LIMITED | codes::NOT_FOUND | codes::INVALID, Some((_, doc)), None) => {
                let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
                doc.sub = Sub::No;
                doc.retry = Some((pause, pause));
                // After a refused create the subscribe comes back `not_found`: that was said already.
                if !doc.create_refused {
                    let line = doc.told(
                        |app| format!("the server can't follow the recents of {app} right now ({message}); trying again shortly"),
                        |id| format!("the server can't follow the live document {id} right now ({message}); trying again shortly"),
                    );
                    doc.say(&mut self.notes, line);
                }
                Ok(Vec::new())
            }
            // A catch-up that failed on the server (its snapshot doesn't read there): tried again after a pause, with the
            // whole log.
            (codes::INTERNAL, Some((_, doc)), None) if doc.kind.is_document() || doc.log_only => {
                let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
                doc.sub = Sub::No;
                doc.retry = Some((pause, pause));
                doc.log_only = true;
                Ok(Vec::new())
            }
            _ => Err(LinkError::Retry(format!("the server refused a live frame ({code}: {message})"))),
        }
    }

    /// Writes every document changed since it was last written. Called after a batch of frames: a crash before it only
    /// loses what the server sends again.
    /// A document of a file is written here only when it must be (it was just joined, started over, or its journal is
    /// long); otherwise now and then ([`Replica::flush_documents`]): its local updates are on disk in its journal
    /// already, and the server's come again.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let space = self.space;
        for (id, doc) in &mut self.docs {
            let now = !doc.kind.is_document() || doc.urgent || doc.journal > MAX_JOURNAL_BYTES;
            if doc.dirty && doc.joined && now {
                // (A document of a file held by another program is written at a later flush: see `Patience`.)
                save_as(&mut self.store, space, *id, doc, Patience::Later)?;
            }
        }
        Ok(())
    }

    /// Writes every document changed since it was last written, documents of files too (when the agent stops: it
    /// waits for another program holding a file).
    pub fn flush_documents(&mut self) -> std::io::Result<()> {
        self.flush_documents_as(Patience::Wait)
    }

    /// [`Replica::flush_documents`] now and then: a document of a file another program holds is left for the next
    /// time rather than waited for.
    pub fn flush_documents_now_and_then(&mut self) -> std::io::Result<()> {
        self.flush_documents_as(Patience::Later)
    }

    fn flush_documents_as(&mut self, patience: Patience) -> std::io::Result<()> {
        let space = self.space;
        for (id, doc) in &mut self.docs {
            if doc.dirty && doc.joined {
                save_as(&mut self.store, space, *id, doc, patience)?;
            }
        }
        Ok(())
    }

    /// The apps whose list changed, with their new list.
    pub fn take_changed(&mut self) -> Vec<(AppName, Vec<RecentEntry>)> {
        let changed = std::mem::take(&mut self.changed);
        changed.into_iter().map(|app| (app.clone(), self.entries(&app))).collect()
    }

    /// What to tell the user since the last call.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }
}

/// The document a file holds (`None` for a file of another space, or one that doesn't read). A file that doesn't read
/// is never deleted: it is set aside (renamed, kept). If its unacked updates can still be read (Loro fails on the
/// snapshot), the document starts afresh from them and catches up from the start; they are kept and sent.
/// The document of a file `bytes` hold, if it can stay on disk unloaded: of this space, and with nothing unacked (in
/// the file or its journal), nothing waiting and no push under way. A file that doesn't read is loaded (and so set
/// aside) as before.
fn idle_on_disk<S: DocStore>(store: &S, space: SpaceId, name: &str, bytes: &[u8]) -> Option<DocId> {
    let (file, more) = read_document_file(bytes).ok()?;
    let ours = file.format <= DOCUMENT_FILE_FORMAT && file.space == space && file.doc == node_doc(space, file.node) && file.doc.to_string() == name;
    let idle = file.pending.is_empty() && file.waiting.is_empty() && file.pushing.is_none() && more.fragments.is_empty();
    let journal = store.load_journal(name).ok()?;
    (ours && idle && journal_records(&journal).0.is_empty()).then_some(file.doc)
}

/// `drop`: a waiting update to leave out (Loro failed on it).
fn load_doc<S: DocStore>(
    store: &mut S,
    space: SpaceId,
    name: &str,
    bytes: &[u8],
    peer: u64,
    drop: Option<[u8; 32]>,
    notes: &mut Vec<String>,
) -> Option<(DocId, Doc)> {
    let aside = |store: &mut S| match store.set_aside(name) {
        Ok(to) => (format!("a copy set aside as {to}"), true),
        Err(e) => (format!("no copy set aside ({e})"), false),
    };
    match read_doc_file(bytes) {
        Ok((mut file, more)) if file.format <= DOC_FILE_FORMAT && file.space == space && file.doc == recents_doc(space, &file.app) => {
            if let Some(hash) = drop {
                file.waiting.retain(|w| w.hash != hash);
            }
            let id = file.doc;
            let (app, pending, next) = (file.app.clone(), file.pending.clone(), file.next_client_seq);
            match Doc::from_file(file, more, peer, notes) {
                Ok(doc) => Some((id, doc)),
                Err(e) => {
                    // A copy aside first (durable), then the rebuilt document over the original: a crash between them
                    // finds the original again, and rebuilds again.
                    let (set, copied) = aside(store);
                    let mut doc = Doc::from_pending(id, Kind::Recents(app.clone()), pending, next, peer);
                    // (Without a copy, the original stays as it is until the document is next written.)
                    let written = match copied.then(|| save(store, space, id, &mut doc)) {
                        Some(Ok(())) => "written afresh".to_owned(),
                        Some(Err(e)) => format!("not yet written afresh ({e})"),
                        None => "kept in memory".to_owned(),
                    };
                    let line = format!(
                        "the live document file {name} doesn't read ({e}); {set}; {app}'s recents start again from its {} unsent update(s), which are kept ({written}), and the rest comes from the server",
                        doc.pending.len()
                    );
                    notes.push(line);
                    Some((id, doc))
                }
            }
        }
        Ok(_) => None,
        Err(e) => {
            // Nothing in it reads: a copy aside, and the file goes (else it is set aside again at every load).
            let (set, copied) = aside(store);
            let removed = match copied.then(|| store.remove(name)) {
                Some(Ok(())) => "and removed".to_owned(),
                Some(Err(e)) => format!("not removed ({e})"),
                None => "left in place".to_owned(),
            };
            let line = format!("the live document file {name} doesn't read ({e}); {set}, {removed}; it is filled from the server again");
            notes.push(line);
            None
        }
    }
}

/// The document of a file a `.doc` file holds, with the local updates its journal has beyond it (`None` for a file of
/// another space, or one that doesn't read at all, which is set aside and removed, as a recents file is). A file whose
/// snapshot doesn't read starts afresh from its unsent updates, which are kept and sent, and catches up from the start.
fn load_document<S: DocStore>(
    store: &mut S,
    space: SpaceId,
    name: &str,
    bytes: &[u8],
    peer: u64,
    drop: Option<[u8; 32]>,
    notes: &mut Vec<String>,
) -> Option<(DocId, Doc)> {
    let journal = store.load_journal(name).unwrap_or_default();
    let aside = |store: &mut S| match store.set_aside_document(name) {
        Ok(to) => (format!("a copy set aside as {to}"), true),
        Err(e) => (format!("no copy set aside ({e})"), false),
    };
    match read_document_file(bytes) {
        Ok((mut file, more)) if file.format <= DOCUMENT_FILE_FORMAT && file.space == space && file.doc == node_doc(space, file.node) => {
            if let Some(hash) = drop {
                file.waiting.retain(|w| w.hash != hash);
            }
            let id = file.doc;
            let kind = Kind::Document { node: file.node, app: file.app.clone() };
            let (mut pending, mut next) = (file.pending, file.next_client_seq.max(1));
            // The journal's records the file doesn't have yet (one cut off by a crash ends it: it was never acked). The
            // journal is cut there, so what is appended next reads after the good ones.
            let mut journalled = 0usize;
            let (records, good) = journal_records(&journal);
            if good < journal.len()
                && let Err(e) = store.truncate_journal(name, good)
            {
                notes.push(format!("the journal of the live document {name} couldn't be cut at its end that doesn't read ({e})"));
            }
            for record in records {
                if record.client_seq >= next {
                    // A change over the frame limit is in the journal whole, under the first of its fragments' numbers.
                    let frames = split(&record.bytes, MAX_UPDATE_BYTES).unwrap_or_else(|_| vec![record.bytes.clone()]);
                    for (k, bytes) in (0u64..).zip(frames) {
                        pending.push(Pending { client_seq: record.client_seq.saturating_add(k), bytes, acked: None, push: false });
                        next = record.client_seq.saturating_add(k + 1);
                    }
                    journalled += 1;
                }
            }
            let parts = Parts {
                id,
                kind: kind.clone(),
                have: file.have,
                next_client_seq: next,
                pending: pending.clone(),
                snapshot: file.snapshot,
                waiting: file.waiting,
                epoch: file.epoch,
                pushing: file.pushing,
                own_peers: file.own_peers,
                push_wait: file.push_wait,
                dropped: file.dropped,
                fragments: more.fragments,
            };
            match Doc::from_parts(parts, peer, notes) {
                Ok(mut doc) => {
                    doc.log_only = file.log_only;
                    doc.journal = good;
                    doc.dirty = journalled > 0;
                    // (Not cut: written whole before the next change goes into it.)
                    doc.journal_failed = good < journal.len() && store.load_journal(name).map_or(true, |j| j.len() > good);
                    Some((id, doc))
                }
                Err(e) => {
                    let (set, copied) = aside(store);
                    let mut doc = Doc::from_pending(id, kind, pending, next, peer);
                    let written = match copied.then(|| save(store, space, id, &mut doc)) {
                        Some(Ok(())) => "written afresh".to_owned(),
                        Some(Err(e)) => format!("not yet written afresh ({e})"),
                        None => "kept in memory".to_owned(),
                    };
                    notes.push(format!(
                        "the live document file {name} doesn't read ({e}); {set}; the document starts again from its {} unsent update(s), which are kept ({written}), and the rest comes from the server",
                        doc.pending.len()
                    ));
                    Some((id, doc))
                }
            }
        }
        Ok(_) => None,
        Err(e) => {
            let (set, copied) = aside(store);
            let removed = match copied.then(|| store.remove_document(name)) {
                Some(Ok(())) => "and removed".to_owned(),
                Some(Err(e)) => format!("not removed ({e})"),
                None => "left in place".to_owned(),
            };
            notes.push(format!("the live document file {name} doesn't read ({e}); {set}, {removed}; it is filled from the server again"));
            None
        }
    }
}

/// Whether every range of `ranges` is in `has`.
fn held_by(has: &Spans, ranges: &[(u64, i32, i32)]) -> bool {
    ranges.iter().all(|(peer, s, e)| s >= e || has.get(peer).is_some_and(|list| list.iter().any(|(hs, he)| hs <= s && e <= he)))
}

/// The document of `app` in `docs`, made (empty) if there is none yet.
fn doc_in<'a>(docs: &'a mut BTreeMap<DocId, Doc>, space: SpaceId, app: &AppName, peers: &mut Peers) -> (DocId, &'a mut Doc) {
    let id = recents_doc(space, app);
    let doc = docs.entry(id).or_insert_with(|| Doc::empty(app, peers(), id));
    (id, doc)
}

/// The document `id` of `kind` after a reload: a recents document is made afresh if it had no file; a document of a
/// file without one is gone.
fn doc_again<'a>(docs: &'a mut BTreeMap<DocId, Doc>, space: SpaceId, peers: &mut Peers, id: DocId, kind: &Kind) -> Option<&'a mut Doc> {
    match kind {
        Kind::Recents(app) => Some(doc_in(docs, space, app, peers).1),
        Kind::Document { .. } => docs.get_mut(&id),
    }
}

/// `create` (the server may lack the document) and `subscribe` from what it has, naming the update it has at that
/// number. A document that knew another epoch (the server was restored since) catches up from the start and goes up
/// whole after the catch-up. Acks of numbers not reached are forgotten: their updates go again after `subscribed`.
fn subscribe(id: DocId, doc: &mut Doc, epoch: Option<Epoch>) -> Vec<ClientFrame> {
    if doc.stopped {
        doc.sub = Sub::No;
        return Vec::new();
    }
    if let Some(epoch) = epoch
        && doc.epoch != Some(epoch)
    {
        if doc.epoch.is_some() {
            doc.start_over();
        }
        doc.epoch = Some(epoch);
        doc.dirty = true;
    }
    doc.forget_acks();
    doc.sent.clear();
    doc.taken_back.clear();
    doc.back_acked.clear();
    // A document of a file is made by import, never by a bare `create`: one the server lost answers `not_found`, and
    // is made again from this copy ([`Replica::refused_document`]).
    let mut frames = match &doc.kind {
        Kind::Recents(app) => vec![ClientFrame::Create { doc: id, kind: DocKind::Recents { app: app.clone() } }],
        Kind::Document { .. } => Vec::new(),
    };
    frames.extend(doc.resubscribe(id));
    frames
}

/// Every unacked update not on its way yet, in order (none while the document is full on the server).
fn send_pending(id: DocId, doc: &mut Doc) -> Vec<ClientFrame> {
    let mut frames = Vec::new();
    if doc.full.is_some() {
        return frames;
    }
    for pending in &doc.pending {
        // While a push is under way, only its parts go: they carry every other unacked update.
        if (pending.push || doc.pushing.is_none()) && doc.sent.insert(pending.client_seq) {
            frames.push(ClientFrame::Update { doc: id, client_seq: pending.client_seq, bytes: pending.bytes.clone() });
        }
    }
    frames
}

/// Whether a write of a document of a file waits for another program holding its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Patience {
    /// It does (something waits for this write: an app's change, a load, the agent stopping).
    Wait,
    /// It doesn't: the document stays dirty and is written at the next flush. For the writes the hub makes now and
    /// then, which nothing waits for (a document's local updates are in its journal already); the hub's thread also
    /// relays every keystroke, and a write waiting for a virus scanner held them all up.
    Later,
}

fn save<S: DocStore>(store: &mut S, space: SpaceId, id: DocId, doc: &mut Doc) -> std::io::Result<()> {
    save_as(store, space, id, doc, Patience::Wait)
}

/// [`save`], waiting for a holder of a document of a file's files or not (recents always wait: an add is answered
/// once it is on disk).
fn save_as<S: DocStore>(store: &mut S, space: SpaceId, id: DocId, doc: &mut Doc, patience: Patience) -> std::io::Result<()> {
    let snapshot = doc.body.copy().snapshot().map_err(std::io::Error::other)?;
    match &doc.kind {
        Kind::Recents(app) => {
            let file = DocFile {
                format: DOC_FILE_FORMAT,
                space,
                doc: id,
                app: app.clone(),
                have: doc.have.clone(),
                next_client_seq: doc.next_client_seq,
                pending: doc.pending.clone(),
                snapshot,
                waiting: doc.waiting.clone(),
                epoch: doc.epoch,
                pushing: doc.pushing.clone(),
                own_peers: doc.own_peers.clone(),
                push_wait: doc.push_wait,
            };
            let mut bytes = postcard::to_stdvec(&file).map_err(std::io::Error::other)?;
            // (Nothing more to say: the file is as an older agent writes it.)
            if doc.dropped.is_some() {
                bytes.extend(postcard::to_stdvec(&DocFileMore { dropped: doc.dropped }).map_err(std::io::Error::other)?);
            }
            store.save(&id.to_string(), &bytes)?;
        }
        Kind::Document { node, app } => {
            let file = DocumentFile {
                format: DOCUMENT_FILE_FORMAT,
                space,
                doc: id,
                node: *node,
                app: app.clone(),
                have: doc.have.clone(),
                next_client_seq: doc.next_client_seq,
                pending: doc.pending.clone(),
                snapshot,
                waiting: doc.waiting.clone(),
                epoch: doc.epoch,
                pushing: doc.pushing.clone(),
                own_peers: doc.own_peers.clone(),
                push_wait: doc.push_wait,
                log_only: doc.log_only,
                dropped: doc.dropped,
            };
            let mut bytes = postcard::to_stdvec(&file).map_err(std::io::Error::other)?;
            // (Nothing more to say without fragments: the file is as an older agent writes it.)
            if !doc.fragments.is_empty() {
                bytes.extend(postcard::to_stdvec(&DocumentFileMore { fragments: doc.fragments.clone() }).map_err(std::io::Error::other)?);
            }
            let name = id.to_string();
            match patience {
                Patience::Wait => store.save_document(&name, &bytes)?,
                Patience::Later => match store.save_document_unless_busy(&name, &bytes) {
                    Ok(()) => {}
                    // Still dirty: written at the next of the documents' writes, not tried again after every frame
                    // (its journal has its local updates meanwhile).
                    Err(e) if crate::account::is_busy(&e) => {
                        doc.urgent = false;
                        return Ok(());
                    }
                    Err(e) => return Err(e),
                },
            }
            // Its journal's updates are in the file now (a crash before this finds them there and skips them).
            let cleared = match patience {
                Patience::Wait => store.clear_journal(&name).map(|()| true),
                // A journal held by another program stays as it is: its records are in the file now, and a load
                // skips them (by their numbers); the next write empties it.
                Patience::Later => match store.clear_journal_unless_busy(&name) {
                    Ok(()) => Ok(true),
                    Err(e) if crate::account::is_busy(&e) => Ok(false),
                    Err(e) => Err(e),
                },
            }?;
            if cleared {
                doc.journal = 0;
            }
        }
    }
    doc.dirty = false;
    doc.urgent = false;
    doc.on_disk = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::store::MemStore;

    fn entry(path: &str, opened_ms: i64) -> RecentEntry {
        RecentEntry { node: None, path: path.into(), title: path.into(), opened_ms, saved_ms: None, machine: "pc".into() }
    }

    fn app() -> AppName {
        AppName::new("wordcraft").unwrap()
    }

    fn peers() -> Peers {
        let mut next = 0u64;
        Box::new(move || {
            next += 1;
            next
        })
    }

    fn epoch(n: u8) -> Epoch {
        Epoch::from_uuid(uuid::Uuid::from_bytes([n; 16]))
    }

    fn welcome(n: u8) -> ServerFrame {
        ServerFrame::Welcome { protocol: 1, epoch: epoch(n) }
    }

    fn device() -> craft_cloud_proto::DeviceId {
        craft_cloud_proto::DeviceId::from_uuid(uuid::Uuid::from_bytes([9; 16]))
    }

    /// Acks every update in `frames` and those that follow (a push goes a part at a time), numbering them after
    /// `seq`; the bytes sent, in order.
    fn push_all(r: &mut Replica<MemStore>, doc: DocId, mut frames: Vec<ClientFrame>, mut seq: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut idle = 0;
        loop {
            let ups: Vec<(u64, Vec<u8>)> = frames
                .iter()
                .filter_map(|f| if let ClientFrame::Update { client_seq, bytes, .. } = f { Some((*client_seq, bytes.clone())) } else { None })
                .collect();
            if ups.is_empty() {
                // Others' operations go after a few ticks.
                idle += 1;
                if idle > PUSH_PATIENCE + 1 {
                    return out;
                }
                frames = r.resend();
                continue;
            }
            idle = 0;
            frames = Vec::new();
            for (client_seq, bytes) in ups {
                seq += 1;
                out.push(bytes);
                frames.extend(r.on_frame(ServerFrame::Ack { doc, client_seq, seq }).unwrap());
            }
        }
    }

    /// A replica following `app`, welcomed and caught up on an empty log.
    fn live(space: SpaceId, store: MemStore) -> Replica<MemStore> {
        let doc = recents_doc(space, &app());
        let mut r = Replica::open(store, space, peers()).unwrap();
        r.follow(&app());
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        r
    }

    /// An add made offline is on disk at once, survives a restart, and is sent once the link is up and caught up.
    #[test]
    fn an_offline_add_is_kept_and_sent_once_connected() {
        let store = MemStore::default();
        let space = SpaceId::generate();
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        assert!(r.add(&app(), [1; 16], entry("a.docx", 1)).unwrap().is_empty(), "offline: nothing to send");
        assert_eq!(r.take_changed().len(), 1);
        drop(r);
        // A crash: what is on disk comes back.
        let mut r = Replica::open(store, space, peers()).unwrap();
        assert_eq!(r.entries(&app()).len(), 1);
        assert_eq!(r.waiting(), 1);
        assert!(matches!(r.connected().as_slice(), [ClientFrame::Hello { .. }]));
        let doc = recents_doc(space, &app());
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Create { .. }, ClientFrame::Subscribe { after: 0, at: None, .. }]));
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Update { client_seq: 1, .. }]));
        r.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 1 }).unwrap();
        assert_eq!(r.waiting(), 0);
        // Live now: the next add goes out at once.
        assert_eq!(r.add(&app(), [2; 16], entry("b.docx", 2)).unwrap().len(), 1);
    }

    /// Found by the end-to-end test: the first add to an app's list while the link is up (its document is new, so it
    /// wasn't subscribed when the link came up) subscribes it, and the add follows the catch-up.
    #[test]
    fn an_add_to_a_new_document_while_up_subscribes_it() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.connected();
        assert!(r.on_frame(welcome(1)).unwrap().is_empty(), "no documents yet");
        let frames = r.add(&app(), [1; 16], entry("a.docx", 1)).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Create { .. }, ClientFrame::Subscribe { after: 0, .. }]), "{frames:?}");
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Update { client_seq: 1, .. }]), "{frames:?}");
    }

    /// The server names a head below what this machine has (head 3 now, 7 before): it catches up from the start (so
    /// what it has, and the `at` it names next, are of this log), and then the whole document goes up.
    #[test]
    fn a_server_behind_gets_the_whole_document_again() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let other = Recents::new(77);
        let mut log = Vec::new();
        for seq in 1..=7u64 {
            let u = other.add([seq as u8; 16], &entry(&format!("f{seq}.txt"), seq as i64)).unwrap();
            r.on_frame(ServerFrame::Update { doc, seq, bytes: u.clone(), device: device() }).unwrap();
            log.push(u);
        }
        r.add(&app(), [99; 16], entry("mine.docx", 99)).unwrap();
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 7, at: Some(_), .. })), "{frames:?}");
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 3 }).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Subscribe { after: 0, at: None, .. }]), "from the start: {frames:?}");
        for (i, u) in log.iter().take(3).enumerate() {
            r.on_frame(ServerFrame::Update { doc, seq: i as u64 + 1, bytes: u.clone(), device: device() }).unwrap();
        }
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 3 }).unwrap();
        let pushed = push_all(&mut r, doc, frames, 3);
        let server = Recents::new(900);
        for part in log.iter().take(3).chain(&pushed) {
            server.import(part).unwrap();
        }
        assert_eq!(server.view().len(), 8, "what the log lacked went up, the add too");
        assert!(pushed.iter().all(|p| !p.is_empty()) && r.waiting() == 0);
        // The next `at` names the update this log has at its end.
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        let (after, want) = (3 + pushed.len() as u64, update_hash(pushed.last().unwrap()));
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: n, at: Some(a), .. } if *n == after && *a == want)), "{frames:?}");
    }

    /// The server was restored, and other machines pushed until its head was back where this one had read
    /// to: `subscribed` names no head below this machine's, but the epoch in `welcome` is new. This machine catches up
    /// from the start (it gets the others' new updates) and sends its whole document again (its acked add, lost in the
    /// restore, is on the server again).
    #[test]
    fn a_restore_is_seen_by_its_epoch_even_when_the_log_caught_up() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let store = MemStore::default();
        let mut a = Replica::open(store.clone(), space, peers()).unwrap();
        a.add(&app(), [1; 16], entry("a-acked.docx", 1)).unwrap();
        a.connected();
        a.on_frame(welcome(1)).unwrap();
        a.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        a.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 1 }).unwrap();
        let b = Recents::new(500);
        let b1 = b.add([2; 16], &entry("b-old.docx", 2)).unwrap();
        a.on_frame(ServerFrame::Update { doc, seq: 2, bytes: b1, device: device() }).unwrap();
        a.flush().unwrap();
        a.disconnected();
        drop(a);
        // Restored to an empty log, new epoch; machine B adds two files (seq 1, 2) before A is back (after a restart).
        let b2 = b.add([3; 16], &entry("b-new1.docx", 3)).unwrap();
        let b3 = b.add([4; 16], &entry("b-new2.docx", 4)).unwrap();
        let mut a = Replica::open(store, space, peers()).unwrap();
        a.connected();
        let frames = a.on_frame(welcome(2)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 0, .. })), "from the start: {frames:?}");
        a.on_frame(ServerFrame::Update { doc, seq: 1, bytes: b2.clone(), device: device() }).unwrap();
        a.on_frame(ServerFrame::Update { doc, seq: 2, bytes: b3.clone(), device: device() }).unwrap();
        let frames = a.on_frame(ServerFrame::Subscribed { doc, head: 2 }).unwrap();
        let pushed = push_all(&mut a, doc, frames, 2);
        assert!(!pushed.is_empty(), "what the restored log lacks goes up");
        let server = Recents::new(900);
        for part in [&b2, &b3].into_iter().chain(&pushed) {
            server.import(part).unwrap();
        }
        assert!(server.view().iter().any(|e| e.path == "a-acked.docx"), "A's acked add is on the server again");
        let paths: Vec<String> = a.entries(&app()).into_iter().map(|e| e.path).collect();
        assert!(paths.contains(&"b-new1.docx".to_owned()) && paths.contains(&"b-new2.docx".to_owned()), "{paths:?}");
        // Seen once: the next connection under the same epoch is a plain catch-up.
        a.disconnected();
        a.connected();
        let frames = a.on_frame(welcome(2)).unwrap();
        let after = 2 + pushed.len() as u64;
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: n, .. } if *n == after)), "{frames:?}");
    }

    /// `subscribe` names the hash of the update this machine has at `after`; a server whose update
    /// there differs (restored, its epoch not reset yet) answers `resync_required`, and the document starts over.
    #[test]
    fn a_log_that_isnt_ours_at_after_is_caught_up_from_the_start() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.follow(&app());
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        let other = Recents::new(77);
        let u1 = other.add([5; 16], &entry("first.txt", 3)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: u1.clone(), device: device() }).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc, head: 1 }).unwrap();
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        let at = update_hash(&u1);
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 1, at: Some(a), .. } if *a == at)), "{frames:?}");
        let frames = r.on_frame(ServerFrame::error("resync_required", "not the update you have", Some(doc), None)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 0, at: None, .. })), "from the start: {frames:?}");
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        let pushed = push_all(&mut r, doc, frames, 0);
        assert_eq!(pushed, vec![u1], "and what the log lacks goes up again (another's, after a pause)");
    }

    /// An update that arrives before the one it depends on counts as had (Loro holds it); it is in the file, so after
    /// a restart it is still waiting and applies when its history comes.
    #[test]
    fn an_update_before_its_history_waits_in_loro_and_survives_a_restart() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let store = MemStore::default();
        let mut r = live(space, store.clone());
        let other = Recents::new(77);
        let u1 = other.add([5; 16], &entry("first.txt", 3)).unwrap();
        let u2 = other.add([6; 16], &entry("second.txt", 4)).unwrap();
        // A rogue's chain sends u2' (depends on its never-sent first commit) at seq 1; the honest pair follows.
        let rogue = Recents::new(4242);
        let _never = rogue.add([1; 16], &entry("never.txt", 1)).unwrap();
        let orphan = rogue.add([2; 16], &entry("orphan.txt", 2)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: orphan, device: device() }).unwrap();
        // u2 before u1 (another device's commits, as a whole-document push in parts can store them).
        r.on_frame(ServerFrame::Update { doc, seq: 2, bytes: u2, device: device() }).unwrap();
        assert_eq!((r.waiting_peers(), r.entries(&app()).len()), ([4242, 77].into(), 0), "both wait");
        r.flush().unwrap();
        drop(r);
        let mut r = Replica::open(store, space, peers()).unwrap();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 2, .. })), "both count as had: {frames:?}");
        r.on_frame(ServerFrame::Update { doc, seq: 3, bytes: u1, device: device() }).unwrap();
        assert_eq!(r.entries(&app()).len(), 2, "u2 applied with its history, after the restart");
        assert_eq!(r.waiting_peers(), [4242].into(), "only the orphan waits");
    }

    /// One update that never applies counts as had: it doesn't pin `have.upto`, so 10 000 more updates past it end no
    /// connection.
    #[test]
    fn a_never_applying_update_doesnt_pin_what_is_had() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.follow(&app());
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        let rogue = Recents::new(7777);
        let _never_sent = rogue.add([0; 16], &entry("never.txt", 1)).unwrap();
        let orphan = rogue.add([1; 16], &entry("orphan.txt", 2)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: orphan, device: device() }).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc, head: 1 }).unwrap();
        let honest = Recents::new(500);
        for i in 0..10_050u32 {
            let mut key = [0u8; 16];
            key[..4].copy_from_slice(&(10 + i).to_le_bytes());
            let u = honest.add(key, &entry(&format!("f{}.txt", i % 50), 10 + i64::from(i))).unwrap();
            assert!(r.on_frame(ServerFrame::Update { doc, seq: 2 + u64::from(i), bytes: u, device: device() }).is_ok(), "cut off at {i}");
        }
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 10_051, .. })), "{frames:?}");
        assert_eq!(r.waiting_peers(), [7777].into());
    }

    /// An ack before the updates numbered below it waits on the unacked update, which goes again on the next
    /// connection: counting it as had past a gap `at` doesn't cover would let a restore with the same prefix make this
    /// machine skip C's new update at that number and never send its own acked add again.
    #[test]
    fn an_early_ack_isnt_trusted_across_connections() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut a = Replica::open(MemStore::default(), space, peers()).unwrap();
        a.follow(&app());
        a.connected();
        a.on_frame(welcome(1)).unwrap();
        let b = Recents::new(500);
        let b1 = b.add([1; 16], &entry("b1.txt", 1)).unwrap();
        let b2 = b.add([2; 16], &entry("b2.txt", 2)).unwrap();
        a.on_frame(ServerFrame::Update { doc, seq: 1, bytes: b1.clone(), device: device() }).unwrap();
        a.on_frame(ServerFrame::Subscribed { doc, head: 1 }).unwrap();
        let frames = a.add(&app(), [3; 16], entry("a-acked.txt", 3)).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Update { client_seq: 1, .. }]), "{frames:?}");
        a.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 3 }).unwrap();
        a.disconnected();
        let c = Recents::new(600);
        c.import(&b1).unwrap();
        c.import(&b2).unwrap();
        let c3 = c.add([4; 16], &entry("c-new.txt", 4)).unwrap();
        a.connected();
        let frames = a.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 1, at: Some(_), .. })), "{frames:?}");
        a.on_frame(ServerFrame::Update { doc, seq: 2, bytes: b2, device: device() }).unwrap();
        a.on_frame(ServerFrame::Update { doc, seq: 3, bytes: c3, device: device() }).unwrap();
        let up = a.on_frame(ServerFrame::Subscribed { doc, head: 3 }).unwrap();
        let paths: Vec<String> = a.entries(&app()).into_iter().map(|e| e.path).collect();
        assert!(paths.contains(&"c-new.txt".to_owned()), "C's add was skipped: {paths:?}");
        assert!(matches!(up.as_slice(), [ClientFrame::Update { client_seq: 1, .. }]), "A's acked add goes up again: {up:?}");
        a.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 4 }).unwrap();
        assert_eq!(a.waiting(), 0);
    }

    /// An ack before the updates below it, on one connection: counted once they came, then the next ones follow.
    #[test]
    fn an_early_ack_counts_once_the_updates_below_it_came() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        r.add(&app(), [3; 16], entry("mine.txt", 3)).unwrap();
        r.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 2 }).unwrap();
        assert_eq!(r.waiting(), 1, "not had yet: seq 1 is on its way");
        let other = Recents::new(77);
        let u1 = other.add([5; 16], &entry("first.txt", 1)).unwrap();
        let u3 = other.add([6; 16], &entry("third.txt", 4)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: u1, device: device() }).unwrap();
        assert_eq!(r.waiting(), 0, "seq 2 is ours");
        assert!(r.on_frame(ServerFrame::Update { doc, seq: 3, bytes: u3, device: device() }).unwrap().is_empty(), "no gap");
        assert_eq!(r.entries(&app()).len(), 3);
    }

    /// An own update that a row brought back before its ack (sent once on an old connection, again on this one): the
    /// ack, numbering a second row with the same bytes that this machine is never sent, still counts once the rows
    /// below it came, so the log goes on without a gap and nothing waits.
    #[test]
    fn an_ack_for_an_update_a_row_brought_back_still_counts() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let frames = r.add(&app(), [3; 16], entry("mine.txt", 3)).unwrap();
        let [ClientFrame::Update { bytes: mine, .. }] = frames.as_slice() else { panic!("{frames:?}") };
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: mine.clone(), device: device() }).unwrap();
        assert_eq!(r.waiting(), 0, "the log has it");
        r.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 3 }).unwrap();
        let other = Recents::new(77);
        let u2 = other.add([5; 16], &entry("second.txt", 1)).unwrap();
        let u4 = other.add([6; 16], &entry("fourth.txt", 4)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 2, bytes: u2, device: device() }).unwrap();
        assert!(r.on_frame(ServerFrame::Update { doc, seq: 4, bytes: u4, device: device() }).unwrap().is_empty(), "no gap at 3");
        assert_eq!(r.entries(&app()).len(), 3);
        assert_eq!(r.waiting(), 0);
    }

    /// A gap on a caught-up document (frames lost on an old connection): a catch-up from what it has.
    #[test]
    fn a_gap_is_caught_up() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let frames = r.on_frame(ServerFrame::Update { doc, seq: 2, bytes: vec![1, 2, 3], device: device() }).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Subscribe { after: 0, .. }]), "{frames:?}");
        assert!(r.on_frame(ServerFrame::Update { doc, seq: 3, bytes: vec![1], device: device() }).unwrap().is_empty(), "once");
    }

    /// After `quota_exceeded` the document is marked full, every unacked update stays (none is dropped), nothing is sent until a growing pause is over (or the next connection), and then everything goes
    /// again.
    #[test]
    fn a_full_document_keeps_every_update() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        r.add(&app(), [1; 16], entry("a.txt", 1)).unwrap();
        r.on_frame(ServerFrame::error("quota_exceeded", "full", Some(doc), Some(1))).unwrap();
        assert!(r.is_full(&app()));
        assert!(r.add(&app(), [2; 16], entry("b.txt", 2)).unwrap().is_empty(), "nothing sent while full");
        assert_eq!(r.waiting(), 2, "both kept");
        assert!(r.wants_tick(), "a try again is due");
        let frames = r.resend();
        assert_eq!(frames.len(), 2, "tried again after one tick: {frames:?}");
        r.on_frame(ServerFrame::error("quota_exceeded", "full", Some(doc), Some(1))).unwrap();
        r.on_frame(ServerFrame::error("quota_exceeded", "full", Some(doc), Some(2))).unwrap();
        assert!(r.resend().is_empty(), "the pause grew");
        assert_eq!(r.resend().len(), 2);
        r.disconnected();
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert_eq!(frames.len(), 2, "both go again: {frames:?}");
    }

    /// A create refused at the account's live-document limit pauses that one list and says so; the link stays up and
    /// the other lists carry on.
    #[test]
    fn a_refused_create_at_the_document_limit_pauses_without_dropping_the_link() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let other_app = AppName::new("sheetcraft").unwrap();
        let other = recents_doc(space, &other_app);
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.follow(&app());
        r.follow(&other_app);
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc: other, head: 0 }).unwrap();
        let refused = r.on_frame(ServerFrame::error("quota_exceeded", "too many live documents", Some(doc), None));
        assert!(refused.is_ok(), "the link stays up: {refused:?}");
        assert!(refused.unwrap().is_empty());
        assert!(r.docs[&doc].sub == Sub::No, "the refused list is not subscribed");
        assert!(r.docs[&doc].retry.is_some(), "a try again is paused");
        assert!(r.docs[&other].sub == Sub::Live, "the other list is unaffected");
        assert!(r.take_notes().iter().any(|n| n.contains("recents of wordcraft")), "the user is told");
        // The subscribe after the refused create comes back not_found: still paused, and nothing more is said.
        let again = r.on_frame(ServerFrame::error("not_found", "document not found", Some(doc), None));
        assert!(again.is_ok_and(|f| f.is_empty()));
        assert!(r.docs[&doc].retry.is_some(), "still paused");
        assert!(r.take_notes().is_empty(), "no second note");
    }

    /// Loro failing on an update may leave the document half-changed: it is read back from its file, the update is
    /// skipped when it comes again, and what followed it applies.
    #[test]
    fn a_loro_failure_reads_the_document_back_and_skips_the_update() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let store = MemStore::default();
        let mut r = live(space, store);
        let other = Recents::new(77);
        let u1 = other.add([5; 16], &entry("first.txt", 1)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: u1, device: device() }).unwrap();
        r.flush().unwrap();
        let frames = r.recover(doc, app(), 2, update_hash(b"bad"));
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 1, .. })), "{frames:?}");
        assert_eq!(r.entries(&app()).len(), 1, "as the file has it");
        assert!(r.on_frame(ServerFrame::Update { doc, seq: 2, bytes: b"bad".to_vec(), device: device() }).unwrap().is_empty());
        let u3 = other.add([6; 16], &entry("third.txt", 3)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 3, bytes: u3, device: device() }).unwrap();
        assert_eq!(r.entries(&app()).len(), 2);
        assert_eq!(r.take_notes().len(), 1);
    }

    /// A replica on `store` with a document of a file joined from an import ("import", peer 40), its file written.
    fn joined(store: &MemStore, space: SpaceId, node: NodeId) -> (Replica<MemStore>, DocId, Vec<u8>) {
        let importer = text_doc(40);
        insert(&importer, 0, "import");
        let import = importer.export(loro::ExportMode::Snapshot).unwrap();
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let (id, _, _) = r.open_document(node, &app());
        snapshot_in(&mut r, id, 0, &import);
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        r.flush_documents().unwrap();
        (r, id, import)
    }

    /// The text of document `id` as `r` has it.
    fn text_of(r: &Replica<MemStore>, id: DocId) -> String {
        let d = loro::LoroDoc::new();
        d.import(&r.snapshot_of(id).unwrap()).unwrap();
        d.get_text("text").to_string()
    }

    /// The snapshot a catch-up starts with is downloaded through the chunk API: the catch-up's updates and its
    /// `subscribed` wait for it. One that can't be got makes this machine follow the whole log instead.
    #[test]
    fn a_catch_up_waits_for_its_snapshot_and_reads_the_whole_log_without_it() {
        for got in [true, false] {
            let (space, node) = (SpaceId::generate(), NodeId::generate());
            let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
            r.connected();
            r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
            let (id, _, _) = r.open_document(node, &app());
            let server = text_doc(40);
            insert(&server, 0, "import");
            insert(&server, 0, "<1>");
            let r2 = insert(&server, 0, "<2>");
            let snapshot = server.export(loro::ExportMode::Snapshot).unwrap();
            let r3 = insert(&server, 0, "<3>");
            let manifest = cut(id, &snapshot).unwrap().id();
            assert!(r.on_frame(ServerFrame::Snapshot { doc: id, base: 2, manifest }).unwrap().is_empty());
            assert_eq!(r.take_fetches(), vec![(id, manifest)]);
            for (seq, bytes) in [(2, r2.clone()), (3, r3.clone())] {
                assert!(r.on_frame(ServerFrame::Update { doc: id, seq, bytes, device: device() }).unwrap().is_empty());
            }
            assert!(r.on_frame(ServerFrame::Subscribed { doc: id, head: 3 }).unwrap().is_empty(), "waits for the snapshot");
            assert!(!r.has_document(id));
            if got {
                r.on_snapshot(id, manifest, Ok(snapshot.clone()));
                assert!(r.take_events().contains(&DocEvent::Joined(id)));
                assert_eq!(text_of(&r, id), "<3><2><1>import");
            } else {
                let frames = r.on_snapshot(id, manifest, Err("gone".into()));
                assert!(frames.iter().any(|f| matches!(f, ClientFrame::SubscribeLog { after: 0, .. })), "{frames:?}");
                assert!(r.take_notes().iter().any(|n| n.contains("can't be got")));
            }
        }
    }

    /// A journal append that fails part-way, or whose flush fails, is refused to the app and leaves nothing a crash
    /// would bring back or that would hide the changes acked after it.
    #[test]
    fn a_failed_journal_append_hides_no_later_change() {
        use crate::live::store::JournalFailure;
        for how in [JournalFailure::Torn, JournalFailure::Unflushed] {
            let store = MemStore::default();
            let (space, node) = (SpaceId::generate(), NodeId::generate());
            let (mut r, id, import) = joined(&store, space, node);
            let edit = |peer: u64, s: &str| {
                let d = text_doc(peer);
                d.import(&import).unwrap();
                insert(&d, 0, s)
            };
            store.fail_journal_as(Some(how));
            assert!(r.local_update(id, &edit(50, "<refused>")).is_err(), "{how:?}");
            store.fail_journal_as(None);
            r.local_update(id, &edit(51, "<kept>")).unwrap();
            // A crash before the document is next written.
            drop(r);
            let mut r = Replica::open(store.clone(), space, peers()).unwrap();
            r.open_document(node, &app());
            let text = text_of(&r, id);
            assert!(text.contains("<kept>"), "{how:?}: an acked change is lost: {text:?}");
            assert!(!text.contains("<refused>"), "{how:?}: a refused change came back: {text:?}");
        }
    }

    /// At start only documents with something unacked or waiting are loaded; the rest stay on disk until an app opens
    /// one. Past [`MAX_LOADED_DOCUMENTS`] loaded, the least recently used one with nothing unacked or waiting and no app
    /// holding it is unloaded (written first, so nothing is lost).
    #[test]
    fn documents_load_when_opened_and_unload_past_the_cap() {
        let store = MemStore::default();
        let space = SpaceId::generate();
        let importer = text_doc(40);
        insert(&importer, 0, "import");
        let import = importer.export(loro::ExportMode::Snapshot).unwrap();
        let nodes: Vec<NodeId> = (0..300).map(|_| NodeId::generate()).collect();
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let mut ids = Vec::new();
        for node in &nodes {
            let (id, _, _) = r.open_document(*node, &app());
            snapshot_in(&mut r, id, 0, &import);
            r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
            ids.push(id);
        }
        assert!(r.loaded_documents() <= MAX_LOADED_DOCUMENTS, "{} loaded", r.loaded_documents());
        // One with a change not yet acked.
        let (_, opening, _) = r.open_document(nodes[0], &app());
        assert_eq!(opening, Opening::Live);
        let d = text_doc(50);
        d.import(&import).unwrap();
        r.local_update(ids[0], &insert(&d, 0, "<unacked>")).unwrap();
        r.flush_documents().unwrap();
        drop(r);
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        assert_eq!(r.loaded_documents(), 1, "only the one with something to send");
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let (_, opening, frames) = r.open_document(nodes[7], &app());
        assert_eq!(opening, Opening::Live, "loaded when opened, from disk");
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { doc, .. } if *doc == ids[7])), "{frames:?}");
        assert_eq!(r.loaded_documents(), 2);
        assert!(text_of(&r, ids[7]).contains("import"));
        for node in &nodes {
            assert_eq!(r.open_document(*node, &app()).1, Opening::Live);
        }
        assert!(r.loaded_documents() <= MAX_LOADED_DOCUMENTS, "{} loaded", r.loaded_documents());
        assert!(text_of(&r, ids[0]).contains("<unacked>"), "the one with a change waiting stays");
    }

    /// A machine with no copy of a document the server has but holds nothing of (an empty log, no snapshot: an admin
    /// reset it, and its machines haven't sent it back yet) doesn't serve it to an app as live: the file is what to
    /// open, and the join is let go of.
    #[test]
    fn a_reset_document_isnt_served_empty_to_a_machine_without_a_copy() {
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let (id, opening, _) = r.open_document(node, &app());
        assert_eq!(opening, Opening::Joining);
        let frames = r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        let events = r.take_events();
        assert!(events.iter().any(|e| matches!(e, DocEvent::NotLive(d, NotLiveReason::Resetting, _) if *d == id)), "{events:?}");
        assert!(!r.has_document(id), "not live here");
        assert!(frames.contains(&ClientFrame::Unsubscribe { doc: id }), "{frames:?}");
        // Once a machine sent it back (rows, or an import's snapshot), it joins.
        let (_, opening, _) = r.open_document(node, &app());
        assert_eq!(opening, Opening::Joining);
        let other = text_doc(60);
        let row = insert(&other, 0, "back");
        r.on_frame(ServerFrame::Update { doc: id, seq: 1, bytes: row, device: device() }).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 1 }).unwrap();
        assert!(r.take_events().contains(&DocEvent::Joined(id)));
    }

    /// After a restore (a new epoch) a document goes up whole: what this machine's apps wrote is its own, so it goes
    /// at once (no random wait), and a change an app makes meanwhile goes at once too, not behind the push.
    #[test]
    fn an_apps_changes_are_its_machines_own_in_a_push() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        // This machine's app imported the file (peer 40) and then edited it (peer 50).
        let app_doc = text_doc(40);
        insert(&app_doc, 0, "import");
        let import = app_doc.export(loro::ExportMode::Snapshot).unwrap();
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let (id, _, _) = r.open_document(node, &app());
        r.on_frame(ServerFrame::error("not_found", "no such document", Some(id), None)).unwrap();
        assert!(r.take_events().contains(&DocEvent::NeedsImport(id)));
        let frames = r.import_document(id, &import).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::CreateDocument { .. }]), "{frames:?}");
        r.on_frame(ServerFrame::CreatedDocument { doc: id, kept: true }).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        let d = text_doc(50);
        d.import(&import).unwrap();
        r.local_update(id, &insert(&d, 0, "<mine>")).unwrap();
        r.on_frame(ServerFrame::Ack { doc: id, client_seq: 1, seq: 1 }).unwrap();
        // The server is restored from a backup that has none of it.
        r.disconnected();
        r.connected();
        let frames = r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(2) }).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 0, .. })), "{frames:?}");
        let frames = r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Update { .. })), "its own goes up at once: {frames:?}");
        // A change its app makes meanwhile goes at once too, not behind the push.
        let typed = r.local_update(id, &insert(&d, 0, "<typed>")).unwrap();
        assert!(matches!(typed.as_slice(), [ClientFrame::Update { .. }]), "{typed:?}");
    }

    /// A write now and then that another program refuses loses nothing. A document whose file is held stays dirty
    /// and is written at the next write. One whose journal is held is written, with the journal kept as it was. Either
    /// way, a crash then (or after more changes) reads every acked change back, each once.
    #[test]
    fn a_write_another_program_refuses_is_made_later_and_loses_nothing() {
        use crate::live::store::Held;
        for held in [Held::Documents, Held::Journals] {
            let store = MemStore::default();
            let (space, node) = (SpaceId::generate(), NodeId::generate());
            let (mut r, id, import) = joined(&store, space, node);
            let name = id.to_string();
            let edit = |peer: u64, s: &str| {
                let d = text_doc(peer);
                d.import(&import).unwrap();
                insert(&d, 0, s)
            };
            r.local_update(id, &edit(50, "<a>")).unwrap();
            let (file, journal) = (store.documents().get(&name).cloned(), store.journals().get(&name).cloned());
            store.hold(Some(held));
            r.flush_documents_now_and_then().unwrap();
            r.flush().unwrap();
            match held {
                Held::Documents => {
                    assert_eq!(store.documents().get(&name), file.as_ref(), "the held file isn't written");
                    assert_eq!(store.journals().get(&name), journal.as_ref(), "nor its journal emptied");
                }
                Held::Journals => {
                    assert_ne!(store.documents().get(&name), file.as_ref(), "the file is written");
                    assert_eq!(store.journals().get(&name), journal.as_ref(), "the held journal is kept as it was");
                }
            }
            r.local_update(id, &edit(51, "<b>")).unwrap();
            // A crash with the files held.
            drop(r);
            let mut r = Replica::open(store.clone(), space, peers()).unwrap();
            r.open_document(node, &app());
            let text = text_of(&r, id);
            assert!(text.contains("<a>") && text.contains("<b>"), "{held:?}: an acked change is lost: {text:?}");
            assert_eq!(r.document_status(id).map(|(_, waiting)| waiting), Some(2), "{held:?}: each change once");
            // Released: the next write now and then writes it and empties the journal.
            store.hold(None);
            r.local_update(id, &edit(52, "<c>")).unwrap();
            r.flush_documents_now_and_then().unwrap();
            assert_eq!(store.journals().get(&name).map_or(0, Vec::len), 0, "{held:?}");
            drop(r);
            let mut r = Replica::open(store.clone(), space, peers()).unwrap();
            r.open_document(node, &app());
            let text = text_of(&r, id);
            assert!(["<a>", "<b>", "<c>"].iter().all(|s| text.contains(s)), "{held:?}: {text:?}");
        }
    }

    /// A journal is written into its document's file at the next write once it holds over [`MAX_JOURNAL_BYTES`], not
    /// before (the file is written now and then besides).
    #[test]
    fn a_journal_past_its_cap_is_written_into_the_file() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        let d = text_doc(50);
        d.import(&import).unwrap();
        let name = id.to_string();
        let big = "x".repeat(900_000);
        for i in 0..4 {
            r.local_update(id, &put(&d, i, &big)).unwrap();
        }
        let held = store.journals().get(&name).map_or(0, Vec::len);
        assert!(held < MAX_JOURNAL_BYTES, "{held}");
        r.flush().unwrap();
        assert_eq!(store.journals().get(&name).map_or(0, Vec::len), held, "under the cap: left for the next write");
        r.local_update(id, &put(&d, 4, &big)).unwrap();
        assert!(store.journals().get(&name).map_or(0, Vec::len) > MAX_JOURNAL_BYTES);
        r.flush().unwrap();
        assert_eq!(store.journals().get(&name).map_or(0, Vec::len), 0, "over it: written into the file");
    }

    /// A catch-up's updates are taken a batch at a time: once more than [`MAX_BATCH_BYTES`] wait, they are taken
    /// before `subscribed`.
    #[test]
    fn a_catch_up_batch_is_taken_past_its_cap() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        r.disconnected();
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let other = text_doc(60);
        other.import(&import).unwrap();
        let big = "y".repeat(1_000_000);
        let mut batched = 0;
        let mut seq = 0;
        while batched <= MAX_BATCH_BYTES {
            let row = put(&other, seq, &big);
            batched += row.len();
            seq += 1;
            assert!(r.describe_doc(id).contains("upto 0"), "not taken at {batched} bytes: {}", r.describe_doc(id));
            r.on_frame(ServerFrame::Update { doc: id, seq, bytes: row, device: device() }).unwrap();
        }
        assert!(r.describe_doc(id).contains(&format!("upto {seq}")), "taken past the cap: {}", r.describe_doc(id));
    }

    /// A document read back from its file after a refused change (its journal failed) is followed again at the next
    /// tick, not left unfollowed while the link is up.
    #[test]
    fn a_document_read_back_after_a_refused_change_is_followed_again() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        let d = text_doc(50);
        d.import(&import).unwrap();
        store.fail_journal(true);
        assert!(r.local_update(id, &insert(&d, 0, "x")).is_err());
        store.fail_journal(false);
        let frames = r.resend();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { doc, .. } if *doc == id)), "{frames:?}");
    }

    /// A journal whose end doesn't read (cut off by a crash) is read up to there and cut there, so what is appended
    /// after it reads.
    #[test]
    fn a_journal_is_cut_at_its_first_bad_record() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        let d = text_doc(50);
        d.import(&import).unwrap();
        r.local_update(id, &insert(&d, 0, "<one>")).unwrap();
        drop(r);
        let name = id.to_string();
        let good = store.journals().get(&name).cloned().unwrap();
        let mut s = store.clone();
        s.append_journal(&name, &[9, 0, 0, 0, 1, 2]).unwrap();
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.open_document(node, &app());
        assert_eq!(store.journals().get(&name), Some(&good), "cut at the bad record");
        r.local_update(id, &insert(&d, 0, "<two>")).unwrap();
        drop(r);
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.open_document(node, &app());
        let text = text_of(&r, id);
        assert!(text.contains("<one>") && text.contains("<two>"), "{text:?}");
    }

    /// Serves `rows` (numbered from 1) to every `subscribe` in `frames` until none is asked for; the rounds it took.
    fn serve_catch_ups(r: &mut Replica<MemStore>, doc: DocId, rows: &[Vec<u8>], mut frames: Vec<ClientFrame>) -> usize {
        let mut rounds = 0;
        while let Some(after) = frames.iter().find_map(|f| match f {
            ClientFrame::Subscribe { after, .. } => Some(*after),
            _ => None,
        }) {
            rounds += 1;
            if rounds > 10 {
                break;
            }
            frames = Vec::new();
            for (i, row) in rows.iter().enumerate() {
                let seq = i as u64 + 1;
                if seq > after {
                    frames.extend(r.on_frame(ServerFrame::Update { doc, seq, bytes: row.clone(), device: device() }).unwrap());
                }
            }
            frames.extend(r.on_frame(ServerFrame::Subscribed { doc, head: rows.len() as u64 }).unwrap());
        }
        rounds
    }

    /// Loro fails on one row of a catch-up taken as a batch: the catch-up is taken again one row at a time, only that
    /// row is dropped (and said), and the document ends live with every other row, in a few rounds.
    #[test]
    fn a_row_loro_fails_on_in_a_batch_is_found_and_only_it_dropped() {
        let space = SpaceId::generate();
        let node = NodeId::generate();
        let w2 = ServerFrame::Welcome { protocol: 2, epoch: epoch(1) };
        let importer = text_doc(40);
        insert(&importer, 0, "import");
        let import = importer.export(loro::ExportMode::Snapshot).unwrap();
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.connected();
        r.on_frame(w2.clone()).unwrap();
        let (id, _, _) = r.open_document(node, &app());
        snapshot_in(&mut r, id, 0, &import);
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        r.flush_documents().unwrap();
        // (Each row from a machine of its own: none needs another's operations.)
        let rows: Vec<Vec<u8>> = (1..=5u64)
            .map(|i| {
                let d = text_doc(70 + i);
                d.import(&import).unwrap();
                insert(&d, 0, &format!("<{i}>"))
            })
            .collect();
        crate::live::copy::fail_on(&rows[2]);
        r.disconnected();
        r.connected();
        let frames = r.on_frame(w2).unwrap();
        let rounds = serve_catch_ups(&mut r, id, &rows, frames);
        assert!(rounds <= 4, "it goes round: {rounds} rounds; {}", r.describe_doc(id));
        let text = {
            let d = loro::LoroDoc::new();
            d.import(&r.snapshot_of(id).unwrap()).unwrap();
            d.get_text("text").to_string()
        };
        for i in [1, 2, 4, 5] {
            assert!(text.contains(&format!("<{i}>")), "row {i} is in: {text:?}");
        }
        assert!(!text.contains("<3>"), "{text:?}");
        assert!(r.take_notes().iter().any(|n| n.contains("made Loro fail")), "said");
        assert_eq!(r.document_status(id), Some((true, 0)), "live: {}", r.describe_doc(id));
    }

    /// Loro fails on a second row in one run: the document stops catching up (no loop), and its apps are told.
    #[test]
    fn a_second_loro_failure_in_a_run_stops_the_document() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        r.flush().unwrap();
        let other = Recents::new(77);
        let rows: Vec<Vec<u8>> = (1..=5u8).map(|i| other.add([i; 16], &entry(&format!("{i}.txt"), i64::from(i))).unwrap()).collect();
        crate::live::copy::fail_on(&rows[1]);
        crate::live::copy::fail_on(&rows[3]);
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        let rounds = serve_catch_ups(&mut r, doc, &rows, frames);
        assert!(rounds <= 5, "it goes round: {rounds} rounds; {}", r.describe(&app()));
        assert!(r.is_stopped(doc), "{}", r.describe(&app()));
        assert!(r.take_notes().iter().any(|n| n.contains("stops following")), "said");
        assert!(r.resend().is_empty(), "nothing tried again in this run");
        r.disconnected();
        r.connected();
        assert!(!r.on_frame(welcome(1)).unwrap().iter().any(|f| matches!(f, ClientFrame::Subscribe { .. })), "nor on the next connection");
    }

    /// Waiting updates are capped by bytes: past the cap the document says so and takes nothing more on this connection.
    #[test]
    fn waiting_updates_are_capped_by_bytes() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let rogue = loro::LoroDoc::new();
        let _ = rogue.set_peer_id(4242);
        rogue.get_map("entries").insert("never", "sent").unwrap();
        rogue.commit();
        let mut seq = 0;
        let mut total = 0;
        while total <= MAX_WAITING_BYTES {
            let vv = rogue.oplog_vv();
            rogue.get_map("entries").insert(&format!("k{seq}"), "x".repeat(60_000)).unwrap();
            rogue.commit();
            let bytes = rogue.export(loro::ExportMode::updates(&vv)).unwrap();
            total += bytes.len();
            seq += 1;
            r.on_frame(ServerFrame::Update { doc, seq, bytes, device: device() }).unwrap();
        }
        assert_eq!(r.take_notes().len(), 1, "said");
        let frames = r.on_frame(ServerFrame::Update { doc, seq: seq + 1, bytes: vec![1], device: device() }).unwrap();
        assert!(frames.is_empty(), "nothing more taken, no catch-up either");
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after, .. } if *after == seq - 1)), "{frames:?}");
    }

    /// A start from the beginning (a log emptied by the admin) drops the waiting updates (the catch-up brings the log
    /// again): orphans held up to just under the cap would otherwise stick the document at the first honest part that
    /// waits a moment.
    #[test]
    fn waiting_orphans_dont_survive_a_start_over() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let rogue = loro::LoroDoc::new();
        let _ = rogue.set_peer_id(4242);
        rogue.get_map("entries").insert("never", "sent").unwrap();
        rogue.commit();
        let mut seq = 0;
        let mut total = 0;
        for (size, margin) in [(60_000usize, 70_000usize), (1_000, 2_000), (50, 400)] {
            while total + margin < MAX_WAITING_BYTES {
                let vv = rogue.oplog_vv();
                rogue.get_map("entries").insert(&format!("k{seq}"), "x".repeat(size)).unwrap();
                rogue.commit();
                let bytes = rogue.export(loro::ExportMode::updates(&vv)).unwrap();
                total += bytes.len();
                seq += 1;
                r.on_frame(ServerFrame::Update { doc, seq, bytes, device: device() }).unwrap();
            }
        }
        assert!(r.take_notes().is_empty(), "not stuck before the reset");
        r.flush().unwrap();
        // The log behind (emptied): from the start.
        r.disconnected();
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 0, .. })), "{frames:?}");
        assert!(!r.waiting_peers().contains(&4242), "the orphans went with the start over");
        let honest = Recents::new(77);
        let p1 = honest.add([5; 16], &entry("first.txt", 1)).unwrap();
        let p2 = honest.add([6; 16], &entry(&format!("{}.txt", "s".repeat(1200)), 2)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: p2, device: device() }).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 2, bytes: p1, device: device() }).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc, head: 2 }).unwrap();
        // (The second's title is too long to show: it is hidden, but in.)
        assert_eq!(r.entries(&app()).iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["first.txt"], "the honest parts applied");
        assert!(r.waiting_peers().is_empty(), "nothing waits");
        assert!(r.take_notes().is_empty(), "not stuck");
        r.disconnected();
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 2, .. })), "{frames:?}");
        // Nor do they come back from the file.
        r.flush().unwrap();
        let store = r.store.clone();
        let again = Replica::open(store, space, peers()).unwrap();
        assert!(!again.waiting_peers().contains(&4242));
    }

    /// Loro failing on a waiting update imported again isn't blamed on the incoming update: the waiting one is dropped,
    /// the document read back, and the incoming update applies when it comes again.
    #[test]
    fn a_waiting_update_loro_fails_on_is_dropped_not_the_incoming_one() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let rogue = Recents::new(4242);
        let _never = rogue.add([1; 16], &entry("never.txt", 1)).unwrap();
        let orphan = rogue.add([2; 16], &entry("orphan.txt", 2)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes: orphan.clone(), device: device() }).unwrap();
        r.flush().unwrap();
        let frames = r.recover_waiting(doc, app(), update_hash(&orphan));
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 1, .. })), "{frames:?}");
        assert!(r.waiting_peers().is_empty(), "the waiting one is dropped");
        let other = Recents::new(77);
        let u2 = other.add([5; 16], &entry("next.txt", 3)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 2, bytes: u2, device: device() }).unwrap();
        assert_eq!(r.entries(&app()).len(), 1, "the incoming one isn't skipped");
        assert_eq!(r.take_notes().len(), 1);
    }

    /// A file Loro fails on (or that doesn't read) is set aside (kept), never dropped with its unacked adds, and the document starts afresh from its unacked updates, which are kept and sent after a catch-up
    /// from the start.
    #[test]
    fn an_unreadable_file_is_set_aside_and_its_unsent_adds_kept() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mine = Recents::new(5);
        let add = mine.add([1; 16], &entry("unsent.txt", 1)).unwrap();
        let file = DocFile {
            format: DOC_FILE_FORMAT,
            space,
            doc,
            app: app(),
            have: Have { upto: 9, upto_hash: Some([1; 32]) },
            next_client_seq: 2,
            pending: vec![Pending { client_seq: 1, bytes: add, acked: None, push: false }],
            snapshot: vec![1, 2, 3],
            waiting: Vec::new(),
            epoch: Some(epoch(1)),
            pushing: None,
            own_peers: vec![5],
            push_wait: None,
        };
        let mut store = MemStore::default();
        store.save(&doc.to_string(), &postcard::to_stdvec(&file).unwrap()).unwrap();
        store.save("garbage", &[0xff; 7]).unwrap();
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        let notes = r.take_notes();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(store.set_aside_files().len(), 2, "both copied aside");
        assert_eq!((r.waiting(), r.entries(&app()).len()), (1, 1), "the unsent add is kept, and shown");
        // Before `open` returns, the rebuilt document is on disk over the original, and the file that
        // didn't read at all is gone: the next load neither sets them aside again nor loses the unsent add.
        let files = store.load().unwrap();
        assert!(!files.contains_key("garbage"));
        let again = Replica::open(store.clone(), space, peers()).unwrap();
        assert_eq!((again.waiting(), again.entries(&app()).len()), (1, 1));
        assert_eq!(store.set_aside_files().len(), 2, "nothing set aside again");
        r.connected();
        let frames = r.on_frame(welcome(1)).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 0, .. })), "from the start: {frames:?}");
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Update { client_seq: 1, .. }]), "and sent: {frames:?}");
    }

    /// A document whose own snapshot is over 1 MiB loads: the limit is on updates from the server only (else it would
    /// be set aside at every load, its history gone from the list and the copies piling up).
    #[test]
    fn a_snapshot_over_a_mebibyte_loads() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let recents = Recents::new(5);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for i in 0..5000u32 {
            let mut key = [0u8; 16];
            key[..4].copy_from_slice(&i.to_le_bytes());
            let title: String = (0..250)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    char::from(b'a' + (state % 26) as u8)
                })
                .collect();
            recents.add(key, &RecentEntry { title, ..entry(&format!("f{i}.txt"), i64::from(i)) }).unwrap();
        }
        let snapshot = recents.snapshot().unwrap();
        assert!(snapshot.len() > MAX_UPDATE_BYTES, "{} bytes", snapshot.len());
        let shown = recents.view();
        let file = DocFile {
            format: DOC_FILE_FORMAT,
            space,
            doc,
            app: app(),
            have: Have::default(),
            next_client_seq: 1,
            pending: Vec::new(),
            snapshot,
            waiting: Vec::new(),
            epoch: None,
            pushing: None,
            own_peers: vec![5],
            push_wait: None,
        };
        let mut store = MemStore::default();
        store.save(&doc.to_string(), &postcard::to_stdvec(&file).unwrap()).unwrap();
        let mut again = Replica::open(store.clone(), space, peers()).unwrap();
        assert!(again.take_notes().is_empty(), "set aside");
        assert!(store.set_aside_files().is_empty());
        assert_eq!(again.entries(&app()), shown);
    }

    /// A create or subscribe refused for a moment is tried again after a pause (growing), not given up
    /// with a note about the device's cap; a subscribe that finds no document (its create was refused) doesn't end the
    /// connection either.
    #[test]
    fn a_refused_subscribe_is_tried_again_with_a_pause() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.follow(&app());
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        assert!(r.on_frame(ServerFrame::error("rate_limited", "too many frames", Some(doc), None)).unwrap().is_empty());
        assert!(r.on_frame(ServerFrame::error("not_found", "no such document", Some(doc), None)).is_ok(), "not the end of the link");
        assert!(r.wants_tick());
        let mut ticks = 0;
        while !r.resend().iter().any(|f| matches!(f, ClientFrame::Subscribe { .. })) {
            ticks += 1;
            assert!(ticks < 10);
        }
        r.on_frame(ServerFrame::error("rate_limited", "too many frames", Some(doc), None)).unwrap();
        let mut again = 0;
        while !r.resend().iter().any(|f| matches!(f, ClientFrame::Subscribe { .. })) {
            again += 1;
            assert!(again < 10);
        }
        assert!(again > ticks, "the pause grows ({ticks} then {again})");
        r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert!(!r.wants_tick(), "caught up: nothing more to try");
    }

    /// An unacked update on its way (or a document not caught up) doesn't keep the tick running.
    #[test]
    fn no_idle_tick() {
        let space = SpaceId::generate();
        let mut r = live(space, MemStore::default());
        r.add(&app(), [1; 16], entry("a.txt", 1)).unwrap();
        assert!(!r.wants_tick(), "on its way");
        r.disconnected();
        assert!(!r.wants_tick(), "offline");
    }

    /// `too_large` doesn't drop the add from the updates to send (the others would never get it): the document goes up
    /// instead, in parts that fit.
    #[test]
    fn too_large_sends_the_document_in_parts_instead() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = Replica::open(MemStore::default(), space, peers()).unwrap();
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        r.add(&app(), [1; 16], entry("a.docx", 1)).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        let frames = r.on_frame(ServerFrame::error("too_large", "smaller, please", Some(doc), Some(1))).unwrap();
        let [ClientFrame::Update { client_seq: 2, bytes, .. }] = frames.as_slice() else { panic!("{frames:?}") };
        assert_eq!(Recents::load(bytes, 99).unwrap().view().len(), 1, "the add travels in the document");
        assert_eq!(r.unacked(), vec![(doc, 2)]);
    }

    /// A failed write of an add takes it back: it isn't left among the updates to send when the app is told it failed.
    #[test]
    fn a_failed_write_takes_the_add_back() {
        struct Failing(MemStore, bool);
        impl DocStore for Failing {
            fn load(&self) -> std::io::Result<BTreeMap<String, Vec<u8>>> {
                self.0.load()
            }
            fn save(&mut self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
                if self.1 { Err(std::io::Error::other("disk full")) } else { self.0.save(name, bytes) }
            }
            fn set_aside(&mut self, name: &str) -> std::io::Result<String> {
                self.0.set_aside(name)
            }
            fn remove(&mut self, name: &str) -> std::io::Result<()> {
                self.0.remove(name)
            }
        }
        let space = SpaceId::generate();
        let mut r = Replica::open(Failing(MemStore::default(), false), space, peers()).unwrap();
        r.add(&app(), [1; 16], entry("kept.docx", 1)).unwrap();
        r.store.1 = true;
        assert!(matches!(r.add(&app(), [2; 16], entry("lost.docx", 2)), Err(AddError::Io(_))));
        assert_eq!(r.waiting(), 1, "only the add that was written waits to be sent");
        assert_eq!(r.entries(&app()).iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["kept.docx"]);
        // The reload after a failed write drops the old document normally (letting go of it without dropping, every
        // time, would leak one per add on a full disk): only a document Loro failed in is let go of so.
        for i in 3..10u8 {
            assert!(r.add(&app(), [i; 16], entry("more.docx", i64::from(i))).is_err());
        }
        assert_eq!(r.forgotten(), 0, "healthy documents were leaked");
    }

    /// A document Loro failed in (any call on it, not only an import) is read back from its file at the
    /// next tick, and the failed one let go of without dropping it (its poisoned lock would panic again).
    #[test]
    fn a_document_loro_failed_in_is_read_back() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        r.add(&app(), [1; 16], entry("a.txt", 1)).unwrap();
        r.on_frame(ServerFrame::Ack { doc, client_seq: 1, seq: 1 }).unwrap();
        r.flush().unwrap();
        r.docs.get(&doc).unwrap().body.poison();
        assert!(r.entries(&app()).is_empty(), "a poisoned document reads as empty, it doesn't panic");
        assert!(r.wants_tick());
        let frames = r.resend();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 1, .. })), "{frames:?}");
        assert_eq!((r.forgotten(), r.entries(&app()).len()), (1, 1), "read back from its file");
    }

    /// The same failure on the same state would heal and fail again for good, and adds that only travel inside a
    /// push's parts would never go up: after the second heal in a run, the push stops, the adds go up as plain
    /// updates, and the app can see why.
    #[test]
    fn a_push_that_keeps_failing_stops_and_sends_the_adds() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        r.on_frame(ServerFrame::error("resync_required", "restored", Some(doc), None)).unwrap();
        r.add(&app(), [1; 16], entry("a.txt", 1)).unwrap();
        r.flush().unwrap();
        for round in 0..2 {
            assert!(r.docs.get(&doc).unwrap().pushing.is_some(), "round {round}: a push is under way");
            r.docs.get(&doc).unwrap().body.poison();
            r.resend();
            r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
            r.flush().unwrap();
        }
        assert!(r.push_stopped(&app()));
        // The app's followers hear it: the list counts as changed, with the status alongside.
        assert!(r.take_changed().iter().any(|(a, _)| *a == app()), "the followers aren't told");
        assert!(r.docs.get(&doc).unwrap().pushing.is_none());
        // (A part made before the push stopped is kept too, and goes like any update.)
        assert!(r.unacked().contains(&(doc, 1)), "the add is kept");
        r.disconnected();
        r.connected();
        r.on_frame(welcome(1)).unwrap();
        let frames = r.on_frame(ServerFrame::Subscribed { doc, head: 0 }).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Update { client_seq: 1, .. })), "sent as a plain update: {frames:?}");
        assert!(r.take_notes().iter().any(|n| n.contains("one by one")));
    }

    #[test]
    fn bad_updates_are_skipped_and_said_once() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        for seq in 1..=2 {
            r.on_frame(ServerFrame::Update { doc, seq, bytes: vec![9, 9, 9], device: device() }).unwrap();
        }
        assert_eq!(r.take_notes().len(), 1);
        assert!(r.entries(&app()).is_empty());
        let other = Recents::new(77);
        let good = other.add([5; 16], &entry("x.txt", 3)).unwrap();
        r.on_frame(ServerFrame::Update { doc, seq: 3, bytes: good, device: device() }).unwrap();
        assert_eq!(r.entries(&app()).len(), 1, "the next good one applies");
        assert!(matches!(r.on_frame(ServerFrame::error(PROTOCOL_MISMATCH, "old", None, None)), Err(LinkError::Mismatch(_))));
    }

    /// A journal reads up to its first record that doesn't (one a crash cut off: never acked to the app).
    #[test]
    fn a_journal_ends_at_a_record_cut_off() {
        let records: Vec<Vec<u8>> = (1..=3u64).map(|i| journal_record(&JournalRecord { client_seq: i, bytes: vec![i as u8; 10] }).unwrap()).collect();
        let whole: Vec<u8> = records.concat();
        let (read, good) = journal_records(&whole);
        assert_eq!(read.iter().map(|r| r.client_seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(good, whole.len());
        for cut in [whole.len() - 1, whole.len() - 9, records[0].len() + 3] {
            let (read, good) = journal_records(&whole[..cut]);
            assert!(read.len() < 3 && read.iter().enumerate().all(|(i, r)| r.client_seq == i as u64 + 1), "cut at {cut}");
            assert_eq!(good, records[..read.len()].iter().map(Vec::len).sum::<usize>(), "where it ends");
        }
        let mut damaged = whole.clone();
        let at = records[0].len() + 6;
        damaged[at] ^= 1;
        assert_eq!(journal_records(&damaged), (journal_records(&records[0]).0, records[0].len()), "a damaged record ends it");
        assert!(journal_records(&[]).0.is_empty());
        assert!(journal_records(&[0xff, 0xff, 0xff, 0xff, 1]).0.is_empty(), "a length past the end");
    }

    /// The version a log's updates make is worked out from the copy at load: everything but its own unsent updates
    /// (when those are each peer's last operations); unknown otherwise.
    #[test]
    fn the_log_version_is_worked_out_from_the_copy() {
        let space = SpaceId::generate();
        let node = NodeId::generate();
        let id = node_doc(space, node);
        let kind = Kind::Document { node, app: app() };
        let mut doc =
            Doc::with(id, kind.clone(), Body::new(&kind, 1), Have { upto: 3, upto_hash: Some([3; 32]) }, Vec::new(), Vec::new(), 1, None, None);
        let other = loro::LoroDoc::new();
        other.set_peer_id(9).unwrap();
        let update = |d: &loro::LoroDoc, s: &str| {
            let vv = d.oplog_vv();
            d.get_text("text").insert(0, s).unwrap();
            d.commit();
            d.export(loro::ExportMode::updates(&vv)).unwrap()
        };
        let logged = update(&other, "abc");
        doc.body.import(&logged).unwrap();
        // Its own, unsent: peer 1's last operations.
        let own = loro::LoroDoc::new();
        own.import(&logged).unwrap();
        own.set_peer_id(1).unwrap();
        let mine = update(&own, "xy");
        doc.body.import(&mine).unwrap();
        doc.pending.push(Pending { client_seq: 1, bytes: mine, acked: None, push: false });
        doc.derive_log();
        assert!(!doc.log.is_known(), "an unacked update may or may not be stored: unknown");
        // Once nothing is unacked, the log holds the whole copy (its own update came back as a row, or was acked).
        doc.pending.clear();
        doc.derive_log();
        let version = doc.log.at(3).unwrap();
        assert_eq!(version.get(&9), Some(&3));
        assert_eq!(version.get(&1), Some(&2));
        doc.dropped = Some(2);
        doc.derive_log();
        assert!(!doc.log.is_known(), "a row was dropped: unknown");
        doc.dropped = None;
        doc.waiting.push(Waiting { hash: [0; 32], needs: vec![(5, 1)], bytes: vec![1] });
        doc.derive_log();
        assert!(!doc.log.is_known(), "something waits: unknown");
    }

    /// The server's snapshot `bytes` at the start of a catch-up, as the hub hands it over: the frame naming it, then
    /// its bytes, downloaded. Frames to send.
    fn snapshot_in(r: &mut Replica<MemStore>, doc: DocId, base: u64, bytes: &[u8]) -> Vec<ClientFrame> {
        let manifest = cut(doc, bytes).unwrap().id();
        let mut frames = r.on_frame(ServerFrame::Snapshot { doc, base, manifest }).unwrap();
        assert_eq!(r.take_fetches(), vec![(doc, manifest)], "asked to download it");
        frames.extend(r.on_snapshot(doc, manifest, Ok(bytes.to_vec())));
        frames
    }

    /// The snapshot `manifest` names, from what the replica asked to upload.
    fn uploaded(r: &mut Replica<MemStore>, manifest: ManifestId) -> Vec<u8> {
        r.take_uploads().into_iter().find(|u| u.id() == manifest).expect("uploaded before it is named").bytes()
    }

    /// Sets key `k<n>` of the map `map` to `s`: the change.
    fn put(d: &loro::LoroDoc, n: u64, s: &str) -> Vec<u8> {
        let vv = d.oplog_vv();
        d.get_map("map").insert(&format!("k{n}"), s).unwrap();
        d.commit();
        d.export(loro::ExportMode::updates(&vv)).unwrap()
    }

    fn text_doc(peer: u64) -> loro::LoroDoc {
        let d = loro::LoroDoc::new();
        d.set_peer_id(peer).unwrap();
        d
    }

    fn insert(d: &loro::LoroDoc, at: usize, s: &str) -> Vec<u8> {
        let vv = d.oplog_vv();
        d.get_text("text").insert(at, s).unwrap();
        d.commit();
        d.export(loro::ExportMode::updates(&vv)).unwrap()
    }

    /// `s` inserted at the start, as an app's kit sends a big change: its slices in a bundle.
    fn sliced(d: &loro::LoroDoc, s: &str) -> Vec<u8> {
        let peer = d.peer_id();
        let start = d.oplog_vv().get(&peer).copied().unwrap_or(0);
        d.get_text("text").insert(0, s).unwrap();
        d.commit();
        let end = d.oplog_vv().get(&peer).copied().unwrap_or(0);
        let mut slices = Vec::new();
        let mut at = start;
        while at < end {
            let to = (at + craft_cloud_proto::fragments::SLICE_OPS).min(end);
            slices.push(d.export(loro::ExportMode::updates_in_range(vec![loro::IdSpan::new(peer, at, to)])).unwrap());
            at = to;
        }
        craft_cloud_proto::fragments::bundle(&slices)
    }

    /// An own update the server stored (its ack lost), whose operations the server's snapshot holds on the next
    /// catch-up: it is stored (not sent again, nothing waits), and a compaction after a restart has it, though another
    /// machine's later rows don't build on it.
    #[test]
    fn an_update_a_snapshot_holds_is_stored_and_in_every_compaction() {
        let store = MemStore::default();
        let space = SpaceId::generate();
        let node = NodeId::generate();
        let w2 = ServerFrame::Welcome { protocol: 2, epoch: epoch(1) };
        // The import (peer 40), this machine's app edit P (peer 50), another machine's concurrent edits (peer 60).
        let importer = text_doc(40);
        insert(&importer, 0, "import");
        let import = importer.export(loro::ExportMode::Snapshot).unwrap();
        let mine = text_doc(50);
        mine.import(&import).unwrap();
        let p = insert(&mine, 6, " P");
        let other = text_doc(60);
        other.import(&import).unwrap();
        let o2 = insert(&other, 0, "O1 ");
        let o3 = insert(&other, 0, "O2 ");
        let server = text_doc(70);
        for u in [&import, &p, &o2, &o3] {
            server.import(u).unwrap();
        }
        let s3 = server.export(loro::ExportMode::Snapshot).unwrap();
        // Joined from the import.
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(w2.clone()).unwrap();
        let (id, opening, _) = r.open_document(node, &app());
        assert_eq!(opening, Opening::Joining);
        snapshot_in(&mut r, id, 0, &import);
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        assert!(r.has_document(id));
        // P: sent, stored as row 1 by the server, its ack lost with the connection.
        let frames = r.local_update(id, &p).unwrap();
        assert!(matches!(frames.as_slice(), [ClientFrame::Update { client_seq: 1, .. }]), "{frames:?}");
        r.disconnected();
        // Rows 2, 3 from the other machine; compacted to base 3. The next catch-up starts from the snapshot.
        r.connected();
        r.on_frame(w2.clone()).unwrap();
        snapshot_in(&mut r, id, 3, &s3);
        r.on_frame(ServerFrame::Update { doc: id, seq: 3, bytes: o3.clone(), device: device() }).unwrap();
        let frames = r.on_frame(ServerFrame::Subscribed { doc: id, head: 3 }).unwrap();
        assert!(!frames.iter().any(|f| matches!(f, ClientFrame::Update { .. })), "P is stored, not sent again: {frames:?}");
        assert_eq!(r.document_status(id), Some((true, 0)), "nothing waits");
        // The documents are written (every 2 s, or at stop) and the agent stops.
        r.flush_documents().unwrap();
        drop(r);
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        let mut frames = r.on_frame(w2.clone()).unwrap();
        // (Nothing to send: followed again once an app opens it.)
        let (_, opening, opened) = r.open_document(node, &app());
        assert_eq!(opening, Opening::Live);
        frames.extend(opened);
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 3, .. })), "{frames:?}");
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 3 }).unwrap();
        // A stray ack of P (from the old connection): no news.
        r.on_frame(ServerFrame::Ack { doc: id, client_seq: 1, seq: 1 }).unwrap();
        assert_eq!(r.document_status(id), Some((true, 0)));
        // Rows 4, 5 from a laptop that was offline since before P (its edits don't build on P).
        let laptop = text_doc(80);
        laptop.import(&import).unwrap();
        let l4 = insert(&laptop, 0, "L4 ");
        let l5 = insert(&laptop, 0, "L5 ");
        server.import(&l4).unwrap();
        server.import(&l5).unwrap();
        r.on_frame(ServerFrame::Update { doc: id, seq: 4, bytes: l4, device: device() }).unwrap();
        r.on_frame(ServerFrame::Update { doc: id, seq: 5, bytes: l5, device: device() }).unwrap();
        // Asked to compact at 5 (base 3: the server takes it).
        let frames = r.on_frame(ServerFrame::Compact { doc: id, upto: 5 }).unwrap();
        let manifest = frames
            .iter()
            .find_map(|f| if let ClientFrame::Snapshot { upto: 5, manifest, .. } = f { Some(*manifest) } else { None })
            .expect("answered");
        let bytes = uploaded(&mut r, manifest);
        let fresh = loro::LoroDoc::new();
        fresh.import(&bytes).unwrap();
        let text = fresh.get_text("text").to_string();
        let all = server.get_text("text").to_string();
        assert_eq!(text, all, "a fresh machine from the new snapshot has every row up to 5, P too");
    }

    /// A row this machine dropped (it can never apply here) is at or below the asked number: no snapshot (its copy
    /// lacks the row), also after a restart (the file keeps it); one past the base of the server's next snapshot is
    /// in the copy again (`dropped` clears when a snapshot's base passes it).
    #[test]
    fn a_machine_that_dropped_a_row_doesnt_compact_past_it() {
        let store = MemStore::default();
        let space = SpaceId::generate();
        let node = NodeId::generate();
        let w2 = ServerFrame::Welcome { protocol: 2, epoch: epoch(1) };
        let importer = text_doc(40);
        insert(&importer, 0, "import");
        let import = importer.export(loro::ExportMode::Snapshot).unwrap();
        let other = text_doc(60);
        other.import(&import).unwrap();
        let o1 = insert(&other, 0, "O1 ");
        let o3 = insert(&other, 0, "O3 ");
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(w2.clone()).unwrap();
        let (id, _, _) = r.open_document(node, &app());
        snapshot_in(&mut r, id, 0, &import);
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        r.on_frame(ServerFrame::Update { doc: id, seq: 1, bytes: o1, device: device() }).unwrap();
        // Row 2 never applies here.
        r.on_frame(ServerFrame::Update { doc: id, seq: 2, bytes: vec![0xff; 40], device: device() }).unwrap();
        r.on_frame(ServerFrame::Update { doc: id, seq: 3, bytes: o3, device: device() }).unwrap();
        assert!(r.take_notes().iter().any(|n| n.contains("leaves compacting")), "said");
        assert!(r.on_frame(ServerFrame::Compact { doc: id, upto: 3 }).unwrap().is_empty(), "no snapshot past a dropped row");
        r.flush_documents().unwrap();
        drop(r);
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(w2.clone()).unwrap();
        r.open_document(node, &app());
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 3 }).unwrap();
        assert!(r.on_frame(ServerFrame::Compact { doc: id, upto: 3 }).unwrap().is_empty(), "nor after a restart");
    }

    /// A recents file says which row was dropped after what an older agent reads; one an older agent wrote reads too.
    #[test]
    fn a_recents_file_keeps_its_dropped_row_where_an_older_agent_looks_past_it() {
        let space = SpaceId::generate();
        let store = MemStore::default();
        let mut r = live(space, store.clone());
        r.add(&app(), [1; 16], entry("a.docx", 1)).unwrap();
        let id = recents_doc(space, &app());
        r.docs.get_mut(&id).unwrap().dropped = Some(7);
        r.docs.get_mut(&id).unwrap().dirty = true;
        r.flush().unwrap();
        let bytes = store.load().unwrap().get(&id.to_string()).cloned().unwrap();
        let older = postcard::from_bytes::<DocFile>(&bytes).unwrap();
        assert_eq!(older.app, app(), "an older agent reads the file");
        let (_, more) = read_doc_file(&bytes).unwrap();
        assert_eq!(more.dropped, Some(7));
        let plain = postcard::to_stdvec(&older).unwrap();
        assert_eq!(read_doc_file(&plain).unwrap().1.dropped, None, "a file without it");
        let r = Replica::open(store, space, peers()).unwrap();
        assert_eq!(r.docs.get(&id).unwrap().dropped, Some(7));
    }

    /// An own update acked at a number above what is had, then a row with the same operations (another device pushed
    /// them too) below it: the ack is kept until the rows below it are here, so what is had reaches the acked row and
    /// the next own ack counts.
    #[test]
    fn an_early_ack_is_kept_when_a_row_below_it_holds_the_same_update() {
        let space = SpaceId::generate();
        let doc = recents_doc(space, &app());
        let mut r = live(space, MemStore::default());
        let frames = r.add(&app(), [1; 16], entry("a.docx", 1)).unwrap();
        let ClientFrame::Update { client_seq, bytes, .. } = frames[0].clone() else { panic!("{frames:?}") };
        // Acked as row 2: row 1 is on its way.
        r.on_frame(ServerFrame::Ack { doc, client_seq, seq: 2 }).unwrap();
        // Row 1 holds the same bytes (stored by another device).
        r.on_frame(ServerFrame::Update { doc, seq: 1, bytes, device: device() }).unwrap();
        // The next own update, acked as row 3.
        let frames = r.add(&app(), [2; 16], entry("b.docx", 2)).unwrap();
        let ClientFrame::Update { client_seq, .. } = frames[0].clone() else { panic!("{frames:?}") };
        r.on_frame(ServerFrame::Ack { doc, client_seq, seq: 3 }).unwrap();
        assert_eq!(r.waiting_for(&app()), 0, "row 2 is this machine's own (never sent to it live): {}", r.describe(&app()));
        let other = Recents::new(77);
        let u4 = other.add([5; 16], &entry("c.docx", 4)).unwrap();
        assert!(r.on_frame(ServerFrame::Update { doc, seq: 4, bytes: u4, device: device() }).unwrap().is_empty(), "no gap at 3");
    }

    /// A change in fragments from the server goes into the copy, and to the apps, only once it is whole: its first
    /// fragments count as had and stay in the file across a restart (the catch-up goes on after them), and no
    /// snapshot is made for the server while it comes, nor at a number between its first fragment and its last.
    #[test]
    fn a_change_in_fragments_goes_in_only_whole_even_across_a_restart() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        r.take_events();
        let other = text_doc(41);
        other.import(&import).unwrap();
        let small = insert(&other, 0, "s ");
        let big = sliced(&other, &"b".repeat(2 * MAX_UPDATE_BYTES));
        let frames = split(&big, MAX_UPDATE_BYTES).unwrap();
        assert_eq!(frames.len(), 3);
        let row = |seq: u64, bytes: &[u8]| ServerFrame::Update { doc: id, seq, bytes: bytes.to_vec(), device: device() };
        r.on_frame(row(1, &small)).unwrap();
        r.on_frame(row(2, &frames[0])).unwrap();
        r.on_frame(row(3, &frames[1])).unwrap();
        assert_eq!(text_of(&r, id), "s import", "nothing of the change in the copy");
        let events = r.take_events();
        assert_eq!(events.iter().filter(|e| matches!(e, DocEvent::Remote(..))).count(), 1, "only the small change went to the apps: {events:?}");
        assert!(r.on_frame(ServerFrame::Compact { doc: id, upto: 3 }).unwrap().is_empty(), "no snapshot while it comes");
        // The agent stops and starts again: the fragments are in the file, and counted as had.
        r.flush_documents().unwrap();
        drop(r);
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        let mut frames_out = r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let (_, opening, opened) = r.open_document(node, &app());
        assert_eq!(opening, Opening::Live);
        frames_out.extend(opened);
        assert!(frames_out.iter().any(|f| matches!(f, ClientFrame::Subscribe { after: 3, .. })), "{frames_out:?}");
        r.on_frame(row(4, &frames[2])).unwrap();
        r.on_frame(ServerFrame::Subscribed { doc: id, head: 4 }).unwrap();
        assert_eq!(text_of(&r, id), format!("{}s import", "b".repeat(2 * MAX_UPDATE_BYTES)), "whole once its last fragment came");
        assert!(r.take_events().contains(&DocEvent::Remote(id, big.clone())), "the apps get the change, whole");
        let last = u32::try_from(2 + 2 * MAX_UPDATE_BYTES - 1).unwrap();
        assert!(r.document_holds(id, &[(41, last)]));
        // No snapshot between its first fragment and its last; one past it holds it.
        assert!(r.on_frame(ServerFrame::Compact { doc: id, upto: 3 }).unwrap().is_empty(), "not inside the change's rows");
        let frames_out = r.on_frame(ServerFrame::Compact { doc: id, upto: 4 }).unwrap();
        let manifest = frames_out
            .iter()
            .find_map(|f| if let ClientFrame::Snapshot { upto: 4, manifest, .. } = f { Some(*manifest) } else { None })
            .expect("answered at 4");
        let fresh = loro::LoroDoc::new();
        fresh.import(&uploaded(&mut r, manifest)).unwrap();
        assert_eq!(fresh.get_text("text").len_utf8(), 2 * MAX_UPDATE_BYTES + "s import".len());
    }

    /// An app's change over the frame limit: imported whole, journalled whole, and sent as fragments that fit a frame
    /// (each its own number). After a crash before the file is written, the journal brings back the fragments to send
    /// with the same numbers, and an app opening the document gets the change whole.
    #[test]
    fn a_local_change_over_a_frame_is_journalled_whole_and_sent_in_fragments() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        let app_doc = text_doc(50);
        app_doc.import(&import).unwrap();
        let big = sliced(&app_doc, &"a".repeat(3 * MAX_UPDATE_BYTES));
        let frames = r.local_update(id, &big).unwrap();
        let sent: Vec<(u64, usize)> = frames
            .iter()
            .filter_map(|f| if let ClientFrame::Update { client_seq, bytes, .. } = f { Some((*client_seq, bytes.len())) } else { None })
            .collect();
        assert_eq!(sent.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![1, 2, 3, 4], "{sent:?}");
        assert!(sent.iter().all(|(_, len)| *len <= MAX_UPDATE_BYTES));
        assert!(r.document_holds(id, &[(50, 1)]));
        // A crash: the journal has it, the file doesn't.
        drop(r);
        let mut r = Replica::open(store.clone(), space, peers()).unwrap();
        r.connected();
        r.on_frame(ServerFrame::Welcome { protocol: 2, epoch: epoch(1) }).unwrap();
        let (_, opening, _) = r.open_document(node, &app());
        assert_eq!(opening, Opening::Live);
        let (_, after) = r.document_state(id).unwrap();
        assert!(after.contains(&big), "an app opening it gets the change whole");
        assert!(r.document_holds(id, &[(50, 1)]), "in the copy again");
        let frames = r.on_frame(ServerFrame::Subscribed { doc: id, head: 0 }).unwrap();
        let again: Vec<u64> =
            frames.iter().filter_map(|f| if let ClientFrame::Update { client_seq, .. } = f { Some(*client_seq) } else { None }).collect();
        assert_eq!(again, vec![1, 2, 3, 4], "the same fragments, the same numbers: {frames:?}");
        for (k, seq) in again.iter().enumerate() {
            r.on_frame(ServerFrame::Ack { doc: id, client_seq: *seq, seq: k as u64 + 1 }).unwrap();
        }
        assert_eq!(r.document_status(id), Some((true, 0)), "nothing waits");
        let frames_out = r.on_frame(ServerFrame::Compact { doc: id, upto: 4 }).unwrap();
        assert!(frames_out.iter().any(|f| matches!(f, ClientFrame::Snapshot { upto: 4, .. })), "its rows are in the log: {frames_out:?}");
    }

    /// Asked to compact while a big change of its own isn't in the log yet, a machine doesn't answer: the snapshot
    /// would have Loro check the document out before the change, which takes tens of seconds for a big paste. Once
    /// the change is in the log, it answers.
    #[test]
    fn no_snapshot_from_a_copy_far_past_the_asked_version() {
        let store = MemStore::default();
        let (space, node) = (SpaceId::generate(), NodeId::generate());
        let (mut r, id, import) = joined(&store, space, node);
        let app_doc = text_doc(50);
        app_doc.import(&import).unwrap();
        let small = insert(&app_doc, 0, "s ");
        r.on_frame(ServerFrame::Update { doc: id, seq: 1, bytes: small, device: device() }).unwrap();
        let big = sliced(&app_doc, &"a".repeat(2 * MAX_UPDATE_BYTES));
        let sent = r.local_update(id, &big).unwrap();
        let started = std::time::Instant::now();
        assert!(r.on_frame(ServerFrame::Compact { doc: id, upto: 1 }).unwrap().is_empty(), "not answered");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        let seqs: Vec<u64> =
            sent.iter().filter_map(|f| if let ClientFrame::Update { client_seq, .. } = f { Some(*client_seq) } else { None }).collect();
        for (k, seq) in seqs.iter().enumerate() {
            r.on_frame(ServerFrame::Ack { doc: id, client_seq: *seq, seq: k as u64 + 2 }).unwrap();
        }
        let upto = seqs.len() as u64 + 1;
        let frames = r.on_frame(ServerFrame::Compact { doc: id, upto }).unwrap();
        assert!(frames.iter().any(|f| matches!(f, ClientFrame::Snapshot { upto: u, .. } if *u == upto)), "{frames:?}");
    }
}
