//! The stability window's bookkeeping: which changed files may still be being written, and when to look at them
//! again. A pure state machine over an injected clock, so every rule here is tested without sleeps; the real disk
//! ([`crate::disk::DiskFs`]) keeps one behind a lock, shared with the `run` loop.
//!
//! - A changed file has **settled** once it kept its size and time for the window: between two looks at least the
//!   window apart, or already by its own time at the first look (its mtime is at least the window old, so a file
//!   that was written a while ago syncs on the first pass that sees it). On Windows the disk then also checks that no
//!   program still has it open for writing. The shortcut by its own time is off where times can't be trusted (a
//!   network share, whose clock may lag ours; FAT's coarse times): see [`Settling::trust_mtime`]. Elsewhere a writer
//!   that paused longer than the window looks settled too; the re-stat after hashing still keeps a file that changed
//!   while it was read from going up.
//! - A file that is **still changing** is looked at again after a delay that starts at the window and doubles each
//!   time it is found changed (or still open for writing), up to [`RECHECK_MAX`]. It starts over once the file
//!   settles. The loop runs its next pass when the earliest look is due ([`Settling::pass_finished`]), so a file
//!   that never settles (a log, a database held open) costs a pass every few minutes, not one every few seconds.
//! - Memory: one small entry per file still waiting. A finished pass forgets the files it didn't ask about (the
//!   engine asks about every changed file in every pass, so those went away or are back to their synced state).
//!   Nothing is evicted while it waits, however many files changed at once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use craft_cloud_sync::RelPath;

use crate::schedule::Ms;

/// The longest delay between two looks at a file that keeps changing.
pub const RECHECK_MAX: Duration = Duration::from_secs(5 * 60);

fn ms(d: Duration) -> Ms {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One file that may still be changing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Look {
    size: u64,
    mtime_ms: i64,
    /// Since when it has had this size and time (on the loop's clock).
    since: Ms,
    /// The current delay before the next look.
    delay: Ms,
    /// When the next look is due.
    due: Ms,
    /// When the engine last asked about it.
    asked: Ms,
    /// When the `run` loop takes a cheap look of its own (one stat, no pass), after the watcher reported it: see
    /// [`Settling::event`].
    peek: Option<Ms>,
}

/// The longest quiet time after a waiting file's last event before the loop takes a cheap look at it.
pub const QUIET_MAX: Duration = Duration::from_secs(30);

/// The files that may still be changing (see the module docs).
#[derive(Debug, Clone)]
pub struct Settling {
    window: Ms,
    looks: HashMap<RelPath, Look>,
    /// Whether a file's own mtime may settle it at the first look (see [`Settling::trust_mtime`]).
    trust_mtime: bool,
}

impl Settling {
    pub fn new(window: Duration) -> Self {
        Self { window: ms(window), looks: HashMap::new(), trust_mtime: true }
    }

    /// Whether a file whose mtime is already a window old may settle at the first look. Not on a filesystem whose
    /// times come from another machine's clock or are coarse (a network share, FAT; see
    /// [`crate::disk::mtime_trusted`]): there a file being written can look old at once, so it settles only after two
    /// looks a window apart.
    pub fn trust_mtime(&mut self, trust: bool) {
        self.trust_mtime = trust;
    }

    /// Whether `mtime_ms` is a window old by the wall clock (`wall_ms`) and may be trusted for that. A time in the
    /// future (another machine's clock ahead of ours) never is.
    fn old_enough(&self, mtime_ms: i64, wall_ms: i64) -> bool {
        self.trust_mtime && wall_ms.saturating_sub(mtime_ms) >= i64::try_from(self.window).unwrap_or(i64::MAX)
    }

    pub fn window(&self) -> Duration {
        Duration::from_millis(self.window)
    }

    /// A look at the changed file `path` (`size`, `mtime_ms`) at `now` on the loop's clock, when the wall clock says
    /// `wall_ms`. True when its size and time have held for the window (since an earlier look, or by its own time);
    /// the caller then makes its last check and reports [`Settling::settled`] or [`Settling::held_open`]. False:
    /// still changing, looked at again later.
    pub fn look(&mut self, path: &RelPath, size: u64, mtime_ms: i64, now: Ms, wall_ms: i64) -> bool {
        let window = self.window;
        let old_enough = self.old_enough(mtime_ms, wall_ms);
        match self.looks.get_mut(path) {
            Some(look) if look.size == size && look.mtime_ms == mtime_ms => {
                look.asked = now;
                old_enough || now.saturating_sub(look.since) >= window
            }
            Some(look) => {
                // Changed since the last look: still being written, unless that was a while ago already.
                look.size = size;
                look.mtime_ms = mtime_ms;
                look.since = now;
                look.asked = now;
                if old_enough {
                    return true;
                }
                look.delay = look.delay.saturating_mul(2).clamp(window, ms(RECHECK_MAX).max(window));
                look.due = now.saturating_add(look.delay);
                false
            }
            None if old_enough => true,
            None => {
                self.looks.insert(
                    path.clone(),
                    Look { size, mtime_ms, since: now, delay: window, due: now.saturating_add(window), asked: now, peek: None },
                );
                false
            }
        }
    }

    /// The file passed every check: it syncs, and its delay starts over if it changes again.
    pub fn settled(&mut self, path: &RelPath) {
        self.looks.remove(path);
    }

    /// The file kept its size and time, but a program still has it open for writing (Windows): looked at again later,
    /// a little later each time.
    pub fn held_open(&mut self, path: &RelPath, size: u64, mtime_ms: i64, now: Ms) {
        let window = self.window;
        let look = self.looks.entry(path.clone()).or_insert(Look { size, mtime_ms, since: now, delay: 0, due: now, asked: now, peek: None });
        look.asked = now;
        look.delay = look.delay.saturating_mul(2).clamp(window, ms(RECHECK_MAX).max(window));
        look.due = now.saturating_add(look.delay);
    }

    /// Whether `path` is waiting to settle.
    pub fn waiting(&self, path: &RelPath) -> bool {
        self.looks.contains_key(path)
    }

    /// How many files wait.
    pub fn len(&self) -> usize {
        self.looks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.looks.is_empty()
    }

    /// The watcher reported a change at `path` at `now`; `closed` when a writer closed it after writing (Linux).
    /// Returns whether the file is waiting to settle, in which case the event runs no pass: the loop takes a cheap
    /// look of its own once the file has been quiet for a quarter of its current delay (at most [`QUIET_MAX`]) since
    /// its last event, or at once after a close. Each event pushes that look back, so a file written all the time
    /// costs nothing; a file that went quiet (a long export that just ended) is looked at soon, not only when its
    /// backed-off look is due.
    pub fn event(&mut self, path: &RelPath, now: Ms, closed: bool) -> bool {
        let Some(look) = self.looks.get_mut(path) else { return false };
        let quiet = (look.delay / 4).min(ms(QUIET_MAX));
        look.peek = Some(if closed { now } else { now.saturating_add(quiet) });
        true
    }

    /// When the earliest cheap look is due, if any.
    pub fn next_peek(&self) -> Option<Ms> {
        self.looks.values().filter_map(|look| look.peek).min()
    }

    /// The files whose cheap look is due at `now`; each one's look is taken off until [`Settling::peeked`] or an
    /// event sets it again.
    pub fn take_peeks(&mut self, now: Ms) -> Vec<RelPath> {
        let mut due = Vec::new();
        for (path, look) in &mut self.looks {
            if look.peek.is_some_and(|at| at <= now) {
                look.peek = None;
                due.push(path.clone());
            }
        }
        due
    }

    /// The loop's cheap look at `path`: its size and time now (`None`: no longer a file there), at `now`, when the
    /// wall clock says `wall_ms`. True when a pass should run: the file kept its size and time for the window (by the
    /// same rule as [`Settling::look`], so the pass's look then settles it too), or it is gone. Otherwise the look is
    /// taken again once the window has passed since it last changed; a change recorded here counts for the pass's
    /// look as well. Neither the delay nor the pass's due look change.
    pub fn peeked(&mut self, path: &RelPath, now_meta: Option<(u64, i64)>, now: Ms, wall_ms: i64) -> bool {
        let window = self.window;
        let old_enough = now_meta.is_some_and(|(_, mtime_ms)| self.old_enough(mtime_ms, wall_ms));
        let Some(look) = self.looks.get_mut(path) else { return false };
        let Some((size, mtime_ms)) = now_meta else { return true };
        if look.size != size || look.mtime_ms != mtime_ms {
            look.size = size;
            look.mtime_ms = mtime_ms;
            look.since = now;
            look.peek = Some(now.saturating_add(window));
            return false;
        }
        if old_enough || now.saturating_sub(look.since) >= window {
            return true;
        }
        look.peek = Some(look.since.saturating_add(window));
        false
    }

    /// A pass that started at `started` finished: the files it didn't ask about are forgotten, and the time the
    /// earliest look at the others is due is returned.
    pub fn pass_finished(&mut self, started: Ms) -> Option<Ms> {
        self.looks.retain(|_, look| look.asked >= started);
        self.looks.values().map(|look| look.due).min()
    }
}

/// A [`Settling`] shared between the disk (which asks) and the `run` loop (which schedules the next look and leaves
/// the watcher's events for waiting files alone), with the loop's monotonic clock.
#[derive(Debug, Clone)]
pub struct Stability {
    settling: Arc<Mutex<Settling>>,
    origin: Instant,
}

impl Stability {
    pub fn new(window: Duration) -> Self {
        Self { settling: Arc::new(Mutex::new(Settling::new(window))), origin: Instant::now() }
    }

    /// Now, on the clock the looks are kept on.
    pub fn now(&self) -> Ms {
        ms(self.origin.elapsed())
    }

    pub fn lock(&self) -> MutexGuard<'_, Settling> {
        self.settling.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A pass that started at `started` (see [`Stability::now`]) finished: how long until the earliest look is due.
    pub fn pass_finished(&self, started: Ms) -> Option<Duration> {
        let due = self.lock().pass_finished(started)?;
        Some(Duration::from_millis(due.saturating_sub(self.now())))
    }

    /// Whether `path` is waiting to settle.
    pub fn waiting(&self, path: &RelPath) -> bool {
        self.lock().waiting(path)
    }

    /// The watcher reported a change at `path` (see [`Settling::event`]): whether it is a waiting file, whose event
    /// runs no pass.
    pub fn event(&self, path: &RelPath, closed: bool) -> bool {
        let now = self.now();
        self.lock().event(path, now, closed)
    }

    /// How long until the earliest cheap look is due.
    pub fn next_peek_in(&self) -> Option<Duration> {
        let due = self.lock().next_peek()?;
        Some(Duration::from_millis(due.saturating_sub(self.now())))
    }

    /// The files whose cheap look is due now.
    pub fn take_peeks(&self) -> Vec<RelPath> {
        let now = self.now();
        self.lock().take_peeks(now)
    }

    /// Whether a file's own time may settle it at the first look (see [`Settling::trust_mtime`]).
    pub fn trust_mtime(&self, trust: bool) {
        self.lock().trust_mtime(trust);
    }

    /// The loop's cheap look at `path` (see [`Settling::peeked`]): whether a pass should run for it.
    pub fn peeked(&self, path: &RelPath, now_meta: Option<(u64, i64)>, wall_ms: i64) -> bool {
        let now = self.now();
        self.lock().peeked(path, now_meta, now, wall_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_sync::rel;

    const WINDOW: Ms = 2_000;
    /// A wall clock far from the files' times, so only the looks decide.
    const LONG_AFTER: i64 = 0;

    fn settling() -> Settling {
        Settling::new(Duration::from_millis(WINDOW))
    }

    #[test]
    fn a_file_settles_after_two_looks_a_window_apart() {
        let mut s = settling();
        let p = rel("render.mov");
        // Written just now (its mtime is the wall clock's now).
        assert!(!s.look(&p, 10, 5_000, 0, 5_000), "the first look");
        assert_eq!(s.pass_finished(0), Some(WINDOW), "looked at again after the window");
        assert!(!s.look(&p, 10, 5_000, 1_000, 6_000), "too soon");
        assert!(s.look(&p, 10, 5_000, 2_000, 7_000), "unchanged a window later");
        s.settled(&p);
        assert!(!s.waiting(&p));
        assert_eq!(s.pass_finished(2_000), None);
    }

    #[test]
    fn a_file_written_a_while_ago_settles_at_the_first_look() {
        let mut s = settling();
        let p = rel("old.psd");
        assert!(s.look(&p, 10, 1_000, 0, 1_000 + 2_000), "its time is a window old");
        assert!(!s.look(&rel("new.psd"), 10, 1_000, 0, 1_000 + 1_999), "a moment younger: it waits");
        // A file in the future (another machine's clock) is never old enough by its own time.
        assert!(!s.look(&rel("future.psd"), 10, 9_000, 0, 1_000));
    }

    /// Finding 8 of the second review: on a filesystem whose times can't be trusted (a share whose clock lags), a file
    /// that looks old at once still needs two looks a window apart, at the pass and at the loop's cheap look.
    #[test]
    fn an_untrusted_mtime_never_settles_a_file_at_the_first_look() {
        let mut s = settling();
        s.trust_mtime(false);
        let p = rel("on-a-share.psd");
        assert!(!s.look(&p, 10, 1_000, 0, 1_000_000), "old by its time, but that time can't be trusted");
        assert!(s.event(&p, 100, true));
        assert_eq!(s.take_peeks(100), vec![p.clone()]);
        assert!(!s.peeked(&p, Some((10, 1_000)), 100, 1_000_000), "the cheap look doesn't trust it either");
        assert!(s.look(&p, 10, 1_000, WINDOW, 1_000_000), "unchanged a window later");
        s.trust_mtime(true);
        assert!(s.look(&rel("local.psd"), 10, 1_000, 0, 1_000_000), "a local disk's time is trusted");
    }

    #[test]
    fn a_file_that_keeps_changing_is_looked_at_less_and_less_often() {
        let mut s = settling();
        let p = rel("app.log");
        let mut now = 0;
        let mut size = 1;
        assert!(!s.look(&p, size, LONG_AFTER + now as i64, now, LONG_AFTER + now as i64));
        let mut delays = Vec::new();
        for _ in 0..10 {
            let due = s.pass_finished(now).unwrap();
            delays.push(due - now);
            now = due;
            size += 1;
            // Appended again just before each look.
            assert!(!s.look(&p, size, now as i64 - 100, now, now as i64));
        }
        assert_eq!(delays, [2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 256_000, 300_000, 300_000]);
        // It stops changing: the next look finds it a window old and it settles; the delay starts over.
        now += 300_000;
        assert!(s.look(&p, size, now as i64 - 300_000, now, now as i64));
        s.settled(&p);
        assert!(!s.look(&p, size + 1, now as i64, now, now as i64));
        assert_eq!(s.pass_finished(now), Some(now + WINDOW));
    }

    #[test]
    fn a_file_held_open_for_writing_is_looked_at_less_and_less_often() {
        let mut s = settling();
        let p = rel("mail.pst");
        let mut now = 0;
        let mut delays = Vec::new();
        for _ in 0..4 {
            // Unchanged for ages, but a writer has it open.
            assert!(s.look(&p, 10, -1_000_000, now, now as i64));
            s.held_open(&p, 10, -1_000_000, now);
            let due = s.pass_finished(now).unwrap();
            delays.push(due - now);
            now = due;
        }
        assert_eq!(delays, [2_000, 4_000, 8_000, 16_000]);
    }

    /// Finding 2 of the review: more files than the old cap of 4096 looks all settle in two passes.
    #[test]
    fn any_number_of_changed_files_settles() {
        let mut s = settling();
        let paths: Vec<RelPath> = (0..10_000).map(|i| rel(&format!("photos/{i:05}.jpg"))).collect();
        for p in &paths {
            assert!(!s.look(p, 1, 0, 0, 0));
        }
        assert_eq!(s.pass_finished(0), Some(WINDOW));
        assert_eq!(s.len(), 10_000);
        for p in &paths {
            assert!(s.look(p, 1, 0, WINDOW, 0), "{p} settles");
            s.settled(p);
        }
        assert!(s.is_empty());
    }

    /// A file found changed at every look until its delay is at the cap.
    fn backed_off(s: &mut Settling, p: &RelPath) -> Ms {
        let mut now = 0;
        let mut size = 1;
        assert!(!s.look(p, size, now as i64, now, now as i64));
        while s.pass_finished(now).unwrap() - now < ms(RECHECK_MAX) {
            now = s.pass_finished(now).unwrap();
            size += 1;
            assert!(!s.look(p, size, now as i64, now, now as i64));
        }
        now
    }

    /// Finding 3 of the second review: a long export that ends just after a look at the 5-minute cap is looked at
    /// again 30 s after its last write (a quarter of its delay, at most 30 s), not when the 5 minutes are up, and the
    /// look that finds it settled runs a pass whose own look settles it.
    #[test]
    fn a_backed_off_file_that_goes_quiet_is_looked_at_soon() {
        let mut s = settling();
        let p = rel("export.mov");
        let now = backed_off(&mut s, &p);
        let due = s.pass_finished(now).unwrap();
        assert_eq!(due - now, ms(RECHECK_MAX));
        // The export goes on writing for a while after the look: every event pushes the cheap look back.
        let (size, mut t) = (999, now);
        for _ in 0..20 {
            t += 1_000;
            assert!(s.event(&p, t, false), "a waiting file's event runs no pass");
            assert_eq!(s.next_peek(), Some(t + 30_000));
            assert!(s.take_peeks(t).is_empty());
        }
        let last = t;
        // Quiet from here on: the cheap look comes 30 s after the last event.
        assert!(s.take_peeks(last + 29_999).is_empty());
        assert_eq!(s.take_peeks(last + 30_000), vec![p.clone()]);
        assert_eq!(s.next_peek(), None, "taken");
        // It changed since the pass looked: not settled yet, looked at again a window later.
        assert!(!s.peeked(&p, Some((size, last as i64)), last + 30_000, last as i64 + 30_000 - 1_000_000));
        assert_eq!(s.next_peek(), Some(last + 30_000 + WINDOW));
        let at = last + 30_000 + WINDOW;
        assert_eq!(s.take_peeks(at), vec![p.clone()]);
        assert!(s.peeked(&p, Some((size, last as i64)), at, at as i64 - 1_000_000), "unchanged for the window: a pass runs");
        // The pass's own look settles it, long before the 5-minute look was due.
        assert!(at < due);
        assert!(s.look(&p, size, last as i64, at + 1_500, at as i64 - 1_000_000));
    }

    /// A file closed after writing (Linux) is looked at once; a file not waiting runs a pass as usual; a file gone is
    /// a pass too.
    #[test]
    fn a_closed_file_is_looked_at_once() {
        let mut s = settling();
        let p = rel("render.exr");
        let now = backed_off(&mut s, &p);
        // The writer closes it: looked at at once. It was written since the pass's look, so it settles a window later.
        assert!(s.event(&p, now + 10, true));
        assert_eq!(s.take_peeks(now + 10), vec![p.clone()]);
        let written = now as i64 + 10;
        assert!(!s.peeked(&p, Some((5_000, written)), now + 10, written));
        assert_eq!(s.take_peeks(now + 10 + WINDOW), vec![p.clone()]);
        assert!(s.peeked(&p, Some((5_000, written)), now + 10 + WINDOW, written + WINDOW as i64));
        assert!(!s.event(&rel("other.txt"), now, false), "not waiting: the event runs a pass");
        assert!(s.event(&p, now + 20, false));
        assert_eq!(s.take_peeks(now + 20 + 30_000), vec![p.clone()]);
        assert!(s.peeked(&p, None, now + 20 + 30_000, 0), "gone: a pass sorts it out");
    }

    /// A small delay gives a short quiet time: a quarter of it.
    #[test]
    fn the_quiet_time_is_a_quarter_of_the_delay() {
        let mut s = settling();
        let p = rel("a.txt");
        assert!(!s.look(&p, 1, 0, 0, 0));
        assert!(s.event(&p, 100, false));
        assert_eq!(s.next_peek(), Some(100 + WINDOW / 4));
    }

    #[test]
    fn a_finished_pass_forgets_the_files_it_didnt_ask_about() {
        let mut s = settling();
        assert!(!s.look(&rel("gone.txt"), 1, 0, 0, 0));
        assert!(!s.look(&rel("kept.txt"), 1, 0, 0, 0));
        assert!(!s.look(&rel("kept.txt"), 1, 0, 500, 0));
        assert_eq!(s.pass_finished(500), Some(WINDOW));
        assert!(!s.waiting(&rel("gone.txt")) && s.waiting(&rel("kept.txt")));
    }
}
