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
//! - **Its own acks** may come before the updates of others numbered below them: such an ack is noted on the unacked
//!   update, which stays unacked until the number is reached (then it counts as had). On every new connection those
//!   notes are forgotten and the updates sent again: the server acks one it has with its number, and stores one a
//!   restore lost. A gap in what the server sends otherwise (a frame lost on a connection given up on) is caught up by
//!   subscribing again from the number it has.
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
//! - **Loro failing** on an update from the server: the document is read back from its file and that update skipped; on
//!   a waiting update imported again: that one is dropped. A file that doesn't read is set aside (kept); its document
//!   starts afresh from its unacked updates, which are kept and sent.
//! - A newer agent's update is taken like any other: once the document says it is of a newer version, this agent only
//!   stops adding to it (the design's rule: an older agent is read-only on a newer document), and says why.

use std::collections::{BTreeMap, BTreeSet};

use craft_cloud_proto::api::codes;
use craft_cloud_proto::live::{
    AppName, ClientFrame, DocKind, LIVE_PROTOCOL_MAX, LIVE_PROTOCOL_MIN, MAX_UPDATE_BYTES, PROTOCOL_MISMATCH, ServerFrame, recents_doc, update_hash,
};
use craft_cloud_proto::local::RecentEntry;
use craft_cloud_proto::{DocId, Epoch, SpaceId};
use serde::{Deserialize, Serialize};

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
/// The Loro peers of this machine's latest runs a push counts as its own (older ones count as others': they still go
/// up, after a wait). One per run, so without a cap the list would grow for good.
const MAX_OWN_PEERS: usize = 16;
/// Heals of one document in a run after which its push stops (the same state fails the same way again).
const MAX_HEALS: u32 = 2;

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
    app: AppName,
    recents: Recents,
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
    /// Loro failed outside an import (an export): the document is read back from its file.
    broken: bool,
    sub: Sub,
    /// Unacked updates sent on this connection (by client_seq).
    sent: BTreeSet<u64>,
    /// `too_large` refusals since the last ack.
    strikes: u32,
    /// A refused create or subscribe: ticks until it is tried again, and the pause it had.
    retry: Option<(u32, u32)>,
    /// Changed since its file was last written.
    dirty: bool,
    /// What was said about this document's updates already.
    said: BTreeSet<String>,
}

impl Doc {
    fn empty(app: &AppName, peer: u64) -> Self {
        Self::with(app.clone(), Recents::new(peer), Have::default(), Vec::new(), Vec::new(), 1, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn with(
        app: AppName,
        recents: Recents,
        have: Have,
        pending: Vec<Pending>,
        waiting: Vec<Waiting>,
        next_client_seq: u64,
        epoch: Option<Epoch>,
        pushing: Option<Spans>,
    ) -> Self {
        Self {
            app,
            own_peers: vec![recents.peer()],
            recents,
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
            broken: false,
            sub: Sub::No,
            sent: BTreeSet::new(),
            strikes: 0,
            retry: None,
            dirty: false,
            said: BTreeSet::new(),
        }
    }

    /// The document as its file holds it, editing as `peer`. The unacked updates are in the snapshot already (made
    /// before it was written); imported again, they change nothing, but a snapshot written before them can't lose
    /// them. The waiting ones go back into Loro's queue. One Loro fails on is left out (the document read again from
    /// the snapshot without it): a waiting one is dropped (the server has it, and a catch-up from the start brings it
    /// if it is ever needed), an unacked one stays unacked (it is sent, and its operations come back with the log).
    /// An error only when the snapshot itself doesn't read.
    fn from_file(mut file: DocFile, peer: u64, notes: &mut Vec<String>) -> Result<Self, UpdateError> {
        let (mut bad_pending, mut bad_waiting) = (BTreeSet::new(), BTreeSet::new());
        let recents = 'load: loop {
            let recents = Recents::load(&file.snapshot, peer)?;
            for (i, pending) in file.pending.iter().enumerate() {
                if !bad_pending.contains(&i) && recents.import(&pending.bytes) == Err(UpdateError::Panicked) {
                    bad_pending.insert(i);
                    continue 'load;
                }
            }
            for (i, waiting) in file.waiting.iter().enumerate() {
                if !bad_waiting.contains(&i) && recents.import(&waiting.bytes) == Err(UpdateError::Panicked) {
                    bad_waiting.insert(i);
                    continue 'load;
                }
            }
            break recents;
        };
        if !bad_pending.is_empty() || !bad_waiting.is_empty() {
            notes.push(format!(
                "Loro failed on {} stored recents update(s) of {}; the waiting ones were dropped, the unsent ones are kept and sent",
                bad_pending.len() + bad_waiting.len(),
                file.app
            ));
        }
        let mut i = 0;
        file.waiting.retain(|_| {
            i += 1;
            !bad_waiting.contains(&(i - 1))
        });
        let mut doc = Self::with(file.app, recents, file.have, file.pending, file.waiting, file.next_client_seq.max(1), file.epoch, file.pushing);
        doc.waiting.retain(|w| !doc.recents.covers(&w.needs));
        for peer in file.own_peers {
            if !doc.own_peers.contains(&peer) && doc.own_peers.len() < MAX_OWN_PEERS {
                doc.own_peers.push(peer);
            }
        }
        doc.push_wait = file.push_wait;
        Ok(doc)
    }

    /// A document whose file doesn't read (the snapshot is damaged, or Loro fails on it): started afresh from its
    /// unacked updates, which are kept and sent; it catches up from the start, so nothing the server has is missed.
    fn from_pending(app: AppName, pending: Vec<Pending>, next_client_seq: u64, peer: u64) -> Self {
        let recents = Recents::new(peer);
        for p in &pending {
            let _ = recents.import(&p.bytes);
        }
        let pending = pending.into_iter().map(|p| Pending { acked: None, push: false, ..p }).collect();
        let mut doc = Self::with(app, recents, Have::default(), pending, Vec::new(), next_client_seq.max(1), None, None);
        doc.dirty = true;
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
            self.advance(seq, hash);
            return Taken::Had;
        }
        let had_history = self.recents.has_history();
        match self.recents.import(bytes) {
            Ok(Imported::Applied { changed: list }) => {
                *changed |= list;
                if !had_history {
                    // The first history, perhaps a snapshot: Loro dropped what waited; it goes in again.
                    for waiting in &self.waiting {
                        match self.recents.import(&waiting.bytes) {
                            Ok(Imported::Applied { changed: list } | Imported::Waiting { changed: list, .. }) => *changed |= list,
                            Err(UpdateError::Panicked) => return Taken::WaitingPanicked(waiting.hash),
                            Err(_) => {}
                        }
                    }
                }
                if let Some(version) = self.recents.newer() {
                    let line = format!(
                        "the recents of {} were written by a newer Craft Cloud agent (version {version}); they are shown, and this agent adds to them again once it is updated",
                        self.app
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
                        let line = format!(
                            "more than {} MiB of recents updates for {} wait for history that never came; this machine takes no more of them until it connects again",
                            MAX_WAITING_BYTES >> 20,
                            self.app
                        );
                        self.say(notes, line);
                        return Taken::Stuck;
                    }
                    self.waiting.push(Waiting { hash, needs, bytes: bytes.to_vec() });
                }
            }
            Err(UpdateError::Panicked) => return Taken::Panicked,
            Err(e) => {
                let line = format!("a recents update for {} from the server can never apply and was skipped: {e}", self.app);
                self.say(notes, line);
            }
        }
        self.waiting.retain(|w| !self.recents.covers(&w.needs));
        self.on_server(bytes);
        self.advance(seq, hash);
        Taken::Had
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
        while let Some(i) = self.pending.iter().position(|p| p.acked == Some(self.have.upto.saturating_add(1))) {
            let own = self.pending.remove(i);
            self.sent.remove(&own.client_seq);
            self.on_server(&own.bytes);
            self.have = Have { upto: self.have.upto.saturating_add(1), upto_hash: Some(update_hash(&own.bytes)) };
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
        self.broken || self.recents.is_poisoned()
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
        let Ok(missing) = self.recents.missing(has) else {
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
                self.recents.export_prefix(&missing[..run], max, MAX_UPDATE_BYTES >> self.strikes).map(|(bytes, _)| Some(bytes))
            }
        };
        match part {
            Ok(Some(bytes)) => {
                let client_seq = self.next_client_seq;
                self.next_client_seq = client_seq.saturating_add(1);
                self.pending.push(Pending { client_seq, bytes, acked: None, push: true });
                self.dirty = true;
                send_pending(id, self)
            }
            Ok(None) => {
                let has = self.pushing.take().unwrap_or_default();
                // (One acked above what is had stays: it is counted when the numbers below it come.)
                self.pending.retain(|p| p.acked.is_some() || !spans_of(&p.bytes).is_some_and(|ranges| held_by(&has, &ranges)));
                self.dirty = true;
                send_pending(id, self)
            }
            Err(UpdateError::Panicked) => {
                // Loro failed: the document is read back from its file ([`Replica::heal`]).
                self.broken = true;
                Vec::new()
            }
            Err(e) => {
                let line = format!("the recents of {} can't go up to the server ({e}); they stay on this machine", self.app);
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
        self.stuck = false;
        if let Ok(snapshot) = self.recents.snapshot()
            && let Ok(fresh) = Recents::load(&snapshot, self.recents.peer())
        {
            self.recents = fresh;
            for pending in &self.pending {
                let _ = self.recents.import(&pending.bytes);
            }
        }
        self.forget_acks();
        self.dirty = true;
    }

    /// Acks of numbers not reached are forgotten: their updates go again (the server acks one it has).
    fn forget_acks(&mut self) {
        for pending in &mut self.pending {
            pending.acked = None;
        }
    }

    /// `subscribe` from what the document has, without `create` (the server has it: on the same connection).
    fn resubscribe(&mut self, id: DocId) -> Vec<ClientFrame> {
        self.sub = Sub::CatchingUp;
        vec![ClientFrame::Subscribe { doc: id, after: self.have.upto, at: self.at() }]
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
    /// Documents let go of without dropping ([`Replica::forgotten`]).
    forgotten: usize,
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
            if let Some((id, doc)) = load_doc(&mut store, space, &name, &bytes, peers(), &mut notes) {
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
            forgotten: 0,
        })
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
        self.docs.get(&self.doc_id(app)).map(|d| d.recents.view()).unwrap_or_default()
    }

    /// The newest entry for a file, if the list shows it (to keep its `saved` time when it is only opened).
    pub fn current(&self, app: &AppName, entry: &RecentEntry) -> Option<RecentEntry> {
        self.docs.get(&self.doc_id(app)).and_then(|d| d.recents.current(entry.node, &entry.path))
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
        let before = doc.recents.view();
        let bytes = doc.recents.add(key, &entry).map_err(|e| AddError::Invalid(e.to_string()))?;
        let client_seq = doc.next_client_seq;
        doc.next_client_seq = client_seq.saturating_add(1);
        doc.pending.push(Pending { client_seq, bytes: bytes.clone(), acked: None, push: false });
        let changed = doc.recents.view() != before;
        // On disk before the app hears it is kept.
        if let Err(e) = save(&mut self.store, space, id, doc) {
            self.reload(id);
            return Err(e.into());
        }
        let doc_app = doc.app.clone();
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
        let stored = self.store.load().ok().and_then(|files| files.get(&name).cloned());
        let stored = stored.map(|bytes| match (drop, postcard::from_bytes::<DocFile>(&bytes)) {
            (Some(hash), Ok(mut file)) => {
                file.waiting.retain(|w| w.hash != hash);
                postcard::to_stdvec(&file).unwrap_or(bytes)
            }
            _ => bytes,
        });
        let peer = (self.peers)();
        match stored.and_then(|bytes| load_doc(&mut self.store, self.space, &name, &bytes, peer, &mut self.notes)) {
            Some((_, mut doc)) => {
                doc.dirty |= drop.is_some();
                self.docs.insert(id, doc);
            }
            None => {
                self.docs.remove(&id);
            }
        }
    }

    /// Loro failed on update `seq` (`hash`) of `id`: the document is read back from its file (nothing half-changed
    /// stays), and caught up again from there, with that update counted as had and not imported.
    fn recover(&mut self, id: DocId, app: AppName, seq: u64, hash: [u8; 32]) -> Vec<ClientFrame> {
        self.reload(id);
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let (_, doc) = doc_in(&mut self.docs, self.space, &app, &mut self.peers);
        doc.skip = Some((seq, hash));
        let line = format!("a recents update for {app} from the server made Loro fail; it was skipped");
        doc.say(&mut self.notes, line);
        self.changed.insert(app);
        if up { subscribe(id, doc, epoch) } else { Vec::new() }
    }

    /// Loro failed on a waiting update of `id` imported again (`hash`): that one is dropped (the incoming update is
    /// not to blame, and comes again), and the document read back from its file and caught up from there.
    fn recover_waiting(&mut self, id: DocId, app: AppName, hash: [u8; 32]) -> Vec<ClientFrame> {
        self.reload_without(id, Some(hash));
        let (up, epoch) = (self.link == Link::Up, self.epoch);
        let (_, doc) = doc_in(&mut self.docs, self.space, &app, &mut self.peers);
        let line = format!("a waiting recents update for {app} made Loro fail; it was dropped");
        doc.say(&mut self.notes, line);
        self.changed.insert(app);
        if up { subscribe(id, doc, epoch) } else { Vec::new() }
    }

    /// A connection to the server is open: the first frame to send.
    pub fn connected(&mut self) -> Vec<ClientFrame> {
        self.link = Link::Hello;
        vec![ClientFrame::Hello { min: LIVE_PROTOCOL_MIN, max: LIVE_PROTOCOL_MAX }]
    }

    /// The connection ended: everything is subscribed and sent again on the next one.
    pub fn disconnected(&mut self) {
        self.link = Link::Down;
        self.epoch = None;
        for doc in self.docs.values_mut() {
            doc.sub = Sub::No;
            doc.sent.clear();
            doc.retry = None;
            doc.full = None;
            doc.stuck = false;
        }
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
        match self.docs.get(&self.doc_id(app)) {
            None => "no document".into(),
            Some(d) => format!(
                "{:?}, upto {}, {} unacked ({} sent, {} push, acked {:?}), pushing {:?}, wait {:?}, full {:?}, stuck {}, retry {:?}",
                d.sub,
                d.have.upto,
                d.pending.len(),
                d.sent.len(),
                d.pending.iter().filter(|p| p.push).count(),
                d.pending.iter().filter_map(|p| p.acked).collect::<Vec<_>>(),
                d.pushing.as_ref().map(|has| d.recents.missing(has)),
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
        self.docs.get(&self.doc_id(app)).map(|d| d.recents.readable()).unwrap_or_default()
    }

    /// A document as a Loro snapshot (for a simulation that wants to look inside).
    pub fn snapshot_of(&self, doc: DocId) -> Option<Vec<u8>> {
        self.docs.get(&doc).and_then(|d| d.recents.snapshot().ok())
    }

    /// Documents Loro failed in outside an import: read back from their files, and caught up again from there.
    /// After the second heal of a document in one run, its push stops (the same state would fail the same way again):
    /// its unacked adds go up as plain updates, and [`Replica::push_stopped`] says so for the app to show.
    fn heal(&mut self) -> Vec<ClientFrame> {
        let broken: Vec<(DocId, AppName)> = self.docs.iter().filter(|(_, d)| d.is_broken()).map(|(id, d)| (*id, d.app.clone())).collect();
        let mut frames = Vec::new();
        for (id, app) in broken {
            self.reload(id);
            let heals = self.heals.entry(id).or_insert(0);
            *heals += 1;
            let stop = *heals >= MAX_HEALS;
            let (up, epoch) = (self.link == Link::Up, self.epoch);
            let (_, doc) = doc_in(&mut self.docs, self.space, &app, &mut self.peers);
            let line = format!("Loro failed on the recents of {app}; they were read back from disk");
            doc.say(&mut self.notes, line);
            if stop && doc.pushing.is_some() {
                doc.pushing = None;
                doc.push_wait = None;
                doc.dirty = true;
                self.push_stopped.insert(app.clone());
                // The app's followers are told (their list event carries the status).
                self.changed.insert(app.clone());
                let line = format!(
                    "the recents of {app} can't be sent up whole after Loro failed again; new ones are sent one by one, and what the server lacks of the rest stays on this machine"
                );
                doc.say(&mut self.notes, line);
            }
            if up {
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
            ServerFrame::Welcome { epoch, .. } => {
                self.link = Link::Up;
                self.epoch = Some(epoch);
                let mut frames = Vec::new();
                for (id, doc) in &mut self.docs {
                    frames.extend(subscribe(*id, doc, Some(epoch)));
                }
                Ok(frames)
            }
            ServerFrame::Created { .. } => Ok(Vec::new()),
            ServerFrame::Update { doc: id, seq, bytes, .. } => {
                let Some(doc) = self.docs.get_mut(&id) else { return Ok(Vec::new()) };
                if seq <= doc.have.upto || doc.stuck {
                    return Ok(Vec::new());
                }
                if seq > doc.have.upto.saturating_add(1) {
                    // A gap: a catch-up under way brings it; else the server's updates were lost on the way (an old
                    // connection), and a catch-up from what it has brings them.
                    return Ok(if doc.sub == Sub::Live { doc.resubscribe(id) } else { Vec::new() });
                }
                let mut changed = false;
                let taken = doc.take(seq, &bytes, &mut self.notes, &mut changed);
                let app = doc.app.clone();
                if changed {
                    self.changed.insert(app.clone());
                }
                Ok(match taken {
                    Taken::Had | Taken::Stuck => Vec::new(),
                    Taken::Panicked => self.recover(id, app, seq, update_hash(&bytes)),
                    Taken::WaitingPanicked(hash) => self.recover_waiting(id, app, hash),
                })
            }
            ServerFrame::Subscribed { doc: id, head } => {
                let Some(doc) = self.docs.get_mut(&id) else { return Ok(Vec::new()) };
                if head < doc.have.upto {
                    // The server has less than this machine read of it (restored without a new epoch): from the start,
                    // so `have` names updates of this log, and then what the log lacks goes up.
                    doc.start_over();
                    return Ok(doc.resubscribe(id));
                }
                doc.sub = Sub::Live;
                doc.retry = None;
                let mut frames = send_pending(id, doc);
                frames.extend(doc.push_next(id, &mut self.notes, &mut self.peers));
                Ok(frames)
            }
            ServerFrame::Ack { doc: id, client_seq, seq } => {
                let Some(doc) = self.docs.get_mut(&id) else { return Ok(Vec::new()) };
                let Some(i) = doc.pending.iter().position(|p| p.client_seq == client_seq) else { return Ok(Vec::new()) };
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
                    doc.advance(seq, update_hash(&own.bytes));
                } else if let Some(pending) = doc.pending.get_mut(i) {
                    // Others' updates below it are on their way: counted once they are here. (A push part counts as
                    // sent: the next one is computed without it.)
                    pending.acked = Some(seq);
                    pending.push = false;
                }
                Ok(if push { doc.push_next(id, &mut self.notes, &mut self.peers) } else { Vec::new() })
            }
            ServerFrame::Error { code, message, doc, client_seq } => self.refused(&code, &message, doc, client_seq),
        }
    }

    fn refused(&mut self, code: &str, message: &str, doc: Option<DocId>, client_seq: Option<u64>) -> Result<Vec<ClientFrame>, LinkError> {
        if code == PROTOCOL_MISMATCH {
            return Err(LinkError::Mismatch(message.to_owned()));
        }
        let epoch = self.epoch;
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
                    let line = format!("the server refuses even small parts of the recents of {} ({message}); they stay on this machine", doc.app);
                    doc.say(&mut self.notes, line);
                    return Ok(Vec::new());
                }
                // Its operations go up in the parts of a push (of the whole document, if no push is under way).
                doc.pending.retain(|p| p.client_seq != client_seq);
                doc.sent.remove(&client_seq);
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
                let line = format!("the recents of {} are full on the server ({message}); new ones stay on this machine", doc.app);
                doc.say(&mut self.notes, line);
                Ok(Vec::new())
            }
            // The update at `after` isn't the one this machine has there: the log was restored (its epoch reset hasn't
            // run). From the start, and the whole document up again.
            (codes::RESYNC_REQUIRED, Some((id, doc)), None) => {
                doc.start_over();
                Ok(subscribe(id, doc, epoch))
            }
            // A create or subscribe refused for a moment (the budget, the device's cap), or a subscribe to a document
            // whose create was refused, or one repeated during a catch-up: tried again after a growing pause.
            (codes::RATE_LIMITED | codes::NOT_FOUND | codes::INVALID, Some((_, doc)), None) => {
                let pause = doc.retry.map_or(1, |(_, pause)| pause.max(1));
                doc.sub = Sub::No;
                doc.retry = Some((pause, pause));
                let line = format!("the server can't follow the recents of {} right now ({message}); trying again shortly", doc.app);
                doc.say(&mut self.notes, line);
                Ok(Vec::new())
            }
            _ => Err(LinkError::Retry(format!("the server refused a live frame ({code}: {message})"))),
        }
    }

    /// Writes every document changed since it was last written. Called after a batch of frames: a crash before it only
    /// loses what the server sends again.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let space = self.space;
        for (id, doc) in &mut self.docs {
            if doc.dirty {
                save(&mut self.store, space, *id, doc)?;
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
fn load_doc<S: DocStore>(store: &mut S, space: SpaceId, name: &str, bytes: &[u8], peer: u64, notes: &mut Vec<String>) -> Option<(DocId, Doc)> {
    let aside = |store: &mut S| match store.set_aside(name) {
        Ok(to) => (format!("a copy set aside as {to}"), true),
        Err(e) => (format!("no copy set aside ({e})"), false),
    };
    match postcard::from_bytes::<DocFile>(bytes) {
        Ok(file) if file.format <= DOC_FILE_FORMAT && file.space == space && file.doc == recents_doc(space, &file.app) => {
            let id = file.doc;
            let (app, pending, next) = (file.app.clone(), file.pending.clone(), file.next_client_seq);
            match Doc::from_file(file, peer, notes) {
                Ok(doc) => Some((id, doc)),
                Err(e) => {
                    // A copy aside first (durable), then the rebuilt document over the original: a crash between them
                    // finds the original again, and rebuilds again.
                    let (set, copied) = aside(store);
                    let mut doc = Doc::from_pending(app.clone(), pending, next, peer);
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

/// Whether every range of `ranges` is in `has`.
fn held_by(has: &Spans, ranges: &[(u64, i32, i32)]) -> bool {
    ranges.iter().all(|(peer, s, e)| s >= e || has.get(peer).is_some_and(|list| list.iter().any(|(hs, he)| hs <= s && e <= he)))
}

/// The document of `app` in `docs`, made (empty) if there is none yet.
fn doc_in<'a>(docs: &'a mut BTreeMap<DocId, Doc>, space: SpaceId, app: &AppName, peers: &mut Peers) -> (DocId, &'a mut Doc) {
    let id = recents_doc(space, app);
    let doc = docs.entry(id).or_insert_with(|| Doc::empty(app, peers()));
    (id, doc)
}

/// `create` (the server may lack the document) and `subscribe` from what it has, naming the update it has at that
/// number. A document that knew another epoch (the server was restored since) catches up from the start and goes up
/// whole after the catch-up. Acks of numbers not reached are forgotten: their updates go again after `subscribed`.
fn subscribe(id: DocId, doc: &mut Doc, epoch: Option<Epoch>) -> Vec<ClientFrame> {
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
    let mut frames = vec![ClientFrame::Create { doc: id, kind: DocKind::Recents { app: doc.app.clone() } }];
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

fn save<S: DocStore>(store: &mut S, space: SpaceId, id: DocId, doc: &mut Doc) -> std::io::Result<()> {
    let file = DocFile {
        format: DOC_FILE_FORMAT,
        space,
        doc: id,
        app: doc.app.clone(),
        have: doc.have.clone(),
        next_client_seq: doc.next_client_seq,
        pending: doc.pending.clone(),
        snapshot: doc.recents.snapshot().map_err(std::io::Error::other)?,
        waiting: doc.waiting.clone(),
        epoch: doc.epoch,
        pushing: doc.pushing.clone(),
        own_peers: doc.own_peers.clone(),
        push_wait: doc.push_wait,
    };
    let bytes = postcard::to_stdvec(&file).map_err(std::io::Error::other)?;
    store.save(&id.to_string(), &bytes)?;
    doc.dirty = false;
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
        r.docs.get(&doc).unwrap().recents.poison();
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
            r.docs.get(&doc).unwrap().recents.poison();
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
}
