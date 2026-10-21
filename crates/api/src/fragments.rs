//! Live updates over the 1 MiB frame limit, sent in fragments: one update (a Loro update of any size up to
//! [`MAX_WHOLE_BYTES`]) cut into frames of at most the limit, each a fragment naming its set (the first 16 bytes of the
//! update's BLAKE3), its index and the set's count. The agent's local API joins an app's fragments in a
//! [`Reassembler`]; its replica keeps the log's fragment rows until they are whole; either imports the update only once
//! it holds every fragment of it: no document ever holds part of a change. The
//! server stages an agent's fragments and stores a set in one go, as updates of their own, or refuses it whole; an
//! agent's log of rows keeps them as they are. Agents hand apps whole changes only.
//!
//! A fragment starts with [`MAGIC`], which no Loro update starts with (Loro's begin `loro`), so a program that doesn't
//! know fragments refuses one as an update that doesn't decode rather than importing part of a change. The cut is
//! deterministic: the same update always gives the same fragments, so a set sent again (after a reconnect) is the same
//! set, and the server keeps a fragment it already has once.
//!
//! What goes in fragments (or in one frame) may be a [`bundle`]: a big change as slices of it, imported together.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::limits::MAX_SNAPSHOT_BYTES;

/// What every fragment starts with.
pub const MAGIC: [u8; 4] = *b"ccf1";
/// What a bundle starts with: one change as several Loro updates, imported one after the other and together
/// ([`bundle`], [`parts`]). Loro's import of one update takes time growing with the square of the text in it (a 1 MiB
/// paste from another peer: seconds), so a big change goes as slices of it, which import in milliseconds, in one
/// bundle, so no receiver ever holds some of the slices and not the others.
pub const BUNDLE_MAGIC: [u8; 4] = *b"ccb1";
/// The most operations a change's slice holds ([`bundle`]'s parts, as the kit and the agent cut them).
pub const SLICE_OPS: i32 = 16 * 1024;
/// The bytes before a fragment's part of the update: [`MAGIC`], the set (16), the index and the count (2 each, big
/// endian).
pub const HEADER_BYTES: usize = 24;
/// The largest change (one update) that may be sent in fragments: 8 MiB, well under the server's 16 MiB cap on a
/// document's log above its snapshot, so a change always fits a document that has room for updates at all.
pub const MAX_WHOLE_BYTES: usize = 8 * 1024 * 1024;
const _: () = assert!(MAX_WHOLE_BYTES <= MAX_SNAPSHOT_BYTES);
/// The most fragments one update is cut into.
pub const MAX_FRAGMENTS: usize = 4096;
/// The most sets a [`Reassembler`] (or anything that keeps fragments) holds at once: each costs bookkeeping besides its
/// bytes.
pub const MAX_SETS: usize = 16;
/// How long the agent's local API keeps the fragments of an app's change that stopped coming (the app went part of
/// the way) before it drops them, never taken.
pub const PATIENCE: Duration = Duration::from_secs(60);

/// What a kept fragment costs beside its part of the update (its index, its map entry).
pub const FRAGMENT_OVERHEAD: usize = 64;

/// The name of a set: the first 16 bytes of its update's BLAKE3.
pub type SetId = [u8; 16];

/// One fragment, as [`parse`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment<'a> {
    pub set: SetId,
    pub index: u16,
    pub count: u16,
    /// Its part of the update.
    pub body: &'a [u8],
}

/// Why an update can't be sent in fragments.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    #[error("an update is at most {MAX_WHOLE_BYTES} bytes ({0} here)")]
    TooLarge(usize),
    #[error("a frame of {0} bytes can't hold a fragment")]
    FrameTooSmall(usize),
}

/// The set an update's fragments name.
pub fn set_of(update: &[u8]) -> SetId {
    let hash = blake3::hash(update);
    let mut set = [0u8; 16];
    set.copy_from_slice(&hash.as_bytes()[..16]);
    set
}

/// `update` as frames of at most `max_frame` bytes: itself when it fits (not a fragment), else its fragments, in
/// order. Deterministic.
pub fn split(update: &[u8], max_frame: usize) -> Result<Vec<Vec<u8>>, SplitError> {
    if update.len() <= max_frame {
        return Ok(vec![update.to_vec()]);
    }
    if update.len() > MAX_WHOLE_BYTES {
        return Err(SplitError::TooLarge(update.len()));
    }
    let room = max_frame.saturating_sub(HEADER_BYTES);
    if room == 0 {
        return Err(SplitError::FrameTooSmall(max_frame));
    }
    let count = update.len().div_ceil(room);
    let Ok(count16) = u16::try_from(count) else { return Err(SplitError::FrameTooSmall(max_frame)) };
    if count > MAX_FRAGMENTS {
        return Err(SplitError::FrameTooSmall(max_frame));
    }
    let set = set_of(update);
    let mut out = Vec::with_capacity(count);
    for (index, body) in (0u16..).zip(update.chunks(room)) {
        let mut frame = Vec::with_capacity(HEADER_BYTES + body.len());
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&set);
        frame.extend_from_slice(&index.to_be_bytes());
        frame.extend_from_slice(&count16.to_be_bytes());
        frame.extend_from_slice(body);
        out.push(frame);
    }
    Ok(out)
}

/// Whether `bytes` are a fragment (they start with [`MAGIC`]; [`parse`] says whether one reads).
pub fn is_fragment(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

/// A fragment's header and part, if `bytes` are a well-formed fragment.
pub fn parse(bytes: &[u8]) -> Option<Fragment<'_>> {
    if !is_fragment(bytes) || bytes.len() <= HEADER_BYTES {
        return None;
    }
    let set: SetId = bytes.get(4..20)?.try_into().ok()?;
    let index = u16::from_be_bytes(bytes.get(20..22)?.try_into().ok()?);
    let count = u16::from_be_bytes(bytes.get(22..24)?.try_into().ok()?);
    if count < 2 || usize::from(count) > MAX_FRAGMENTS || index >= count {
        return None;
    }
    Some(Fragment { set, index, count, body: bytes.get(HEADER_BYTES..)? })
}

/// The update `parts` (one set's fragments, any order, each index at least once) make, if they are all of it and
/// they make the update the set names.
pub fn join<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    let mut set = None;
    let mut bodies: BTreeMap<u16, &[u8]> = BTreeMap::new();
    let mut count = 0;
    for bytes in parts {
        let f = parse(bytes)?;
        if set.is_some_and(|s| s != f.set) || (count != 0 && count != f.count) {
            return None;
        }
        set = Some(f.set);
        count = f.count;
        bodies.entry(f.index).or_insert(f.body);
    }
    if count == 0 || bodies.len() != usize::from(count) {
        return None;
    }
    let whole: Vec<u8> = bodies.values().flat_map(|b| b.iter().copied()).collect();
    (Some(set_of(&whole)) == set).then_some(whole)
}

/// One change's updates (its slices, in order) as one: the update itself when there is one, else a bundle
/// ([`BUNDLE_MAGIC`], their count, and each one's length and bytes; lengths and count are 4 bytes, big endian).
pub fn bundle(updates: &[Vec<u8>]) -> Vec<u8> {
    if let [one] = updates {
        return one.clone();
    }
    let len = 8 + updates.iter().map(|u| 4 + u.len()).sum::<usize>();
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&BUNDLE_MAGIC);
    out.extend_from_slice(&u32::try_from(updates.len()).unwrap_or(u32::MAX).to_be_bytes());
    for u in updates {
        out.extend_from_slice(&u32::try_from(u.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(u);
    }
    out
}

/// Whether `bytes` are a bundle (they start with [`BUNDLE_MAGIC`]).
pub fn is_bundle(bytes: &[u8]) -> bool {
    bytes.starts_with(&BUNDLE_MAGIC)
}

/// The updates to import for `bytes`, in order: a bundle's, or the update itself; `None` for a bundle that doesn't
/// read (nothing of it is imported).
pub fn parts(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    if !is_bundle(bytes) {
        return Some(vec![bytes]);
    }
    let count = u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
    let mut out = Vec::with_capacity(count.min(bytes.len() / 4));
    let mut at = 8;
    for _ in 0..count {
        let len = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        out.push(bytes.get(at + 4..at.checked_add(4 + len)?)?);
        at += 4 + len;
    }
    (at == bytes.len()).then_some(out)
}

/// What [`Reassembler::take`] did with a fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// It was the last of its set: the whole update, to import.
    Whole(Vec<u8>),
    /// Kept: more of its set is to come.
    Waiting,
    /// Not kept: it doesn't read, doesn't fit its set, its set doesn't make the update it names, or too much waits.
    Refused(&'static str),
}

struct Partial {
    count: u16,
    parts: BTreeMap<u16, Vec<u8>>,
    bytes: usize,
    last: Instant,
}

/// The fragments of the sets still coming, by set: a set is imported only once it is whole, and dropped (never
/// applied) when no fragment of it came for a while ([`Reassembler::expire`]) or the connection it came on ends.
pub struct Reassembler {
    sets: HashMap<SetId, Partial>,
    bytes: usize,
    max_bytes: usize,
}

impl std::fmt::Debug for Reassembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reassembler").field("sets", &self.sets.len()).field("bytes", &self.bytes).finish()
    }
}

impl Reassembler {
    /// One that keeps at most `max_bytes` of fragments.
    pub fn new(max_bytes: usize) -> Self {
        Self { sets: HashMap::new(), bytes: 0, max_bytes }
    }

    /// Takes a fragment that came at `now`.
    pub fn take(&mut self, frame: &[u8], now: Instant) -> Taken {
        let Some(f) = parse(frame) else { return Taken::Refused("not a fragment") };
        // (Each fragment is charged its bookkeeping too, so tiny ones can't make many sets cheap.)
        let cost = f.body.len().saturating_add(FRAGMENT_OVERHEAD);
        let fits = self.bytes.saturating_add(cost) <= self.max_bytes;
        let room = self.sets.len() < MAX_SETS;
        let partial = match self.sets.get_mut(&f.set) {
            Some(p) if p.count != f.count => return Taken::Refused("a fragment that doesn't fit its set"),
            Some(p) => p,
            None if !fits || !room => return Taken::Refused("too many fragments wait"),
            None => self.sets.entry(f.set).or_insert(Partial { count: f.count, parts: BTreeMap::new(), bytes: 0, last: now }),
        };
        partial.last = now;
        if let std::collections::btree_map::Entry::Vacant(slot) = partial.parts.entry(f.index) {
            if !fits {
                return Taken::Refused("too many fragments wait");
            }
            slot.insert(f.body.to_vec());
            partial.bytes += cost;
            self.bytes += cost;
        }
        if partial.parts.len() < usize::from(partial.count) {
            return Taken::Waiting;
        }
        let Some(done) = self.sets.remove(&f.set) else { return Taken::Waiting };
        self.bytes = self.bytes.saturating_sub(done.bytes);
        let whole: Vec<u8> = done.parts.into_values().flatten().collect();
        if set_of(&whole) == f.set { Taken::Whole(whole) } else { Taken::Refused("its fragments don't make the update they name") }
    }

    /// Drops the sets no fragment of which came within `patience` of `now`: how many.
    pub fn expire(&mut self, now: Instant, patience: Duration) -> usize {
        let before = self.sets.len();
        let mut freed = 0;
        self.sets.retain(|_, p| {
            let keep = now.saturating_duration_since(p.last) < patience;
            if !keep {
                freed += p.bytes;
            }
            keep
        });
        self.bytes = self.bytes.saturating_sub(freed);
        before - self.sets.len()
    }

    /// Whether no set is coming.
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// How many sets are coming.
    pub fn len(&self) -> usize {
        self.sets.len()
    }

    /// The bytes of fragments kept.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i * 7 % 251).unwrap_or(0)).collect()
    }

    #[test]
    fn an_update_that_fits_is_one_frame_and_not_a_fragment() {
        let u = update(100);
        assert_eq!(split(&u, 100).unwrap(), vec![u.clone()]);
        assert!(!is_fragment(&u));
    }

    #[test]
    fn a_big_update_is_cut_in_frames_that_fit_and_join_in_any_order() {
        let u = update(10_000);
        let frames = split(&u, 1000).unwrap();
        assert_eq!(frames.len(), 10_000usize.div_ceil(1000 - HEADER_BYTES));
        assert!(frames.iter().all(|f| f.len() <= 1000 && is_fragment(f)));
        assert_eq!(split(&u, 1000).unwrap(), frames, "deterministic");
        let mut r = Reassembler::new(1 << 20);
        let now = Instant::now();
        let mut order: Vec<&Vec<u8>> = frames.iter().rev().collect();
        // One fragment twice: kept once.
        order.insert(1, &frames[frames.len() - 1]);
        let last = order.pop().unwrap();
        for f in order {
            assert_eq!(r.take(f, now), Taken::Waiting);
        }
        assert_eq!(r.take(last, now), Taken::Whole(u.clone()));
        assert!(r.is_empty());
        assert_eq!(r.bytes(), 0);
        assert_eq!(join(frames.iter().map(Vec::as_slice)), Some(u));
    }

    #[test]
    fn a_set_that_stops_coming_is_dropped_and_never_whole() {
        let frames = split(&update(5000), 1000).unwrap();
        let mut r = Reassembler::new(1 << 20);
        let now = Instant::now();
        for f in &frames[..frames.len() - 1] {
            assert_eq!(r.take(f, now), Taken::Waiting);
        }
        assert_eq!(r.expire(now + PATIENCE / 2, PATIENCE), 0);
        assert_eq!(r.expire(now + PATIENCE, PATIENCE), 1);
        assert!(r.is_empty());
        assert_eq!(r.bytes(), 0);
        // Its last fragment alone, later: a set of its own again, still waiting.
        assert_eq!(r.take(&frames[frames.len() - 1], now + PATIENCE), Taken::Waiting);
        assert!(join(frames[1..].iter().map(Vec::as_slice)).is_none(), "a part missing");
    }

    #[test]
    fn a_bundle_gives_its_updates_back_and_one_update_is_itself() {
        let ups = vec![update(10), Vec::new(), update(70_000)];
        let b = bundle(&ups);
        assert!(is_bundle(&b));
        assert_eq!(parts(&b).unwrap(), ups.iter().map(Vec::as_slice).collect::<Vec<_>>());
        assert_eq!(bundle(&[update(5)]), update(5), "one update: itself");
        assert_eq!(parts(&update(5)).unwrap(), vec![update(5).as_slice()]);
        assert!(parts(&b[..b.len() - 1]).is_none(), "cut short");
        let mut longer = b.clone();
        longer.push(0);
        assert!(parts(&longer).is_none(), "junk after it");
        assert!(parts(b"ccb1\xff\xff\xff\xff").is_none());
    }

    #[test]
    fn a_forged_or_broken_set_is_refused() {
        let u = update(3000);
        let mut frames = split(&u, 1000).unwrap();
        // A changed byte: the set doesn't make the update it names.
        let last = frames.len() - 1;
        if let Some(b) = frames[last].last_mut() {
            *b ^= 1;
        }
        let mut r = Reassembler::new(1 << 20);
        let now = Instant::now();
        for f in &frames[..last] {
            r.take(f, now);
        }
        assert!(matches!(r.take(&frames[last], now), Taken::Refused(_)));
        assert!(parse(b"ccf1short").is_none());
        assert!(matches!(r.take(b"loro", now), Taken::Refused(_)));
        let mut small = Reassembler::new(10);
        assert_eq!(small.take(&split(&u, 1000).unwrap()[0], now), Taken::Refused("too many fragments wait"));
        assert_eq!(split(&update(MAX_WHOLE_BYTES + 1), 1 << 20), Err(SplitError::TooLarge(MAX_WHOLE_BYTES + 1)));
        assert_eq!(split(&u, HEADER_BYTES), Err(SplitError::FrameTooSmall(HEADER_BYTES)));
    }

    #[test]
    fn the_sets_kept_are_bounded_in_number_as_well_as_bytes() {
        let mut r = Reassembler::new(1 << 30);
        let now = Instant::now();
        for i in 0..MAX_SETS {
            let frames = split(&[update(300), vec![u8::try_from(i).unwrap()]].concat(), 100).unwrap();
            assert_eq!(r.take(&frames[0], now), Taken::Waiting);
        }
        let one_more = split(&update(301), 100).unwrap();
        assert_eq!(r.take(&one_more[0], now), Taken::Refused("too many fragments wait"));
        assert_eq!(r.len(), MAX_SETS);
    }
}
