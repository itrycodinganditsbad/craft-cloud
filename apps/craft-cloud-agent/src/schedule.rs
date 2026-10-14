//! When the `run` loop syncs: a pure state machine over an injected clock and random source, so every rule here is
//! tested without threads or sleeps. The loop ([`crate::daemon`]) feeds it what happens (a change in the folder,
//! one on the server, a pass ending) and asks it what to do next ([`Scheduler::next`]).
//!
//! - **Debounce**: changes in the folder are collected; a pass runs once nothing new has come for
//!   [`Timing::debounce`], but no later than [`Timing::max_delay`] after the first change, so a folder that is
//!   always busy still syncs. A change on the server (the long-poll) or a watcher that lost events runs a pass
//!   without waiting.
//! - **Stability**: a pass that left files waiting as still changing runs again when the earliest look at them is
//!   due ([`crate::settle`]: the window, doubling for a file that keeps changing, up to 5 minutes), not only on the
//!   next event.
//! - **Safety net**: a pass at least every [`Timing::periodic`], or every [`Timing::unwatched_periodic`] while the
//!   folder isn't watched (the watcher couldn't start).
//! - **Backoff**: a pass that failed for a reason that may pass by itself (offline, a server error) is tried again
//!   after an exponentially growing delay with jitter, capped at [`Timing::backoff_max`]; a pass that finishes
//!   resets it. A change on the server (the server answered, so it is back) runs a pass at once.
//! - **Paused**: a problem that needs the user stops the passes. Some are rechecked by themselves (see
//!   [`Recheck`]); the others wait until the loop is stopped.
//! - **Approval**: while deletions wait for approval, the loop looks for an approval every
//!   [`Timing::approval_check`]; passes go on meanwhile (the guard only holds the deletions back). While only
//!   deletes held behind a link wait (a hold may stand for days, and pauses nothing), it looks every
//!   [`Timing::held_check`].
//! - **Stop**: no new pass starts; a running one is waited for.

use std::time::Duration;

/// A point on the loop's monotonic clock, in milliseconds since it started.
pub type Ms = u64;

fn ms(d: Duration) -> Ms {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The loop's timings. [`Timing::default`] is what `run` uses; tests make them short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Quiet time after the last change in the folder before a pass runs.
    pub debounce: Duration,
    /// Longest wait from the first change in the folder to a pass, however busy the folder stays.
    pub max_delay: Duration,
    /// How long a changed file must keep its size and time before it syncs (the stability window, see
    /// [`crate::settle`]; the pass reports when the next look at a file still changing is due).
    pub stability: Duration,
    /// A full pass at least this often, whatever the triggers say.
    pub periodic: Duration,
    /// A full pass at least this often while the folder isn't watched ([`Scheduler::set_watching`]).
    pub unwatched_periodic: Duration,
    /// The first retry delay after a failed pass; it doubles with every failure.
    pub backoff_first: Duration,
    /// The longest retry delay.
    pub backoff_max: Duration,
    /// How often a pause that is rechecked by itself is looked at again ([`Recheck::Soon`]).
    pub recheck: Duration,
    /// How often a pause rechecked slowly is looked at again ([`Recheck::Slowly`]).
    pub recheck_slow: Duration,
    /// How often the loop looks for an approval while deletions wait for one.
    pub approval_check: Duration,
    /// How often the loop looks for an approval while only deletes held behind a link can be approved.
    pub held_check: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(1_500),
            max_delay: Duration::from_secs(10),
            stability: Duration::from_secs(2),
            periodic: Duration::from_secs(10 * 60),
            unwatched_periodic: Duration::from_secs(60),
            backoff_first: Duration::from_secs(2),
            backoff_max: Duration::from_secs(5 * 60),
            recheck: Duration::from_secs(60),
            recheck_slow: Duration::from_secs(10 * 60),
            approval_check: Duration::from_secs(2),
            held_check: Duration::from_secs(30),
        }
    }
}

/// Whether a pause looks again by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    /// Something the user may put right at any moment, and checking is harmless (a folder or marker missing, a
    /// drive not mounted): looked at again every [`Timing::recheck`], and the loop goes on once it is back.
    Soon,
    /// Something only an update or an admin fixes (the server speaks another protocol, it or a proxy refuses
    /// requests): looked at again every [`Timing::recheck_slow`].
    Slowly,
    /// Something trying again never fixes (the device was revoked, saved files are damaged or another account's,
    /// the server sent damaged data): the loop stays paused until it is stopped.
    Never,
}

/// How a pass ended, as far as scheduling goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassEnd {
    /// It finished. `recheck_in`: files that may still be being written wait, and the earliest is looked at again
    /// after this long ([`crate::settle`]). `needs_approval`: deletions wait for approval (the mass-delete guard).
    /// `held`: deletes are held behind a link, which an approval naming their set releases.
    Done { recheck_in: Option<Duration>, needs_approval: bool, held: bool },
    /// It failed for a reason that may pass by itself (exit code 1).
    Failed(String),
    /// It stopped for something that needs the user (exit code 3).
    NeedsYou { reason: String, recheck: Recheck },
}

/// What the loop is doing, for the status file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for something to sync.
    Idle,
    /// A pass is running.
    Syncing,
    /// The last pass failed; the next try is at `next_try`.
    Retrying { next_try: Ms, message: String },
    /// Stopped until the user does something; `next_check` when it looks again by itself.
    Paused { reason: String, next_check: Option<Ms> },
    /// Stopping: no new pass starts.
    Stopping,
}

/// What the loop should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Start a pass (then report [`Scheduler::pass_started`] and, when it ends, [`Scheduler::pass_finished`]).
    RunPass,
    /// Look for an approval of the waiting deletions (then report [`Scheduler::approval_checked`]).
    CheckApproval,
    /// Wait for an event, at most until this time (`None`: until an event).
    Wait(Option<Ms>),
    /// Stopped and no pass running: the loop ends.
    Exit,
}

/// The scheduling state. See the module docs for the rules.
pub struct Scheduler {
    timing: Timing,
    /// A random number source for the backoff's jitter.
    random: Box<dyn FnMut() -> u64 + Send>,
    /// Changes in the folder since the last pass started: when the first and the last came.
    first_change: Option<Ms>,
    last_change: Option<Ms>,
    /// A pass should run at once (a change on the server, a watcher that lost events, a start).
    urgent: bool,
    /// A pass skipped a file as still changing: look again at this time.
    stability_due: Option<Ms>,
    /// When the last pass ended (or the loop started).
    last_pass_end: Ms,
    running: bool,
    failures: u32,
    retry: Option<(Ms, String)>,
    paused: Option<(String, Recheck, Option<Ms>)>,
    waiting_approval: bool,
    /// Deletes are held behind a link (as of the last pass that finished).
    holding: bool,
    next_approval_check: Ms,
    stopping: bool,
    /// Whether the folder is watched (otherwise the safety net runs more often).
    watching: bool,
}

impl Scheduler {
    /// A scheduler that starts with a pass (`now` is the loop's start). `random` gives the jitter.
    pub fn new(timing: Timing, now: Ms, random: impl FnMut() -> u64 + Send + 'static) -> Self {
        Self {
            timing,
            random: Box::new(random),
            first_change: None,
            last_change: None,
            urgent: true,
            stability_due: None,
            last_pass_end: now,
            running: false,
            failures: 0,
            retry: None,
            paused: None,
            waiting_approval: false,
            holding: false,
            next_approval_check: now,
            stopping: false,
            watching: true,
        }
    }

    pub fn timing(&self) -> &Timing {
        &self.timing
    }

    /// Something changed in the folder (a watcher event).
    pub fn local_change(&mut self, now: Ms) {
        self.first_change.get_or_insert(now);
        self.last_change = Some(now);
    }

    /// Something changed on the server, the watcher lost events (its queue overflowed, it failed) or an approval
    /// came: a pass runs at once, or when a pending retry is due (only a pass that finishes ends the backoff).
    pub fn urgent_change(&mut self) {
        self.urgent = true;
    }

    /// The server answers again after failing (the long-poll got through): a pending retry runs now. The backoff
    /// isn't reset: if the pass fails again, the next delay goes on growing.
    pub fn server_back(&mut self) {
        self.urgent = true;
        if let Some((at, _)) = &mut self.retry {
            *at = 0;
        }
    }

    /// Whether the folder is watched: while it isn't (the watcher couldn't start), a full pass runs every
    /// [`Timing::unwatched_periodic`] instead of every [`Timing::periodic`].
    pub fn set_watching(&mut self, watching: bool) {
        self.watching = watching;
    }

    /// Stop starting passes; a running one is waited for.
    pub fn stop(&mut self) {
        self.stopping = true;
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping
    }

    /// Whether deletions wait for approval, held back by the mass-delete guard (as of the last pass that finished).
    pub fn waiting_for_approval(&self) -> bool {
        self.waiting_approval
    }

    /// Whether deletes are held behind a link (as of the last pass that finished).
    pub fn holding(&self) -> bool {
        self.holding
    }

    /// Whether the loop looks for an approval: deletions wait for one, or deletes are held.
    pub fn looks_for_approval(&self) -> bool {
        self.waiting_approval || self.holding
    }

    /// How often the loop looks for an approval now: often while deletions wait for one, slowly while only holds do.
    fn approval_every(&self) -> Ms {
        ms(if self.waiting_approval { self.timing.approval_check } else { self.timing.held_check })
    }

    /// A pass started: everything that asked for one is answered by it.
    pub fn pass_started(&mut self) {
        self.running = true;
        self.first_change = None;
        self.last_change = None;
        self.urgent = false;
        self.stability_due = None;
    }

    /// The running pass ended.
    pub fn pass_finished(&mut self, now: Ms, end: PassEnd) {
        self.running = false;
        self.last_pass_end = now;
        match end {
            PassEnd::Done { recheck_in, needs_approval, held } => {
                self.failures = 0;
                self.retry = None;
                self.paused = None;
                self.stability_due = recheck_in.map(|after| now.saturating_add(ms(after)));
                let was_looking = self.looks_for_approval();
                let was_waiting = self.waiting_approval;
                self.waiting_approval = needs_approval;
                self.holding = held;
                // The check keeps its own pace, but a guard set that comes while only holds waited is looked for at
                // the guard's pace from now on.
                let next = now.saturating_add(self.approval_every());
                if !was_looking {
                    self.next_approval_check = next;
                } else if needs_approval && !was_waiting {
                    self.next_approval_check = self.next_approval_check.min(next);
                }
            }
            PassEnd::Failed(message) => {
                self.paused = None;
                let delay = self.backoff();
                self.failures = self.failures.saturating_add(1);
                self.retry = Some((now.saturating_add(delay), message));
            }
            PassEnd::NeedsYou { reason, recheck } => {
                self.retry = None;
                self.failures = 0;
                let next_check = match recheck {
                    Recheck::Soon => Some(now.saturating_add(ms(self.timing.recheck))),
                    Recheck::Slowly => Some(now.saturating_add(ms(self.timing.recheck_slow))),
                    Recheck::Never => None,
                };
                self.paused = Some((reason, recheck, next_check));
            }
        }
    }

    /// The loop looked for an approval and found none (one it found runs a pass: report [`Scheduler::urgent_change`]).
    pub fn approval_checked(&mut self, now: Ms) {
        self.next_approval_check = now.saturating_add(self.approval_every());
    }

    /// The delay before the next retry: `backoff_first` doubled per failure so far, capped at `backoff_max`, of which
    /// the second half is random (so agents that failed together don't all come back at once).
    fn backoff(&mut self) -> Ms {
        let first = ms(self.timing.backoff_first).max(1);
        let cap = ms(self.timing.backoff_max).max(first);
        let full = first.checked_shl(self.failures.min(40)).unwrap_or(cap).min(cap);
        let half = full / 2;
        // A uniform fraction in [0, 1) from the top 53 bits.
        let unit = ((self.random)() >> 11) as f64 / (1u64 << 53) as f64;
        let jitter = (half as f64 * unit).round() as u64;
        full.saturating_sub(half).saturating_add(jitter.min(half))
    }

    /// When the next pass is due, if one is.
    fn pass_due(&self) -> Option<Ms> {
        if let Some((_, _, next_check)) = &self.paused {
            return *next_check;
        }
        if let Some((at, _)) = &self.retry {
            return Some(*at);
        }
        if self.urgent {
            return Some(0);
        }
        let every = if self.watching { self.timing.periodic } else { self.timing.unwatched_periodic };
        let mut due = self.last_pass_end.saturating_add(ms(every));
        if let (Some(first), Some(last)) = (self.first_change, self.last_change) {
            let quiet = last.saturating_add(ms(self.timing.debounce));
            let latest = first.saturating_add(ms(self.timing.max_delay));
            due = due.min(quiet.min(latest));
        }
        if let Some(at) = self.stability_due {
            due = due.min(at);
        }
        Some(due)
    }

    /// What to do at `now`.
    pub fn next(&mut self, now: Ms) -> Next {
        if self.running {
            return Next::Wait(None);
        }
        if self.stopping {
            return Next::Exit;
        }
        let checking = self.looks_for_approval() && self.paused.is_none();
        if checking && self.next_approval_check <= now {
            return Next::CheckApproval;
        }
        let due = self.pass_due();
        if due.is_some_and(|at| at <= now) {
            return Next::RunPass;
        }
        let approval = checking.then_some(self.next_approval_check);
        Next::Wait(match (due, approval) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }

    /// What the loop is doing, for the status file.
    pub fn phase(&self) -> Phase {
        if self.stopping {
            return Phase::Stopping;
        }
        if self.running {
            return Phase::Syncing;
        }
        if let Some((reason, _, next_check)) = &self.paused {
            return Phase::Paused { reason: reason.clone(), next_check: *next_check };
        }
        if let Some((next_try, message)) = &self.retry {
            return Phase::Retrying { next_try: *next_try, message: message.clone() };
        }
        Phase::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> Timing {
        Timing {
            debounce: Duration::from_millis(1_000),
            max_delay: Duration::from_millis(10_000),
            stability: Duration::from_millis(5_000),
            periodic: Duration::from_millis(600_000),
            unwatched_periodic: Duration::from_millis(60_000),
            backoff_first: Duration::from_millis(2_000),
            backoff_max: Duration::from_millis(300_000),
            recheck: Duration::from_millis(60_000),
            recheck_slow: Duration::from_millis(600_000),
            approval_check: Duration::from_millis(2_000),
            held_check: Duration::from_millis(30_000),
        }
    }

    /// A scheduler whose jitter is always the largest (`u64::MAX` modulo the range) or zero.
    fn scheduler(max_jitter: bool) -> Scheduler {
        Scheduler::new(timing(), 0, move || if max_jitter { u64::MAX } else { 0 })
    }

    fn done() -> PassEnd {
        PassEnd::Done { recheck_in: None, needs_approval: false, held: false }
    }

    /// Runs the pass the scheduler asks for at `now`, ending as `end`.
    fn run(s: &mut Scheduler, now: Ms, end: PassEnd) {
        assert_eq!(s.next(now), Next::RunPass, "a pass is due at {now}");
        s.pass_started();
        assert_eq!(s.next(now), Next::Wait(None), "nothing else while a pass runs");
        assert_eq!(s.phase(), Phase::Syncing);
        s.pass_finished(now, end);
    }

    #[test]
    fn the_loop_starts_with_a_pass_and_then_waits_for_the_safety_net() {
        let mut s = scheduler(false);
        run(&mut s, 0, done());
        assert_eq!(s.next(1), Next::Wait(Some(600_000)));
        assert_eq!(s.phase(), Phase::Idle);
        run(&mut s, 600_000, done());
        assert_eq!(s.next(600_001), Next::Wait(Some(1_200_000)));
    }

    #[test]
    fn changes_are_debounced_but_never_held_back_longer_than_the_max_delay() {
        let mut s = scheduler(false);
        run(&mut s, 0, done());
        s.local_change(100);
        assert_eq!(s.next(100), Next::Wait(Some(1_100)));
        s.local_change(900);
        assert_eq!(s.next(900), Next::Wait(Some(1_900)), "a new change restarts the quiet time");
        // A folder that never goes quiet still syncs within the max delay of its first change.
        let mut now = 900;
        while now < 10_100 {
            now += 500;
            s.local_change(now);
            if s.next(now) == Next::RunPass {
                break;
            }
        }
        assert_eq!(now, 10_400, "the first look after 100 + 10 000 ms");
        run(&mut s, now, done());
        assert_eq!(s.next(now + 1), Next::Wait(Some(now + 600_000)));
    }

    #[test]
    fn a_change_on_the_server_or_a_lost_event_runs_a_pass_at_once() {
        let mut s = scheduler(false);
        run(&mut s, 0, done());
        s.urgent_change();
        run(&mut s, 5, done());
        assert_eq!(s.next(6), Next::Wait(Some(600_005)));
    }

    #[test]
    fn changes_during_a_pass_get_a_pass_of_their_own() {
        let mut s = scheduler(false);
        assert_eq!(s.next(0), Next::RunPass);
        s.pass_started();
        s.local_change(50);
        s.pass_finished(200, done());
        assert_eq!(s.next(200), Next::Wait(Some(1_050)));
        run(&mut s, 1_050, done());
        assert_eq!(s.next(1_051), Next::Wait(Some(601_050)), "nothing more: an idle loop settles");
    }

    #[test]
    fn a_file_still_changing_is_looked_at_again_when_its_look_is_due() {
        let mut s = scheduler(false);
        let waiting = |after: u64| PassEnd::Done { recheck_in: Some(Duration::from_millis(after)), needs_approval: false, held: false };
        run(&mut s, 0, waiting(2_000));
        assert_eq!(s.next(1), Next::Wait(Some(2_000)), "when the disk says the next look is due");
        // A file that never settles is looked at less and less often (the disk doubles its delay).
        run(&mut s, 2_000, waiting(240_000));
        assert_eq!(s.next(2_001), Next::Wait(Some(242_000)), "not every few seconds");
        run(&mut s, 242_000, done());
        assert_eq!(s.next(242_001), Next::Wait(Some(842_000)), "settled: only the safety net is left");
    }

    #[test]
    fn failures_back_off_exponentially_with_jitter_up_to_the_cap_and_success_resets() {
        for max_jitter in [false, true] {
            let mut s = scheduler(max_jitter);
            let mut now = 0;
            let mut delays = Vec::new();
            for _ in 0..12 {
                run(&mut s, now, PassEnd::Failed("offline".into()));
                let Next::Wait(Some(at)) = s.next(now) else { panic!("a retry is scheduled") };
                assert!(matches!(s.phase(), Phase::Retrying { next_try, .. } if next_try == at));
                // Local changes don't hasten a retry.
                s.local_change(now);
                assert_eq!(s.next(now + 1), Next::Wait(Some(at)));
                delays.push(at - now);
                now = at;
            }
            // Full delay 2 s, 4 s, 8 s … capped at 300 s; the jitter takes up to half of it.
            let full = [2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 256_000, 300_000, 300_000, 300_000, 300_000];
            let want: Vec<Ms> = full.iter().map(|f| if max_jitter { *f } else { f - f / 2 }).collect();
            assert_eq!(delays, want);
            run(&mut s, now, done());
            run(&mut s, now + 600_000, PassEnd::Failed("offline".into()));
            assert_eq!(s.next(now + 600_000), Next::Wait(Some(now + 600_000 + if max_jitter { 2_000 } else { 1_000 })), "reset");
        }
    }

    #[test]
    fn the_server_answering_again_ends_the_wait_for_a_retry() {
        let mut s = scheduler(false);
        run(&mut s, 0, PassEnd::Failed("offline".into()));
        s.server_back();
        run(&mut s, 10, done());
        assert_eq!(s.phase(), Phase::Idle);
    }

    /// A change on the server, a watcher that lost events or an approval doesn't cut a retry wait short, and only a
    /// pass that finishes resets the backoff (the server answering again only runs the retry now).
    #[test]
    fn only_a_finished_pass_resets_the_backoff() {
        let mut s = scheduler(true);
        run(&mut s, 0, PassEnd::Failed("a server error".into()));
        assert_eq!(s.next(1), Next::Wait(Some(2_000)));
        s.urgent_change();
        assert_eq!(s.next(10), Next::Wait(Some(2_000)), "an urgent change waits for the retry");
        run(&mut s, 2_000, PassEnd::Failed("a server error".into()));
        assert_eq!(s.next(2_001), Next::Wait(Some(6_000)), "the delay went on growing");
        s.server_back();
        run(&mut s, 2_010, PassEnd::Failed("a server error".into()));
        assert_eq!(s.next(2_011), Next::Wait(Some(10_010)), "still growing after the server came back");
        s.server_back();
        run(&mut s, 2_020, done());
        run(&mut s, 3_000 + 600_000, PassEnd::Failed("a server error".into()));
        assert_eq!(s.next(603_001), Next::Wait(Some(605_000)), "a finished pass reset it");
    }

    #[test]
    fn pauses_say_why_and_only_some_recheck_by_themselves() {
        let mut s = scheduler(false);
        run(&mut s, 0, PassEnd::NeedsYou { reason: "the folder is missing".into(), recheck: Recheck::Soon });
        assert_eq!(s.phase(), Phase::Paused { reason: "the folder is missing".into(), next_check: Some(60_000) });
        s.local_change(10);
        s.urgent_change();
        assert_eq!(s.next(20), Next::Wait(Some(60_000)), "nothing but the recheck runs a pass while paused");
        run(&mut s, 60_000, done());
        assert_eq!(s.phase(), Phase::Idle, "back once the folder is");

        run(&mut s, 660_000, PassEnd::NeedsYou { reason: "protocol".into(), recheck: Recheck::Slowly });
        assert_eq!(s.next(660_001), Next::Wait(Some(1_260_000)));

        run(&mut s, 1_260_000, PassEnd::NeedsYou { reason: "revoked".into(), recheck: Recheck::Never });
        assert_eq!(s.next(1_260_001), Next::Wait(None));
        assert_eq!(s.next(99_999_999), Next::Wait(None), "never by itself");
        s.stop();
        assert_eq!(s.next(99_999_999), Next::Exit);
    }

    #[test]
    fn waiting_deletions_are_checked_for_approval_while_passes_go_on() {
        let mut s = scheduler(false);
        run(&mut s, 0, PassEnd::Done { recheck_in: None, needs_approval: true, held: false });
        assert!(s.waiting_for_approval());
        assert_eq!(s.next(1), Next::Wait(Some(2_000)));
        assert_eq!(s.next(2_000), Next::CheckApproval);
        s.approval_checked(2_000);
        assert_eq!(s.next(2_001), Next::Wait(Some(4_000)));
        s.local_change(2_500);
        assert_eq!(s.next(3_500), Next::RunPass, "other changes still sync");
        s.pass_started();
        s.pass_finished(3_600, PassEnd::Done { recheck_in: None, needs_approval: true, held: false });
        assert_eq!(s.next(3_700), Next::Wait(Some(4_000)), "the check keeps its own pace");
        // An approval was found: the loop runs a pass at once.
        s.approval_checked(4_000);
        s.urgent_change();
        run(&mut s, 4_000, done());
        assert!(!s.waiting_for_approval());
        assert_eq!(s.next(4_001), Next::Wait(Some(604_000)));
    }

    /// Review 8, finding 2: a hold isn't deletions waiting for approval. It pauses nothing, and while only holds
    /// wait the loop looks for an approval of their set every `held_check`; a guard set that comes meanwhile is
    /// looked for at its own pace.
    #[test]
    fn holds_alone_are_checked_for_approval_slowly() {
        let held = PassEnd::Done { recheck_in: None, needs_approval: false, held: true };
        let mut s = scheduler(false);
        run(&mut s, 0, held.clone());
        assert!(!s.waiting_for_approval() && s.holding() && s.looks_for_approval());
        assert_eq!(s.next(1), Next::Wait(Some(30_000)));
        s.local_change(1_000);
        assert_eq!(s.next(2_000), Next::RunPass, "other changes still sync");
        s.pass_started();
        s.pass_finished(2_100, held.clone());
        assert_eq!(s.next(2_200), Next::Wait(Some(30_000)), "the check keeps its own pace");
        assert_eq!(s.next(30_000), Next::CheckApproval);
        s.approval_checked(30_000);
        assert_eq!(s.next(30_001), Next::Wait(Some(60_000)));
        // The guard holds deletions back too: from now on, its pace.
        s.urgent_change();
        run(&mut s, 31_000, PassEnd::Done { recheck_in: None, needs_approval: true, held: true });
        assert_eq!(s.next(31_001), Next::Wait(Some(33_000)));
        s.approval_checked(33_000);
        assert_eq!(s.next(33_001), Next::Wait(Some(35_000)));
        // The guard's set went (approved), the hold stands: slowly again.
        s.urgent_change();
        run(&mut s, 34_000, held);
        s.approval_checked(35_000);
        assert_eq!(s.next(35_001), Next::Wait(Some(65_000)));
        s.urgent_change();
        run(&mut s, 36_000, done());
        assert!(!s.looks_for_approval());
        assert_eq!(s.next(36_001), Next::Wait(Some(636_000)));
    }

    #[test]
    fn an_unwatched_folder_gets_a_full_pass_every_minute() {
        let mut s = scheduler(false);
        run(&mut s, 0, done());
        s.set_watching(false);
        assert_eq!(s.next(1), Next::Wait(Some(60_000)));
        run(&mut s, 60_000, done());
        assert_eq!(s.next(60_001), Next::Wait(Some(120_000)));
        s.set_watching(true);
        assert_eq!(s.next(60_002), Next::Wait(Some(660_000)), "watched again: the usual safety net");
    }

    #[test]
    fn a_stop_waits_for_the_running_pass() {
        let mut s = scheduler(false);
        assert_eq!(s.next(0), Next::RunPass);
        s.pass_started();
        s.stop();
        assert_eq!(s.next(1), Next::Wait(None));
        s.pass_finished(2, done());
        assert_eq!(s.phase(), Phase::Stopping);
        assert_eq!(s.next(2), Next::Exit);
        s.local_change(3);
        assert_eq!(s.next(3), Next::Exit, "no new pass starts");
    }
}
