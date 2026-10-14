//! `craft-cloud-agent run`: the agent on its own, a foreground loop that syncs whenever something changes.
//!
//! - **Locks**: the data folder's lock is taken first and held for the loop's whole life; the folder's lock is held
//!   by the open session ([`Session`]) on the pass thread, also for as long as the loop runs (it is taken again
//!   when a missing folder comes back). A second agent stops with exit code 3.
//! - **Threads**: passes run on a dedicated thread with its own current-thread tokio runtime, so the blocking disk
//!   work of a pass never stalls the loop. The loop's control, the folder watcher's events and the server's
//!   long-poll run on the caller's runtime.
//! - **Triggers** ([`crate::schedule`] decides when): the folder watcher ([`crate::watch`]), the long-poll
//!   `GET /changes?wait=30` (it backs off on errors and on `429` with the server's `Retry-After`, never polling in a
//!   hot loop), and a full pass every 10 minutes.
//! - **Stop**: the first stop request (Ctrl-C, or SIGTERM on Unix) starts no new pass, lets a running one finish,
//!   writes the status and ends with exit code 0. A second one ends at once with exit code 1 (the pass is cut
//!   short; its checkpoints keep that safe, as after a crash).
//! - **Status**: `status.json` in the data folder ([`LoopStatus`]) is written on every change of state and as a
//!   heartbeat, so `status` can tell whether the loop runs and what it is doing.
//! - **Approval**: while deletions wait, the loop looks for `approval.json` (written by `sync --approve-deletes`
//!   from another terminal, for the set `status` showed) and releases exactly the deletions it names, if they are
//!   still the set that waits; any other approval is discarded with a message. Deletes held behind a link are a set
//!   of their own, looked for while any is held (every 30 s while only holds wait: a hold pauses nothing and may
//!   stand for days), and released only by an approval naming that set. `status.json` says the two apart
//!   (`waiting_for_approval`, `held`).

use std::io::Write;
use std::time::{Duration, Instant};

use craft_cloud_proto::NodeId;
use tokio::sync::{mpsc, watch};

use crate::account::{Approval, DataDir, LastPass, LoopState, LoopStatus, PassSummary, STATUS_SCHEMA};
use crate::cli::{AgentError, Exit, Session, hold_data, now_ms, print_summary};
use crate::http::{Connection, HttpTransport, PollError};
use crate::lock::Lock;
use crate::schedule::{Ms, Next, PassEnd, Phase, Recheck, Scheduler, Timing};
use crate::settle::Stability;
use crate::watch::{FolderWatcher, WatchEvent};

/// The loop's settings. [`RunOptions::default`] is what `craft-cloud-agent run` uses; tests make the times short.
#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub timing: Timing,
    /// How often `status.json` is written while nothing changes.
    pub heartbeat: Duration,
    /// How long one long-poll waits for a change on the server.
    pub poll_wait: Duration,
    /// The first delay after a failed long-poll (it doubles, up to [`RunOptions::poll_retry_max`]).
    pub poll_retry_first: Duration,
    pub poll_retry_max: Duration,
    /// The shortest time from the start of one long-poll to the next, so a server or proxy that answers at once
    /// (ignoring `wait`) never makes it a hot loop.
    pub poll_min_spacing: Duration,
    /// The delay after a `429` without a `Retry-After`.
    pub poll_busy: Duration,
    /// Tests: the folder watcher fails to start, as it does when the system's watch limit is reached.
    #[doc(hidden)]
    pub fail_watch: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timing: Timing::default(),
            heartbeat: Duration::from_secs(15),
            poll_wait: Duration::from_secs(30),
            poll_retry_first: Duration::from_secs(2),
            poll_retry_max: Duration::from_secs(5 * 60),
            poll_min_spacing: Duration::from_secs(1),
            poll_busy: Duration::from_secs(5),
            fail_watch: false,
        }
    }
}

/// What reaches the loop.
enum Event {
    Watch(WatchEvent),
    /// The server's change feed moved, or can't continue (a pass sorts it out).
    Remote,
    /// The long-poll got an answer after failing: the server is back.
    ServerBack,
    /// The pass thread finished a pass.
    Pass(Box<PassDone>),
    /// The watcher of this generation stopped sending (dropped, or its thread ended).
    WatcherGone(u64),
}

/// A finished pass, from the pass thread.
struct PassDone {
    result: Result<PassSummary, AgentError>,
    last_pass: Option<LastPass>,
    /// Where the engine read the change feed to, once the space is known.
    seq: Option<u64>,
    /// After a finished pass: when the earliest look at a file still changing is due.
    recheck_in: Option<Duration>,
}

/// How a pass that ended with `error` is scheduled: a pause for what needs the user (rechecked by itself where that
/// is safe and may help; see [`Recheck`]), a retry with backoff for the rest. A finished pass that left files still
/// changing runs again when the disk says the earliest look at them is due (`recheck_in`).
fn pass_end(result: &Result<PassSummary, AgentError>, recheck_in: Option<Duration>) -> PassEnd {
    match result {
        // Deletes held behind a link can be approved too, so the loop looks for an approval while any is held (more
        // slowly: nothing else waits for it).
        Ok(summary) => PassEnd::Done { recheck_in, needs_approval: summary.needs_approval(), held: summary.held_set().is_some() },
        Err(e) if e.exit() == Exit::NeedsYou => PassEnd::NeedsYou { reason: e.to_string(), recheck: recheck_for(e) },
        Err(e) => PassEnd::Failed(e.to_string()),
    }
}

/// Which pauses look again by themselves: a folder or marker that is missing (a drive not mounted yet), folders
/// that lie inside one another, or another agent on the folder can be put right at any moment and checking is
/// harmless, so every minute; a server that speaks another protocol or refuses requests is fixed by an update, so
/// every ten minutes; a revoked device, damaged or newer saved files, another account's state and damaged data from
/// the server never fix themselves, so the loop waits until it is stopped.
fn recheck_for(e: &AgentError) -> Recheck {
    match e {
        AgentError::Root(_) | AgentError::DataInFolder { .. } | AgentError::Lock(_) => Recheck::Soon,
        AgentError::Server(craft_cloud_sync::TransportError::Protocol(_)) => Recheck::Slowly,
        _ => Recheck::Never,
    }
}

/// The pass thread: opens the folder when a pass is asked for (again after a stop that needs the user, so a folder
/// that came back is opened afresh), runs the pass on its own current-thread runtime and reports it. Ends when the
/// loop drops its end of `requests`, releasing the folder's lock.
fn pass_thread(data: DataDir, stability: Stability, requests: std::sync::mpsc::Receiver<Vec<NodeId>>, events: mpsc::UnboundedSender<Event>) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => Some(runtime),
        Err(e) => {
            let _ = events.send(Event::Pass(Box::new(PassDone {
                result: Err(AgentError::Usage(format!("can't start the sync thread's runtime: {e}"))),
                last_pass: None,
                seq: None,
                recheck_in: None,
            })));
            None
        }
    };
    let Some(runtime) = runtime else { return };
    let mut session: Option<Session> = None;
    while let Ok(approve) = requests.recv() {
        let result = runtime.block_on(async {
            let open = match session.take() {
                Some(open) => open,
                None => Session::open(&data, Some(stability.clone()), false)?,
            };
            let session = session.insert(open);
            session.pass(approve).await
        });
        // A stop that needs the user (the folder went, say) opens the folder afresh next time.
        if result.as_ref().is_err_and(|e| e.exit() == Exit::NeedsYou) {
            session = None;
        }
        let last_pass = session.as_ref().and_then(Session::last_pass).or_else(|| data.load_state().ok().and_then(|s| s.last_pass));
        let seq = session.as_ref().filter(|s| s.state().space.is_some()).map(|s| s.state().seq);
        let recheck_in = session.as_ref().and_then(Session::recheck_in);
        if events.send(Event::Pass(Box::new(PassDone { result, last_pass, seq, recheck_in }))).is_err() {
            return;
        }
    }
}

/// What the loop says when it throws an approval away.
const DISCARDED: &str = "an approval was discarded: the deletions it names aren't the ones waiting now; \
                         check `craft-cloud-agent status` and approve again if they still wait";

fn ms(d: Duration) -> Ms {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The folder watcher's life in the loop (see `run_loop`).
#[derive(Default)]
struct Watching {
    watcher: Option<FolderWatcher>,
    /// Counts the watchers started, so the end of an old one's events isn't taken for the current one stopping.
    generation: u64,
    /// When the current one started.
    started: Ms,
    /// Starts that failed in a row, and when the next try is due.
    failures: u32,
    next_try: Ms,
    /// Why the folder isn't watched, while it isn't.
    not_watching: Option<String>,
}

impl Watching {
    /// The watcher failed, dropped events or stopped: it is made afresh, at once, or `first` after it started if it
    /// only just did (a watcher that keeps failing is never restarted in a hot loop).
    fn lost(&mut self, now: Ms, first: Ms) {
        self.watcher = None;
        self.next_try = now.max(self.started.saturating_add(first));
    }

    /// Windows: `notify` 8 neither reports nor survives a full change buffer (`ReadDirectoryChangesW` drops the
    /// events, or stops watching), so a watcher that has run for `every` is made afresh just before the next pass
    /// (every pass scans the whole folder; when idle it is the safety-net pass). That pass finds what was dropped
    /// and is the pass the restart asks for (the restart's change is taken by the pass that starts right after it),
    /// so an idle loop still scans once per `every`. Returns whether the watcher was dropped: the loop then starts
    /// the new one before it runs the pass.
    fn renew_before_pass(&mut self, now: Ms, every: Ms) -> bool {
        if self.watcher.is_none() || now < self.started.saturating_add(every) {
            return false;
        }
        self.watcher = None;
        self.next_try = now;
        true
    }

    /// A watcher started at `now`. Every start but the very first is followed by a full pass: events between the
    /// last pass and this start were lost (a watcher that failed or stopped, a pause, the Windows re-creation, whose
    /// pass is the one about to run). After a time unwatched it runs at once, otherwise after the usual debounce.
    /// Returns whether the folder is watched again after it couldn't be.
    fn started_one(&mut self, now: Ms, scheduler: &mut Scheduler) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.started = now;
        self.failures = 0;
        let again = self.not_watching.take().is_some();
        if again {
            // What changed while it wasn't watched is found by a pass now, not at the next minute.
            scheduler.urgent_change();
        } else if self.generation > 1 {
            scheduler.local_change(now);
        }
        scheduler.set_watching(true);
        again
    }
}

/// Hands a watcher's events to the loop; when the watcher's side ends (it was dropped, or its thread died), says so.
async fn forward_watch_events(mut from: mpsc::UnboundedReceiver<WatchEvent>, to: mpsc::UnboundedSender<Event>, generation: u64) {
    while let Some(event) = from.recv().await {
        if to.send(Event::Watch(event)).is_err() {
            return;
        }
    }
    let _ = to.send(Event::WatcherGone(generation));
}

/// A random number for jitter (0 if the system has none to give: the jitter only spreads retries out).
fn random() -> u64 {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// What the long-poll tells the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polled {
    /// The server's change feed moved, or can't continue (a pass sorts it out).
    Changed,
    /// An answer came after failed polls: the server is back.
    Back,
}

/// The long-poll: waits for the server's change feed to move past what the passes read (`since`, `None` until a
/// first sync), and tells the loop (`tell`, which returns `false` once the loop is gone). Errors back off
/// exponentially (with jitter from `random`, from `poll_retry_first` up to `poll_retry_max`); a `429` waits for the
/// server's `Retry-After`. After a change it polls on from the server's new position, so it never asks twice for the
/// same change. It keeps time on tokio's clock, so a simulated network's tests run it on simulated time.
pub async fn long_poll<C: Connection>(
    transport: HttpTransport<C>,
    mut since: watch::Receiver<Option<u64>>,
    tell: impl Fn(Polled) -> bool,
    options: RunOptions,
    mut random: impl FnMut() -> u64,
) {
    let mut known: Option<u64> = None;
    let mut failures: u32 = 0;
    let mut last_start: Option<tokio::time::Instant> = None;
    loop {
        // Never two polls closer together than the minimum spacing, whatever the server answers.
        if let Some(last) = last_start {
            let gap = options.poll_min_spacing.saturating_sub(last.elapsed());
            if !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
        }
        // The passes' position wins once it is past what the poll saw (the pass read further).
        let passes = *since.borrow_and_update();
        let from = match (passes, known) {
            (Some(passes), Some(seen)) => passes.max(seen),
            (Some(passes), None) => passes,
            // No pass has read the feed yet (a first sync): wait for one.
            (None, _) => {
                if since.changed().await.is_err() {
                    return;
                }
                continue;
            }
        };
        last_start = Some(tokio::time::Instant::now());
        let pause = match transport.wait_for_changes(from, options.poll_wait).await {
            Ok(seq) => {
                if failures > 0 && !tell(Polled::Back) {
                    return;
                }
                failures = 0;
                if seq != from {
                    known = Some(seq);
                    if !tell(Polled::Changed) {
                        return;
                    }
                }
                None
            }
            // Spread out, so agents the server turned away together don't all come back at once.
            Err(PollError::Busy(after)) => Some(jittered(after.unwrap_or(options.poll_busy), random())),
            Err(PollError::Resync) => {
                // A pass reads the feed afresh; until it has, polling again would only get the same answer.
                known = None;
                if !tell(Polled::Changed) {
                    return;
                }
                failures = failures.saturating_add(1);
                Some(poll_backoff(&options, failures, random()))
            }
            Err(PollError::Failed(_)) => {
                failures = failures.saturating_add(1);
                Some(poll_backoff(&options, failures, random()))
            }
        };
        if let Some(pause) = pause {
            tokio::time::sleep(pause).await;
        }
    }
}

/// `delay` plus a part of up to half of it, chosen by the random number `r`.
fn jittered(delay: Duration, r: u64) -> Duration {
    let half = u64::try_from((delay / 2).as_nanos()).unwrap_or(u64::MAX);
    delay.saturating_add(Duration::from_nanos(r % half.saturating_add(1)))
}

/// The delay after the `failures`-th failed long-poll in a row: doubled each time from `poll_retry_first`, capped at
/// `poll_retry_max`, the second half of it chosen by the random number `r`.
fn poll_backoff(options: &RunOptions, failures: u32, r: u64) -> Duration {
    let first = options.poll_retry_first.max(Duration::from_millis(1));
    let full = first.saturating_mul(1u32.checked_shl(failures.saturating_sub(1).min(20)).unwrap_or(u32::MAX)).min(options.poll_retry_max);
    let half = full / 2;
    let jitter = Duration::from_nanos(r % u64::try_from(half.as_nanos()).unwrap_or(u64::MAX).saturating_add(1));
    full.saturating_sub(half).saturating_add(jitter)
}

/// `at` on the loop's monotonic clock as a wall-clock time, given both clocks now. Only ever relative to now, so a
/// sleep of the machine (the monotonic clock stood still) or a set clock doesn't shift what `status.json` says.
fn to_wall(at: Ms, mono_now: Ms, wall_now: i64) -> i64 {
    if at >= mono_now {
        wall_now.saturating_add(i64::try_from(at - mono_now).unwrap_or(i64::MAX))
    } else {
        wall_now.saturating_sub(i64::try_from(mono_now - at).unwrap_or(i64::MAX))
    }
}

/// What a status says, without its times (see [`Status::write`]).
fn without_times(status: &LoopStatus) -> LoopStatus {
    let state = match &status.state {
        LoopState::Retrying { message, .. } => LoopState::Retrying { next_try_ms: 0, message: message.clone() },
        LoopState::Paused { reason, next_check_ms } => LoopState::Paused { reason: reason.clone(), next_check_ms: next_check_ms.map(|_| 0) },
        other => other.clone(),
    };
    LoopStatus { heartbeat_ms: 0, state, ..status.clone() }
}

/// The loop's clock and its status file. Its own clock is monotonic (scheduling only); every time it writes is the
/// wall clock's.
struct Status {
    data: DataDir,
    start: Instant,
    started_ms: i64,
    heartbeat: Duration,
    last_written: Option<LoopStatus>,
    next_beat: Ms,
    last_pass: Option<LastPass>,
    /// Why the folder isn't watched, while it isn't.
    not_watching: Option<String>,
    /// Deletes are held behind a link (as of the last pass that finished).
    held: bool,
}

impl Status {
    fn now(&self) -> Ms {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// A time on the loop's clock as a wall-clock time, from the wall clock now: the loop's monotonic clock may
    /// stand still while the machine sleeps, and the wall clock may be set, so the two drift apart (see [`to_wall`]).
    fn wall(&self, at: Ms) -> i64 {
        to_wall(at, self.now(), now_ms())
    }

    fn state(&self, phase: Phase) -> LoopState {
        match phase {
            Phase::Idle => LoopState::Idle,
            Phase::Syncing => LoopState::Syncing,
            Phase::Retrying { next_try, message } => LoopState::Retrying { next_try_ms: self.wall(next_try), message },
            Phase::Paused { reason, next_check } => LoopState::Paused { reason, next_check_ms: next_check.map(|at| self.wall(at)) },
            Phase::Stopping => LoopState::Stopping,
        }
    }

    /// Writes `status.json` if what it says changed, or the heartbeat is due. Times don't count as a change: they are
    /// converted from the loop's clock afresh at every call, so they jitter by a millisecond or so, and a new next try
    /// or next check only ever comes with a pass, whose `Syncing` is written in between. The loop wakes on every
    /// watcher event, so comparing them would write (and flush) the file on every other event while it retries.
    /// A failed write is reported once per failure and tried again next time; it never stops the loop.
    fn write(&mut self, state: LoopState, waiting_for_approval: bool, force: bool) {
        let now = self.now();
        let status = LoopStatus {
            schema: STATUS_SCHEMA,
            pid: std::process::id(),
            started_ms: self.started_ms,
            heartbeat_ms: now_ms(),
            heartbeat_every_ms: u64::try_from(self.heartbeat.as_millis()).unwrap_or(u64::MAX),
            state,
            waiting_for_approval,
            held: self.held,
            last_pass: self.last_pass.clone(),
            not_watching: self.not_watching.clone(),
        };
        let same = self.last_written.as_ref().is_some_and(|old| without_times(old) == without_times(&status));
        if same && !force && now < self.next_beat {
            return;
        }
        // Tried again at the next beat either way, never in a hot loop.
        self.next_beat = now.saturating_add(u64::try_from(self.heartbeat.as_millis()).unwrap_or(u64::MAX));
        match self.data.save_status(&status) {
            Ok(()) => self.last_written = Some(status),
            Err(e) => eprintln!("can't write the status file: {e}"),
        }
    }
}

/// Runs the loop until `stop` asks twice, or once and the running pass is done (see the module docs). `stop` gets one
/// message per stop request (Ctrl-C, SIGTERM). What the loop does is printed to `out`, a line per pass that changed
/// something, failed or needs the user.
pub async fn run_loop(data: &DataDir, options: RunOptions, mut stop: mpsc::UnboundedReceiver<()>, out: &mut impl Write) -> Result<Exit, AgentError> {
    let data_lock: Lock = hold_data(data)?;
    // Interrupted writes of the data folder's own files are cleaned once, before the loop writes any of its own;
    // sessions opened later don't (see `Session::open`). Young ones are left alone: `status` and
    // `sync --approve-deletes` may be writing one from another terminal.
    data.clean_tmp()?;
    let account = data.load_account()?.ok_or(AgentError::NotSignedIn)?;
    let roots = crate::watch::roots(&account.folder);
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let (requests, requests_rx) = std::sync::mpsc::channel::<Vec<NodeId>>();
    let pass_events = events_tx.clone();
    // The stability window's looks, shared: the disk asks, the loop leaves the watcher's events for waiting files alone.
    let stability = Stability::new(options.timing.stability);
    let (thread_data, thread_stability) = (data.clone(), stability.clone());
    let pass = std::thread::Builder::new()
        .name("craft-cloud-pass".into())
        .spawn(move || pass_thread(thread_data, thread_stability, requests_rx, pass_events))
        .map_err(|source| AgentError::Io { path: data.path().to_owned(), source })?;
    // Where the saved state read the change feed to, once a first sync is done.
    let since = data.load_state().ok().and_then(|saved| saved.state.space.map(|_| saved.state.seq));
    let (since_tx, since_rx) = watch::channel::<Option<u64>>(since);
    let transport = HttpTransport::new(account.server.clone(), &account.token)?;
    let polled = events_tx.clone();
    let tell = move |p: Polled| {
        polled
            .send(match p {
                Polled::Changed => Event::Remote,
                Polled::Back => Event::ServerBack,
            })
            .is_ok()
    };
    let poll = tokio::spawn(long_poll(transport, since_rx, tell, options, random));

    let start = Instant::now();
    let mut status = Status {
        data: data.clone(),
        start,
        started_ms: now_ms(),
        heartbeat: options.heartbeat,
        last_written: None,
        next_beat: 0,
        last_pass: None,
        not_watching: None,
        held: false,
    };
    let mut scheduler = Scheduler::new(options.timing, 0, random);
    let mut watching = Watching::default();
    let mut approved: Vec<NodeId> = Vec::new();
    let mut stops = 0;
    let mut stop_open = true;
    let mut last_message: Option<String> = None;
    // The set of deletes held behind a link the loop last listed (see `summary_to_show`).
    let mut last_held: Option<String> = None;
    let say = |out: &mut dyn Write, line: &str| {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    };
    say(out, &format!("craft-cloud-agent: syncing {} with {} (Ctrl-C stops)", account.folder.display(), account.server));

    let exit = loop {
        let now = status.now();
        // The watcher starts (again) whenever it isn't running and is due: at the start, after it failed or stopped,
        // and after a folder that was missing came back (not while the loop is paused: the folder may be gone). One
        // that can't start is said once and tried again later, less and less often; meanwhile a full pass runs every
        // minute, and the long-poll still syncs the server's changes.
        let paused = matches!(scheduler.phase(), Phase::Paused { .. });
        if watching.watcher.is_none() && !scheduler.is_stopping() && !paused && now >= watching.next_try {
            let (wtx, wrx) = mpsc::unbounded_channel();
            let started = if options.fail_watch {
                Err(notify::Error::generic("the watcher is switched off (test)"))
            } else {
                crate::watch::watch(&account.folder, wtx)
            };
            match started {
                Ok(w) => {
                    watching.watcher = Some(w);
                    if watching.started_one(now, &mut scheduler) {
                        say(out, "watching the folder again");
                    }
                    tokio::spawn(forward_watch_events(wrx, events_tx.clone(), watching.generation));
                }
                Err(e) => {
                    watching.failures = watching.failures.saturating_add(1);
                    let first = ms(options.timing.unwatched_periodic).max(1);
                    let delay = first
                        .checked_shl(watching.failures.saturating_sub(1).min(20))
                        .unwrap_or(u64::MAX)
                        .min(ms(options.timing.periodic).max(first));
                    watching.next_try = now.saturating_add(delay);
                    if watching.not_watching.is_none() {
                        say(out, &format!("not watching the folder ({e}): changes there are found by a full pass every minute until it can"));
                    }
                    watching.not_watching = Some(e.to_string());
                    scheduler.set_watching(false);
                }
            }
            status.not_watching = watching.not_watching.clone();
        }
        // A file still settling that went quiet after its events: a look at it that finds it settled runs a pass.
        if take_due_peeks(&account.folder, &stability) {
            scheduler.local_change(now);
        }
        status.write(status.state(scheduler.phase()), scheduler.waiting_for_approval(), false);
        let until = match scheduler.next(now) {
            Next::Exit => break Exit::Done,
            Next::RunPass => {
                // Windows: the watcher is made afresh with this pass (see `Watching::renew_before_pass`); the loop
                // goes round once to start it, then runs this same pass.
                if cfg!(windows) && !matches!(scheduler.phase(), Phase::Paused { .. }) && watching.renew_before_pass(now, ms(options.timing.periodic))
                {
                    continue;
                }
                scheduler.pass_started();
                if requests.send(std::mem::take(&mut approved)).is_err() {
                    break Exit::Failed;
                }
                status.write(LoopState::Syncing, scheduler.waiting_for_approval(), true);
                continue;
            }
            Next::CheckApproval => {
                // Released only if it names the set that waits now; deletions that came since still wait (the engine
                // releases exactly the approved nodes) and are listed again.
                // The set of deletes held behind a link is its own set, released the same way.
                let summary = status.last_pass.as_ref().and_then(|l| l.summary.as_ref());
                let waiting = [summary.and_then(PassSummary::pending_set), summary.and_then(PassSummary::held_set)];
                match data.take_approval() {
                    Ok(Some(Approval { set, nodes, .. })) if waiting.contains(&Some(set.clone())) => {
                        say(out, &format!("approval received for {} deletion(s)", nodes.len()));
                        approved.extend(nodes);
                        scheduler.urgent_change();
                    }
                    Ok(Some(_)) => say(out, DISCARDED),
                    Ok(None) => {}
                    Err(e) => say(out, &format!("the approval couldn't be read ({e}); run `craft-cloud-agent sync --approve-deletes` again")),
                }
                scheduler.approval_checked(now);
                continue;
            }
            Next::Wait(until) => until,
        };
        let watch_due = (watching.watcher.is_none() && !scheduler.is_stopping() && !paused).then_some(watching.next_try);
        let peek_due = stability.next_peek_in().map(|d| now.saturating_add(ms(d)));
        let wake =
            until.unwrap_or(u64::MAX).min(status.next_beat).min(watch_due.unwrap_or(u64::MAX)).min(peek_due.unwrap_or(u64::MAX)).saturating_sub(now);
        let sleep = tokio::time::sleep(Duration::from_millis(wake));
        tokio::select! {
            event = events.recv() => match event {
                // A file still settling is looked at when its look is due (see `crate::settle`): its own writes
                // don't run passes, or a file that never settles would keep the loop scanning every few seconds.
                // Instead, it gets a cheap look of its own once it has been quiet for a while (below).
                Some(Event::Watch(WatchEvent::Changed { paths, closed })) => {
                    if counts_as_change(&roots, &stability, &paths, closed) {
                        scheduler.local_change(status.now());
                    }
                }
                // A watcher that failed or dropped events: a full pass now, and the watcher is made afresh.
                Some(Event::Watch(WatchEvent::Lost(why))) => {
                    say(out, &format!("the folder watcher lost track ({why}): looking at the whole folder"));
                    scheduler.urgent_change();
                    watching.lost(status.now(), ms(options.timing.unwatched_periodic));
                }
                // The watcher stopped by itself (its thread ended): the same.
                Some(Event::WatcherGone(generation)) => {
                    if generation == watching.generation && watching.watcher.is_some() {
                        say(out, "the folder watcher stopped: looking at the whole folder");
                        scheduler.urgent_change();
                        watching.lost(status.now(), ms(options.timing.unwatched_periodic));
                    }
                }
                Some(Event::Remote) => scheduler.urgent_change(),
                Some(Event::ServerBack) => scheduler.server_back(),
                Some(Event::Pass(done)) => {
                    let end = pass_end(&done.result, done.recheck_in);
                    if let Some(seq) = done.seq {
                        let _ = since_tx.send(Some(seq));
                    }
                    status.last_pass = done.last_pass;
                    match &done.result {
                        Ok(summary) => {
                            last_message = None;
                            let (shown, released) = summary_to_show(summary, &mut last_held);
                            if let Some(shown) = shown {
                                let _ = print_summary(out, &shown);
                            }
                            if released {
                                say(out, "no delete is held behind a link any more");
                            }
                        }
                        Err(e) => {
                            let message = e.to_string();
                            if last_message.as_ref() != Some(&message) {
                                say(out, &message);
                            }
                            last_message = Some(message);
                            // A folder that went away takes the watcher's watches with it.
                            if matches!(end, PassEnd::NeedsYou { .. }) {
                                watching.watcher = None;
                            }
                        }
                    }
                    scheduler.pass_finished(status.now(), end);
                    status.held = scheduler.holding();
                    // An approval found while nothing waits (this pass, or one before, settled the deletions) is never
                    // kept for later: it would release a set that may come up again weeks from now.
                    if !scheduler.looks_for_approval() && matches!(data.take_approval(), Ok(Some(_))) {
                        say(out, DISCARDED);
                    }
                }
                // Everything that sends is still here while the loop runs.
                None => break Exit::Failed,
            },
            signal = stop.recv(), if stop_open => match signal {
                Some(()) => {
                    stops += 1;
                    if stops > 1 {
                        say(out, "stopping at once");
                        break Exit::Failed;
                    }
                    say(out, "stopping after the running pass (press Ctrl-C again to stop at once)");
                    scheduler.stop();
                    watching.watcher = None;
                }
                None => stop_open = false,
            },
            () = sleep => {}
        }
    };
    poll.abort();
    drop(watching);
    status.write(LoopState::Stopped, scheduler.waiting_for_approval(), true);
    drop(requests);
    // After a second stop the pass thread may still be running a pass; it ends with it (and releases the folder's
    // lock). Otherwise it is waiting for a request and ends now.
    if exit == Exit::Done {
        let _ = pass.join();
    }
    drop(data_lock);
    say(out, "stopped");
    Ok(exit)
}

/// Whether a watcher event runs a pass: it does unless every path in it is a file waiting to settle (an event without
/// paths may be anything). Each waiting file's event is recorded, so its cheap look follows once it goes quiet (see
/// [`crate::settle::Settling::event`]).
fn counts_as_change(roots: &[std::path::PathBuf], stability: &Stability, paths: &[std::path::PathBuf], closed: bool) -> bool {
    let mut change = paths.is_empty();
    for p in paths {
        let waiting = crate::watch::relative(roots, p).is_some_and(|r| stability.event(&r, closed));
        change |= !waiting;
    }
    change
}

/// The cheap looks that are due (one stat each, see [`crate::settle::Settling::peeked`]): whether one found a file
/// settled (or gone), so a pass should run.
fn take_due_peeks(folder: &std::path::Path, stability: &Stability) -> bool {
    let wall = now_ms();
    let mut settled = false;
    for path in stability.take_peeks() {
        settled |= stability.peeked(&path, crate::disk::quick_stat(folder, &path), wall);
    }
    settled
}

/// What `run` prints after a finished pass: the summary, if it is worth a line, and whether to say that nothing is
/// held behind a link any more. The deletes held behind a link are listed only when their set changed since the
/// loop last listed them (`last_held`): a hold may stand for days, and listing it after every pass would bury
/// everything else.
fn summary_to_show(summary: &PassSummary, last_held: &mut Option<String>) -> (Option<PassSummary>, bool) {
    // A summary without the nodes (never the loop's own) is told apart by its list.
    let held = summary.held_set().or_else(|| (!summary.held_deletes.is_empty()).then(|| format!("{:?}", summary.held_deletes)));
    let mut shown = summary.clone();
    if held == *last_held {
        shown.held_deletes.clear();
        shown.held_nodes.clear();
    }
    let released = last_held.is_some() && held.is_none();
    *last_held = held;
    (summary_worth_showing(&shown).then_some(shown), released)
}

/// Whether a finished pass is worth a line: it changed something, skipped something or deletions wait.
fn summary_worth_showing(s: &PassSummary) -> bool {
    s.downloaded + s.uploaded + s.removed_locally + s.removed_remotely + s.moved_locally + s.moved_remotely + s.created_dirs + s.refused > 0
        || !s.conflict_copies.is_empty()
        || !s.skipped.is_empty()
        || !s.held_deletes.is_empty()
        || s.needs_approval()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::RootError;
    use craft_cloud_sync::TransportError;
    use std::path::PathBuf;

    #[test]
    fn pass_ends_are_scheduled_by_what_they_need() {
        let skipped = PassSummary { skipped: vec![("a".into(), "still changing; synced once it stops".into())], ..PassSummary::default() };
        let due = Some(Duration::from_secs(2));
        assert_eq!(pass_end(&Ok(skipped), due), PassEnd::Done { recheck_in: due, needs_approval: false, held: false });
        let waiting = PassSummary { pending_deletes: vec!["a".into()], ..PassSummary::default() };
        assert_eq!(pass_end(&Ok(waiting), None), PassEnd::Done { recheck_in: None, needs_approval: true, held: false });
        // A delete held behind a link: the loop looks for its approval (slowly), but no deletions wait for one.
        let held = PassSummary {
            held_deletes: vec![("Footage/c0.mov".into(), vec!["Clips/c0.mov".into()])],
            held_nodes: vec![craft_cloud_proto::NodeId::generate()],
            ..PassSummary::default()
        };
        assert_eq!(pass_end(&Ok(held), None), PassEnd::Done { recheck_in: None, needs_approval: false, held: true });
        assert!(matches!(pass_end(&Err(AgentError::Offline("x".into())), None), PassEnd::Failed(_)));
        let missing = AgentError::Root(RootError::MarkerMissing(PathBuf::from("f")));
        assert!(matches!(pass_end(&Err(missing), None), PassEnd::NeedsYou { recheck: Recheck::Soon, .. }));
        let protocol = AgentError::Server(TransportError::Protocol("x".into()));
        assert!(matches!(pass_end(&Err(protocol), None), PassEnd::NeedsYou { recheck: Recheck::Slowly, .. }));
        assert!(matches!(pass_end(&Err(AgentError::Revoked), None), PassEnd::NeedsYou { recheck: Recheck::Never, .. }));
    }

    /// Review 8, finding 3: `run` lists a held set once, and again only when it changes; other news is still shown
    /// (without the unchanged list), and the end of the last hold is said once.
    #[test]
    fn run_lists_the_held_set_only_when_it_changes() {
        let node = craft_cloud_proto::NodeId::generate();
        let held = PassSummary {
            held_deletes: vec![("Footage/c0.mov".into(), vec!["Clips/c0.mov".into()])],
            held_nodes: vec![node],
            ..PassSummary::default()
        };
        let mut last = None;
        assert_eq!(summary_to_show(&held, &mut last), (Some(held.clone()), false), "listed the first time");
        assert_eq!(summary_to_show(&held, &mut last), (None, false), "not after every pass");
        let uploaded = PassSummary { uploaded: 1, ..held.clone() };
        assert_eq!(summary_to_show(&uploaded, &mut last), (Some(PassSummary { uploaded: 1, ..PassSummary::default() }), false));
        let more = PassSummary { held_deletes: vec![("Footage/c0.mov".into(), vec!["A/c0.mov".into(), "Clips/c0.mov".into()])], ..held.clone() };
        assert_eq!(summary_to_show(&more, &mut last), (Some(more.clone()), false), "a changed set is listed again");
        assert_eq!(summary_to_show(&PassSummary::default(), &mut last), (None, true), "the end is said once");
        assert_eq!(summary_to_show(&PassSummary::default(), &mut last), (None, false));
        assert_eq!(summary_to_show(&held, &mut last), (Some(held), false), "a new hold is listed");
    }

    #[test]
    fn loop_times_convert_to_the_wall_clock_from_now() {
        assert_eq!(to_wall(15_000, 10_000, 1_700_000_000_000), 1_700_000_005_000);
        assert_eq!(to_wall(4_000, 10_000, 1_700_000_000_000), 1_699_999_994_000);
        assert_eq!(to_wall(u64::MAX, 0, i64::MAX - 1), i64::MAX, "saturates");
    }

    /// The heartbeat and every time in `status.json` are wall-clock times, whatever the loop's own clock says: after
    /// the machine slept for 8 hours (the monotonic clock stood still, so it lags the wall clock), a running loop
    /// still looks running.
    #[test]
    fn the_status_file_says_wall_clock_times_after_a_sleep() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        let eight_hours = 8 * 3_600 * 1_000;
        let mut status = Status {
            data: data.clone(),
            start: Instant::now(),
            // Started 8 h (of wall-clock time) ago, but the monotonic clock only counted the time awake.
            started_ms: now_ms() - eight_hours,
            heartbeat: Duration::from_secs(15),
            last_written: None,
            next_beat: 0,
            last_pass: None,
            not_watching: None,
            held: false,
        };
        let next_try = status.now() + 60_000;
        let state = status.state(Phase::Retrying { next_try, message: "offline".into() });
        status.write(state, false, true);
        let written = data.load_status().unwrap().unwrap();
        let now = now_ms();
        assert!((now - written.heartbeat_ms).abs() < 5_000, "heartbeat {} vs now {now}", written.heartbeat_ms);
        assert!(written.running(now), "a fresh heartbeat, whatever the uptime");
        let LoopState::Retrying { next_try_ms, .. } = written.state else { panic!("{:?}", written.state) };
        assert!((next_try_ms - (now + 60_000)).abs() < 5_000, "next try {next_try_ms} vs now {now}");
    }

    /// Many wake-ups in one state write the file once until the heartbeat is due, even though the times in it are
    /// converted afresh each time and jitter; a real change is written at once.
    #[test]
    fn an_unchanged_state_is_written_only_at_the_heartbeat() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        let mut status = Status {
            data: data.clone(),
            start: Instant::now(),
            started_ms: now_ms(),
            heartbeat: Duration::from_secs(15),
            last_written: None,
            next_beat: 0,
            last_pass: None,
            not_watching: None,
            held: false,
        };
        let at = now_ms() + 60_000;
        status.write(LoopState::Retrying { next_try_ms: at, message: "offline".into() }, false, false);
        assert!(data.load_status().unwrap().is_some());
        let file = dir.path().join("status.json");
        std::fs::remove_file(&file).unwrap();
        let jitter = [0, 1, -1, 2];
        for i in 0..200 {
            let next_try_ms = at + jitter[i % jitter.len()];
            status.write(LoopState::Retrying { next_try_ms, message: "offline".into() }, false, false);
            let next_check_ms = Some(at + jitter[(i + 1) % jitter.len()]);
            if i == 100 {
                // A pause in between is a real change: written at once, then left alone the same way.
                status.write(LoopState::Paused { reason: "gone".into(), next_check_ms }, false, false);
                assert!(file.exists(), "a new state is written at once");
                std::fs::remove_file(&file).unwrap();
                for _ in 0..50 {
                    let next_check_ms = Some(at + jitter[i % jitter.len()]);
                    status.write(LoopState::Paused { reason: "gone".into(), next_check_ms }, false, false);
                }
                assert!(!file.exists(), "an unchanged pause isn't written again before the heartbeat");
                status.write(LoopState::Retrying { next_try_ms, message: "offline".into() }, false, false);
                std::fs::remove_file(&file).unwrap();
            }
        }
        assert!(!file.exists(), "an unchanged state isn't written again before the heartbeat");
        // Once the heartbeat is due, the same state is written again.
        status.next_beat = status.now();
        status.write(LoopState::Retrying { next_try_ms: at, message: "offline".into() }, false, false);
        assert!(file.exists(), "the heartbeat writes");
    }

    /// Finding 4 of the second review: every watcher start but the first is followed by a full pass, since events
    /// between the last pass and the start were lost.
    #[test]
    fn a_pass_follows_every_watcher_restart() {
        let timing = Timing::default();
        let debounce = ms(timing.debounce);
        let mut scheduler = Scheduler::new(timing, 0, || 0);
        let mut watching = Watching::default();
        let idle = |scheduler: &mut Scheduler, now: Ms| {
            assert_eq!(scheduler.next(now), Next::RunPass);
            scheduler.pass_started();
            scheduler.pass_finished(now, PassEnd::Done { recheck_in: None, needs_approval: false, held: false });
        };
        // The loop's start: its first pass, and the first watcher, which asks for nothing more.
        assert!(!watching.started_one(0, &mut scheduler));
        idle(&mut scheduler, 0);
        assert!(matches!(scheduler.next(100_000), Next::Wait(Some(at)) if at > 100_000), "nothing after the first start");
        // The watcher stops; the pass for that runs, then the watcher is made afresh a minute later.
        watching.lost(100_000, ms(timing.unwatched_periodic));
        scheduler.urgent_change();
        idle(&mut scheduler, 100_000);
        assert!(!watching.started_one(160_000, &mut scheduler));
        assert!(matches!(scheduler.next(160_000), Next::Wait(Some(at)) if at == 160_000 + debounce));
        idle(&mut scheduler, 160_000 + debounce);
        // A restart for any other reason (a pause ended, the Windows re-creation) is the same.
        assert!(!watching.started_one(300_000, &mut scheduler));
        assert!(matches!(scheduler.next(300_000 + debounce), Next::RunPass));
        // After a time unwatched: at once.
        watching.not_watching = Some("limit".into());
        scheduler.pass_started();
        scheduler.pass_finished(300_000 + debounce, PassEnd::Done { recheck_in: None, needs_approval: false, held: false });
        assert!(watching.started_one(400_000, &mut scheduler), "watched again");
        assert_eq!(scheduler.next(400_000), Next::RunPass);
    }

    /// The Windows re-creation of the watcher rides on the pass about to run. An hour of the loop (passes taking
    /// 5 s, one local change 5 minutes in) has the first pass, the change's pass and then one safety-net pass per
    /// interval, each made with a fresh watcher once the old one is 10 minutes old. A re-creation timed from the
    /// watcher's start (as before) added a pass of its own 10 minutes after the start, 5 minutes after the
    /// change's pass.
    #[test]
    fn the_watcher_renewal_runs_no_pass_of_its_own() {
        let timing = Timing::default();
        let (periodic, debounce) = (ms(timing.periodic), ms(timing.debounce));
        let mut scheduler = Scheduler::new(timing, 0, || 0);
        let mut watching = Watching::default();
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let (mut now, mut passes, mut starts) = (0, Vec::new(), 0);
        let mut change = Some(300_000);
        while now <= 6 * periodic {
            if let Some(at) = change
                && now >= at
            {
                scheduler.local_change(at);
                change = None;
            }
            // What the loop does at each wake-up, on Windows.
            if watching.watcher.is_none() {
                watching.watcher = Some(crate::watch::watch(dir.path(), tx.clone()).unwrap());
                watching.started_one(now, &mut scheduler);
                starts += 1;
            }
            match scheduler.next(now) {
                Next::RunPass => {
                    if watching.renew_before_pass(now, periodic) {
                        continue;
                    }
                    scheduler.pass_started();
                    passes.push(now);
                    now += 5_000;
                    scheduler.pass_finished(now, PassEnd::Done { recheck_in: None, needs_approval: false, held: false });
                }
                // The loop also wakes for the status heartbeat every 15 s.
                Next::Wait(Some(at)) => now = at.min(change.unwrap_or(u64::MAX)).min(now + 15_000).max(now + 1),
                other => panic!("{other:?}"),
            }
        }
        let changed = 300_000 + debounce;
        let mut expected = vec![0, changed];
        expected.extend((1..6).map(|i| changed + i * (periodic + 5_000)));
        assert_eq!(passes, expected, "no pass but the change's and one per interval");
        assert_eq!(starts, 6, "the first watcher and one made afresh with each safety-net pass");
    }

    /// The watcher's events for a file waiting to settle run no pass (they set its cheap look instead); any other
    /// path in the event, or an event without paths, does.
    #[test]
    fn a_waiting_files_events_run_no_pass() {
        let stability = Stability::new(Duration::from_secs(2));
        let p = craft_cloud_sync::rel("export.mov");
        let now = stability.now();
        assert!(!stability.lock().look(&p, 1, now_ms(), now, now_ms()), "written just now: it waits");
        let roots = vec![PathBuf::from("/f")];
        let at = |rel: &str| PathBuf::from("/f").join(rel);
        assert!(!counts_as_change(&roots, &stability, &[at("export.mov")], false), "its own writes");
        assert!(stability.lock().next_peek().is_some(), "a cheap look instead");
        assert!(counts_as_change(&roots, &stability, &[at("other.txt")], false));
        assert!(counts_as_change(&roots, &stability, &[at("export.mov"), at("other.txt")], false));
        assert!(counts_as_change(&roots, &stability, &[], false), "an event without paths may be anything");
    }

    #[test]
    fn a_busy_server_is_waited_for_with_jitter() {
        for _ in 0..100 {
            let delay = jittered(Duration::from_secs(10), random());
            assert!(delay >= Duration::from_secs(10) && delay <= Duration::from_secs(15), "{delay:?}");
        }
        assert_eq!(jittered(Duration::ZERO, random()), Duration::ZERO);
    }

    #[test]
    fn the_long_poll_backs_off_up_to_its_cap() {
        let options = RunOptions::default();
        for failures in 1..30 {
            let delay = poll_backoff(&options, failures, random());
            let full = Duration::from_secs(2).saturating_mul(1 << (failures - 1).min(20)).min(Duration::from_secs(300));
            assert!(delay >= full / 2 && delay <= full, "{failures}: {delay:?}");
        }
    }
}
