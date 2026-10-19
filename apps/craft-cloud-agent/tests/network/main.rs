//! The simulation over a simulated network (`turmoil`): the real server (its axum router, the metadata store on
//! SQLite in memory, the blob store in memory) and three agents on hosts of their own, talking HTTP through the
//! agent's own request code ([`HttpTransport`] over a hyper client on turmoil's TCP, see `harness::SimConn`) while
//! the network misbehaves:
//!
//! - **partitions** between an agent and the server, both ways or one way (requests lost, or answers lost after the
//!   server applied the request), which stall connections until the request's timeout ends them;
//! - **held links**: everything between an agent and the server waits, then arrives at once;
//! - **latency** of up to a few hundred milliseconds per message;
//! - **lost answers**: the server applies a request (a commit, an upload) and the connection breaks before its
//!   answer goes out;
//! - **cut answers**: a tree or change-feed page, a chunk or a manifest breaks off halfway;
//! - **small pages**, so the change feed and the tree come in many pages.
//!
//! The agents do the same random folder work as `crates/sync/tests/simulation.rs` (the actions and checks are
//! shared, `common/sim.rs`), and sync at random moments, each machine's passes running while the others' do, so
//! commits from several machines interleave on the server. Then the network heals, everyone syncs until nothing
//! changes, and the same checks run: convergence, nothing the engine saw is lost, nothing live is deleted, nothing
//! is stuck, and the checks after every pass. The server's clock is the simulated one.
//!
//! This file has the random histories and the scenarios; `harness.rs` the server and agent hosts, the connection,
//! the weather and the world a seed runs in.
//!
//! Not simulated: reqwest itself (its connection pool, TLS, the connect and read timeouts; the simulation's
//! connection keeps only the whole-request timeout, and opens a connection per request), the server's `serve`
//! (graceful shutdown; hyper serves the same router here), the `run` loop's own event handling (its scheduler and
//! long-poll do run here, see [`run_loop`]), the file watcher and machine crashes (`tests/simulation.rs` crashes
//! machines).
//!
//! Each seed replays exactly: `SIM_SEED=17 cargo test -p craft-cloud-agent --test network`. By default a few seeds
//! run; `SIM_SEEDS=1000 SIM_FIRST=2000` runs seeds 2000 to 2999 (use `--release`), in parallel on `SIM_JOBS`
//! threads. `SIM_TRACE=1` prints every pass and every change of the network.

#[path = "../docs_common/blobs.rs"]
mod blobs;
#[path = "../../../../crates/sync/tests/common/mod.rs"]
mod common;
mod harness;
mod live;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::sim::{
    Before, Books, Exempt, Linked, Outcome, Rng, act, before_pass, check_pass, check_places, differing, files_of, for_each_seed, hashes_of, listing,
    settle_and_check, tracing,
};
use craft_cloud_agent::daemon::{Polled, RunOptions, long_poll};
use craft_cloud_agent::http::{DeviceToken, HttpTransport, PollError, check_compatible, info_over, login_over};
use craft_cloud_agent::schedule::{Next, PassEnd, Scheduler, Timing};
use craft_cloud_chunker::{Chunker, FASTCDC_V1};
use craft_cloud_proto::{FileHash, NodeId};
use craft_cloud_server::notify::MAX_WAITERS_PER_SPACE;
use craft_cloud_sync::{Fs, RelPath, Report, SyncError, Transport, TransportError};
use harness::{Agent, Fault, MACHINES, Net, SERVER, SIM_EPOCH_MS, Setup, SimConn, server_url, simulate, stamp, weather, world};
use hyper::Method;
use tokio::sync::{mpsc, watch};

// The random histories.

/// A pass under way.
struct Running {
    step: usize,
    before: Before,
    task: tokio::task::JoinHandle<(Agent, Result<Report, SyncError>)>,
}

/// The random history (see the module docs), with the bookkeeping the shared checks keep.
struct Driver {
    seed: u64,
    net: Arc<Net>,
    /// Each machine, unless its pass is under way.
    slots: Vec<Option<Agent>>,
    running: Vec<Option<Running>>,
    links: Vec<Option<Linked>>,
    last_pending: Vec<Vec<NodeId>>,
    hold_dirs: Vec<Option<RelPath>>,
    link_dirs: Vec<Option<RelPath>>,
    /// The shared checks' crash bookkeeping: always none here (no machine crashes over the network, and a lost
    /// commit answer isn't one), so no check is relaxed.
    recovering: Vec<u32>,
    exempt: BTreeSet<Exempt>,
    reuploaded: BTreeSet<FileHash>,
    at_crash: Vec<BTreeSet<FileHash>>,
    seen: BTreeSet<FileHash>,
    destroyed: BTreeSet<FileHash>,
}

/// What a pass did, without node IDs (they are random), for the trace.
fn summary(r: &Result<Report, SyncError>) -> String {
    match r {
        Ok(r) => format!(
            "ok down {:?} up {:?} removed {:?}/{:?} moved {:?}/{:?} dirs {:?} copies {:?} skipped {:?} pending {:?}/{:?} held {:?} refused {}",
            r.downloaded,
            r.uploaded,
            r.removed_locally,
            r.removed_remotely,
            r.moved_locally,
            r.moved_remotely,
            r.created_dirs,
            r.conflict_copies,
            r.skipped,
            r.pending_deletes,
            r.pending_local_deletes,
            r.held_deletes,
            r.refused.len()
        ),
        Err(e) => format!("failed: {}", without_ids(&e.to_string())),
    }
}

/// `text` with every UUID replaced by `<id>`.
fn without_ids(text: &str) -> String {
    let shape =
        |s: &str| s.len() == 36 && s.char_indices().all(|(k, c)| if matches!(k, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.get(..36).is_some_and(shape) {
            out.push_str("<id>");
            rest = rest.get(36..).unwrap_or("");
        } else {
            let mut chars = rest.chars();
            out.extend(chars.next());
            rest = chars.as_str();
        }
    }
    out
}

impl Driver {
    /// Waits for machine `i`'s pass, if one is under way, and checks it.
    async fn join(&mut self, i: usize) {
        let Some(run) = self.running.get_mut(i).and_then(Option::take) else { return };
        let (m, r) = run.task.await.unwrap();
        self.net.event(format!("{} step {} m{i} {}", stamp(), run.step, summary(&r)));
        if tracing() {
            eprintln!("  files {}\n  dirs {:?}", listing(&m), m.fs().dirs());
        }
        let mut books = Books {
            links: &mut self.links,
            last_pending: &mut self.last_pending,
            hold_dirs: &mut self.hold_dirs,
            link_dirs: &mut self.link_dirs,
            recovering: &mut self.recovering,
            exempt: &mut self.exempt,
            reuploaded: &mut self.reuploaded,
            at_crash: &self.at_crash,
        };
        check_pass(self.seed, run.step, i, &m, &r, &run.before, false, &mut books);
        // A commit that applied while its answer never arrived is only traced: the next pass finds its own work on
        // the server by its client IDs (see `a_commit_whose_answer_is_lost_is_found_on_the_server_next_pass`), so
        // the machine isn't treated as recovering from a crash and every check applies in full.
        if self.net.lost_commit_answer(i) {
            self.net.event(format!("m{i} lost the answer to a commit that applied"));
        }
        check_places(self.seed, run.step, i, &m, &r);
        if let Some(slot) = self.slots.get_mut(i) {
            *slot = Some(m);
        }
    }

    /// `steps` random steps: each waits a random moment, then acts on a machine or starts a pass of it (after
    /// waiting for its last one).
    async fn run(&mut self, steps: usize) {
        let seed = self.seed;
        let mut rng = Rng::new(seed);
        // The same streams as in `tests/simulation.rs`, and one for the moments things happen.
        let mut writes = Rng::new(seed ^ 0x57AB_1E5E_ED00_0001);
        let mut extra = Rng::new(seed ^ 0xC04B_0ED0_0000_0002);
        let mut moments = Rng::new(seed ^ 0x7177_0000_0000_0005);
        for step in 0..steps {
            tokio::time::sleep(Duration::from_millis(moments.below(2_000) as u64)).await;
            let i = rng.below(MACHINES);
            self.join(i).await;
            let Some(m) = self.slots.get_mut(i).and_then(Option::as_mut) else { continue };
            if writes.below(10) == 0
                && let Some(f) = writes.pick(&files_of(m)).cloned()
            {
                let on = !m.fs().changing().contains(&f);
                m.fs_mut().set_changing(&f, on);
            }
            if rng.below(3) == 0 {
                self.seen.extend(hashes_of(m));
                match rng.below(4) {
                    0 | 1 => m.approve_deletes(),
                    2 => {
                        let listed = self.last_pending.get(i).cloned().unwrap_or_default();
                        let some: Vec<NodeId> = listed.into_iter().filter(|_| rng.below(2) == 0).collect();
                        m.approve_deletes_of(some);
                    }
                    _ => {}
                }
                if writes.below(3) == 0
                    && let Some(f) = writes.pick(&differing(m)).cloned()
                {
                    m.fs_mut().set_changing(&f, true);
                }
                let before = before_pass(m);
                let Some(mut m) = self.slots.get_mut(i).and_then(Option::take) else { continue };
                let task = tokio::task::spawn_local(async move {
                    let r = m.sync_once().await;
                    (m, r)
                });
                if let Some(slot) = self.running.get_mut(i) {
                    *slot = Some(Running { step, before, task });
                }
            } else {
                let mut no_link = None;
                let link = self.links.get_mut(i).unwrap_or(&mut no_link);
                act(&mut rng, &mut extra, m, &mut self.destroyed, &mut self.exempt, link);
                if tracing() {
                    eprintln!("TRACE step {step} m{i} act\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
                }
            }
        }
        for i in 0..MACHINES {
            self.join(i).await;
        }
    }

    /// Settles on a healed network and runs the end checks.
    async fn finish(mut self) {
        let mut ms: Vec<Agent> = self.slots.drain(..).flatten().collect();
        let out = Outcome {
            links: &mut self.links,
            seen: &mut self.seen,
            destroyed: &self.destroyed,
            recovering: &mut self.recovering,
            exempt: &mut self.exempt,
            reuploaded: &mut self.reuploaded,
            at_crash: &self.at_crash,
        };
        settle_and_check(self.seed, &self.net.server, &mut ms, out).await;
        for (i, m) in ms.iter().enumerate() {
            self.net.event(format!("m{i} ends with {}", listing(m)));
        }
    }
}

/// One seed's history; returns its trace (every pass with its simulated time, every change of the network, every
/// fault, and the folders at the end), which a replay must repeat exactly.
fn run_seed(seed: u64, steps: usize) -> Vec<String> {
    let mut choose = Rng::new(seed ^ 0x5E70_0000_0000_0006);
    let page = [5, 20, 1000][choose.below(3)];
    let latency = [2, 20, 100, 400][choose.below(4)];
    let stormy = choose.below(4) != 0;
    let (lose, cut) = ([0, 6, 25][choose.below(3)], [0, 6, 25][choose.below(3)]);
    let (net, machines) = world(seed, &Setup { page, latency, lose, cut, password: None });
    net.event(format!("seed {seed}: pages of {page}, latency up to {latency} ms, stormy {stormy}, lose 1/{lose}, cut 1/{cut}"));
    let driving = net.clone();
    simulate(seed, latency, &net, async move {
        let calm = Arc::new(AtomicBool::new(false));
        if stormy {
            for i in 0..MACHINES {
                let rng = Rng::new(seed ^ 0x3EA7_0000_0000_0010 ^ i as u64);
                tokio::task::spawn_local(weather(driving.clone(), i, rng, calm.clone()));
            }
        }
        let mut driver = Driver {
            seed,
            net: driving.clone(),
            slots: machines.into_iter().map(Some).collect(),
            running: (0..MACHINES).map(|_| None).collect(),
            links: (0..MACHINES).map(|_| None).collect(),
            last_pending: vec![Vec::new(); MACHINES],
            hold_dirs: vec![None; MACHINES],
            link_dirs: vec![None; MACHINES],
            recovering: vec![0; MACHINES],
            exempt: BTreeSet::new(),
            reuploaded: BTreeSet::new(),
            at_crash: vec![BTreeSet::new(); MACHINES],
            seen: BTreeSet::new(),
            destroyed: BTreeSet::new(),
        };
        driver.run(steps).await;
        // The network heals and the server's faults stop.
        calm.store(true, Ordering::Relaxed);
        for i in 0..MACHINES {
            let host = format!("m{i}");
            turmoil::release(host.as_str(), SERVER);
            turmoil::repair(host.as_str(), SERVER);
        }
        driving.faults.lock().unwrap().on = false;
        driving.event(format!("{} healed", stamp()));
        driver.finish().await;
    });
    std::mem::take(&mut *net.events.lock().unwrap())
}

/// Steps in a history (as in `tests/simulation.rs`).
const STEPS: usize = 120;

#[test]
fn random_histories_converge_over_a_flaky_network() {
    for_each_seed(4, |seed| {
        run_seed(seed, STEPS);
    });
}

/// Runs each of `seeds` (`steps` steps) on `threads` threads that take them in turn; returns each seed's trace.
fn traces_on_threads(seeds: &[u64], steps: usize, threads: usize) -> BTreeMap<u64, Vec<String>> {
    let next = AtomicUsize::new(0);
    let traces = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while let Some(&seed) = seeds.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let trace = run_seed(seed, steps);
                    traces.lock().unwrap().insert(seed, trace);
                }
            });
        }
    });
    traces.into_inner().unwrap()
}

/// The same seed gives the same history, to the simulated millisecond: SQLite's own thread, which the server waits
/// on, takes no simulated time, and the server's clock is the simulated one. Twenty seeds run twice, the second time
/// in the other order on another number of threads, so most replay on another thread than the first time (another
/// hash seed, other scheduling around them).
#[test]
fn a_seed_replays_exactly() {
    let seeds: Vec<u64> = (0..20).collect();
    let first = traces_on_threads(&seeds, 60, 4);
    let backwards: Vec<u64> = seeds.iter().rev().copied().collect();
    let second = traces_on_threads(&backwards, 60, 3);
    for seed in seeds {
        let (Some(a), Some(b)) = (first.get(&seed), second.get(&seed)) else { panic!("seed {seed} didn't run") };
        assert!(a.len() > 8, "seed {seed}: too little happened: {a:?}");
        assert_eq!(a, b, "seed {seed} didn't replay exactly");
    }
}

/// Live documents over the same kind of network (`live.rs`): the documents simulation's random work, with the
/// agents' replicas and the server's sessions talking WebSockets through partitions, held links and latency.
#[test]
fn live_documents_converge_over_a_flaky_network() {
    for_each_seed(2, |seed| {
        live::run_seed(seed);
    });
}

#[test]
fn a_live_seed_replays_exactly() {
    for seed in 0..3 {
        let (a, b) = (live::run_seed(seed), live::run_seed(seed));
        assert!(a.len() > 8, "seed {seed}: too little happened: {a:?}");
        assert_eq!(a, b, "seed {seed} didn't replay exactly");
    }
}

// Scenarios: what the engine and the agent do with each fault, alone.

/// A scenario on a calm network (no faults but the ones it arms; pages of 5, so the feed comes in several), with two
/// of the machines.
fn scenario<Fut: Future<Output = ()> + 'static>(test: impl FnOnce(Arc<Net>, Agent, Agent) -> Fut) {
    let setup = Setup { page: 5, latency: 20, lose: 0, cut: 0, password: None };
    let (net, machines) = world(1, &setup);
    let mut machines = machines.into_iter();
    let (Some(a), Some(b)) = (machines.next(), machines.next()) else { panic!("two machines") };
    simulate(1, setup.latency, &net, test(net.clone(), a, b));
}

fn write(m: &mut Agent, p: &str, data: &[u8]) {
    m.fs_mut().write(&common::path(p), data, false, 0).unwrap();
}

/// The files on the server, as the machine's last pass saw them.
fn server_files(m: &Agent) -> usize {
    m.state().remote.values().filter(|n| n.file.is_some()).count()
}

fn s(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

/// A commit that applied on the server while its answer never arrived: the pass fails, and the next one finds its
/// own work on the server. Client IDs make that clean: the new file isn't a second node, the edit isn't a conflict,
/// the rename and the delete aren't sent again, and the other machine gets exactly the result.
#[test]
fn a_commit_whose_answer_is_lost_is_found_on_the_server_next_pass() {
    scenario(|net, mut a, mut b| async move {
        for name in ["a.txt", "b.txt", "c.txt"] {
            write(&mut a, name, name.as_bytes());
        }
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
        write(&mut a, "new.txt", b"new");
        write(&mut a, "a.txt", b"a, edited");
        a.fs_mut().rename(&common::path("b.txt"), &common::path("d.txt")).unwrap();
        a.fs_mut().remove_file(&common::path("c.txt")).unwrap();
        net.arm(Method::POST, "/api/v1/commit", Fault::Lose);
        let failed = a.sync_once().await;
        assert!(matches!(failed, Err(SyncError::Transport(TransportError::Unavailable(_)))), "{failed:?}");
        assert!(net.lost_commit_answer(0), "the commit applied");
        let commits = net.served(&Method::POST, "/api/v1/commit");
        let report = a.sync_once().await.unwrap();
        assert!(report.conflict_copies.is_empty() && report.refused.is_empty(), "{report:?}");
        assert!(report.uploaded.is_empty() && report.moved_remotely.is_empty() && report.removed_remotely.is_empty(), "{report:?}");
        assert_eq!(net.served(&Method::POST, "/api/v1/commit"), commits, "nothing is committed again");
        assert!(!a.sync_once().await.unwrap().changed());
        b.sync_once().await.unwrap();
        assert_eq!(a.fs().files(), b.fs().files());
        assert_eq!(server_files(&b), 3, "one node per file: {:?}", b.fs().files());
    });
}

/// How many chunks the real chunker (`fastcdc-v1`) cuts `data` into.
fn chunk_count(data: &[u8]) -> usize {
    let chunker = Chunker::new(FASTCDC_V1).unwrap();
    let (mut rest, mut chunks) = (data, 0);
    while !rest.is_empty() {
        rest = rest.get(chunker.cut_point(rest)..).unwrap_or_default();
        chunks += 1;
    }
    chunks
}

/// An upload the server stored while its answer never arrived: the pass fails, and the next one asks which chunks
/// the server lacks and sends exactly the others, not the stored one again. The file has several chunks (counted
/// by the real chunker), so a chunk sent twice shows in the count.
#[test]
fn an_upload_whose_answer_is_lost_is_not_sent_again() {
    scenario(|net, mut a, mut b| async move {
        let mut rng = Rng::new(7);
        let big: Vec<u8> = (0..512 * 1024).flat_map(|_| rng.next().to_le_bytes()).collect();
        let chunks = chunk_count(&big);
        assert!(chunks >= 3, "4 MiB of random bytes make several chunks: {chunks}");
        write(&mut a, "big.bin", &big);
        net.arm(Method::PUT, "/api/v1/chunks/", Fault::Lose);
        let failed = a.sync_once().await;
        assert!(matches!(failed, Err(SyncError::Transport(TransportError::Unavailable(_)))), "{failed:?}");
        let uploads = net.served(&Method::PUT, "/api/v1/chunks/");
        assert_eq!(uploads, 1, "the first chunk went up and the pass stopped");
        a.sync_once().await.unwrap();
        assert_eq!(net.served(&Method::PUT, "/api/v1/chunks/"), uploads + chunks - 1, "every other chunk once, the stored one not again");
        b.sync_once().await.unwrap();
        assert_eq!(b.fs().read(&common::path("big.bin")).unwrap(), big);
    });
}

/// A page of the change feed that breaks off halfway: the pass fails as offline, and the next one reads the feed
/// again and catches up.
#[test]
fn a_change_feed_page_cut_halfway_fails_the_pass_and_the_next_reads_it_again() {
    scenario(|net, mut a, mut b| async move {
        write(&mut a, "first.txt", b"first");
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
        for i in 0..12 {
            write(&mut a, &format!("f{i}.txt"), format!("file {i}").as_bytes());
        }
        a.sync_once().await.unwrap();
        // The first page comes whole, the second breaks off.
        net.arm(Method::GET, "/api/v1/changes", Fault::None);
        net.arm(Method::GET, "/api/v1/changes", Fault::Cut);
        let failed = b.sync_once().await;
        assert!(matches!(failed, Err(SyncError::Transport(TransportError::Unavailable(_)))), "{failed:?}");
        b.sync_once().await.unwrap();
        assert_eq!(a.fs().files(), b.fs().files());
    });
}

/// A long-poll whose connection a partition cuts: no answer comes, and the request's timeout (the wait plus the
/// usual cap for a small answer: 30 s + 60 s + 2 × 15 s) ends it, at its time to the step. Once the network heals,
/// a poll works again and a change on the server ends it at once.
#[test]
fn a_long_poll_cut_off_by_a_partition_ends_at_its_timeout() {
    scenario(|_net, mut a, mut b| async move {
        write(&mut a, "first.txt", b"first");
        a.sync_once().await.unwrap();
        // The next pass reads the feed past its own commit.
        a.sync_once().await.unwrap();
        let since = a.state().seq;
        let wait = s(30);
        let start = turmoil::elapsed();
        let poll = tokio::task::spawn_local(async move {
            let r = a.transport().wait_for_changes(since, wait).await;
            (a, r)
        });
        tokio::time::sleep(s(5)).await;
        turmoil::partition("m0", SERVER);
        let (a, r) = poll.await.unwrap();
        let took = turmoil::elapsed() - start;
        assert!(matches!(r, Err(PollError::Failed(TransportError::Unavailable(_)))), "{r:?} after {took:?}");
        assert!(took >= s(120) && took < Duration::from_millis(120_100), "{took:?}");
        turmoil::repair("m0", SERVER);
        let start = turmoil::elapsed();
        let poll = tokio::task::spawn_local(async move { a.transport().wait_for_changes(since, wait).await });
        tokio::time::sleep(s(5)).await;
        write(&mut b, "second.txt", b"second");
        b.sync_once().await.unwrap();
        let seq = poll.await.unwrap();
        assert!(seq.as_ref().is_ok_and(|seq| *seq > since), "{seq:?}");
        assert!(turmoil::elapsed() - start < s(10), "the change ended the wait");
    });
}

/// A space whose long-poll slots are all taken (another machine holds every one): the server turns the next poll
/// away with `429` and `Retry-After: 5`, which the agent reads as busy for 5 s, and the `run` loop's long-poll waits
/// that long (plus up to half of it) before each new try, never in a hot loop. Once a slot is free, its poll waits
/// again, and a change on the server ends it.
#[test]
fn a_busy_server_turns_a_long_poll_away_and_it_waits_for_retry_after() {
    scenario(|net, mut a, mut b| async move {
        write(&mut a, "first.txt", b"first");
        a.sync_once().await.unwrap();
        a.sync_once().await.unwrap();
        b.sync_once().await.unwrap();
        let since = a.state().seq;
        let start = turmoil::elapsed();
        let mut waiting = Vec::new();
        for _ in 0..MAX_WAITERS_PER_SPACE {
            let transport = b.transport().clone();
            waiting.push(tokio::task::spawn_local(async move { transport.wait_for_changes(since, s(60)).await }));
        }
        tokio::time::sleep(s(1)).await;
        assert_eq!(a.transport().wait_for_changes(since, s(30)).await, Err(PollError::Busy(Some(s(5)))));
        // The run loop's long-poll.
        let (_since, since_rx) = watch::channel(Some(since));
        let (tx, mut polled) = mpsc::unbounded_channel();
        let mut jitter = Rng::new(5);
        let polling = turmoil::elapsed();
        let poll = tokio::task::spawn_local(long_poll(
            a.transport().clone(),
            since_rx,
            move |p| tx.send(p).is_ok(),
            RunOptions::default(),
            move || jitter.next(),
        ));
        tokio::time::sleep(s(40)).await;
        let turned: Vec<Duration> = net.polls_of(0).into_iter().filter(|at| *at >= polling).collect();
        assert!((4..=9).contains(&turned.len()), "{turned:?}");
        let gaps: Vec<Duration> = turned.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.iter().all(|g| *g >= s(5) && *g <= Duration::from_millis(7_600)), "Retry-After, with jitter: {gaps:?}");
        // The slots free up after their 60 s; then a change on the server reaches the loop.
        for w in waiting {
            assert_eq!(w.await.unwrap(), Ok(since), "nothing changed while they waited");
        }
        assert!(turmoil::elapsed() - start < s(62));
        tokio::time::sleep(s(10)).await;
        write(&mut b, "second.txt", b"second");
        b.sync_once().await.unwrap();
        let told = tokio::time::timeout(s(10), polled.recv()).await;
        assert_eq!(told, Ok(Some(Polled::Changed)), "the change reached the loop");
        poll.abort();
    });
}

/// `info` and the sign-in go over the simulated network too, with the server's own sign-in handler (its rate limit
/// keyed on the client's address). The server's clock is the simulated one: a new device's creation time and its
/// last use are simulated times, and the last use is written again once a minute of simulated time went by (with
/// the real clock, the few milliseconds of real time this takes would never be a minute).
#[test]
fn info_and_sign_in_go_over_the_network_and_the_server_keeps_simulated_time() {
    let setup = Setup { page: 5, latency: 20, lose: 0, cut: 0, password: Some("correct horse battery") };
    let (net, _machines) = world(2, &setup);
    let on = net.clone();
    simulate(2, setup.latency, &net, async move {
        let conn = SimConn { host: 0, net: on.clone() };
        let url = server_url();
        check_compatible(&info_over(&conn, &url).await.unwrap()).unwrap();
        let wrong = login_over(&conn, &url, "bo", "wrong horse battery", "laptop").await;
        assert!(matches!(wrong, Err(TransportError::Unauthorized)), "{wrong:?}");
        tokio::time::sleep(s(100)).await;
        let signed_in = login_over(&conn, &url, "bo", "correct horse battery", "laptop").await.unwrap();
        let transport = HttpTransport::over(conn.clone(), url, &DeviceToken::new(signed_in.token)).unwrap();
        assert_eq!(transport.space().await.unwrap().username, "bo");
        // The server's time of day now, by the simulated clock.
        let now = || SIM_EPOCH_MS + i64::try_from(turmoil::sim_elapsed().unwrap_or_default().as_millis()).unwrap();
        let meta = on.server.meta.clone();
        let device = || {
            let meta = meta.clone();
            async move {
                let bo = meta.user_by_name("bo").await.unwrap().unwrap();
                meta.list_devices(bo.id).await.unwrap().into_iter().next().unwrap()
            }
        };
        // The server records a device's use after answering (off the request's path): a moment of simulated time
        // lets it, since simulated time stands still while the database works.
        tokio::time::sleep(s(1)).await;
        // Taken before the read: the read goes to the database's own thread, which simulated time doesn't hold.
        let at = now();
        let first = device().await;
        assert!((at - 2_000..=at).contains(&first.created_ms), "created at {} by a clock at {at}", first.created_ms);
        assert!((at - 2_000..=at).contains(&first.last_used_ms), "used at {} by a clock at {at}", first.last_used_ms);
        tokio::time::sleep(s(30)).await;
        transport.space().await.unwrap();
        tokio::time::sleep(s(1)).await;
        assert_eq!(device().await.last_used_ms, first.last_used_ms, "written at most once a minute");
        tokio::time::sleep(s(40)).await;
        transport.space().await.unwrap();
        tokio::time::sleep(s(1)).await;
        let later = device().await.last_used_ms;
        assert!(later >= first.last_used_ms + 70_000 && later <= now(), "a minute of simulated time later: {later}");
    });
}

// The `run` loop on simulated time.

/// The `run` loop's scheduling as `daemon::run_loop` does it, on simulated time: its [`Scheduler`] (with the default
/// timings) and its [`long_poll`], with passes run inline instead of on the pass thread, and the user's writes
/// handed in (they say what the watcher would) instead of a watcher. Runs until `until` (simulated time since it
/// started); returns the machine and when each pass started, and whether it finished.
///
/// This is a copy of `run_loop`'s event handling, not that code: it maps every failed pass to `PassEnd::Failed`
/// (the real loop also pauses for what needs the user, and passes `recheck_in`), and a change to `run_loop`'s events
/// doesn't reach it. Driving the real loop needs its core split from the pass thread, the watcher and the status
/// file (a follow-up).
async fn run_loop(mut a: Agent, mut user: mpsc::UnboundedReceiver<(RelPath, Vec<u8>)>, until: Duration) -> (Agent, Vec<(Duration, bool)>) {
    let start = tokio::time::Instant::now();
    let now = || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut jitter = Rng::new(11);
    let mut scheduler = Scheduler::new(Timing::default(), 0, move || jitter.next());
    let (since_tx, since_rx) = watch::channel(None);
    let (polled_tx, mut polled) = mpsc::unbounded_channel();
    let mut poll_jitter = Rng::new(12);
    let tell = move |p: Polled| polled_tx.send(p).is_ok();
    let poll = tokio::task::spawn_local(long_poll(a.transport().clone(), since_rx, tell, RunOptions::default(), move || poll_jitter.next()));
    let mut passes = Vec::new();
    while start.elapsed() < until {
        match scheduler.next(now()) {
            Next::RunPass => {
                scheduler.pass_started();
                passes.push((start.elapsed(), false));
                let r = a.sync_once().await;
                if a.state().space.is_some() {
                    let _ = since_tx.send(Some(a.state().seq));
                }
                let end = match &r {
                    Ok(report) => {
                        if let Some(last) = passes.last_mut() {
                            last.1 = true;
                        }
                        PassEnd::Done { recheck_in: None, needs_approval: !report.pending_nodes.is_empty(), held: !report.held_nodes.is_empty() }
                    }
                    Err(e) => PassEnd::Failed(e.to_string()),
                };
                scheduler.pass_finished(now(), end);
            }
            Next::CheckApproval => scheduler.approval_checked(now()),
            Next::Exit => break,
            Next::Wait(at) => {
                let left = u64::try_from(until.saturating_sub(start.elapsed()).as_millis()).unwrap_or(u64::MAX);
                let wake = at.map_or(u64::MAX, |at| at.saturating_sub(now())).min(left);
                tokio::select! {
                    Some(p) = polled.recv() => match p {
                        Polled::Changed => scheduler.urgent_change(),
                        Polled::Back => scheduler.server_back(),
                    },
                    Some((p, data)) = user.recv() => {
                        a.fs_mut().write(&p, &data, false, 0).unwrap();
                        scheduler.local_change(now());
                    }
                    () = tokio::time::sleep(Duration::from_millis(wake)) => {}
                }
            }
        }
    }
    poll.abort();
    (a, passes)
}

/// The loop over a partition: a change on the server reaches it within a few seconds; cut off for ten minutes, with
/// a change on each side, its passes and polls back off (no hot loop: turmoil refuses a new connection across a
/// partition at once, and the next try waits longer each time, up to 5 minutes); once the network heals, the
/// long-poll's first answer runs the pending retry at once and both changes sync; idle afterwards, it polls every
/// 30 s and runs no pass but the 10-minute safety net.
#[test]
fn the_run_loop_backs_off_through_a_partition_and_recovers_when_it_heals() {
    scenario(|net, a, mut b| async move {
        let t0 = turmoil::elapsed();
        let (user, rx) = mpsc::unbounded_channel();
        let looping = tokio::task::spawn_local(run_loop(a, rx, s(1_800)));
        tokio::time::sleep(s(10)).await;
        write(&mut b, "from-b.txt", b"b");
        b.sync_once().await.unwrap();
        let synced_b = turmoil::elapsed() - t0;
        tokio::time::sleep(s(50)).await;
        turmoil::partition("m0", SERVER);
        tokio::time::sleep(s(10)).await;
        write(&mut b, "during-b.txt", b"b, offline here");
        b.sync_once().await.unwrap();
        tokio::time::sleep(s(30)).await;
        user.send((common::path("during-a.txt"), b"a, offline".to_vec())).unwrap();
        tokio::time::sleep(s(560)).await;
        turmoil::repair("m0", SERVER);
        let healed = turmoil::elapsed() - t0;
        let (a, passes) = looping.await.unwrap();
        b.sync_once().await.unwrap();
        assert_eq!(a.fs().files(), b.fs().files(), "both changes synced after the partition");
        // A change on the server: the long-poll wakes the loop (its pass may start while the other machine's is
        // still finishing).
        let first = passes.iter().find(|(at, ok)| *ok && *at > s(10)).map(|(at, _)| *at);
        assert!(first.is_some_and(|at| at < synced_b + s(3)), "{passes:?}");
        // Cut off: passes and polls back off (from 2 s, doubling with jitter, up to 5 minutes).
        let cut = (s(60), healed);
        let failed: Vec<Duration> = passes.iter().filter(|(at, ok)| !ok && *at >= cut.0 && *at < cut.1).map(|(at, _)| *at).collect();
        assert!((2..=10).contains(&failed.len()), "passes while cut off: {passes:?}");
        let gaps: Vec<Duration> = failed.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).all(|g| g[1] > g[0] / 2 && g[1] >= s(1)), "backing off: {gaps:?}");
        let polls = |from: Duration, to: Duration| net.polls_of(0).into_iter().filter(|at| *at >= t0 + from && *at < t0 + to).count();
        assert!(polls(cut.0, cut.1) <= 8, "polls while cut off: {}", polls(cut.0, cut.1));
        // Healed: the pending retry runs once the long-poll gets an answer, within its longest backoff and a poll.
        let back = passes.iter().find(|(at, ok)| *ok && *at > healed).map(|(at, _)| *at);
        assert!(back.is_some_and(|at| at < healed + s(300 + 30)), "healed at {healed:?}: {passes:?}");
        // Idle: a poll every 30 s, and no pass but the 10-minute safety net.
        let idle = (healed + s(500), s(1_800));
        assert!((20..=22).contains(&polls(idle.0, idle.1)), "polls while idle: {}", polls(idle.0, idle.1));
        assert!(passes.iter().filter(|(at, _)| *at >= idle.0).count() <= 1, "{passes:?}");
    });
}
